// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W5a 档2 test client（**experiments harness，非生产**）。
//!
//! 在真 OpenHCL VTL2 内跑：open /dev/mshv_vtl_low → mmap 一段真 guest RAM →
//! connect 真 nvme_firmware AF_UNIX server → 握手 → DMA_MAP 把 mshv_vtl_low fd
//! （真 guest RAM）传给 firmware → 驱 NVMe Identify（CC.EN/SQE/doorbell）→
//! firmware 零拷贝 DMA 进真 guest RAM → client 从 guest RAM 读回 Identify MN 验证。
//!
//! 端口自 POC-1 `poclib.py` 的 NVMe bring-up，但用真 vfio_user_device 客户端 API
//! + 真 guest 物理内存（非 memfd）。**含 unsafe（mmap 设备 fd）——harness 性质，
//! 不在 deny-clean 的 vfio_user_device crate 内。**

use anyhow::Context as _;
use anyhow::bail;
use nix::sys::mman::{MapFlags, ProtFlags, mmap};
use pal_async::DefaultPool;
use std::num::NonZeroUsize;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use vfio_user_device::VfioUserClient;
use vfio_user_wire::proto::pci_region;

// NVMe BAR0 寄存器 offset（nvme_firmware regs.rs）
const R_CC: u64 = 0x14;
const R_CSTS: u64 = 0x1C;
const R_AQA: u64 = 0x24;
const R_ASQ: u64 = 0x28;
const R_ACQ: u64 = 0x30;
const SQ0TDBL: u64 = 0x1000;
const CC_VALUE: u32 = (4 << 20) | (6 << 16) | 1; // IOCQES=4, IOSQES=6, EN=1
const CSTS_RDY: u32 = 1;
const QDEPTH: u32 = 2;

// 真 guest RAM 布局：GPA_BASE 起一段（env 可调）。queues/PRP 相对 GPA_BASE 偏移。
// 默认 0x100000(1MiB)，POC-3/6 在此 GPA 写 marker 真机存活过。
const RAM_LEN: u64 = 64 * 1024;
const OFF_ASQ: u64 = 0x0000;
const OFF_ACQ: u64 = 0x1000;
const OFF_PRP1: u64 = 0x2000; // Identify 数据落点

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k)
        .ok()
        .and_then(|s| {
            let s = s.trim().to_string();
            match s.strip_prefix("0x") {
                Some(h) => u64::from_str_radix(h, 16).ok(),
                None => s.parse().ok(),
            }
        })
        .unwrap_or(d)
}

fn main() -> anyhow::Result<()> {
    let sock = std::env::args().nth(1).unwrap_or_else(|| "/tmp/fw.sock".into());
    let gpa_base = env_u64("W5A_GPA_BASE", 0x100000);
    eprintln!("[w5a-client] sock={sock} gpa_base={gpa_base:#x} ram_len={RAM_LEN:#x}");

    // W6a: client 已 async（pal_async PolledSocket）→ 整个 NVMe bring-up 包进
    // DefaultPool::run_with，client 调用全 .await。mmap/ram 操作放闭包内最前（不涉 async）。
    DefaultPool::run_with(async |driver| -> anyhow::Result<()> {
        // 1. open /dev/mshv_vtl_low + mmap [gpa_base, gpa_base+RAM_LEN) = 真 guest RAM。
        let dev = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/mshv_vtl_low")
            .context("open /dev/mshv_vtl_low")?;
        let nz = NonZeroUsize::new(RAM_LEN as usize).unwrap();
        // SAFETY: mmap 真 guest RAM 设备 fd，file_offset=GPA（非-CVM 裸 GPA，POC-3 验）。
        // 长度 RAM_LEN，PROT_READ|WRITE，MAP_SHARED；harness 进程独占该映射。
        let addr = unsafe {
            mmap(
                None,
                nz,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                &dev,
                gpa_base as i64,
            )
            .context("mmap mshv_vtl_low @ gpa_base")?
        };
        let ram_ptr = addr.as_ptr() as *mut u8;
        // SAFETY: addr 为 mmap 成功返回的 RAM_LEN 字节有效映射。
        let ram = unsafe { std::slice::from_raw_parts_mut(ram_ptr, RAM_LEN as usize) };
        ram.fill(0); // 清零工作区（ASQ/ACQ/PRP）。

        // 2. connect firmware + 握手 + enumerate。
        let stream = UnixStream::connect(&sock).context("connect firmware sock")?;
        let mut client = VfioUserClient::from_stream(&driver, stream).context("wrap stream")?;
        let neg = client.handshake().await.context("handshake")?;
        eprintln!(
            "[w5a-client] handshake ok: server {}.{}",
            neg.server_major, neg.server_minor
        );
        let info = client.get_device_info().await.context("get_device_info")?;
        let nr = info.num_regions;
        let ni = info.num_irqs;
        eprintln!("[w5a-client] num_regions={nr} num_irqs={ni}");

        // 3. DMA_MAP：真 guest RAM [gpa_base, +RAM_LEN) 经 mshv_vtl_low fd 传给 firmware
        //    （READABLE|WRITEABLE=0x3），fd_offset=gpa_base（firmware mmap 也按此 offset）。
        client
            .dma_map(gpa_base, RAM_LEN, 0x1 | 0x2, dev.as_fd(), gpa_base)
            .await
            .context("dma_map guest RAM")?;
        eprintln!("[w5a-client] dma_map [{gpa_base:#x}, +{RAM_LEN:#x}) ok");

        // 4. NVMe enable：写 AQA/ASQ/ACQ/CC.EN + poll CSTS.RDY。queues GPA = gpa_base+offset。
        let asq = gpa_base + OFF_ASQ;
        let acq = gpa_base + OFF_ACQ;
        let prp1 = gpa_base + OFF_PRP1;
        let aqa: u32 = ((QDEPTH - 1) << 16) | (QDEPTH - 1);
        client.region_write(pci_region::BAR0, R_AQA, &aqa.to_le_bytes()).await?;
        client.region_write(pci_region::BAR0, R_ASQ, &asq.to_le_bytes()).await?;
        client.region_write(pci_region::BAR0, R_ACQ, &acq.to_le_bytes()).await?;
        client.region_write(pci_region::BAR0, R_CC, &CC_VALUE.to_le_bytes()).await?;
        let mut ready = false;
        for _ in 0..100 {
            let csts = client.region_read(pci_region::BAR0, R_CSTS, 4).await?;
            let v = u32::from_le_bytes(csts[..4].try_into().unwrap());
            if v & CSTS_RDY != 0 {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !ready {
            bail!("CSTS.RDY never set");
        }
        eprintln!("[w5a-client] controller enabled (CSTS.RDY)");

        // 5. 在真 guest RAM 写 Identify Controller SQE（opcode 0x06, CNS=0x01, PRP1）。
        //    ASQ 第 0 个 SQE 在 asq 偏移处 = ram[OFF_ASQ..]。
        let sqe = build_identify_sqe(1, prp1);
        ram[(OFF_ASQ as usize)..(OFF_ASQ as usize + 64)].copy_from_slice(&sqe);

        // 6. ring SQ0 tail doorbell = 1（提交一个 SQE）。
        client.region_write(pci_region::BAR0, SQ0TDBL, &1u32.to_le_bytes()).await?;

        // 7. poll CQE（在 acq 偏移处）。CQE DW3 bit16 = phase bit（初值 0，首 CQE 翻 1）。
        let mut completed = false;
        for _ in 0..100 {
            let dw3 = u32::from_le_bytes(
                ram[(OFF_ACQ as usize + 12)..(OFF_ACQ as usize + 16)]
                    .try_into()
                    .unwrap(),
            );
            if dw3 & (1 << 16) != 0 {
                completed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !completed {
            bail!("Identify CQE phase bit never flipped (firmware 未完成 / DMA 未命中真 guest RAM)");
        }

        // 8. 从真 guest RAM 读回 Identify 数据的 Model Number（offset 24..64，40 字节）。
        //    零拷贝命中证据：firmware 在 VTL2 经 mmap 把 Identify 数据写进了 client mmap
        //    的同一真 guest 物理页。
        let mn_bytes = &ram[(OFF_PRP1 as usize + 24)..(OFF_PRP1 as usize + 64)];
        let mn = String::from_utf8_lossy(mn_bytes);
        let mn_trim = mn.trim();
        eprintln!("[w5a-client] Identify MN = {mn_trim:?}");
        if !mn_trim.contains("OpenHCL Userspace NVMe") {
            bail!("Identify MN 不含预期串（零拷贝 DMA 未落进真 guest RAM？）：{mn_trim:?}");
        }

        eprintln!(
            "[w5a-client] W5A PASSED ✓ — firmware-in-VTL2 经 DMA_MAP 真 guest RAM 零拷贝 Identify 端到端"
        );
        Ok(())
    })?;
    Ok(())
}

/// 64-byte Identify Controller SQE（opcode 0x06, CNS=0x01）。
fn build_identify_sqe(cid: u16, prp1: u64) -> [u8; 64] {
    let mut sqe = [0u8; 64];
    sqe[0] = 0x06; // opcode Identify
    sqe[2..4].copy_from_slice(&cid.to_le_bytes()); // CID @ offset 2
    sqe[24..32].copy_from_slice(&prp1.to_le_bytes()); // PRP1 @ offset 24
    sqe[40..44].copy_from_slice(&1u32.to_le_bytes()); // CDW10 CNS=0x01 (Identify Controller)
    sqe
}
