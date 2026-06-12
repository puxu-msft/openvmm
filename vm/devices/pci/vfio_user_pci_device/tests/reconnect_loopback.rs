// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W6b A2.4 重连引擎 SHIP gate：对**真**可重启的 `vfio_user_transport` server
//! （非 mock socket）跑完整重连生命周期，证明 3 个 CRITICAL 不变量在 usnvmemu
//! stop/restart 后存活。
//!
//! 被测引擎（in-tree，A2.1/A2.2 刚建）：
//! - [`vfio_user_pci_device::worker::Worker`]：持全双工 split 通道，按 `msg_id`
//!   路由 reply；transport 死亡 → 进 Lost + drain + 边沿通知 connector。
//! - [`vfio_user_pci_device::reconnect::reconnect_loop`]：持久重连（connect →
//!   handshake → 读真几何 → identity 校验 → set_irqs(C-3) → into_channel →
//!   `Connected` 交付 worker；等下次 Lost 边沿再重连）。
//!
//! **为什么不能用 socketpair**：`reconnect_loop` 内部调
//! `VfioUserClient::connect(&driver, &unix_path)` —— 必须有一个**文件系统路径上的
//! 真 AF_UNIX listener**，且要能 stop（关连接 + 撤 listener → client EOF + 后续
//! connect ECONNREFUSED）/ restart（rm 残留 socket + 重 bind + 重 accept）。这正是
//! `serve_unix` 的真服务路径，只是加了 stop/restart 控制。
//!
//! **harness 设计（stop/restart on one path）**：见 [`ServerController`] 文档。
//! 核心：server 跑在独立 std::thread（`VfioUserSession` 是同步的），test 经
//! `cmd`（[`ServerCmd`]）/ `ack`（[`ServerAck`]）两条 std mpsc **确定性同步**
//! server 的状态（accept-ready / probes-pumped / closed），**不靠 sleep 猜**。
//! worker + reconnect_loop 跑在 `DefaultPool` driver 上的 spawned task；test 段对
//! `state.load()` 做**有界轮询**（`PolledTimer::sleep` 让出给 reactor，非裸 yield，
//! 避开 self-waking 陷阱）。
//!
//! 覆盖 6 个场景（见各 `#[test]`）：① connect→Live→MMIO；② stop→Lost+在途 drain；
//! ③ restart→revive→MMIO；④ pair-swap 原子性（C-1，2 个在途）；⑤ identity 超
//! declared → 永不 Live（决策 b）；⑥ set_irqs 在重连时重发（C-3，非空 eventfds：
//! 初次连接 + 重连后都到达 Live ⇒ set_irqs 两次都成功）。

use chipset_device::io::IoError;
use chipset_device::io::deferred::DeferredToken;
use chipset_device::io::deferred::defer_read;
use pal_async::DefaultPool;
use pal_async::driver::Driver;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use pcie_device_core::BarKind;
use pcie_device_core::BarLayout;
use pcie_device_core::DeviceCtx;
use pcie_device_core::DeviceDescribe;
use pcie_device_core::PcieDevice;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use vfio_user_pci_device::DeclaredGeometry;
use vfio_user_pci_device::DeviceRequest;
use vfio_user_pci_device::DeviceState;
use vfio_user_pci_device::ReconnectChannels;
use vfio_user_pci_device::ReconnectEvent;
use vfio_user_pci_device::ReqKind;
use vfio_user_pci_device::SharedState;
use vfio_user_pci_device::SharedWorkerStats;
use vfio_user_pci_device::Worker;
use vfio_user_pci_device::reconnect_loop;
use vfio_user_pci_device::worker::lost_reason;

/// usnvmemu 真几何对齐（identity.rs DEFAULT_*）：BAR0 16 KiB / MSI-X 4 向量。
/// 场景 ①②③④ 用此（actual == declared → 放行）；场景 ⑤ 服务此但 declared 故意调小。
const SERVED_BAR0_SIZE: u64 = 16 * 1024;
const SERVED_MSIX_COUNT: u16 = 4;

/// 有界等待上限：state 轮询 / ack 等待。CI 慢机器留足余量但绝不无限 spin。
const WAIT_BUDGET: Duration = Duration::from_secs(10);
/// state 轮询间隔（PolledTimer::sleep，让出给 reactor 跑 worker/connector task）。
const POLL_STEP: Duration = Duration::from_millis(5);
/// ack 轮询间隔（async `try_recv` + `PolledTimer::sleep` 步进，绝不 blocking-recv）。
const ACK_STEP: Duration = Duration::from_millis(50);

// ───────────────────────────── MockDev ─────────────────────────────

/// 测试 MockDev：BAR0（region 0）+ MSI-X + identity。BAR0 大小可配（场景 ⑤ 用
/// 大于 declared 的几何触发 Exceeds）。`bar0` 槽为 u64，按 `offset/8` 寻址。
struct MockDev {
    bar0: Vec<u64>,
    bar0_size: u64,
    msix_count: u16,
    last_reset: u32,
}
impl MockDev {
    fn new(bar0_size: u64, msix_count: u16) -> Self {
        Self {
            bar0: vec![0u64; (bar0_size / 8) as usize],
            bar0_size,
            msix_count,
            last_reset: 0xFFFF_FFFF,
        }
    }
}
impl PcieDevice for MockDev {
    fn describe(&self) -> DeviceDescribe {
        DeviceDescribe {
            vendor_id: 0x1414,
            device_id: 0x00a9,
            class_code: 0x01_08_02,
            revision: 1,
            subsystem_vendor: 0,
            subsystem_device: 0,
            bars: vec![BarLayout {
                index: 0,
                size: self.bar0_size,
                kind: BarKind::Mmio32,
                prefetchable: false,
            }],
            msix_count: self.msix_count as u32,
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

// ──────────────────────── server controller ────────────────────────

/// `reconnect_loop` 连上后、发 `Connected` 前会发的 pump 数（empty eventfds → 跳过
/// set_irqs）：`get_region_info(BAR0)` + `get_irq_info(MSIX)` = 2 条 request/reply。
/// "probe-then-deaf" 模式 pump 恰好这么多就到达「worker 即将 Live」的点，之后任何
/// MMIO read 都会滞留 socket 成为**真在途**请求（server 不再 pump → 不回 reply）。
const PRELIVE_PROBE_PUMPS: usize = 2;

/// 同上但**非空 eventfds**：reconnect_loop 在 into_channel 前**还会** set_irqs
/// （C-3），故到 Live 前夜多一条 request/reply（probes 2 + set_irqs 1 = 3）。场景 ⑥
/// 用此 pump 数让连接器把 set_irqs 也发完后再到 Live。
const PRELIVE_PROBE_PUMPS_WITH_IRQS: usize = PRELIVE_PROBE_PUMPS + 1;

/// test → server 的控制命令。
enum ServerCmd {
    /// accept 一条连接 + handshake + pump-forever（answer 所有 probe/MMIO）直到
    /// client 关闭或硬错；然后回到 accept（天然支持场景 ⑤ 的反复 connect/reject）。
    /// 每次准备好 accept 前回 [`ServerAck::AcceptReady`]。
    ServeForever,
    /// **反复** accept + handshake + pump probes（每条连接都答 region/irq info）直到
    /// 收到 [`ServerCmd::Shutdown`]。用于场景 ⑤：reconnect_loop 因 Exceeds 不停
    /// connect→probe→reject→重连，server 须对**每一轮**都应答 probes 才能真正驱动
    /// 多次 reject 循环（验「stays retrying」）。非阻塞 accept + 轮询 cmd_rx。
    /// 就绪后回 [`ServerAck::AcceptReady`]（一次）。
    ServeRejectCycles,
    /// accept 一条连接 + handshake + pump 恰好 [`PRELIVE_PROBE_PUMPS`] 条（到 Live
    /// 前夜），回 [`ServerAck::ProbesPumped`]，然后**变聋**（不再 pump）等下一条命令。
    AcceptProbesThenDeaf,
    /// 同 [`AcceptProbesThenDeaf`] 但 pump [`PRELIVE_PROBE_PUMPS_WITH_IRQS`] 条
    /// （多吞一条 set_irqs）。场景 ⑥ 用：连接器带非空 eventfds，set_irqs 成功后才到
    /// Live；deaf 之后照样可被 StopConnection 关掉走重连。
    AcceptProbesIrqsThenDeaf,
    /// 关掉当前 deaf 连接 + 撤 listener（→ client reader EOF + 后续 connect 失败），
    /// 回 [`ServerAck::Closed`]。模拟 usnvmemu stop。
    StopConnection,
    /// rm 残留 socket + 重 bind listener（POC-4：必须先 rm 否则 bind EADDRINUSE），
    /// 回 [`ServerAck::Restarted`]。模拟 usnvmemu restart。
    Restart,
    /// 退出 server 线程。
    Shutdown,
}

/// server → test 的应答（确定性同步，免 sleep 猜状态）。
#[derive(Debug, PartialEq, Eq)]
enum ServerAck {
    /// `ServeForever` / 重启后：listener 已就绪，即将 accept（client connect 可成功）。
    AcceptReady,
    /// `AcceptProbesThenDeaf`：probes 已 pump 完，连接保持 open 但变聋。
    ProbesPumped,
    /// `StopConnection`：连接已关 + listener 已撤。
    Closed,
    /// `Restart`：listener 已在原 path 重 bind。
    Restarted,
}

/// 在文件系统路径上跑的可停/可重启 server 控制器（独立 std::thread）。
///
/// 持久 [`UnixListener`]（bind 在 `path`）；命令驱动 accept / probe-deaf /
/// stop（关连接 + drop listener）/ restart（rm + 重 bind）。
struct ServerController {
    path: PathBuf,
    cmd_tx: mpsc::Sender<ServerCmd>,
    ack_rx: mpsc::Receiver<ServerAck>,
    handle: Option<thread::JoinHandle<anyhow::Result<()>>>,
}

impl ServerController {
    /// 起 server 线程：先在 `path` bind listener，初始空闲等命令。`bar0_size` /
    /// `msix_count` = MockDev 服务的真几何（每次 accept 新建一个干净 MockDev）。
    fn start(path: PathBuf, bar0_size: u64, msix_count: u16) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<ServerCmd>();
        let (ack_tx, ack_rx) = mpsc::channel::<ServerAck>();
        let thread_path = path.clone();
        let handle = thread::spawn(move || {
            server_thread(thread_path, bar0_size, msix_count, cmd_rx, ack_tx)
        });
        Self {
            path,
            cmd_tx,
            ack_rx,
            handle: Some(handle),
        }
    }

    /// 发命令（线程若已退出则 sender 失败，测试随即在 ack 等待处暴露）。
    fn send(&self, cmd: ServerCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// **async** 有界等待一个期望的 ack。关键：用 `try_recv` 轮询 + `PolledTimer::sleep`
    /// 让出，**绝不**在 reactor 线程上 blocking-recv —— `DefaultPool` 单线程协作调度，
    /// 阻塞 recv 会饿死 worker/connector task（它们要跑才能 connect/probe 让 server
    /// 产 ack），造成 server↔reactor 死锁。轮询让 reactor 持续驱动 task。
    /// 超 [`WAIT_BUDGET`] panic（绝不无限挂）。
    async fn expect_ack(&self, driver: &impl Driver, want: ServerAck) {
        let mut timer = PolledTimer::new(driver);
        let mut waited = Duration::ZERO;
        loop {
            match self.ack_rx.try_recv() {
                Ok(got) => {
                    assert_eq!(got, want, "server ack 不符");
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    assert!(
                        waited < WAIT_BUDGET,
                        "等 server ack {want:?} 超时（{WAIT_BUDGET:?}）"
                    );
                    timer.sleep(ACK_STEP).await;
                    waited += ACK_STEP;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("server 线程已退出，无法收到 ack {want:?}");
                }
            }
        }
    }
}

impl Drop for ServerController {
    fn drop(&mut self) {
        // 收尾：让 server 线程退出 + 清 socket 文件（tempdir 也会清，双保险）。
        self.send(ServerCmd::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 一次 accept + handshake + 建 session；失败返回 None（让 caller backoff 重试）。
fn accept_one(listener: &UnixListener) -> Option<vfio_user_transport::VfioUserSession> {
    let (mut stream, _addr) = listener.accept().ok()?;
    let neg = vfio_user_transport::server_handshake(&mut stream).ok()?;
    Some(vfio_user_transport::VfioUserSession::new(stream, neg))
}

/// "probe-then-deaf" 服务体的结果，告诉外层 server_thread 该做什么。
enum DeafOutcome {
    /// accept/handshake 失败：已回 ProbesPumped；外层应保留 listener + 继续等命令
    /// （不撤 listener、不再回 ack）。
    AcceptFailed,
    /// 收到 StopConnection：连接已关（session drop），外层应撤 listener + 回 Closed。
    Stopped,
    /// 收到 Shutdown / sender 关 / 协议外命令：外层应退出 server 线程。
    Exit,
}

/// accept 一条连接 + handshake + pump 恰好 `pumps` 条（到 Live 前夜）+ 回
/// [`ServerAck::ProbesPumped`] + **变聋**（持 session 不再 pump）阻塞等下条命令。
/// [`ServerCmd::AcceptProbesThenDeaf`]（`pumps = PRELIVE_PROBE_PUMPS`）与
/// [`ServerCmd::AcceptProbesIrqsThenDeaf`]（`pumps = PRELIVE_PROBE_PUMPS_WITH_IRQS`，
/// 多吞一条 set_irqs）共用此体（DRY）。返回 [`DeafOutcome`] 指示外层后续动作。
fn serve_probes_then_deaf(
    listener: &UnixListener,
    bar0_size: u64,
    msix_count: u16,
    pumps: usize,
    cmd_rx: &mpsc::Receiver<ServerCmd>,
    ack_tx: &mpsc::Sender<ServerAck>,
) -> DeafOutcome {
    let Some(mut sess) = accept_one(listener) else {
        // accept/handshake 失败：仍回 ack 让 test 推进（随后必失败暴露）；保留 listener。
        let _ = ack_tx.send(ServerAck::ProbesPumped);
        return DeafOutcome::AcceptFailed;
    };
    let mut dev = MockDev::new(bar0_size, msix_count);
    // pump 恰好 pre-Live 条（probes [+ set_irqs]），到 Live 前夜。
    let mut pumped = 0usize;
    while pumped < pumps {
        match sess.pump_one(&mut dev) {
            Ok(true) => pumped += 1,
            Ok(false) | Err(_) => break,
        }
    }
    // 变聋：通知 test，然后**持着 session（连接 open）**阻塞等下条命令。
    // 关键：此后不再 pump → worker 发来的 MMIO read 滞留 socket = 真在途。
    let _ = ack_tx.send(ServerAck::ProbesPumped);
    // 阻塞等 StopConnection / Shutdown；期间连接保持 open 不回任何 reply。
    match cmd_rx.recv() {
        Ok(ServerCmd::StopConnection) => {
            drop(sess); // 关连接 → client reader EOF。
            DeafOutcome::Stopped
        }
        // Shutdown / sender 关 / 协议外命令：丢弃 session 保守退出。
        _ => DeafOutcome::Exit,
    }
}

/// server 线程主体：bind listener，命令循环。
fn server_thread(
    path: PathBuf,
    bar0_size: u64,
    msix_count: u16,
    cmd_rx: mpsc::Receiver<ServerCmd>,
    ack_tx: mpsc::Sender<ServerAck>,
) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(&path);
    let mut listener = Some(UnixListener::bind(&path)?);

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            ServerCmd::ServeForever => {
                // 通知 test：listener 就绪，client connect 会成功。
                let _ = ack_tx.send(ServerAck::AcceptReady);
                let l = listener.as_ref().expect("listener present in ServeForever");
                // accept-pump 循环：client 关闭（Ok(false)）/ 硬错就回到外层等下条命令。
                if let Some(mut sess) = accept_one(l) {
                    let mut dev = MockDev::new(bar0_size, msix_count);
                    // pump 到 client 断（Ok(false)）或硬错（Err）为止。
                    while let Ok(true) = sess.pump_one(&mut dev) {}
                }
                // 连接结束（client 主动断 / 出错）；listener 仍在，等下条命令
                // （场景 ⑤：reconnect_loop reject 后 drop client → 这里 break →
                //  test 不发新命令，靠 drop 收尾；或下一个 ServeForever 再 accept）。
            }
            ServerCmd::ServeRejectCycles => {
                let _ = ack_tx.send(ServerAck::AcceptReady);
                let l = listener
                    .as_ref()
                    .expect("listener present in reject-cycles");
                // 非阻塞 accept：轮询期间检查 Shutdown，避免一轮 reject 后卡死。
                l.set_nonblocking(true).expect("set listener nonblocking");
                loop {
                    // 收到 Shutdown / sender 关 → 收工。
                    match cmd_rx.try_recv() {
                        Ok(ServerCmd::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => {
                            return Ok(());
                        }
                        Ok(_) | Err(mpsc::TryRecvError::Empty) => {}
                    }
                    match l.accept() {
                        Ok((mut stream, _addr)) => {
                            // 接上后切回阻塞做同步 handshake + probes（client 侧由 reactor 驱动）。
                            stream.set_nonblocking(false).expect("stream blocking");
                            let Ok(neg) = vfio_user_transport::server_handshake(&mut stream) else {
                                continue; // 握手失败：drop 连接，等下一轮重连。
                            };
                            let mut sess = vfio_user_transport::VfioUserSession::new(stream, neg);
                            let mut dev = MockDev::new(bar0_size, msix_count);
                            // 答 probes（region_info + irq_info）；client 读到 actual>declared →
                            // Exceeds → drop client → 本连接 pump_one 得 Ok(false) 退出 → 再 accept。
                            while let Ok(true) = sess.pump_one(&mut dev) {}
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            // 暂无新连接：小睡让出 CPU（独立线程，sleep 安全）再轮询。
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return Ok(()), // listener 异常：收工。
                    }
                }
            }
            ServerCmd::AcceptProbesThenDeaf => {
                let l = listener.as_ref().expect("listener present in deaf-accept");
                match serve_probes_then_deaf(
                    l,
                    bar0_size,
                    msix_count,
                    PRELIVE_PROBE_PUMPS,
                    &cmd_rx,
                    &ack_tx,
                ) {
                    DeafOutcome::AcceptFailed => continue, // 保留 listener，等下条命令。
                    DeafOutcome::Stopped => {
                        listener = None; // 撤 listener → 后续 connect ECONNREFUSED。
                        let _ = ack_tx.send(ServerAck::Closed);
                    }
                    DeafOutcome::Exit => return Ok(()),
                }
            }
            ServerCmd::AcceptProbesIrqsThenDeaf => {
                // 同上但多吞一条 set_irqs（连接器带非空 eventfds，C-3）。场景 ⑥。
                let l = listener
                    .as_ref()
                    .expect("listener present in deaf-accept-irqs");
                match serve_probes_then_deaf(
                    l,
                    bar0_size,
                    msix_count,
                    PRELIVE_PROBE_PUMPS_WITH_IRQS,
                    &cmd_rx,
                    &ack_tx,
                ) {
                    DeafOutcome::AcceptFailed => continue,
                    DeafOutcome::Stopped => {
                        listener = None;
                        let _ = ack_tx.send(ServerAck::Closed);
                    }
                    DeafOutcome::Exit => return Ok(()),
                }
            }
            ServerCmd::StopConnection => {
                // 无 deaf 连接持有时收到（防御）：直接撤 listener。
                listener = None;
                let _ = ack_tx.send(ServerAck::Closed);
            }
            ServerCmd::Restart => {
                // POC-4：rm 残留 socket（StopConnection 撤 listener 后文件仍在）再重 bind。
                let _ = std::fs::remove_file(&path);
                listener = Some(UnixListener::bind(&path)?);
                let _ = ack_tx.send(ServerAck::Restarted);
            }
            ServerCmd::Shutdown => return Ok(()),
        }
    }
    Ok(())
}

// ─────────────────────── worker + reconnect 装配 ───────────────────────

/// test 侧持有的 worker/connector 句柄 + 控制端点。
struct Engine {
    state: SharedState,
    stats: SharedWorkerStats,
    to_worker: mesh::Sender<DeviceRequest>,
    _shutdown_tx: mesh::Sender<()>,
    _worker_task: Task<()>,
    _connector_task: Task<()>,
}

/// 装配 worker + reconnect_loop（最小版，对标 resolver::assemble_device 但无 cfg
/// 仿真器）：state 初始 Connecting，declared 由 caller 给（场景 ⑤ 调小触发 Exceeds）。
/// `eventfds` 透传给 reconnect_loop：空 vec → 跳过 set_irqs（场景 ①②③④⑤）；非空
/// → 每次（重）连接都 set_irqs（场景 ⑥ 验 C-3）。irq task 这里**不需要**（worker 传
/// 空 irq_tasks）；重要的只是 connector 拿到的 eventfds，镜像 resolver 把
/// `connector_events` 交给 `reconnect_loop` 的方式。
fn spawn_engine(
    driver: &(impl Driver + Spawn + Clone),
    unix_path: String,
    declared: DeclaredGeometry,
    eventfds: Vec<pal_event::Event>,
) -> Engine {
    let state = SharedState::new(DeviceState::Connecting);
    let stats: SharedWorkerStats = std::sync::Arc::new(Default::default());
    let (to_worker, worker_inbox) = mesh::channel::<DeviceRequest>();
    let (shutdown_tx, shutdown_rx) = mesh::channel::<()>();
    let (reconnect_tx, reconnect_rx) = mesh::channel::<ReconnectEvent>();
    let (lost_tx, lost_rx) = mesh::channel::<()>();

    let worker = Worker::new(
        state.clone(),
        worker_inbox,
        Vec::new(), // interrupts：MMIO read 不需 MSI-X。
        Vec::new(), // irq_tasks。
        stats.clone(),
        reconnect_rx,
        lost_tx,
    );
    let worker_task = driver.spawn("a24-worker", worker.run(shutdown_rx));

    let connector_task = driver.spawn(
        "a24-reconnect",
        reconnect_loop(
            driver.clone(),
            unix_path,
            declared,
            eventfds, // 空 → 跳过 set_irqs；非空 → 每次连接 set_irqs（C-3）。
            ReconnectChannels {
                reconnect_tx,
                lost_rx,
            },
        ),
    );

    Engine {
        state,
        stats,
        to_worker,
        _shutdown_tx: shutdown_tx,
        _worker_task: worker_task,
        _connector_task: connector_task,
    }
}

/// 有界轮询直到 `state == want`；每步 `PolledTimer::sleep`（让出 reactor 跑 task），
/// 超 [`WAIT_BUDGET`] panic（绝不无限 spin）。
async fn wait_state(driver: &impl Driver, state: &SharedState, want: DeviceState) {
    let mut timer = PolledTimer::new(driver);
    let mut waited = Duration::ZERO;
    while state.load() != want {
        assert!(
            waited < WAIT_BUDGET,
            "等 state == {want:?} 超时（{WAIT_BUDGET:?}），实际 = {:?}",
            state.load()
        );
        timer.sleep(POLL_STEP).await;
        waited += POLL_STEP;
    }
}

/// 有界轮询直到 `inflight_current == want`；每步让出 reactor。用于在 stop 前确认
/// MMIO read 已被 worker 发帧 + 插 in_flight（**真在途**），从而让随后的 outage
/// 走 reader-EOF drain 路径而非 write-fail 短路 —— 这才是「在途 drain」的精确测点。
async fn wait_inflight(driver: &impl Driver, stats: &SharedWorkerStats, want: u64) {
    let mut timer = PolledTimer::new(driver);
    let mut waited = Duration::ZERO;
    while stats.inflight_current.load(Ordering::Relaxed) != want {
        assert!(
            waited < WAIT_BUDGET,
            "等 inflight_current == {want} 超时（{WAIT_BUDGET:?}），实际 = {}",
            stats.inflight_current.load(Ordering::Relaxed)
        );
        timer.sleep(POLL_STEP).await;
        waited += POLL_STEP;
    }
}
async fn assert_state_never(
    driver: &impl Driver,
    state: &SharedState,
    forbidden: DeviceState,
    window: Duration,
) {
    let mut timer = PolledTimer::new(driver);
    let mut waited = Duration::ZERO;
    while waited < window {
        assert_ne!(
            state.load(),
            forbidden,
            "state 不该达到 {forbidden:?}（决策 b：identity 超 declared 须永不 Live）"
        );
        timer.sleep(POLL_STEP).await;
        waited += POLL_STEP;
    }
}

/// 发一个 MMIO read 并 await 其 token（size 8）。返回 token 结果（Ok=数据，Err=IoError）。
async fn mmio_read8(
    to_worker: &mesh::Sender<DeviceRequest>,
    bar: u32,
    offset: u64,
) -> Result<[u8; 8], IoError> {
    let (rd, tok) = defer_read();
    to_worker.send(DeviceRequest {
        kind: ReqKind::MmioRead {
            bar,
            offset,
            size: 8,
            token: rd,
        },
    });
    let mut buf = [0u8; 8];
    read_token(tok, &mut buf).await?;
    Ok(buf)
}

/// await 一个 DeferredToken（read），把结果搬进 `buf`。
async fn read_token(tok: DeferredToken, buf: &mut [u8; 8]) -> Result<(), IoError> {
    tok.read_future(buf).await
}

/// 在 Live 后选一个非 0 BAR0 offset 做 write→read oracle，避开「恰好读到零初值」
/// 的伪通过。
fn seed_then_read_offset() -> u64 {
    64
}

// ──────────────────────────── 场景 ① ────────────────────────────

/// ① connect→Live→MMIO works：起 server（ServeForever），spawn worker+reconnect
/// （declared = 真几何），等 Live，经 worker MMIO write 预置 BAR0 寄存器，再 MMIO
/// read 拿回 seed 值。证明：reconnect_loop 首次连接 + worker 转 Live + MMIO 路由通。
#[test]
fn scenario1_connect_live_mmio() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("vfio.sock");
    let server = ServerController::start(path.clone(), SERVED_BAR0_SIZE, SERVED_MSIX_COUNT);

    DefaultPool::run_with(async |driver| {
        server.send(ServerCmd::ServeForever);
        server.expect_ack(&driver, ServerAck::AcceptReady).await;

        let declared = DeclaredGeometry::new(Some(SERVED_BAR0_SIZE), Some(SERVED_MSIX_COUNT));
        let eng = spawn_engine(
            &driver,
            path.to_string_lossy().into_owned(),
            declared,
            Vec::new(),
        );

        // 等 reconnect_loop 连上 + worker 转 Live（有界）。
        wait_state(&driver, &eng.state, DeviceState::Live).await;

        // 经 worker MMIO write 预置 BAR0@offset（fire-and-forget），再 read 回来。
        let off = seed_then_read_offset();
        let pat = 0x0102_0304_0506_0708_u64.to_le_bytes();
        eng.to_worker.send(DeviceRequest {
            kind: ReqKind::MmioWrite {
                bar: 0,
                offset: off,
                data: pat.to_vec(),
            },
        });
        let got = mmio_read8(&eng.to_worker, 0, off)
            .await
            .expect("Live 期 MMIO read 应成功");
        assert_eq!(
            got, pat,
            "MMIO write→read 应 round-trip（连接健康 + msg_id 路由）"
        );
    });
    // server 在 ServeForever 的 pump 循环里；client（reconnect_loop）随 DefaultPool
    // 结束被 drop → 连接关 → server pump_one Ok(false) → 回外层等命令。drop 收尾。
    drop(server);
}

// ──────────────────────────── 场景 ② ────────────────────────────

/// ② server stop → Lost + 在途 drain + lost 边沿一次：用 "probe-then-deaf" server
/// 让 worker 到 Live 前夜，发一个 MMIO read（server 聋 → 滞留 = 真在途），再
/// StopConnection（关 socket）→ 断言 state 到 Lost 且在途 token resolve 成 NoResponse。
#[test]
fn scenario2_stop_lost_and_drain() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("vfio.sock");
    let server = ServerController::start(path.clone(), SERVED_BAR0_SIZE, SERVED_MSIX_COUNT);

    DefaultPool::run_with(async |driver| {
        // server：accept + pump probes 后变聋。
        server.send(ServerCmd::AcceptProbesThenDeaf);

        let declared = DeclaredGeometry::new(Some(SERVED_BAR0_SIZE), Some(SERVED_MSIX_COUNT));
        let eng = spawn_engine(
            &driver,
            path.to_string_lossy().into_owned(),
            declared,
            Vec::new(),
        );

        // server pump 完 2 probes 后回 ack；此刻 reconnect_loop 已 into_channel +
        // 发 Connected → worker 即将/已 Live。
        server.expect_ack(&driver, ServerAck::ProbesPumped).await;
        wait_state(&driver, &eng.state, DeviceState::Live).await;

        // 发一个 MMIO read：server 聋，不会回 reply → 真在途（挂在 worker in_flight）。
        let (rd, tok) = defer_read();
        eng.to_worker.send(DeviceRequest {
            kind: ReqKind::MmioRead {
                bar: 0,
                offset: seed_then_read_offset(),
                size: 8,
                token: rd,
            },
        });

        // **关键**：先确认 read 已被 worker 发帧 + 插 in_flight（真在途）再 stop。
        // 否则 stop 与 worker 处理 read 竞争：若 stop 先到，worker 的 send_region_read
        // 会写到已关 socket → 走 write-fail 短路（token 直接 NoResponse，不入 in_flight），
        // 测不到「在途 drain」。等 inflight==1 锁定走 reader-EOF drain 路径。
        wait_inflight(&driver, &eng.stats, 1).await;

        // stop server：关连接 → worker reader EOF → go_lost（store Lost + drain in_flight）。
        server.send(ServerCmd::StopConnection);
        server.expect_ack(&driver, ServerAck::Closed).await;

        // 断言①：state 到 Lost（有界）。
        wait_state(&driver, &eng.state, DeviceState::Lost).await;

        // 断言②：在途 token resolve 成 NoResponse（drain_in_flight 的语义）。
        let mut buf = [0u8; 8];
        let r = read_token(tok, &mut buf).await;
        assert!(
            matches!(r, Err(IoError::NoResponse)),
            "在途 read 应被 drain 成 NoResponse，got {r:?}"
        );

        // 断言③（观察 lost 边沿 + drain 完成）：token 已 resolve ⇒ go_lost 必已跑过
        // store(Lost)+drain。校验 stats 的可观察足迹：
        //  - last_lost_reason 含 READ_ERR（in_flight 锁定后唯一能察觉 socket 死的是
        //    reader 的 EOF；worker 此刻无新帧要写，故必走 reader-EOF 边沿）；
        //  - inflight_current 归零（drain 把在途清空）；
        //  - last_lost_at_ms 非 0（时间戳已记）。
        let reason = eng.stats.last_lost_reason.load(Ordering::Relaxed);
        assert!(
            reason & lost_reason::READ_ERR != 0,
            "in_flight 锁定后 lost 原因应含 READ_ERR（reader EOF 边沿），got bits {reason:#x}"
        );
        assert_eq!(
            eng.stats.inflight_current.load(Ordering::Relaxed),
            0,
            "drain 后 inflight_current 应归零"
        );
        assert_ne!(
            eng.stats.last_lost_at_ms.load(Ordering::Relaxed),
            0,
            "进入 Lost 应记录时间戳"
        );
    });
    drop(server);
}

/// ③ restart → revive Live → MMIO works again：场景 ② 的 Lost 之后 Restart server
/// （同 path 重 bind + ServeForever）→ 断言 state 回 Live（reconnect_loop 自动重连）
/// → 再发 MMIO read 成功。证明 **C-1**（换上新连接对 + 新连接健康，旧 in_flight 不
/// 污染）。注：本场景用 **empty eventfds**，故 reconnect_loop **跳过** set_irqs，
/// 并**不**验证 C-3（set_irqs 在重连时重发）——C-3 由场景 ⑥（非空 eventfds）覆盖。
#[test]
fn scenario3_restart_revive_mmio() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("vfio.sock");
    let server = ServerController::start(path.clone(), SERVED_BAR0_SIZE, SERVED_MSIX_COUNT);

    DefaultPool::run_with(async |driver| {
        // ── 先走到 Lost（同场景 ②）──
        server.send(ServerCmd::AcceptProbesThenDeaf);
        let declared = DeclaredGeometry::new(Some(SERVED_BAR0_SIZE), Some(SERVED_MSIX_COUNT));
        let eng = spawn_engine(
            &driver,
            path.to_string_lossy().into_owned(),
            declared,
            Vec::new(),
        );
        server.expect_ack(&driver, ServerAck::ProbesPumped).await;
        wait_state(&driver, &eng.state, DeviceState::Live).await;
        server.send(ServerCmd::StopConnection);
        server.expect_ack(&driver, ServerAck::Closed).await;
        wait_state(&driver, &eng.state, DeviceState::Lost).await;

        // ── restart：rm 残留 socket + 重 bind（POC-4）──
        server.send(ServerCmd::Restart);
        server.expect_ack(&driver, ServerAck::Restarted).await;
        // 重 bind 后让 server accept-pump-forever（answer reconnect_loop 的重连 probes
        // + 之后的 MMIO read）。
        server.send(ServerCmd::ServeForever);
        server.expect_ack(&driver, ServerAck::AcceptReady).await;

        // 断言：reconnect_loop 自动重连 → worker 回 Live（有界；含 backoff 退避）。
        wait_state(&driver, &eng.state, DeviceState::Live).await;

        // 新连接上 MMIO write→read round-trip（证明换上的新连接健康）。
        let off = seed_then_read_offset();
        let pat = 0xCAFE_F00D_1234_5678_u64.to_le_bytes();
        eng.to_worker.send(DeviceRequest {
            kind: ReqKind::MmioWrite {
                bar: 0,
                offset: off,
                data: pat.to_vec(),
            },
        });
        let got = mmio_read8(&eng.to_worker, 0, off)
            .await
            .expect("revive 后 MMIO read 应成功");
        assert_eq!(got, pat, "revive 后新连接 MMIO write→read 应 round-trip");
    });
    drop(server);
}

// ──────────────────────────── 场景 ④ ────────────────────────────

/// ④ pair-swap 原子性（C-1）：2 个并发在途 MmioRead（不同 offset / msg_id），stop
/// server 前都不回 → 断言**两个** token 都 resolve 成 NoResponse（不交叉/不挂）。
/// restart→Live 后一个 fresh read 成功。证明 drain-on-swap 把旧在途恰好各 drain 一次，
/// 新连接不被旧 in_flight 污染。
#[test]
fn scenario4_pair_swap_atomicity() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("vfio.sock");
    let server = ServerController::start(path.clone(), SERVED_BAR0_SIZE, SERVED_MSIX_COUNT);

    DefaultPool::run_with(async |driver| {
        server.send(ServerCmd::AcceptProbesThenDeaf);
        let declared = DeclaredGeometry::new(Some(SERVED_BAR0_SIZE), Some(SERVED_MSIX_COUNT));
        let eng = spawn_engine(
            &driver,
            path.to_string_lossy().into_owned(),
            declared,
            Vec::new(),
        );
        server.expect_ack(&driver, ServerAck::ProbesPumped).await;
        wait_state(&driver, &eng.state, DeviceState::Live).await;

        // 2 个并发在途 read（不同 offset → 不同 msg_id），server 聋故都滞留。
        let (rd_a, tok_a) = defer_read();
        let (rd_b, tok_b) = defer_read();
        eng.to_worker.send(DeviceRequest {
            kind: ReqKind::MmioRead {
                bar: 0,
                offset: 64,
                size: 8,
                token: rd_a,
            },
        });
        eng.to_worker.send(DeviceRequest {
            kind: ReqKind::MmioRead {
                bar: 0,
                offset: 128,
                size: 8,
                token: rd_b,
            },
        });

        // **关键**：先确认两个 read 都已发帧 + 插 in_flight（inflight==2）再 stop，
        // 锁定走 reader-EOF drain 路径（而非任一 write-fail 短路），才真正测「drain-on-swap
        // 把旧在途各 drain 一次」。
        wait_inflight(&driver, &eng.stats, 2).await;

        // stop → 两个在途都该被 drain。
        server.send(ServerCmd::StopConnection);
        server.expect_ack(&driver, ServerAck::Closed).await;
        wait_state(&driver, &eng.state, DeviceState::Lost).await;

        let mut buf_a = [0u8; 8];
        let mut buf_b = [0u8; 8];
        let ra = read_token(tok_a, &mut buf_a).await;
        let rb = read_token(tok_b, &mut buf_b).await;
        assert!(
            matches!(ra, Err(IoError::NoResponse)),
            "在途 read A 应 drain 成 NoResponse，got {ra:?}"
        );
        assert!(
            matches!(rb, Err(IoError::NoResponse)),
            "在途 read B 应 drain 成 NoResponse，got {rb:?}"
        );
        // drain 后 inflight 必归零（两个都恰好 drain 一次，无残留 / 无重复）。
        assert_eq!(
            eng.stats.inflight_current.load(Ordering::Relaxed),
            0,
            "两个在途 drain 后 inflight_current 应归零"
        );

        // restart → Live → fresh read 成功（新连接不被旧 in_flight 污染）。
        server.send(ServerCmd::Restart);
        server.expect_ack(&driver, ServerAck::Restarted).await;
        server.send(ServerCmd::ServeForever);
        server.expect_ack(&driver, ServerAck::AcceptReady).await;
        wait_state(&driver, &eng.state, DeviceState::Live).await;

        let off = seed_then_read_offset();
        let pat = 0x1111_2222_3333_4444_u64.to_le_bytes();
        eng.to_worker.send(DeviceRequest {
            kind: ReqKind::MmioWrite {
                bar: 0,
                offset: off,
                data: pat.to_vec(),
            },
        });
        let got = mmio_read8(&eng.to_worker, 0, off)
            .await
            .expect("pair-swap 后 fresh MMIO read 应成功");
        assert_eq!(got, pat, "pair-swap + restart 后新连接 read 应 round-trip");
    });
    drop(server);
}

// ──────────────────────────── 场景 ⑤ ────────────────────────────

/// ⑤ identity 超 declared → 永不 Live（决策 b）：server 服务 BAR0=16384/msix=4，但
/// declared = (512, 4)（declared bar0 512 < actual 16384）→ reconnect_loop 在
/// validate_identity 命中 Exceeds → 永远 backoff 重试、绝不发 Connected → state 在
/// 有界窗口内**从不**达到 Live。
#[test]
fn scenario5_identity_exceeds_stays_non_live() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("vfio.sock");
    // server 服务真几何（16 KiB / 4），但 declared 故意调小到 512。
    let server = ServerController::start(path.clone(), SERVED_BAR0_SIZE, SERVED_MSIX_COUNT);

    DefaultPool::run_with(async |driver| {
        // 反复 accept + 答 probes：让 reconnect_loop 每轮都能 connect→probe→reject，
        // 真正驱动多次 reject 循环（验「stays retrying」），而非一轮后卡死。
        server.send(ServerCmd::ServeRejectCycles);
        server.expect_ack(&driver, ServerAck::AcceptReady).await;

        // declared bar0 512 < actual 16384 → Exceeds。
        let declared = DeclaredGeometry::new(Some(512), Some(SERVED_MSIX_COUNT));
        let eng = spawn_engine(
            &driver,
            path.to_string_lossy().into_owned(),
            declared,
            Vec::new(),
        );

        // 有界窗口内 state 从不达到 Live（reconnect_loop 反复 connect→probe→reject）。
        // 窗口需足够长以覆盖至少几轮 connect+probe+reject（backoff 100ms 起）。
        assert_state_never(
            &driver,
            &eng.state,
            DeviceState::Live,
            Duration::from_secs(2),
        )
        .await;

        // 额外确认：仍停在 Connecting（从未 Live，也未因别的原因 Lost）。
        // 注：reconnect_loop 的 reject 走 backoff continue，不触碰 worker state；
        // worker 初始 Connecting 且无 transport，故应恒为 Connecting。
        assert_eq!(
            eng.state.load(),
            DeviceState::Connecting,
            "Exceeds 拒绝期 worker 应停在 Connecting（从未收到 Connected）"
        );
    });
    drop(server);
}

// ──────────────────────────── 场景 ⑥ ────────────────────────────

/// ⑥ set_irqs 在重连时重发（**C-3**）：前 5 个场景都用 **empty eventfds**，故
/// `reconnect_loop` **跳过** set_irqs —— C-3（连接器在重连后把 set_irqs 重发给**新**
/// client）在 wire 层从未被测。本场景用 **非空 eventfds**（4 个 `pal_event::Event`，
/// 镜像 `resolver::assemble_device` 把 `connector_events` 交给 `reconnect_loop` 的方式）
/// 补上这条覆盖。
///
/// **如何观测 set_irqs（采用 Live-with-fds 代理，理由如下）**：SET_IRQS 在本仓库
/// 的 server 侧由 `VfioUserSession::pump_one` 内部 dispatch 到 `handle_set_irqs`，落进
/// `session.irq_vectors`——但该字段是 `pub(crate)`，且 `PcieDevice`（MockDev）trait 根本
/// 看不到 SET_IRQS（它在 session 层就被消费）。故从测试 server **无法**在不改 engine /
/// transport crate 的前提下计数 SET_IRQS 收据（选项 a/b 均不可行；选项 c 需 server 绕开
/// `pump_one` 自行解析 wire 帧 = 大改且不再走真 `handle_set_irqs` 路径）。
///
/// 因此用 prompt 指定的 fallback —— **Live-with-fds 代理**：`reconnect_loop` 在
/// `into_channel`（拆全双工半 → 发 `Connected`）**之前**先 set_irqs，且 set_irqs 失败会
/// `continue`（backoff）**绝不**发 `Connected`。所以 **带非空 eventfds 还能到达 Live ⇒
/// 这一轮的 set_irqs 必然成功**。本场景断言初次连接 + 一次 stop→restart 重连**两次**都
/// 到 Live（非空 eventfds），即证明 C-3 的「每次连接都（重新）发 set_irqs 且成功」路径在
/// **初连与重连**都被执行（既非被跳过、也未 error 阻断 connect→Live）。
#[test]
fn scenario6_set_irqs_reissued_on_reconnect() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("vfio.sock");
    let server = ServerController::start(path.clone(), SERVED_BAR0_SIZE, SERVED_MSIX_COUNT);

    DefaultPool::run_with(async |driver| {
        // 非空 eventfds：4 个 `pal_event::Event`（= SERVED_MSIX_COUNT），透传给
        // reconnect_loop → 每次（重）连接都 set_irqs（C-3）。这里**不**需要 irq task
        // （worker 收空 irq_tasks）；要紧的只是连接器持有的 eventfds，镜像 resolver
        // 把 connector_events 交给 reconnect_loop 的方式。
        let eventfds: Vec<pal_event::Event> = (0..SERVED_MSIX_COUNT)
            .map(|_| pal_event::Event::new())
            .collect();

        // ── 初次连接：server 答 probes + set_irqs（PRELIVE_PROBE_PUMPS_WITH_IRQS）后变聋 ──
        server.send(ServerCmd::AcceptProbesIrqsThenDeaf);
        let declared = DeclaredGeometry::new(Some(SERVED_BAR0_SIZE), Some(SERVED_MSIX_COUNT));
        let eng = spawn_engine(
            &driver,
            path.to_string_lossy().into_owned(),
            declared,
            eventfds,
        );

        // server pump 完 probes + set_irqs 后回 ack；此刻连接器已 into_channel + 发
        // Connected → worker 即将/已 Live。
        server.expect_ack(&driver, ServerAck::ProbesPumped).await;
        // **断言①（初连 set_irqs 成功代理）**：带非空 eventfds 到达 Live ⇒ 初次连接的
        // set_irqs 必已成功（否则 reconnect_loop backoff continue、绝不发 Connected）。
        wait_state(&driver, &eng.state, DeviceState::Live).await;

        // ── stop → Lost ──
        server.send(ServerCmd::StopConnection);
        server.expect_ack(&driver, ServerAck::Closed).await;
        wait_state(&driver, &eng.state, DeviceState::Lost).await;

        // ── restart → 重连：server 同样答 probes + set_irqs 后变聋 ──
        server.send(ServerCmd::Restart);
        server.expect_ack(&driver, ServerAck::Restarted).await;
        server.send(ServerCmd::AcceptProbesIrqsThenDeaf);
        server.expect_ack(&driver, ServerAck::ProbesPumped).await;
        // **断言②（重连 set_irqs 重发成功代理 = C-3）**：重连后**再次**到达 Live ⇒
        // 连接器对**新** client **重新**发的 set_irqs 也成功。这正是 C-3：每次连接
        // （含重连）都重发 set_irqs，而非只在首连发一次。
        wait_state(&driver, &eng.state, DeviceState::Live).await;
    });
    // drop → ServerController 发 Shutdown：deaf 路径的 cmd_rx.recv() 收到 → Exit。
    drop(server);
}

// ──────────────────────────── 场景 ⑦ ────────────────────────────

/// ⑦ **idle peer death → Lost**（复现真机 bug）：connect→Live 后**不发任何 MMIO**
/// （`in_flight` 全空 = 空闲），随后 StopConnection（peer 死）→ 断言 worker 在有界时间
/// 内到达 Lost。
///
/// 与场景 ② 的关键区别：场景 ② 在 stop 前用 `wait_inflight(1)` 钉住一个**在途 read**，
/// 故只测「正在 await reply 时」的 reader-EOF 检测。本场景**空闲**（无在途 read）—— 真机
/// 上 `pkill usnvmemu` 时设备常处此态（无 guest MMIO 正在飞）。若 worker 的 recv 臂在
/// 空闲时不真正 `.await` 一个能观测 EOF 的 socket read，则空闲 peer 死亡不会触发
/// `go_lost` → 永不重连（revive 断裂）。本场景就是这条空闲路径的回归 gate。
#[test]
fn scenario7_idle_peer_death_goes_lost() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("vfio.sock");
    let server = ServerController::start(path.clone(), SERVED_BAR0_SIZE, SERVED_MSIX_COUNT);

    DefaultPool::run_with(async |driver| {
        // server：accept + pump probes 后变聋（持连接 open）。
        server.send(ServerCmd::AcceptProbesThenDeaf);

        let declared = DeclaredGeometry::new(Some(SERVED_BAR0_SIZE), Some(SERVED_MSIX_COUNT));
        let eng = spawn_engine(
            &driver,
            path.to_string_lossy().into_owned(),
            declared,
            Vec::new(),
        );

        // 等连接器 into_channel + 发 Connected → worker 到 Live。
        server.expect_ack(&driver, ServerAck::ProbesPumped).await;
        wait_state(&driver, &eng.state, DeviceState::Live).await;

        // **关键**：到 Live 后**不发任何 MMIO**——保持 in_flight 全空（空闲）。这正是
        // 真机 bug 的触发态：peer 死时无在途 read。
        assert_eq!(
            eng.stats.inflight_current.load(Ordering::Relaxed),
            0,
            "空闲场景：到 Live 时不应有任何在途 read"
        );

        // stop server：关连接 → 对端 socket 干净关闭（EOF）。空闲 worker 必须靠 reader
        // 的 recv_exact 读到 0 字节 → UnexpectedEof → go_lost。
        server.send(ServerCmd::StopConnection);
        server.expect_ack(&driver, ServerAck::Closed).await;

        // 断言：空闲态下 peer 死亡仍把 state 推到 Lost（有界）。若 worker 空闲时不读
        // socket，这里会超时 panic（= 复现真机 bug）。
        wait_state(&driver, &eng.state, DeviceState::Lost).await;

        // 附加：Lost 原因应含 READ_ERR（空闲态唯一能察觉 socket 死的就是 reader EOF
        // 边沿——worker 无新帧要写），且时间戳已记。
        let reason = eng.stats.last_lost_reason.load(Ordering::Relaxed);
        assert!(
            reason & lost_reason::READ_ERR != 0,
            "空闲 peer 死亡的 lost 原因应含 READ_ERR（reader EOF 边沿），got bits {reason:#x}"
        );
        assert_ne!(
            eng.stats.last_lost_at_ms.load(Ordering::Relaxed),
            0,
            "进入 Lost 应记录时间戳"
        );
    });
    drop(server);
}
