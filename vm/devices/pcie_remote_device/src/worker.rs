// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 后台 worker：通过 Transport 收发 protobuf 帧（v2 完整实现）。
//!
//! 设计要点（spec §3.3 / §3.5 / §3.7 / §3.8）：
//! - worker 拥有 transport、in-flight map、dead-man、SharedState、
//!   **Vec<Interrupt>**（MSI-X 路由）、**GuestMemory**（DMA 读写）
//! - 进 Lost 时同步 drain in-flight (complete_error NoResponse)
//! - 关闭信号通过 mesh::Receiver<()> 接收
//! - A4 / K-NEW-A: 连续 ≥ MAX_BAD_FRAMES 个非法/越界 inbound → Lost
//! - K-NEW-C: DMA 累积速率限制（默认 64 MiB/s）防止 host 饱和 guest 内存带宽

use crate::deadman::DeadMan;
use crate::state::DeviceState;
use crate::state::SharedState;
use chipset_device::io::IoError;
use chipset_device::io::deferred::DeferredRead;
use chipset_device::io::deferred::DeferredWrite;
use cvm_tracing::CVM_ALLOWED;
use futures::FutureExt;
use futures::StreamExt;
use futures::select_biased;
use guestmem::GuestMemory;
use inspect::Inspect;
use mesh::Receiver;
use pcie_remote_protocol::DmaCompletion;
use pcie_remote_protocol::MAX_DMA_BYTES;
use pcie_remote_protocol::ToHost;
use pcie_remote_protocol::ToOpenhcl;
use pcie_remote_protocol::codec;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use vmcore::interrupt::Interrupt;

/// 可观察的 worker 计数器（device.rs 也持 clone 用于 inspect 暴露）。
///
/// 全是 AtomicU64，**单写者不变量：仅 `Worker::run` 持的 worker task
/// 写入**；ohcldiag-dev 通过 Inspect 只读。所以 Relaxed ordering 足够：
/// 这是诊断数据，不影响 program correctness。
///
/// 字段含义（每个都对应 worker hot path 中的一处 fetch_add/store）：
#[derive(Inspect, Default)]
pub struct WorkerStats {
    /// 处理过的 MmioReadResult 帧数（host → OpenHCL）。
    pub mmio_read_results: AtomicU64,
    /// 处理过的 InterruptFire 数（已调用 `Interrupt::deliver()`；
    /// 不代表 guest 已收到 — deliver 是 fire-and-forget）。
    pub interrupts_fired: AtomicU64,
    /// 越界 InterruptFire 数（msix_index 超 msix_count）。
    pub interrupts_oob: AtomicU64,
    /// 处理过的 ReadGpa 请求数（含成功 + 失败）。
    pub read_gpa_requests: AtomicU64,
    /// 处理过的 WriteGpa 请求数。
    pub write_gpa_requests: AtomicU64,
    /// DMA 速率限制拒绝次数。
    pub dma_rate_limit_rejects: AtomicU64,
    /// 当前 inflight 请求数；drain_in_flight 后归零。
    pub inflight_current: AtomicU64,
    /// 历史 inflight 峰值（fetch_max 单调递增，仅写不重置）。
    pub inflight_peak: AtomicU64,
    /// 连续 bad-frame 计数（达 MAX_BAD_FRAMES 即 Lost；这里实时反映）。
    pub consecutive_bad_frames: AtomicU64,
    // ---
    // 注：以下 4 字段独立用 Relaxed atomic 写入；inspect 单次快照可能跨多
    // 个 Lost↔Revive 周期看到字段间不一致（例如 reason 是旧的、revive_count
    // 是新的）。读者应当作"最近若干周期的指示"而非强一致快照。
    // ---
    /// 最近一次进入 Lost 的 unix 毫秒时间戳；0 = 从未 Lost。
    /// 配合 `last_revive_at_ms` 计算 host 重连前的离线时长。
    pub last_lost_at_ms: AtomicU64,
    /// 最近一次从 Lost → Live（K-20 hotplug 复活）的 unix 毫秒时间戳；
    /// 0 = 从未复活。
    pub last_revive_at_ms: AtomicU64,
    /// Lost → Live 复活次数。0 = 从未 hot-reconnect 过。
    pub revive_count: AtomicU64,
    /// 最近一次 transport 死亡的错误来源分类（位标记）：
    /// - bit 0: read_frame Err
    /// - bit 1: write_frame Err
    /// - bit 2: dispatch_inbound 返 false（连续 bad ≥ MAX_BAD_FRAMES）
    ///
    /// 0 = 未发生过。每次进 Lost 用 `store` 覆盖（仅最后一次原因）。
    pub last_lost_reason: AtomicU64,
}

/// 共享 stats 句柄。
pub type SharedWorkerStats = Arc<WorkerStats>;

/// A4：连续非法/越界 inbound 数 ≥ 此阈值 → 立即进 Lost。
const MAX_BAD_FRAMES: u32 = 4;

/// `WorkerStats::last_lost_reason` 位标记。
pub mod lost_reason {
    /// read_frame 返 Err（transport read 死）。
    pub const READ_ERR: u64 = 1 << 0;
    /// write_frame 返 Err（transport write 死）。
    pub const WRITE_ERR: u64 = 1 << 1;
    /// dispatch_inbound 返 false（连续 bad-frame ≥ 阈值）。
    pub const DISPATCH_FAIL: u64 = 1 << 2;
    /// worker 主循环正常退出（shutdown 信号 / from_device 全 drop /
    /// transport_swap channel 关闭）。区别于异常 Lost：诊断时见到此 bit
    /// 说明设备是被有序卸载，不是 transport 真死了。
    pub const WORKER_EXIT: u64 = 1 << 3;
}

/// 取当前 unix 毫秒。fallback 0 若 system clock 异常（保留显式 0 = 未记录）。
fn now_unix_ms() -> u64 {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// K-NEW-C：DMA 累积速率限制窗口。
const DMA_RATE_WINDOW: Duration = Duration::from_secs(1);
/// K-NEW-C：DMA 累积速率上限默认值（字节/秒），64 MiB/s。
///
/// 可被环境变量 `OPENHCL_PCIE_REMOTE_DMA_BPS` 覆盖（u64，单位 byte/s）：
/// - 0 = 禁用速率限制（仅协议 MAX_DMA_BYTES 上限仍生效）
/// - >0 = 自定义阈值
///
/// **env 只在 `Worker::new` 时读一次并缓存到 `dma_rate_limit_bps_cached`；
/// 之后 K-20 swap-arm 复用缓存值，绝不重读 env**。运行期 env 漂移
/// （另一线程 setenv）不影响阈值，避免诊断混乱。
const DMA_RATE_LIMIT_DEFAULT_BPS: u64 = 64 * 1024 * 1024;

/// 解析 env override；非法值（非数字 / 解析失败）退回默认 + warn。
fn dma_rate_limit_bps() -> u64 {
    match std::env::var("OPENHCL_PCIE_REMOTE_DMA_BPS") {
        Ok(s) => match s.parse::<u64>() {
            Ok(v) => {
                tracing::info!(
                    CVM_ALLOWED,
                    bps = v,
                    "pcie_remote: DMA rate limit override via env"
                );
                v
            }
            Err(e) => {
                tracing::warn!(
                    CVM_ALLOWED,
                    value = %s,
                    error = %e,
                    default = DMA_RATE_LIMIT_DEFAULT_BPS,
                    "pcie_remote: OPENHCL_PCIE_REMOTE_DMA_BPS parse failed; using default"
                );
                DMA_RATE_LIMIT_DEFAULT_BPS
            }
        },
        Err(_) => DMA_RATE_LIMIT_DEFAULT_BPS,
    }
}

/// Pending request stored against a sequence number.
pub enum InFlight {
    /// MMIO read 等 host 回 MmioReadResult。
    Read {
        /// DeferredRead 由 device shim 创建并通过 DeviceRequest 移交进来。
        token: DeferredRead,
        /// 实际访问字节数（1/2/4/8），用于 complete 截断。
        access_size: usize,
    },
    /// **当前不构造** — MMIO write 走 fire-and-forget（device.rs commit
    /// 291d8645 修复 nvme.sys OS hang bug）。保留 variant 是因为：
    /// (a) drain_in_flight 仍需要 match 兜底；
    /// (b) 未来若加入"strict-ack write"模式（如需要 host 实际 OK 才
    ///     返 IoResult::Ok），可重新构造此 variant。
    #[allow(dead_code)]
    Write {
        /// DeferredWrite，由 device shim 创建。
        token: DeferredWrite,
    },
}

/// 由 device shim 发给 worker 的请求。
pub struct DeviceRequest {
    /// 序号（也是协议帧的 seq）。
    pub seq: u64,
    /// 要发给 host 的帧。
    pub frame: ToHost,
    /// 可选：等 host 回应的 DeferredRead/DeferredWrite。
    /// 如果是 fire-and-forget（如 cfg_write_side_effect）则为 None。
    pub pending: Option<InFlight>,
}

/// DMA 速率限制状态（K-NEW-C）。窗口起点 + 窗口内字节数 + 阈值。
struct DmaRate {
    window_start: Instant,
    bytes_in_window: u64,
    /// 缓存的阈值（来自 env override 或默认值）；0 = 禁用。
    limit_bps: u64,
}

impl DmaRate {
    /// 用指定 limit 构造（启动期 Worker::new 缓存 + K-20 swap-arm 复用 +
    /// 测试直接指定）。limit=0 表示禁用速率限制。
    fn with_limit(limit_bps: u64) -> Self {
        Self {
            window_start: Instant::now(),
            bytes_in_window: 0,
            limit_bps,
        }
    }

    /// 申请 `bytes` 配额。返回 true 表示允许。
    fn try_consume(&mut self, bytes: u64) -> bool {
        // limit=0 → 速率限制 disabled，永远允许（仅协议 MAX_DMA_BYTES 仍生效）。
        if self.limit_bps == 0 {
            return true;
        }
        let now = Instant::now();
        if now.duration_since(self.window_start) >= DMA_RATE_WINDOW {
            self.window_start = now;
            self.bytes_in_window = 0;
        }
        let new_total = self.bytes_in_window.saturating_add(bytes);
        if new_total > self.limit_bps {
            false
        } else {
            self.bytes_in_window = new_total;
            true
        }
    }
}

/// K-20 hotplug helper: 在 worker `run` 主循环中包裹 `codec::read_frame`，
/// 让 Lost 状态下不实际 read（避免 dead-transport busy-loop）—— 改为永远
/// pending，由 swap channel 或 from_device 决定下一步。
async fn read_inbound_or_pending(
    transport: &mut crate::prepared::BoxedTransport,
    is_lost: bool,
) -> Result<ToOpenhcl, codec::CodecError> {
    if is_lost {
        // 永远 pending（等 swap channel 或 shutdown / from_device）
        std::future::pending::<()>().await;
        unreachable!()
    } else {
        codec::read_frame::<_, ToOpenhcl>(transport).await
    }
}

///
/// **v2 hotplug 设计 (K-20)**：worker 不再 generic over `T`，transport 用
/// `BoxedTransport` 让 runtime swap 成为可能。新 transport 通过
/// `transport_swap` channel 传入；listener 任务在 host 重连 / late-attach
/// 时通过它把新 socket 投递给 worker，worker 自动 drain 旧 inflight + 切换
/// transport + state Lost → Live 复活。
pub struct Worker {
    transport: crate::prepared::BoxedTransport,
    state: SharedState,
    in_flight: HashMap<u64, InFlight>,
    _deadman: DeadMan,
    from_device: Receiver<DeviceRequest>,
    /// MSI-X 中断向量（resolver 阶段从 `MsixEmulator::interrupt(i)` 构造）。
    /// 长度 = describe.msix_count；index 越界由 `.get()` 安全处理。
    interrupts: Vec<Interrupt>,
    /// GuestMemory 用以处理 host 的 ReadGpa/WriteGpa DMA 请求。
    guest_memory: GuestMemory,
    /// A4：连续非法 inbound 帧计数。
    consecutive_bad_frames: u32,
    /// K-NEW-C: DMA rate state.
    dma_rate: DmaRate,
    /// 缓存的 DMA 限速阈值（启动期一次性从 env 读取）。K-20 swap-arm 复活
    /// 时复用此值构造新 DmaRate，**不**重读 env —— 避免运行期 env 漂移
    /// 导致诊断混乱。
    dma_rate_limit_bps_cached: u64,
    /// 用于 DMA reply 帧的 seq（与 device.rs `next_seq` 配合）。
    ///
    /// seq 空间约定：
    /// - device.rs 用低半 u64（从 1 起）
    /// - 本 worker DMA reply 用高半（`1u64 << 63` 起）
    ///
    /// 两半互不重叠便于日志排查；host 关联请求实际用 token 不用 seq。
    /// K-20 swap arm 故意**不**重置 `next_dma_seq`：跨重连保持单调，便于
    /// 日志关联同一 device 在 hot-reconnect 前后的 DMA reply 帧。
    next_dma_seq: u64,
    /// 可观察 stats（与 device.rs 共享 Arc，inspect 暴露）。
    stats: SharedWorkerStats,
    /// K-20 hotplug: listener 通过此 channel 投递新 transport（host 重连）。
    /// drop 此 sender 意味着 listener 关闭；worker 继续用现有 transport。
    transport_swap: Receiver<crate::prepared::BoxedTransport>,
}

impl Worker {
    /// 构造 Worker。
    pub fn new(
        transport: crate::prepared::BoxedTransport,
        state: SharedState,
        from_device: Receiver<DeviceRequest>,
        interrupts: Vec<Interrupt>,
        guest_memory: GuestMemory,
        stats: SharedWorkerStats,
        transport_swap: Receiver<crate::prepared::BoxedTransport>,
    ) -> Self {
        let dma_rate_limit_bps_cached = dma_rate_limit_bps();
        Self {
            transport,
            state,
            in_flight: HashMap::new(),
            _deadman: DeadMan::new(),
            from_device,
            interrupts,
            guest_memory,
            consecutive_bad_frames: 0,
            dma_rate: DmaRate::with_limit(dma_rate_limit_bps_cached),
            dma_rate_limit_bps_cached,
            next_dma_seq: 1 << 63,
            stats,
            transport_swap,
        }
    }

    /// 集中处理 transport 死亡 → Lost 转换：记录时间戳 + 原因 bit。
    ///
    /// 调用方应按下述顺序，保证 device.rs 一读到 state=Lost 就短路新请求
    /// （即便 inflight 还在 drain）：
    /// ```ignore
    /// self.record_lost(reason);
    /// self.state.store(DeviceState::Lost);   // 让外部 reader 立刻短路
    /// self.drain_in_flight();                // 再失败既有 inflight
    /// ```
    /// 中间窗口可能短暂出现 `state=Lost && inflight_current > 0`；inspect
    /// 消费者（ohcldiag-dev）应将其视为合法 transient — 表示 drain 正在进行。
    fn record_lost(&self, reason_bit: u64) {
        self.stats
            .last_lost_at_ms
            .store(now_unix_ms(), Ordering::Relaxed);
        self.stats
            .last_lost_reason
            .store(reason_bit, Ordering::Relaxed);
    }

    /// 集中处理 Lost → Live（K-20 hotplug）：记录时间戳 + revive_count++。
    fn record_revive(&self) {
        self.stats
            .last_revive_at_ms
            .store(now_unix_ms(), Ordering::Relaxed);
        self.stats.revive_count.fetch_add(1, Ordering::Relaxed);
    }

    /// 主循环：直到 shutdown 信号或 transport 出错。
    ///
    /// K-20 hotplug: 收到 `transport_swap` 新 transport → drain 旧 inflight +
    /// 替换 transport + state Lost → Live 复活。transport 死亡时 state
    /// → Lost 但 worker **不退出**，等待 swap channel；只有 from_device
    /// sender 全 drop（device 被 unbind）或 shutdown 信号才真正退出。
    pub async fn run(mut self, mut shutdown: Receiver<()>) {
        loop {
            // 透明状态：Lost 期不 select read_frame（transport 死，会立即返回
            // Err，导致 busy-loop）；只 wait swap channel 与 from_device。
            let is_lost = matches!(self.state.load(), DeviceState::Lost);
            select_biased! {
                _ = shutdown.next().fuse() => {
                    tracing::info!(CVM_ALLOWED, "pcie_remote worker shutdown signal");
                    break;
                }
                new_transport = self.transport_swap.next().fuse() => {
                    let Some(new) = new_transport else {
                        // 不变量：swap sender 由 TransportSwapMap (Arc) 持到
                        // 进程退出（dispatch.rs / underhill_core/worker.rs 持有
                        // Arc）。除非进程退出否则不会走到这里；保守起见 break。
                        tracing::info!(CVM_ALLOWED, "pcie_remote: transport_swap closed (unexpected), worker exiting");
                        break;
                    };
                    tracing::info!(CVM_ALLOWED, "pcie_remote: transport refreshed via swap channel; resuming Live");
                    self.drain_in_flight();
                    self.transport = new;
                    self.consecutive_bad_frames = 0;
                    self.stats.consecutive_bad_frames.store(0, Ordering::Relaxed);
                    self.dma_rate = DmaRate::with_limit(self.dma_rate_limit_bps_cached);
                    // 注：next_dma_seq 故意不重置 —— 保持跨重连单调，便于日志关联。
                    self.record_revive();
                    self.state.store(DeviceState::Live);
                }
                req = self.from_device.next().fuse() => {
                    let Some(req) = req else {
                        // sender 全 drop（device unbind），worker 真正退出
                        break;
                    };
                    if let Some(pending) = req.pending {
                        self.in_flight.insert(req.seq, pending);
                        let cur = self.in_flight.len() as u64;
                        self.stats.inflight_current.store(cur, Ordering::Relaxed);
                        self.stats.inflight_peak.fetch_max(cur, Ordering::Relaxed);
                    }
                    if let Err(e) = codec::write_frame(&mut self.transport, &req.frame).await {
                        tracing::warn!(CVM_ALLOWED, error = %e, "write_frame failed; transport dead, going Lost (awaiting refresh)");
                        // K-20: 不 break；进 Lost 等 transport_swap
                        self.record_lost(lost_reason::WRITE_ERR);
                        self.state.store(DeviceState::Lost);
                        self.drain_in_flight();
                    }
                }
                // K-20: Lost 时跳过 read_frame arm，避免 busy-loop。
                // 用 if guard：select_biased! 不直接支持 guard，但我们 wrap 在
                // 一个 conditional pending future 中。
                inbound = read_inbound_or_pending(&mut self.transport, is_lost).fuse() => {
                    match inbound {
                        Ok(m) => {
                            if !self.dispatch_inbound(m).await {
                                tracing::warn!(
                                    CVM_ALLOWED,
                                    consecutive = self.consecutive_bad_frames,
                                    "pcie_remote: dispatch failed, going Lost (awaiting refresh)"
                                );
                                self.record_lost(lost_reason::DISPATCH_FAIL);
                                self.state.store(DeviceState::Lost);
                                self.drain_in_flight();
                            }
                        }
                        Err(e) => {
                            tracing::warn!(CVM_ALLOWED, error = %e, "read_frame failed; transport dead, going Lost (awaiting refresh)");
                            self.record_lost(lost_reason::READ_ERR);
                            self.state.store(DeviceState::Lost);
                            self.drain_in_flight();
                        }
                    }
                }
            }
        }
        self.drain_in_flight();
        self.record_lost(lost_reason::WORKER_EXIT);
        self.state.store(DeviceState::Lost);
    }

    /// 处理一个 inbound ToOpenhcl 帧。返回 false 表示应立即终止 worker
    /// （A4：连续非法帧超阈值；或致命协议错）。
    async fn dispatch_inbound(&mut self, msg: ToOpenhcl) -> bool {
        use pcie_remote_protocol::to_openhcl::Body;
        let seq = msg.seq;
        match msg.body {
            Some(Body::MmioReadResult(r)) => {
                if let Some(InFlight::Read { token, access_size }) = self.in_flight.remove(&seq) {
                    self.stats
                        .inflight_current
                        .store(self.in_flight.len() as u64, Ordering::Relaxed);
                    // K-18: MMIO 访问尺寸严格 ∈ {1,2,4,8}。其他值是协议错。
                    if !matches!(access_size, 1 | 2 | 4 | 8) {
                        tracing::warn!(
                            CVM_ALLOWED,
                            seq,
                            access_size,
                            "pcie_remote: invalid access_size for MMIO read; failing request"
                        );
                        token.complete_error(IoError::InvalidRegister);
                        return self.record_bad();
                    }
                    let bytes = r.value.to_le_bytes();
                    token.complete(&bytes[..access_size]);
                    self.stats.mmio_read_results.fetch_add(1, Ordering::Relaxed);
                    self.consecutive_bad_frames = 0;
                    self.stats
                        .consecutive_bad_frames
                        .store(0, Ordering::Relaxed);
                    true
                } else {
                    // 未知 seq；非致命但记一次"非法"。
                    tracing::warn!(
                        CVM_ALLOWED,
                        seq,
                        "pcie_remote: MmioReadResult with unknown seq"
                    );
                    self.record_bad()
                }
            }
            Some(Body::InterruptFire(f)) => {
                let idx = f.msix_index as usize;
                if let Some(intr) = self.interrupts.get(idx) {
                    intr.deliver();
                    self.stats.interrupts_fired.fetch_add(1, Ordering::Relaxed);
                    self.consecutive_bad_frames = 0;
                    self.stats
                        .consecutive_bad_frames
                        .store(0, Ordering::Relaxed);
                    true
                } else {
                    self.stats.interrupts_oob.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        CVM_ALLOWED,
                        msix_index = f.msix_index,
                        count = self.interrupts.len(),
                        "pcie_remote: InterruptFire msix_index out of bounds"
                    );
                    self.record_bad()
                }
            }
            Some(Body::ReadGpa(req)) => self.handle_read_gpa(req).await,
            Some(Body::WriteGpa(req)) => self.handle_write_gpa(req).await,
            None => {
                tracing::warn!(CVM_ALLOWED, "ToOpenhcl missing body");
                self.record_bad()
            }
        }
    }

    /// 累加 bad-frame 计数。返回 true 表示尚未到阈值（worker 继续）；
    /// false 表示超阈值（worker 应退出）。
    fn record_bad(&mut self) -> bool {
        self.consecutive_bad_frames = self.consecutive_bad_frames.saturating_add(1);
        self.stats
            .consecutive_bad_frames
            .store(self.consecutive_bad_frames as u64, Ordering::Relaxed);
        self.consecutive_bad_frames < MAX_BAD_FRAMES
    }

    /// 处理 host 主动发起的 ReadGpa：从 guest 内存读 len 字节，回 DmaCompletion。
    async fn handle_read_gpa(&mut self, req: pcie_remote_protocol::ReadGpaRequest) -> bool {
        self.stats.read_gpa_requests.fetch_add(1, Ordering::Relaxed);
        let pcie_remote_protocol::ReadGpaRequest { token, gpa, len } = req;
        let len = len as usize;

        // 协议级别尺寸限制（K-NEW-C 一部分）：单次 ≤ MAX_DMA_BYTES。
        if len == 0 || len > MAX_DMA_BYTES {
            tracing::warn!(
                CVM_ALLOWED,
                token,
                gpa,
                len,
                max = MAX_DMA_BYTES,
                "pcie_remote: ReadGpa len out of bounds; replying ok=false"
            );
            let _ = self.reply_dma(token, false, Vec::new()).await;
            return self.record_bad();
        }

        // K-NEW-C 速率限制：超 64 MiB/s 拒绝（不算 bad-frame，host 可能合法繁忙）。
        if !self.dma_rate.try_consume(len as u64) {
            self.stats
                .dma_rate_limit_rejects
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                CVM_ALLOWED,
                token,
                gpa,
                len,
                "pcie_remote: DMA rate limit exceeded; replying ok=false"
            );
            let _ = self.reply_dma(token, false, Vec::new()).await;
            return true;
        }

        let mut buf = vec![0u8; len];
        let ok = self.guest_memory.read_at(gpa, &mut buf).is_ok();
        if !ok {
            tracing::warn!(
                CVM_ALLOWED,
                token,
                gpa,
                len,
                "pcie_remote: guest_memory.read_at failed; replying ok=false"
            );
            // 注：合法 host 可能问到 MMIO 洞或越界，是协议范围内错误响应；
            // 只有"消息结构非法"才计 bad-frame，故这里 reset 计数（含 stats）。
            let _ = self.reply_dma(token, false, Vec::new()).await;
            self.consecutive_bad_frames = 0;
            self.stats
                .consecutive_bad_frames
                .store(0, Ordering::Relaxed);
            true
        } else {
            self.consecutive_bad_frames = 0;
            self.stats
                .consecutive_bad_frames
                .store(0, Ordering::Relaxed);
            self.reply_dma(token, true, buf).await
        }
    }

    /// 处理 host 主动发起的 WriteGpa：写 data 到 guest gpa，回 DmaCompletion。
    async fn handle_write_gpa(&mut self, req: pcie_remote_protocol::WriteGpaRequest) -> bool {
        self.stats
            .write_gpa_requests
            .fetch_add(1, Ordering::Relaxed);
        let pcie_remote_protocol::WriteGpaRequest { token, gpa, data } = req;

        if data.is_empty() || data.len() > MAX_DMA_BYTES {
            tracing::warn!(
                CVM_ALLOWED,
                token,
                gpa,
                len = data.len(),
                max = MAX_DMA_BYTES,
                "pcie_remote: WriteGpa len out of bounds; replying ok=false"
            );
            let _ = self.reply_dma(token, false, Vec::new()).await;
            return self.record_bad();
        }

        if !self.dma_rate.try_consume(data.len() as u64) {
            self.stats
                .dma_rate_limit_rejects
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                CVM_ALLOWED,
                token,
                gpa,
                len = data.len(),
                "pcie_remote: DMA rate limit exceeded; replying ok=false"
            );
            let _ = self.reply_dma(token, false, Vec::new()).await;
            return true;
        }

        let ok = self.guest_memory.write_at(gpa, &data).is_ok();
        if !ok {
            tracing::warn!(
                CVM_ALLOWED,
                token,
                gpa,
                len = data.len(),
                "pcie_remote: guest_memory.write_at failed; replying ok=false"
            );
        }
        self.consecutive_bad_frames = 0;
        self.stats
            .consecutive_bad_frames
            .store(0, Ordering::Relaxed);
        self.reply_dma(token, ok, Vec::new()).await
    }

    /// 发 DmaCompletion 给 host。返回 true 表示 worker 继续；false 表示
    /// transport 已死，worker 应退出。
    ///
    /// 注：DmaCompletion 走 **ToHost** 方向（OpenHCL → host），用 token
    /// 而不是 seq 跟踪请求；这里 frame 的 seq 自分配（不会被 host 用来
    /// 关联请求），便于 inflight map 调试。
    async fn reply_dma(&mut self, token: u64, ok: bool, data: Vec<u8>) -> bool {
        use pcie_remote_protocol::to_host::Body;
        let seq = self.next_dma_seq;
        self.next_dma_seq = self.next_dma_seq.wrapping_add(1);
        let frame = ToHost {
            seq,
            body: Some(Body::DmaCompletion(DmaCompletion { token, ok, data })),
        };
        if let Err(e) = codec::write_frame(&mut self.transport, &frame).await {
            tracing::warn!(CVM_ALLOWED, error = %e, "pcie_remote: DmaCompletion write_frame failed");
            return false;
        }
        true
    }

    fn drain_in_flight(&mut self) {
        for (_, inflight) in self.in_flight.drain() {
            match inflight {
                InFlight::Read { token, .. } => token.complete_error(IoError::NoResponse),
                InFlight::Write { token } => token.complete_error(IoError::NoResponse),
            }
        }
        // drain 完归零 inflight_current 让 ohcldiag-dev 反映真实状态。
        self.stats.inflight_current.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod worker_tests;
