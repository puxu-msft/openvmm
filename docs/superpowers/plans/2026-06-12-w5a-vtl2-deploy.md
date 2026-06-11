# W5a（档2）— firmware-in-VTL2 真机部署 + DMA 零拷贝真 NVMe IO

> **执行方式：inline（控制者本人执行，非 subagent-driven）。** 单 commit。控制者能经 `ohcldiag-dev.exe` 直驱真 OpenHCL VM `pcie-remote-exp`（VTL2 reachable，已验）。
>
> **Plan type: 真机部署 harness（非纯 standalone）。** firmware + test client cross-build 静态 musl（WSL 本地），committed harness（对标 `hyperv_interop/`），**真 VM e2e 由控制者经 ohcldiag-dev 跑**。
>
> **⚠️ 单 commit 纪律（用户 2026-06-11）**：整个 W5a 合成**一个 commit**。见 [[commit-granularity-coarse-not-per-step]]。
>
> **scope（explore + 用户拍板档2）**：W5a = **firmware-as-VTL2-进程 dev 部署 + DMA 零拷贝真 NVMe IO**（POC-1/3/6 真机汇合：真 nvme_firmware ELF 在真 VTL2 跑，test client 经真 vfio_user_device 客户端 API 驱 NVMe，firmware 零拷贝 DMA 进**真 guest RAM**(/dev/mshv_vtl_low)，client 验回）。**W5b（underhill supervisor）≡ W6**（explore 坐实：需改 underhill_core，standalone 测不了，spec 自承"W5b 起依赖 W6"）→ 推迟并入 W6。**W5c（IGVM 生产）** 远期。spec §6 W5 落地后记此 scope。
>
> **POC 已验承重假设**：完整 nvme_firmware `--no-default-features --features vfio-user --target x86_64-unknown-linux-musl` → **static-pie musl ELF**（statically linked，已实测）。VTL2 reachable + `/dev/mshv_vtl_low` 存在（HCL kernel 6.18，已探）。
>
> **⚠️ 真机 POC 揪出 W3 真 bug（必须先修，architect review B2）**：`/dev/mshv_vtl_low` 是**字符设备 st_size=0**（已 fstat 实测）。server `map_dma_fd`（dma.rs:464-475）的 fstat `st_size` 上界校验（防 SIGBUS，对 memfd 有意义）会**拒绝字符设备**（`offset+size > 0`）→ 退回 message 路径 → 而 `VfioUserClient` 是纯同步请求/应答、**不实现 server-initiated DMA pump** → 连接挂。**W3 只用 memfd（有 st_size）测，漏了真目标（mshv_vtl_low 字符设备）。** 真 OpenHCL guest RAM fd 永远是字符设备 → **零拷贝在真目标上从未真正工作过**。W5a 真机 e2e 首次暴露。**修**：`map_dma_fd` 仅对**普通文件**(S_IFREG)施 st_size 上界（memfd SIGBUS 防护）；字符/块设备跳过（mmap 有效性由驱动的 GPA-range mmap handler 定，POC-6 证裸 mmap @ file_offset=GPA 工作）。**这是 W5a 必含的生产修复（碰 vfio_user_transport），不是 firmware 零改**——real-VM surfaced 的真 gap。

**Goal:** 把真 `nvme_firmware` ELF + 一个 test client ELF 经 `ohcldiag-dev run` + base64 stdin 推进真 OpenHCL VM 的 VTL2，firmware 起 AF_UNIX server；test client（用真 vfio_user_device 客户端 API）connect → 握手 → `dma_map` 真 guest RAM(/dev/mshv_vtl_low) fd 给 firmware → 驱 NVMe Identify（CC.EN/SQE/doorbell）→ firmware 零拷贝 DMA 进真 guest RAM → client 从 guest RAM 读回 Identify MN `"OpenHCL Userspace NVMe v2.0"` 验证零拷贝命中真 guest 物理内存。committed harness 固化全流程。

**Architecture:** firmware = 现有 `nvme_firmware` bin（cross-build 静态 musl，零新代码）。test client = experiments/ 下新 Rust musl bin（**unsafe-ok harness，不污染 deny-clean 的 vfio_user_device**），依赖 `vfio_user_device`（W1-W4 客户端 API）+ `vfio_user_wire`（proto 常量）+ `nix`（mmap mshv_vtl_low）。harness 把两个 ELF 打 tar → base64 → 单条 `ohcldiag-dev run` stdin 推进 VTL2，sh 解包 + 后台起 firmware + 跑 client + 收 oracle。

**Tech Stack:** Rust 2024，rustc 1.95，`x86_64-unknown-linux-musl`（`.cargo/config.toml:78` 已有 underhill musl 链）；test client harness 用 nix（socket+uio+mman）+ unsafe mmap（experiments 允许）。

**Spec 来源：** `docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md` §6 W5。

**前置 commit：** `a02a33cd`（W4）。

**真机参数**：VM `pcie-remote-exp`（Running）；`ohcldiag-dev.exe` 在 `/mnt/c/temp/pcie_remote_exp/ohcldiag-dev.exe`；VTL2 HCL kernel 6.18，root，`/dev/mshv_vtl_low` 存在、只支持 mmap（POC-3：不支持 read()）。

**回退指引：** W5a 全程未 commit 前 `git checkout -- <file>` / `rm -rf experiments/2026-06-12-*`。真 VM 跑挂（guest 失稳）= 真发现（指向需 W6 underhill-mediated GuestMemory），记录不强推。不碰 `usnvmemu/docs/DECISIONS.md`（别会话）。

---

## Baseline（起手必跑）

```bash
# firmware 静态 musl 可编（已 POC 验，复确认）
cd /home/xp/refs/openvmm/usnvmemu/crates/nvme_firmware && \
  cargo build --bin nvme_firmware --no-default-features --features vfio-user \
    --target x86_64-unknown-linux-musl --release 2>&1 | tail -2
file target/x86_64-unknown-linux-musl/release/nvme_firmware | grep -o "statically linked\|static-pie" 
# VM 可达
cd /mnt/c/temp/pcie_remote_exp && timeout 20 ./ohcldiag-dev.exe pcie-remote-exp run /bin/sh -- -c 'echo VTL2-OK; ls /dev/mshv_vtl_low; which tar base64' 2>&1 | tr -d '\r' | head
```
预期：firmware release static ELF 编出；VTL2 echo OK + mshv_vtl_low 存在 + tar/base64 可用。**若 VTL2 无 tar**：改用两段 base64 拼接投递（见 Stage 3 备选）。

---

## File Structure

新建（全在 experiments/，不碰 4 个 crate 的生产代码）：
- `usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/client/Cargo.toml` —— test client musl bin（`[workspace]` 空表，独立解析）
- `usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/client/src/main.rs` —— NVMe bring-up + DMA_MAP 真 guest RAM
- `usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/build_and_stage.sh` —— cross-build firmware + client 静态 musl + strip
- `usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/run_e2e.sh` —— tar + base64 + ohcldiag-dev run 推送 + 收 oracle
- `usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/README.md` —— 流程 + 真机结果

修改：
- `docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md` §6 W5 —— 记 W5a 完成 + W5b≡W6/W5c 推迟 scope

**不动**：vfio_user_wire / vfio_user_device / nvme_firmware 生产代码（firmware bin 只是换 target 编，零改）。**改 vfio_user_transport/src/dma.rs `map_dma_fd`**（Stage 0，char-device fstat 修复，real-VM surfaced 的真 bug）。

---

## Stage 0 — 修 `map_dma_fd` char-device fstat gate（W3 真 bug，real-VM surfaced）

**Files:** `usnvmemu/crates/vfio_user_transport/src/dma.rs`

`/dev/mshv_vtl_low` 字符设备 st_size=0 → 现 fstat 上界校验 `offset+size > 0` 必拒 → 零拷贝失效（详见 plan 头 ⚠️）。修：仅普通文件施 st_size 上界。

- [ ] **Step 0.1: map_dma_fd 仅对 S_IFREG 施 st_size 上界**

`dma.rs` 的 `map_dma_fd`，把 fstat 上界校验改为只对普通文件生效：

```rust
    let st = nix::sys::stat::fstat(fd.as_fd())
        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
    let file_len = u64::try_from(st.st_size).unwrap_or(0);
    let end = offset
        .checked_add(size)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "offset+size 溢出"))?;
    // **W5a real-VM 修复** — fstat `st_size` 上界（防 SIGBUS）仅对**普通文件**
    // （memfd）有意义。真 OpenHCL guest RAM fd 是**字符设备** /dev/mshv_vtl_low
    // （st_size=0），其 mmap 有效性由驱动的 GPA-range mmap handler 决定，非 st_size。
    // 对字符/块设备施此上界会误拒（offset+size > 0）→ 零拷贝失效（W3 仅用 memfd 测，
    // 漏了真目标）。故仅普通文件校验上界；设备 fd 跳过，由 mmap+驱动 enforce。
    use nix::sys::stat::SFlag;
    let is_regular =
        SFlag::from_bits_truncate(st.st_mode as nix::libc::mode_t) & SFlag::S_IFMT == SFlag::S_IFREG;
    if is_regular && end > file_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("DMA_MAP offset+size {end} 超出 fd 真实大小 {file_len}（防 SIGBUS）"),
        ));
    }
```

并更新两处 `// SAFETY:` 注释的不变量 #1（原说"fstat 已校验 offset+size ≤ st_size 故有真实页 backing"）：补一句"——对字符设备（st_size=0）此上界跳过，backing 由设备驱动的 mmap handler 保证（mshv_vtl_low 映射真 guest 物理页，POC-6 验）；越界 GPA 由驱动 mmap 失败 enforce，非 fstat"。

注意：`st.st_mode` 类型在 nix 0.30 是 `mode_t`（Linux u32）；`SFlag::from_bits_truncate` 接 `mode_t`。`nix::libc::mode_t` 或直接 `st.st_mode`（已是 mode_t）——执行时按 nix 0.30 FileStat.st_mode 实际类型调（可能无需 `as`）。

- [ ] **Step 0.2: 验证 server 测不退化（memfd 路径不变）**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && \
  cargo test --lib 2>&1 | grep "test result" && \
  cargo clippy --lib --tests -- -D warnings 2>&1 | tail -1
```
预期：68 passed 不退化（memfd = 普通文件，仍走 st_size 校验 → `dma_map_fd_smaller_than_declared_size_falls_back_no_sigbus` 等回归测照常通过）+ clippy clean。char-device 路径由 W5a 真机 e2e 验（无 mshv 的单测覆盖不了）。

---

## Stage 1 — firmware 静态 musl release（formalize 进 build script）

POC 已验 debug 可编；W5a 用 release（小、strip）。具体在 Stage 3 的 build_and_stage.sh 里调，本 stage 只确认 release 也通 + 拿 MN 串作 oracle 锚。

- [ ] **Step 1.1: 确认 release 静态 musl + 提取 firmware 广告的 MN 串**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/nvme_firmware && \
  cargo build --bin nvme_firmware --no-default-features --features vfio-user \
    --target x86_64-unknown-linux-musl --release 2>&1 | tail -2
BIN=target/x86_64-unknown-linux-musl/release/nvme_firmware
file $BIN; ldd $BIN 2>&1 | head -1
# 找 firmware Identify 广告的 Model Number（client oracle 要比对）
grep -rn "OpenHCL Userspace NVMe" /home/xp/refs/openvmm/usnvmemu/crates/nvme_firmware/src/ | head -2
```
预期：release static ELF + MN 串确认（poc1 验过是 `OpenHCL Userspace NVMe v2.0`；以 grep 实际为准）。

---

## Stage 2 — test client harness bin（experiments/，unsafe-ok）

**Files:** `experiments/2026-06-12-openhcl-vtl2-deploy/client/{Cargo.toml,src/main.rs}`

### Step 2.1: client Cargo.toml

```toml
[package]
name = "w5a_vtl2_client"
version = "0.1.0"
edition = "2024"
license = "MIT"

# experiments 独立 crate（不进 workspace；自解析 deps）。
[workspace]

[dependencies]
vfio_user_device = { path = "../../../crates/vfio_user_device" }
vfio_user_wire = { path = "../../../crates/vfio_user_wire" }
nix = { version = "0.30", features = ["socket", "uio", "mman"] }
anyhow = "1.0"

[profile.release]
strip = true
```

注：本 crate **允许 unsafe**（harness，需 mmap mshv_vtl_low），不设 `#![deny(unsafe_code)]`。

### Step 2.2: client main.rs

逻辑 = poclib.py 的 NVMe bring-up，但：(a) Rust，(b) 用真 `vfio_user_device::VfioUserClient` API（W1-W4），(c) guest RAM 是真 `/dev/mshv_vtl_low` mmap（非 memfd）。

```rust
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
const OFF_SQE: u64 = 0x3000; // ASQ 内第 0 个 SQE 实际就在 ASQ_GPA；这里单独留作 scratch

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|s| {
        let s = s.trim();
        s.strip_prefix("0x")
            .map(|h| u64::from_str_radix(h, 16))
            .unwrap_or_else(|| s.parse())
            .ok()
    }).unwrap_or(d)
}

fn main() -> anyhow::Result<()> {
    let sock = std::env::args().nth(1).unwrap_or_else(|| "/tmp/fw.sock".into());
    let gpa_base = env_u64("W5A_GPA_BASE", 0x100000);
    eprintln!("[w5a-client] sock={sock} gpa_base={gpa_base:#x} ram_len={RAM_LEN:#x}");

    // 1. open /dev/mshv_vtl_low + mmap [gpa_base, gpa_base+RAM_LEN) = 真 guest RAM。
    let dev = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/mshv_vtl_low")
        .context("open /dev/mshv_vtl_low")?;
    let ram = unsafe {
        // SAFETY: mmap 真 guest RAM 设备 fd，file_offset=GPA（非-CVM 裸 GPA，POC-3 验）。
        // 长度 RAM_LEN，PROT_READ|WRITE，MAP_SHARED；harness 进程独占该映射。
        use nix::sys::mman::{mmap, MapFlags, ProtFlags};
        use std::num::NonZeroUsize;
        mmap(
            None,
            NonZeroUsize::new(RAM_LEN as usize).unwrap(),
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_SHARED,
            dev.as_fd(),
            gpa_base as i64,
        )
        .context("mmap mshv_vtl_low @ gpa_base")?
    };
    let ram_ptr = ram.as_ptr() as *mut u8;
    // SAFETY: ram 为 mmap 成功返回的 RAM_LEN 字节有效映射。
    let ram_slice = unsafe { std::slice::from_raw_parts_mut(ram_ptr, RAM_LEN as usize) };
    // 清零工作区（ASQ/ACQ/PRP），避免脏数据干扰。
    ram_slice.fill(0);

    // 2. connect firmware + 握手 + enumerate。
    let stream = UnixStream::connect(&sock).context("connect firmware sock")?;
    let mut client = VfioUserClient::from_stream(stream);
    let neg = client.handshake().context("handshake")?;
    eprintln!("[w5a-client] handshake ok: server {}.{}", neg.server_major, neg.server_minor);
    let info = client.get_device_info().context("get_device_info")?;
    eprintln!("[w5a-client] num_regions={} num_irqs={}", info.num_regions, info.num_irqs);

    // 3. DMA_MAP：把真 guest RAM [gpa_base, +RAM_LEN) 经 mshv_vtl_low fd 传给 firmware
    //    （READABLE|WRITEABLE=0x3），fd_offset=gpa_base（firmware mmap 也按此 offset）。
    client
        .dma_map(gpa_base, RAM_LEN, 0x1 | 0x2, dev.as_fd(), gpa_base)
        .context("dma_map guest RAM")?;
    eprintln!("[w5a-client] dma_map [{gpa_base:#x}, +{RAM_LEN:#x}) ok");

    // 4. NVMe enable：写 AQA/ASQ/ACQ/CC.EN + poll CSTS.RDY。queues GPA = gpa_base+offset。
    let asq = gpa_base + OFF_ASQ;
    let acq = gpa_base + OFF_ACQ;
    let prp1 = gpa_base + OFF_PRP1;
    let aqa: u32 = ((QDEPTH - 1) << 16) | (QDEPTH - 1);
    client.region_write(pci_region::BAR0, R_AQA, &aqa.to_le_bytes())?;
    client.region_write(pci_region::BAR0, R_ASQ, &asq.to_le_bytes())?;
    client.region_write(pci_region::BAR0, R_ACQ, &acq.to_le_bytes())?;
    client.region_write(pci_region::BAR0, R_CC, &CC_VALUE.to_le_bytes())?;
    let mut ready = false;
    for _ in 0..100 {
        let csts = client.region_read(pci_region::BAR0, R_CSTS, 4)?;
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
    //    ASQ 第 0 个 SQE 在 asq 偏移处 = ram_slice[OFF_ASQ..]。
    let sqe = build_identify_sqe(1, prp1);
    ram_slice[(OFF_ASQ as usize)..(OFF_ASQ as usize + 64)].copy_from_slice(&sqe);

    // 6. ring SQ0 tail doorbell = 1（提交一个 SQE）。
    client.region_write(pci_region::BAR0, SQ0TDBL, &1u32.to_le_bytes())?;

    // 7. poll CQE（在 acq 偏移处）。CQE phase bit（DW3 bit16）翻转表示完成。
    let mut completed = false;
    for _ in 0..100 {
        let dw3 = u32::from_le_bytes(
            ram_slice[(OFF_ACQ as usize + 12)..(OFF_ACQ as usize + 16)]
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
    let mn_bytes = &ram_slice[(OFF_PRP1 as usize + 24)..(OFF_PRP1 as usize + 64)];
    let mn = String::from_utf8_lossy(mn_bytes);
    let mn_trim = mn.trim();
    eprintln!("[w5a-client] Identify MN = {mn_trim:?}");
    if !mn_trim.contains("OpenHCL Userspace NVMe") {
        bail!("Identify MN 不含预期串（零拷贝 DMA 未落进真 guest RAM？）：{mn_trim:?}");
    }

    eprintln!("[w5a-client] W5A PASSED ✓ — firmware-in-VTL2 经 DMA_MAP 真 guest RAM 零拷贝 Identify 端到端");
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
```

注意（执行时核对）：
- `VfioUserClient::dma_map(gpa, size, flags, BorrowedFd, fd_offset)` / `region_read(region, offset, count) -> Vec<u8>` / `region_write(region, offset, &[u8])` / `get_device_info()` —— W2/W3 API，签名已确认。
- `pci_region::BAR0`（=0，proto.rs）/ `pci_region` 在 `vfio_user_wire::proto`。
- NVMe SQE/CQE/寄存器布局：与 poclib.py 一致（R_CC=0x14 等、CC_VALUE、AQA、Identify opcode 0x06 CNS 0x01、PRP1@24、CDW10@40）。CQE phase bit 位置（DW3 bit16）——**执行时核对 nvme_firmware 的 CQE 写法**（poclib.py 没显式 poll phase，用的是…需确认 firmware CQE 格式；若 phase bit 位置不同按实调）。
- MN 在 Identify 数据 offset 24..64（NVMe spec：SN 4..24, MN 24..64）。
- **真 guest RAM 安全**：默认 GPA_BASE=0x100000（POC-3/6 真机写存活过）；env `W5A_GPA_BASE` 可调。写 64KiB NVMe 工作区有干扰活 guest 的风险——真机跑时观察 guest 稳定性，失稳=真发现（→ W6 需 underhill GuestMemory mediation）。

### Step 2.3: client 静态 musl 编译验证

```bash
cd /home/xp/refs/openvmm/usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/client && \
  cargo build --release --target x86_64-unknown-linux-musl 2>&1 | tail -5
file target/x86_64-unknown-linux-musl/release/w5a_vtl2_client | grep -o "statically linked\|static-pie"
```
预期：static ELF。若编译错（API 签名/CQE 格式），按 vfio_user_device 实际 API + nvme_firmware CQE 写法修。

---

## Stage 3 — committed deploy harness

**Files:** `experiments/2026-06-12-openhcl-vtl2-deploy/{build_and_stage.sh,run_e2e.sh,README.md}`

### Step 3.1: build_and_stage.sh

```bash
#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation. Licensed under the MIT License.
# W5a 档2：cross-build firmware + test client 静态 musl ELF + stage 到 /tmp 打包。
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../../../.." && pwd)
HERE=$(cd "$(dirname "$0")" && pwd)
STAGE="${STAGE:-/tmp/w5a_stage}"
mkdir -p "$STAGE"

echo "[build] firmware (static musl release)"
( cd "$REPO/usnvmemu/crates/nvme_firmware" && \
  cargo build --bin nvme_firmware --no-default-features --features vfio-user \
    --target x86_64-unknown-linux-musl --release )
cp "$REPO/usnvmemu/crates/nvme_firmware/target/x86_64-unknown-linux-musl/release/nvme_firmware" "$STAGE/fw"

echo "[build] test client (static musl release)"
( cd "$HERE/client" && cargo build --release --target x86_64-unknown-linux-musl )
cp "$HERE/client/target/x86_64-unknown-linux-musl/release/w5a_vtl2_client" "$STAGE/cl"

strip "$STAGE/fw" "$STAGE/cl" 2>/dev/null || true
ls -lh "$STAGE/fw" "$STAGE/cl"
echo "[build] staged at $STAGE (fw + cl)"
```

### Step 3.2: run_e2e.sh

单条 `ohcldiag-dev run` 推 tar(fw+cl) + sh 解包 + 后台起 firmware + 跑 client。

```bash
#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation. Licensed under the MIT License.
# W5a 档2：把 firmware+client 打 tar→base64→单条 ohcldiag-dev run 推进 VTL2，
# 起 firmware（后台）+ 跑 client（前台 oracle）。
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
STAGE="${STAGE:-/tmp/w5a_stage}"
VM="${VM:-pcie-remote-exp}"
OHCLDIAG="${OHCLDIAG:-/mnt/c/temp/pcie_remote_exp/ohcldiag-dev.exe}"
GPA_BASE="${W5A_GPA_BASE:-0x100000}"

[ -x "$STAGE/fw" ] && [ -x "$STAGE/cl" ] || { echo "missing $STAGE/fw|cl — run build_and_stage.sh first"; exit 1; }

# VTL2 侧脚本：解 tar → 造 backing file → 后台起 firmware → 等 socket → 跑 client → kill firmware。
# 注：NvmeController::open 要求 ≥1 个 --backing-file（即便 Identify Controller 不读 NS），
# 否则 firmware 起不来。set +e 包住 client 调用，保证失败也 dump fw.log。
VTL2_SCRIPT='
set -e
cd /tmp
base64 -d | tar x
chmod +x fw cl
truncate -s 1M /tmp/ns1.img 2>/dev/null || dd if=/dev/zero of=/tmp/ns1.img bs=1024 count=1024 2>/dev/null
./fw --vfio-user-sock /tmp/fw.sock --backing-file /tmp/ns1.img >/tmp/fw.log 2>&1 &
FWPID=$!
for i in $(seq 1 100); do [ -S /tmp/fw.sock ] && break; sleep 0.05; done
set +e
W5A_GPA_BASE='"$GPA_BASE"' ./cl /tmp/fw.sock
RC=$?
set -e
kill $FWPID 2>/dev/null || true
echo "=== fw.log ==="; cat /tmp/fw.log
exit $RC
'

# 打 tar（fw+cl）→ base64 → 经 ohcldiag-dev run 的 stdin 流进 VTL2。
tar c -C "$STAGE" fw cl | base64 -w0 | \
  "$OHCLDIAG" "$VM" run /bin/sh -- -c "$VTL2_SCRIPT" 2>&1 | tr -d '\r'
```

### Step 3.3: README.md

记：目的（W5a 档2 真机部署 + 零拷贝真 NVMe IO）、流程（build_and_stage → run_e2e）、scope（W5b≡W6 推迟 / W5c 远期）、真机结果（Stage 4 填）、guest RAM 安全说明（GPA_BASE 默认 0x100000，失稳=W6 信号）。

```markdown
# W5a 档2 — firmware-in-VTL2 真机部署 + DMA 零拷贝真 NVMe IO

**日期**：2026-06-12  **VM**：pcie-remote-exp（真 OpenHCL）

## 目的
firmware-as-VTL2-进程 dev 部署（spec §6 W5a）+ POC-1/3/6 真机汇合：真 `nvme_firmware`
ELF 在真 VTL2 跑 AF_UNIX server，test client（真 `vfio_user_device` 客户端 API）
经 DMA_MAP 把**真 guest RAM**(/dev/mshv_vtl_low) fd 传给 firmware，驱 NVMe Identify，
firmware **零拷贝 DMA 进真 guest 物理内存**，client 读回 MN 验证。

## 跑
```bash
./build_and_stage.sh        # cross-build firmware+client 静态 musl → /tmp/w5a_stage
./run_e2e.sh                # tar→base64→ohcldiag-dev run 推进 VTL2 + 起 fw + 跑 client
```
预期末行：`W5A PASSED ✓ — firmware-in-VTL2 经 DMA_MAP 真 guest RAM 零拷贝 Identify 端到端`

## scope（用户 2026-06-12 拍板档2）
- W5a（本 harness）= dev 部署 + 真 guest RAM 零拷贝真 NVMe IO。
- **W5b（underhill Command::new supervisor + reconnect + 降权）≡ W6**，推迟（需改 underhill_core，standalone 测不了）。
- **W5c（IGVM initrd 出厂 / A-B image）** 远期。

## guest RAM 安全
test client 在真 guest 物理内存 [GPA_BASE, +64KiB) 布 NVMe 队列（默认 GPA_BASE=0x100000，
POC-3/6 真机在此写 marker 存活过；env `W5A_GPA_BASE` 可调）。写活 guest RAM 有干扰风险——
失稳 = 真发现，指向 W6 需 underhill GuestMemory mediation（underhill 知 guest RAM 布局）。

## 真机结果
（Stage 4 填实测输出）
```

---

## Stage 4 — 真 VM e2e 跑 + oracle

- [ ] **Step 4.1: build + stage**

```bash
cd /home/xp/refs/openvmm/usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy && \
  bash build_and_stage.sh 2>&1 | tail -5
```
预期：fw + cl 两个 static ELF staged 到 /tmp/w5a_stage。

- [ ] **Step 4.2: 真 VM e2e**

```bash
cd /home/xp/refs/openvmm/usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy && \
  timeout 120 bash run_e2e.sh 2>&1 | tail -30
```
预期末尾：`W5A PASSED ✓`。client 日志显示 handshake ok / num_regions=9 / dma_map ok / controller enabled / Identify MN = "OpenHCL Userspace NVMe v2.0"。
fw.log 应显示 firmware 收 DMA_MAP（zero_copy=true）+ region 访问 + 无 server-initiated DMA_READ（零拷贝命中）。

若失败分档诊断：
- handshake/enumerate 挂 → AF_UNIX/部署问题（档1 都没过）。
- CSTS.RDY 不翻 → NVMe enable 寄存器写问题（核对 regs.rs offset）。
- CQE phase 不翻 → firmware 没 DMA 进真 guest RAM / SQE 格式错 / CQE phase 位置错。
- MN 不符 → DMA 命中了但数据错 / Identify 布局错。
- guest 失稳/VM 异常 → 真发现（记 README + spec：需 W6 underhill-mediated GPA）。

- [ ] **Step 4.3: 把真机输出填进 README "真机结果" 段**

---

## Final — spec W5 scope + 单 commit

### Step F.1: 更新 spec §6 W5

`docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md` §6 W5 三段：W5a 标 ✅ 完成（committed harness + 真机 e2e），W5b 标"≡ W6 推迟"，W5c 标"远期"。

### Step F.2: 单 commit

```bash
cd /home/xp/refs/openvmm && git status --short
git add usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/ \
        docs/superpowers/plans/2026-06-12-w5a-vtl2-deploy.md \
        docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md
# guards
git diff --cached --name-only | grep -i decisions && echo "ERR DECISIONS" || echo "OK no DECISIONS"
git diff --cached --name-only | grep -E 'experiments/.*/(target|Cargo\.lock)' && echo "ERR artifact" || echo "OK no artifact"
git diff --cached --name-only
```

注：experiments/client 需 `.gitignore`（target/ + Cargo.lock）—— Step 2.1 时一并建（镜像 poc6_fd_pass_mmap）。

commit：
```bash
git commit -m "feat(experiments): W5a 档2 firmware-in-VTL2 真机部署 + DMA 零拷贝真 NVMe IO

firmware-as-VTL2-进程 dev 部署（spec §6 W5a）+ POC-1/3/6 真机汇合：真
nvme_firmware ELF（cross-build static-pie musl，零生产代码改）在真 OpenHCL VM
pcie-remote-exp 的 VTL2 跑 AF_UNIX server；test client（experiments harness，
用真 vfio_user_device W1-W4 客户端 API）经 ohcldiag-dev run + base64 stdin 推进
VTL2 → connect → 握手 → dma_map 真 guest RAM(/dev/mshv_vtl_low) fd 给 firmware
→ 驱 NVMe Identify（CC.EN/SQE/doorbell）→ firmware 零拷贝 DMA 进真 guest 物理
内存 → client 从 guest RAM 读回 Identify MN 验证零拷贝命中。

- experiments/2026-06-12-openhcl-vtl2-deploy/：client musl bin（unsafe-ok harness，
  mmap mshv_vtl_low，不污染 deny-clean vfio_user_device）+ build_and_stage.sh +
  run_e2e.sh（tar+base64+ohcldiag-dev run 单条推送）+ README（真机结果）。
- firmware 静态 musl POC 验证通过（完整 nvme_firmware → static-pie ELF）。

scope（用户 2026-06-12 拍板档2）：W5a = dev 部署 + 真 guest RAM 零拷贝真 NVMe IO。
**W5b（underhill Command::new supervisor + reconnect + 降权）≡ W6 推迟**（需改
underhill_core，standalone 测不了，spec 自承 W5b 起依赖 W6）。W5c（IGVM 生产）远期。
spec §6 W5 记此 scope。

真机 e2e：<Stage 4 实测结果填入>

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
git log --oneline -1
```

---

## Self-Review

**1. Spec 覆盖**：spec §6 W5a（ohcldiag-dev stdin 部署）→ harness；档2（DMA 零拷贝真 NVMe IO）→ client + 真机 e2e。W5b≡W6 / W5c 远期 → F.1 记。

**2. Placeholder 扫描**：CQE phase bit 位置 + MN 串标"执行时核对 firmware 实际"（真验证步骤，非占位）。真机结果 Stage 4 填（e2e 性质，必须实跑）。

**3. 类型一致性**：client 用 `VfioUserClient::{from_stream, handshake, get_device_info, dma_map, region_read, region_write}`（W1-W4 API）+ `pci_region::BAR0`。NVMe 常量对齐 poclib.py。

**4. 单 commit + 不碰生产代码**：firmware 零改（只换 target 编）；4 crate 不动；全新代码在 experiments/。F.2 精确 pathspec + DECISIONS/artifact guard + client/.gitignore。

**5. 风险**：
- 静态 musl：firmware 已 POC 验（debug static-pie），release 同链。client 依赖 vfio_user_device（musl 友好）+ nix mman。
- 真 guest RAM 安全：GPA_BASE 默认 0x100000（POC-3/6 存活）；失稳=真发现记 W6 信号，不强推。
- VM 可达：已验 ohcldiag-dev 直驱 VTL2。
- VTL2 无 tar 备选：两段 base64 投递（Stage 3 备选，README 注）。
- W5b≡W6：明确推迟，不在本 plan。

---

**Plan 完。** 4 Stage + Final = 5 段，单 commit 收口。真机 e2e 由控制者经 ohcldiag-dev 跑。
