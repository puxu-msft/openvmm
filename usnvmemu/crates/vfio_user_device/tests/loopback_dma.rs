// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W3 loopback 集成测：client DMA_MAP fd → server mmap 零拷贝。
//! DmaTriggerDev doorbell → ctx.dma_read 命中 mmap（wire 上无 DMA_READ command
//! = 零拷贝反证）→ on_dma_complete 拿到 marker + 零拷贝 dma_write 回写 GPA_DST。
//! socketpair + tempfile，Linux 直跑，无需 VTL。对标 POC-1。
//!
//! W6a：client 改 async；client 段包进 `DefaultPool::run_with`，server 半段仍同步。
//! **fd-passing 端到端 oracle**：server mmap client 经 SCM_RIGHTS 传的 fd 零拷贝命中。

use pal_async::DefaultPool;
use pcie_device_core::BarKind;
use pcie_device_core::BarLayout;
use pcie_device_core::DeviceCtx;
use pcie_device_core::DeviceDescribe;
use pcie_device_core::PcieDevice;
use parking_lot::Mutex;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use vfio_user_device::VfioUserClient;

const GPA_SRC: u64 = 0x100; // dma_read 源
const GPA_DST: u64 = 0x200; // dma_write 目标
const MARKER: [u8; 8] = *b"W3-DMAOK";

/// on_dma_complete 拿到的 (ok, data) 的跨线程共享句柄。
type ReadBack = Arc<Mutex<Option<(bool, Vec<u8>)>>>;

/// doorbell → dma_read(GPA_SRC) → on_dma_complete 存数据 + **一次性**零拷贝
/// dma_write(GPA_DST,数据)。
///
/// **关键**：`Transport::dma_write` 本身也产生一个 DMA 完成事件（推
/// pending_completions），故 on_dma_complete 会被该 dma_write 的完成事件再次
/// 调用。若无条件 dma_write 会无限递归（dma_write→完成→on_dma_complete→
/// dma_write→…）。用 `echoed` 守门：仅在首个完成（dma_read 的）echo 一次。
///
/// 注：production device 应按 `token`（`dma_read`/`dma_write` 各返回不同 token）
/// 区分自己发起的 read-完成 vs write-完成，而非本测试图省事用的全局 bool。
struct DmaTriggerDev {
    read_back: ReadBack,
    echoed: bool,
}
impl PcieDevice for DmaTriggerDev {
    fn describe(&self) -> DeviceDescribe {
        DeviceDescribe {
            vendor_id: 0x1234,
            device_id: 0x5678,
            class_code: 0x01_08_02,
            revision: 1,
            subsystem_vendor: 0,
            subsystem_device: 0,
            bars: vec![BarLayout {
                index: 0,
                size: 8192,
                kind: BarKind::Mmio32,
                prefetchable: false,
            }],
            msix_count: 1,
            capabilities: vec![],
            cfg_write_side_effect_offsets: vec![],
        }
    }
    fn mmio_read(&mut self, _bar: u32, _offset: u64, _size: u32) -> u64 {
        0
    }
    fn mmio_write(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        _bar: u32,
        offset: u64,
        _size: u32,
        _value: u64,
    ) {
        // doorbell @ 0x1000 → dma_read GPA_SRC 8 字节（命中 mmap 零拷贝）。
        if offset == 0x1000 {
            let _tok = ctx.dma_read(GPA_SRC, MARKER.len() as u32);
        }
    }
    fn on_dma_complete(&mut self, ctx: &mut DeviceCtx<'_>, _token: u64, ok: bool, data: Vec<u8>) {
        if self.echoed {
            // 这是上面 dma_write 自身的完成事件——不再 echo，避免无限递归。
            return;
        }
        self.echoed = true;
        *self.read_back.lock() = Some((ok, data.clone()));
        // 把 dma_read 拿到的数据零拷贝写回 GPA_DST（验证 dma_write 命中 mmap）。
        ctx.dma_write(GPA_DST, data);
    }
}

fn spawn_server(
    server_end: UnixStream,
    read_back: ReadBack,
) -> thread::JoinHandle<anyhow::Result<()>> {
    thread::spawn(move || -> anyhow::Result<()> {
        let mut s = server_end;
        let neg = vfio_user_transport::server_handshake(&mut s)?;
        let mut sess = vfio_user_transport::VfioUserSession::new(s, neg);
        let mut dev = DmaTriggerDev {
            read_back,
            echoed: false,
        };
        while sess.pump_one(&mut dev)? {}
        Ok(())
    })
}

#[test]
fn loopback_dma_map_zero_copy() {
    // "guest RAM" backing file：4 KiB，写 marker @ GPA_SRC。
    let ram = tempfile::tempfile().unwrap();
    ram.set_len(4096).unwrap();
    ram.write_at(&MARKER, GPA_SRC).unwrap();

    let (server_end, client_end) = UnixStream::pair().unwrap();
    let read_back = Arc::new(Mutex::new(None));
    let server = spawn_server(server_end, read_back.clone());

    DefaultPool::run_with(async |driver| {
        let mut client =
            VfioUserClient::from_stream(&driver, client_end).expect("wrap client stream");
        client.handshake().await.expect("handshake");

        // 1. DMA_MAP 整段 [0, 4096) RW（READABLE|WRITEABLE = 0x1|0x2），fd = ram。
        client
            .dma_map(0, 4096, 0x1 | 0x2, ram.as_fd(), 0)
            .await
            .expect("dma_map");

        // 2. doorbell：BAR0 REGION_WRITE @ 0x1000 → server device dma_read(GPA_SRC)
        //    命中 mmap 零拷贝。若零拷贝失败（无 fd/miss），server 会发 server-initiated
        //    DMA_READ command 回打 client，则 region_write reply 读到的会是 DMA_READ
        //    (cmd=11) 而非 REGION_WRITE(cmd=10)，expect_reply cmd 校验失败 → 测挂。
        //    故 region_write 成功本身 = 零拷贝反证。
        client
            .region_write(0, 0x1000, &1u32.to_le_bytes())
            .await
            .expect("doorbell region_write（成功即证 wire 上无 DMA_READ = 零拷贝命中）");

        // 3. dma_unmap 撤销。
        //    **时序注意**：server 在 region_write 的 reply **发出后**才 drain
        //    completions → on_dma_complete 的 dma_write(GPA_DST) 发生在 reply 之后。
        //    故 GPA_DST / read_back 的验证必须等 server 跑完（join 后），不能在收到
        //    region_write reply 后立即读。
        client.dma_unmap(0, 4096).await.expect("dma_unmap");

        drop(client);
    });

    server.join().unwrap().expect("server loop");

    // 4. 验 server dma_write 零拷贝落地（join 后 server 已 drain；MAP_SHARED 写经
    //    page cache，pread 可见）：读 backing file @ GPA_DST 应见 marker。
    let mut dst = [0u8; 8];
    ram.read_at(&mut dst, GPA_DST).expect("read_at GPA_DST");
    assert_eq!(dst, MARKER, "server dma_write 应零拷贝写 marker 到 GPA_DST");

    // 5. 验 dma_read 拿到的字节 == marker（零拷贝读命中正确数据）。
    let rb = read_back.lock();
    let (ok, data) = rb.as_ref().expect("on_dma_complete 应被调用");
    assert!(*ok, "dma_read 应成功");
    assert_eq!(data.as_slice(), &MARKER, "dma_read 零拷贝拿到 marker");
}
