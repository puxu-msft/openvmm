// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W6b Task 1.3 worker loopback 集成测：真 `vfio_user_transport::VfioUserSession` +
//! `MockDev` server 线程 ⇄ [`vfio_user_pci_device::Worker`]（持全双工 split 通道）。
//!
//! 验证：
//! - **全双工 MMIO read x2**：先连发两个 `DeviceRequest::MmioRead`（不读任何 reply），
//!   再 await 两个 `DeferredToken`，按 msg_id 路由拿回各自 seed 的 pattern；
//! - **MMIO write→read 顺序**：fire-and-forget write 后紧跟 read 同 offset，值 round-trip，
//!   证明 FIFO 顺序 + write 真落到 MockDev。
//!
//! socketpair，Linux 直跑，无需 VTL。server 半段同步（独立线程），worker 段 async。

use chipset_device::io::deferred::defer_read;
use pal_async::DefaultPool;
use pal_async::task::Spawn;
use pcie_device_core::BarKind;
use pcie_device_core::BarLayout;
use pcie_device_core::DeviceCtx;
use pcie_device_core::DeviceDescribe;
use pcie_device_core::PcieDevice;
use std::os::unix::net::UnixStream;
use std::thread;
use vfio_user_device::VfioUserClient;
use vfio_user_pci_device::DeviceRequest;
use vfio_user_pci_device::DeviceState;
use vfio_user_pci_device::ReqKind;
use vfio_user_pci_device::SharedState;
use vfio_user_pci_device::Worker;
use vfio_user_pci_device::WorkerStats;

/// 测试 MockDev：BAR0 8 KiB（u64 槽）+ 8 MSI-X + identity。region 0 = BAR0。
struct MockDev {
    bar0: Vec<u64>,
    last_reset: u32,
}
impl MockDev {
    fn new() -> Self {
        Self {
            bar0: vec![0u64; 1024],
            last_reset: 0xFFFF_FFFF,
        }
    }
}
impl PcieDevice for MockDev {
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
            msix_count: 8,
            capabilities: vec![],
            cfg_write_side_effect_offsets: vec![],
        }
    }
    fn mmio_read(&mut self, _bar: u32, offset: u64, _size: u32) -> u64 {
        *self.bar0.get((offset / 8) as usize).unwrap_or(&0)
    }
    fn mmio_write(
        &mut self,
        _ctx: &mut DeviceCtx<'_>,
        _bar: u32,
        offset: u64,
        _size: u32,
        value: u64,
    ) {
        let idx = (offset / 8) as usize;
        if idx < self.bar0.len() {
            self.bar0[idx] = value;
        }
    }
    fn reset(&mut self, kind: u32) {
        self.last_reset = kind;
    }
}

/// 起 server 线程：handshake → session 循环 pump_one(MockDev) 直到 peer close。
fn spawn_server(server_end: UnixStream) -> thread::JoinHandle<anyhow::Result<()>> {
    thread::spawn(move || -> anyhow::Result<()> {
        let mut s = server_end;
        let neg = vfio_user_transport::server_handshake(&mut s)?;
        let mut sess = vfio_user_transport::VfioUserSession::new(s, neg);
        let mut dev = MockDev::new();
        while sess.pump_one(&mut dev)? {}
        Ok(())
    })
}

/// 全双工 read x2 + write→read 顺序：经真 server + Worker 跑通。
#[test]
fn worker_full_duplex_reads_and_write_then_read() {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    let server = spawn_server(server_end);

    DefaultPool::run_with(async |driver| {
        // connect + handshake，然后预置两个 BAR0 offset（串行 client，into_channel 前）。
        let mut client =
            VfioUserClient::from_stream(&driver, client_end).expect("wrap client stream");
        client.handshake().await.expect("handshake");

        let pat0 = 0x1111_2222_3333_4444_u64.to_le_bytes();
        let pat1 = 0xAAAA_BBBB_CCCC_DDDD_u64.to_le_bytes();
        client.region_write(0, 0, &pat0).await.expect("seed BAR0@0");
        client.region_write(0, 8, &pat1).await.expect("seed BAR0@8");

        // 拆全双工读写半 → 建 Worker（空 interrupts / irq_tasks）。
        let (writer, reader) = client.into_channel();
        let state = SharedState::new(DeviceState::Live);
        let stats = std::sync::Arc::new(WorkerStats::default());
        let (tx, rx) = mesh::channel::<DeviceRequest>();
        let (shutdown_tx, shutdown_rx) = mesh::channel::<()>();

        let worker = Worker::new(
            writer,
            reader,
            state.clone(),
            rx,
            Vec::new(), // interrupts
            Vec::new(), // irq_tasks
            stats.clone(),
        );
        let worker_task = driver.spawn("w6b-worker", worker.run(shutdown_rx));

        // ── 全双工：连发两个 MmioRead（offset 0 / 8，size 8）不读任何 reply ──
        let (rd0, tok0) = defer_read();
        let (rd1, tok1) = defer_read();
        tx.send(DeviceRequest {
            kind: ReqKind::MmioRead {
                bar: 0,
                offset: 0,
                size: 8,
                token: rd0,
            },
        });
        tx.send(DeviceRequest {
            kind: ReqKind::MmioRead {
                bar: 0,
                offset: 8,
                size: 8,
                token: rd1,
            },
        });

        let mut buf0 = [0u8; 8];
        let mut buf1 = [0u8; 8];
        tok0.read_future(&mut buf0).await.expect("read offset 0");
        tok1.read_future(&mut buf1).await.expect("read offset 8");
        assert_eq!(buf0, pat0, "offset 0 应回 seed pattern（msg_id 路由正确）");
        assert_eq!(buf1, pat1, "offset 8 应回 seed pattern（msg_id 路由正确）");

        // ── write→read 顺序：fire-and-forget write offset 16，再 read 同 offset ──
        let wpat = 0xFEED_FACE_0BAD_F00D_u64.to_le_bytes();
        tx.send(DeviceRequest {
            kind: ReqKind::MmioWrite {
                bar: 0,
                offset: 16,
                data: wpat.to_vec(),
            },
        });
        let (rd2, tok2) = defer_read();
        tx.send(DeviceRequest {
            kind: ReqKind::MmioRead {
                bar: 0,
                offset: 16,
                size: 8,
                token: rd2,
            },
        });
        let mut buf2 = [0u8; 8];
        tok2.read_future(&mut buf2).await.expect("read offset 16");
        assert_eq!(buf2, wpat, "write→read 同寄存器应 round-trip（FIFO 顺序）");

        // 优雅收尾：shutdown + drop sender → worker 退出。
        shutdown_tx.send(());
        drop(tx);
        worker_task.await;

        // 三个成功 read。
        assert_eq!(
            stats
                .mmio_read_results
                .load(std::sync::atomic::Ordering::Relaxed),
            3,
            "应记 3 次成功 MMIO read"
        );
    });

    server.join().unwrap().expect("server loop");
}

/// server 握手后立即关 socket → worker 收 EOF（或发帧失败）→ go_lost + drain：
/// 在途 MMIO read token 必 resolve 成错误，state 必转 Lost。覆盖失败路径（contract 3-5）。
#[test]
fn worker_goes_lost_and_drains_on_server_close() {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    // server 线程：只握手，然后 drop socket（关连接），不 pump 任何请求。
    let server = thread::spawn(move || -> anyhow::Result<()> {
        let mut s = server_end;
        let _neg = vfio_user_transport::server_handshake(&mut s)?;
        drop(s); // 立即关连接 → 对端 recv 得 EOF。
        Ok(())
    });

    DefaultPool::run_with(async |driver| {
        let mut client =
            VfioUserClient::from_stream(&driver, client_end).expect("wrap client stream");
        client.handshake().await.expect("handshake");

        let (writer, reader) = client.into_channel();
        let state = SharedState::new(DeviceState::Live);
        let stats = std::sync::Arc::new(WorkerStats::default());
        let (tx, rx) = mesh::channel::<DeviceRequest>();
        let (_shutdown_tx, shutdown_rx) = mesh::channel::<()>();

        let worker = Worker::new(
            writer,
            reader,
            state.clone(),
            rx,
            Vec::new(),
            Vec::new(),
            stats.clone(),
        );
        let worker_task = driver.spawn("w6b-worker-lost", worker.run(shutdown_rx));

        // 发一个 read：server 已关，worker 要么 send 失败要么 recv EOF → go_lost。
        let (rd, tok) = defer_read();
        tx.send(DeviceRequest {
            kind: ReqKind::MmioRead {
                bar: 0,
                offset: 0,
                size: 8,
                token: rd,
            },
        });

        // token 必 resolve 成错误（drain 的 NoResponse 或 error reply）。
        let mut buf = [0u8; 8];
        let r = tok.read_future(&mut buf).await;
        assert!(r.is_err(), "server 关连接后在途 read 应失败，got {r:?}");

        // 关 from_device → worker 退出（若已 Lost 则主循环 pending 在 recv 上，靠 drop tx 触发 break）。
        drop(tx);
        worker_task.await;

        assert_eq!(
            state.load(),
            DeviceState::Lost,
            "server 关连接后 state 应转 Lost"
        );
    });

    server.join().unwrap().expect("server thread");
}
