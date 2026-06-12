// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **CMB-P3b** — OpenHCL client（`vfio_user_pci_device`）的 trap-based CMB e2e 集成测：
//! 真 `vfio_user_transport::VfioUserSession` + 暴露 **CMB 数据 BAR（index 2）** 的 MockDev
//! server 线程 ⇄ [`vfio_user_pci_device::Worker`] / [`reconnect_loop`]。
//!
//! 覆盖 P3b 三件事（e2e 边界：单元/集成，**非** 真 OpenHCL guest-boot——后者需
//! Windows/Hyper-V，属 P3b 之外真机验证）：
//! 1. **CMB BAR 转发**（[`cmb_bar_region_rw_forwards_with_correct_index`]）：worker 对
//!    `bar=2` 的 MMIO read/write → REGION_READ/WRITE(region=2) → 命中 server 的 CMB
//!    backing；round-trip 证明 **region index 2 正确透传**（非硬编码 bar0）。BAR0(index0)
//!    的访问独立命中 BAR0 backing，证明两 BAR 路由互不串。
//! 2. **CMB discover + 到 Live**（[`reconnect_discovers_cmb_bar_and_goes_live`]）：
//!    `reconnect_loop` 经真 AF_UNIX 连上暴露 CMB BAR（trap 模式）的 server，discover 出
//!    CMB 几何（BIR=2 == declared 预留槽）→ validate_cmb Ok → 到 Live。
//! 3. **map 模式优雅降级**（[`reconnect_degrades_map_mode_cmb_to_trap_and_goes_live`]）：
//!    server `cmb_region_fd(2)` 返 `Some(memfd)` → region info 置 FLAG_MMAP（map 模式）。
//!    OpenHCL client 无 mapper，**绝不 mmap**，降级 trap：仍 discover 成功 + 到 Live，
//!    且 CMB 经 REGION_RW 可达（功能完整，只是非零拷贝）。
//!
//! socketpair / 文件系统 AF_UNIX，Linux 直跑，无需 VTL。server 半段同步（独立线程），
//! worker/connector 段 async（`DefaultPool`）。

use chipset_device::io::deferred::defer_read;
use pal_async::DefaultPool;
use pal_async::driver::Driver;
use pal_async::task::Spawn;
use pal_async::timer::PolledTimer;
use pcie_device_core::BarKind;
use pcie_device_core::BarLayout;
use pcie_device_core::DeviceCtx;
use pcie_device_core::DeviceDescribe;
use pcie_device_core::PcieDevice;
use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use vfio_user_device::VfioUserClient;
use vfio_user_pci_device::DeclaredGeometry;
use vfio_user_pci_device::DeviceRequest;
use vfio_user_pci_device::DeviceState;
use vfio_user_pci_device::ReconnectChannels;
use vfio_user_pci_device::ReconnectEvent;
use vfio_user_pci_device::ReqKind;
use vfio_user_pci_device::SharedState;
use vfio_user_pci_device::Worker;
use vfio_user_pci_device::WorkerStats;
use vfio_user_transport::MemfdRamRegion;

/// CMB 数据 BAR 的 index（= firmware CMBLOC.BIR 默认；与 `DeclaredGeometry` 的
/// `DEFAULT_CMB_BIR` 一致，独立 BAR2）。
const CMB_BIR: u8 = 2;
/// CMB BAR 窗口大小（4 KiB，页对齐，≤ declared 默认 2 MiB → validate_cmb Ok）。
const CMB_SIZE: u64 = 4096;
/// MockDev 的 BAR0 窗口（NVMe 寄存器；与 `DEFAULT_BAR0_SIZE` 16 KiB 对齐 ≤ declared）。
const BAR0_SIZE: u64 = 16 * 1024;
/// MockDev 的 MSI-X 向量数（≤ declared 默认 4）。
const MSIX_COUNT: u16 = 4;

/// 测试 MockDev：BAR0（寄存器，index 0）+ CMB 数据 BAR（index 2）。CMB backing 用
/// `Option<MemfdRamRegion>`：
/// - `None` → trap-only（`cmb_region_fd` 返 None，server 不置 FLAG_MMAP）；
/// - `Some` → map 模式（`cmb_region_fd` 返 memfd，server 置 FLAG_MMAP；client 须降级 trap）。
///
/// trap 路径（两模都走）：CMB 访问经 REGION_READ/WRITE → `mmio_read/write(bar=2, off)` →
/// 命中 `cmb_backing` 字节数组。这是 firmware P3a `cmb_bar_read/write` 服务 backing 的
/// MockDev 镜像。
struct CmbMockDev {
    bar0: Vec<u64>,
    cmb_backing: Vec<u8>,
    /// map 模式时持有的 memfd（`cmb_region_fd` 借出它 → server 置 FLAG_MMAP）。
    cmb_memfd: Option<MemfdRamRegion>,
}

impl CmbMockDev {
    /// trap-only：无 memfd，`cmb_region_fd` 恒 None。
    fn new_trap() -> Self {
        Self {
            bar0: vec![0u64; (BAR0_SIZE / 8) as usize],
            cmb_backing: vec![0u8; CMB_SIZE as usize],
            cmb_memfd: None,
        }
    }

    /// map 模式：持一个 CMB_SIZE 的 memfd，`cmb_region_fd(2)` 返它 → server 置 FLAG_MMAP。
    fn new_map() -> Self {
        Self {
            bar0: vec![0u64; (BAR0_SIZE / 8) as usize],
            cmb_backing: vec![0u8; CMB_SIZE as usize],
            cmb_memfd: Some(MemfdRamRegion::new(CMB_SIZE as usize).expect("memfd")),
        }
    }
}

impl PcieDevice for CmbMockDev {
    fn describe(&self) -> DeviceDescribe {
        DeviceDescribe {
            vendor_id: 0x1414,
            device_id: 0xc0de,
            class_code: 0x01_08_02,
            revision: 1,
            subsystem_vendor: 0,
            subsystem_device: 0,
            bars: vec![
                BarLayout {
                    index: 0,
                    size: BAR0_SIZE,
                    kind: BarKind::Mmio32,
                    prefetchable: false,
                },
                // CMB 数据 BAR（独立 slot，index = CMBLOC.BIR）。
                BarLayout {
                    index: CMB_BIR,
                    size: CMB_SIZE,
                    kind: BarKind::Mmio32,
                    prefetchable: false,
                },
            ],
            msix_count: MSIX_COUNT as u32,
            capabilities: vec![],
            cfg_write_side_effect_offsets: vec![],
        }
    }

    fn mmio_read(&mut self, bar: u32, offset: u64, size: u32) -> u64 {
        if bar == CMB_BIR as u32 {
            // CMB backing：按字节读 `size` 字节拼 u64（little-endian）。
            let off = offset as usize;
            let n = size as usize;
            if off + n > self.cmb_backing.len() {
                return 0;
            }
            let mut buf = [0u8; 8];
            buf[..n].copy_from_slice(&self.cmb_backing[off..off + n]);
            u64::from_le_bytes(buf)
        } else {
            *self.bar0.get((offset / 8) as usize).unwrap_or(&0)
        }
    }

    fn mmio_write(
        &mut self,
        _ctx: &mut DeviceCtx<'_>,
        bar: u32,
        offset: u64,
        size: u32,
        value: u64,
    ) {
        if bar == CMB_BIR as u32 {
            let off = offset as usize;
            let n = size as usize;
            if off + n <= self.cmb_backing.len() {
                self.cmb_backing[off..off + n].copy_from_slice(&value.to_le_bytes()[..n]);
            }
        } else {
            let idx = (offset / 8) as usize;
            if idx < self.bar0.len() {
                self.bar0[idx] = value;
            }
        }
    }

    fn cmb_region_fd(&self, bar: u32) -> Option<BorrowedFd<'_>> {
        use pcie_device_core::SharedRamRegion;
        if bar == CMB_BIR as u32 {
            self.cmb_memfd.as_ref().and_then(|m| m.as_fd())
        } else {
            None
        }
    }
}

/// 起 server 线程（pump-forever）：handshake → session 循环 pump_one 直到 peer close。
fn spawn_server(
    server_end: UnixStream,
    mut dev: CmbMockDev,
) -> thread::JoinHandle<anyhow::Result<()>> {
    thread::spawn(move || -> anyhow::Result<()> {
        let mut s = server_end;
        let neg = vfio_user_transport::server_handshake(&mut s)?;
        let mut sess = vfio_user_transport::VfioUserSession::new(s, neg);
        while sess.pump_one(&mut dev)? {}
        Ok(())
    })
}

/// ① CMB BAR 转发：worker 对 `bar=2` 的 MMIO write→read round-trip，证明 region index 2
///    正确透传（非硬编码 bar0）；BAR0 访问独立命中 BAR0 backing，证明两 BAR 路由不串。
#[test]
fn cmb_bar_region_rw_forwards_with_correct_index() {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    let server = spawn_server(server_end, CmbMockDev::new_trap());

    DefaultPool::run_with(async |driver| {
        let mut client =
            VfioUserClient::from_stream(&driver, client_end).expect("wrap client stream");
        client.handshake().await.expect("handshake");

        let (writer, reader) = client.into_channel();
        let state = SharedState::new(DeviceState::Connecting);
        let stats = std::sync::Arc::new(WorkerStats::default());
        let (tx, rx) = mesh::channel::<DeviceRequest>();
        let (shutdown_tx, shutdown_rx) = mesh::channel::<()>();
        let (reconnect_tx, reconnect_rx) = mesh::channel::<ReconnectEvent>();
        let (lost_tx, _lost_rx) = mesh::channel::<()>();

        let worker = Worker::new(
            state.clone(),
            rx,
            Vec::new(),
            Vec::new(),
            stats.clone(),
            reconnect_rx,
            lost_tx,
        );
        let worker_task = driver.spawn("cmb-worker", worker.run(shutdown_rx));

        reconnect_tx.send(ReconnectEvent::Connected { writer, reader });

        // ── CMB BAR（index 2）：write@0x40 → read@0x40 round-trip ──
        let cmb_pat = 0xCAFE_BABE_DEAD_BEEF_u64.to_le_bytes();
        tx.send(DeviceRequest {
            kind: ReqKind::MmioWrite {
                bar: CMB_BIR as u32,
                offset: 0x40,
                data: cmb_pat.to_vec(),
            },
        });
        let (rd_cmb, tok_cmb) = defer_read();
        tx.send(DeviceRequest {
            kind: ReqKind::MmioRead {
                bar: CMB_BIR as u32,
                offset: 0x40,
                size: 8,
                token: rd_cmb,
            },
        });
        let mut buf_cmb = [0u8; 8];
        tok_cmb
            .read_future(&mut buf_cmb)
            .await
            .expect("read CMB@0x40");
        assert_eq!(
            buf_cmb, cmb_pat,
            "CMB BAR(index 2) write→read 应 round-trip（region index 2 正确透传，非 bar0）"
        );

        // ── BAR0（index 0）：独立 write@0 → read@0，证明两 BAR 路由不串 ──
        let bar0_pat = 0x1111_2222_3333_4444_u64.to_le_bytes();
        tx.send(DeviceRequest {
            kind: ReqKind::MmioWrite {
                bar: 0,
                offset: 0,
                data: bar0_pat.to_vec(),
            },
        });
        let (rd_b0, tok_b0) = defer_read();
        tx.send(DeviceRequest {
            kind: ReqKind::MmioRead {
                bar: 0,
                offset: 0,
                size: 8,
                token: rd_b0,
            },
        });
        let mut buf_b0 = [0u8; 8];
        tok_b0.read_future(&mut buf_b0).await.expect("read BAR0@0");
        assert_eq!(buf_b0, bar0_pat, "BAR0 与 CMB BAR 路由互不串");

        // 再读回 CMB@0x40，确认未被 BAR0 写污染。
        let (rd_cmb2, tok_cmb2) = defer_read();
        tx.send(DeviceRequest {
            kind: ReqKind::MmioRead {
                bar: CMB_BIR as u32,
                offset: 0x40,
                size: 8,
                token: rd_cmb2,
            },
        });
        let mut buf_cmb2 = [0u8; 8];
        tok_cmb2
            .read_future(&mut buf_cmb2)
            .await
            .expect("re-read CMB@0x40");
        assert_eq!(buf_cmb2, cmb_pat, "CMB backing 不被 BAR0 写影响");

        shutdown_tx.send(());
        drop(tx);
        worker_task.await;
    });

    server.join().unwrap().expect("server loop");
}

// ──────────────────── reconnect-based discover + degrade ────────────────────

/// 有界轮询 state == want；超时 panic（CI 慢机器留足，绝不无限 spin）。
async fn wait_for_state(driver: &impl Driver, state: &SharedState, want: DeviceState) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if state.load() == want {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "等 state == {want:?} 超时（10s），实际 = {:?}",
                state.load()
            );
        }
        PolledTimer::new(driver)
            .sleep(Duration::from_millis(20))
            .await;
    }
}

/// 起一个「accept 一条连接 + pump-forever（用给定 dev factory 建 dev）」的 server 线程，
/// bind 在文件系统 `path`。返回 (handle, ready_rx)：ready_rx 收到 () 表 listener 已就绪。
fn spawn_fs_server(
    path: std::path::PathBuf,
    make_dev: fn() -> CmbMockDev,
) -> (thread::JoinHandle<anyhow::Result<()>>, mpsc::Receiver<()>) {
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let h = thread::spawn(move || -> anyhow::Result<()> {
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        ready_tx.send(()).ok();
        let (mut s, _) = listener.accept()?;
        let neg = vfio_user_transport::server_handshake(&mut s)?;
        let mut sess = vfio_user_transport::VfioUserSession::new(s, neg);
        let mut dev = make_dev();
        while sess.pump_one(&mut dev)? {}
        Ok(())
    });
    (h, ready_rx)
}

/// 驱动 `reconnect_loop` 连上 server（discover CMB）并断言到 Live。`make_dev` 决定
/// server 是 trap-only（`new_trap`）还是 map 模式（`new_map`，置 FLAG_MMAP）。两路径
/// client 都应到 Live：trap 直接转发；map 模式 client **降级 trap**（无 mapper，绝不
/// mmap）+ 告警，功能仍完整。
fn run_reconnect_discover_test(make_dev: fn() -> CmbMockDev) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cmb.sock");
    let (server, ready_rx) = spawn_fs_server(path.clone(), make_dev);
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener ready");

    DefaultPool::run_with(async |driver| {
        let state = SharedState::new(DeviceState::Connecting);
        let stats = std::sync::Arc::new(WorkerStats::default());
        let (to_worker, worker_inbox) = mesh::channel::<DeviceRequest>();
        let (_shutdown_tx, shutdown_rx) = mesh::channel::<()>();
        let (reconnect_tx, reconnect_rx) = mesh::channel::<ReconnectEvent>();
        let (lost_tx, lost_rx) = mesh::channel::<()>();

        let worker = Worker::new(
            state.clone(),
            worker_inbox,
            Vec::new(),
            Vec::new(),
            stats.clone(),
            reconnect_rx,
            lost_tx,
        );
        let worker_task = driver.spawn("cmb-recon-worker", worker.run(shutdown_rx));

        // declared 显式配置 CMB BAR（BIR=2 / 2 MiB）——本测试验证「配置了 CMB 的设备」
        // 经 reconnect discover 出真 CMB 几何 → validate_cmb（BIR/size ≤ declared）通过 →
        // 到 Live。默认 `new()` 设备不带 CMB（MEDIUM-2 无 phantom BAR），故这里走
        // `new_with_cmb` 显式装上。empty eventfds → 跳过 set_irqs。
        let declared = DeclaredGeometry::new_with_cmb(
            Some(BAR0_SIZE),
            Some(MSIX_COUNT),
            CMB_BIR,
            2 * 1024 * 1024,
        );
        let connector_task = driver.spawn(
            "cmb-recon-connector",
            vfio_user_pci_device::reconnect_loop(
                driver.clone(),
                path.to_string_lossy().into_owned(),
                declared,
                Vec::new(), // eventfds
                Vec::new(), // dma regions
                ReconnectChannels {
                    reconnect_tx,
                    lost_rx,
                },
            ),
        );

        // discover（含 CMB BAR 扫描）+ validate_cmb 通过 → 到 Live。
        wait_for_state(&driver, &state, DeviceState::Live).await;

        // 到 Live 后，CMB BAR 经 trap（REGION_RW）可达（map 模式已降级，仍走 trap）。
        let cmb_pat = 0x0BAD_F00D_FEED_FACE_u64.to_le_bytes();
        to_worker.send(DeviceRequest {
            kind: ReqKind::MmioWrite {
                bar: CMB_BIR as u32,
                offset: 0x80,
                data: cmb_pat.to_vec(),
            },
        });
        let (rd, tok) = defer_read();
        to_worker.send(DeviceRequest {
            kind: ReqKind::MmioRead {
                bar: CMB_BIR as u32,
                offset: 0x80,
                size: 8,
                token: rd,
            },
        });
        let mut buf = [0u8; 8];
        tok.read_future(&mut buf).await.expect("read CMB@0x80");
        assert_eq!(buf, cmb_pat, "降级后 CMB 仍经 trap REGION_RW 完整可达");

        drop(to_worker);
        drop(connector_task);
        worker_task.await;
    });

    drop(dir);
    let _ = server.join();
}

/// ② trap 模式 server：reconnect discover CMB BAR（BIR=2）→ 到 Live + CMB 可达。
#[test]
fn reconnect_discovers_cmb_bar_and_goes_live() {
    run_reconnect_discover_test(CmbMockDev::new_trap);
}

/// ③ map 模式 server（region info 置 FLAG_MMAP）：OpenHCL client 无 mapper，**绝不 mmap**，
///    降级 trap → 仍 discover 成功 + 到 Live + CMB 经 REGION_RW 完整可达（非零拷贝）。
#[test]
fn reconnect_degrades_map_mode_cmb_to_trap_and_goes_live() {
    run_reconnect_discover_test(CmbMockDev::new_map);
}

/// ④ discover **观测**（关 M1 gap）：直接对真 server 跑 discover_cmb_geometry，断言
///    trap server → `wants_mmap == false`、map server（置 FLAG_MMAP）→ `wants_mmap == true`。
///    场景 ②③ 的 e2e 两路径都到 Live、外观相同，无法区分「降级正确」vs「FLAG_MMAP 被静默
///    忽略」；本测试在 discover 边界**直接观测** flags 真的随 server 模式变化 + 被正确解出，
///    证明降级决策的输入（wants_mmap）确由 server 实报驱动（非 no-op）。
#[test]
fn discover_observes_mmap_flag_per_server_mode() {
    use vfio_user_pci_device::CmbGeometry;
    use vfio_user_pci_device::discover_cmb_geometry;
    use vfio_user_wire::proto::pci_region;

    // 对给定 server dev 跑：connect + handshake + 逐 region 查 region info（带 fd 捕获，
    // 镜像 reconnect 的 discover 扫描），把 (index, flags, size) 喂 discover_cmb_geometry。
    fn discover_against(make_dev: fn() -> CmbMockDev) -> Option<CmbGeometry> {
        let (server_end, client_end) = UnixStream::pair().unwrap();
        let server = spawn_server(server_end, make_dev());
        let cmb = DefaultPool::run_with(async |driver| {
            let mut client =
                VfioUserClient::from_stream(&driver, client_end).expect("wrap client stream");
            client.handshake().await.expect("handshake");
            // 扫 [1, BAR5]，跳过 MSI-X BAR(index 4)，用带 fd 的 GET_REGION_INFO（map 模式
            // reply 附 memfd，client 收下即 drop——绝不 mmap；只读 flags 判 wants_mmap）。
            const MSIX_BAR_INDEX: u32 = 4;
            let mut tuples: Vec<(u8, u32, u64)> = Vec::new();
            for index in 1..=pci_region::BAR5 {
                if index == MSIX_BAR_INDEX {
                    continue;
                }
                let (info, _fds) = client
                    .get_region_info_with_fd(index)
                    .await
                    .expect("get_region_info_with_fd");
                let flags = info.flags; // packed copy
                let size = info.size;
                tuples.push((index as u8, flags, size));
            }
            // 关连接让 server 线程退出。
            drop(client);
            discover_cmb_geometry(tuples, 0, 4)
        });
        let _ = server.join();
        cmb
    }

    let trap = discover_against(CmbMockDev::new_trap).expect("trap server 应暴露 CMB BAR");
    assert_eq!(trap.bir, CMB_BIR, "trap：CMB BIR=2");
    assert!(
        !trap.wants_mmap,
        "trap server 不置 FLAG_MMAP → wants_mmap=false"
    );

    let map = discover_against(CmbMockDev::new_map).expect("map server 应暴露 CMB BAR");
    assert_eq!(map.bir, CMB_BIR, "map：CMB BIR=2");
    assert!(
        map.wants_mmap,
        "map server 置 FLAG_MMAP → wants_mmap=true（驱动 client 降级 trap + 告警；非 no-op）"
    );
}
