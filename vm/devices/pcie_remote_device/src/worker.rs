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
}

/// 共享 stats 句柄。
pub type SharedWorkerStats = Arc<WorkerStats>;

/// A4：连续非法/越界 inbound 数 ≥ 此阈值 → 立即进 Lost。
const MAX_BAD_FRAMES: u32 = 4;

/// K-NEW-C：DMA 累积速率限制窗口。
const DMA_RATE_WINDOW: Duration = Duration::from_secs(1);
/// K-NEW-C：DMA 累积速率上限（字节/秒），默认 64 MiB/s。host 在 1s 窗口内
/// 累积请求字节超过此值的多余 DMA 直接拒绝（DmaCompletion ok=false）。
const DMA_RATE_LIMIT_BYTES_PER_SEC: u64 = 64 * 1024 * 1024;

/// Pending request stored against a sequence number.
pub enum InFlight {
    /// MMIO read 等 host 回 MmioReadResult。
    Read {
        /// DeferredRead 由 device shim 创建并通过 DeviceRequest 移交进来。
        token: DeferredRead,
        /// 实际访问字节数（1/2/4/8），用于 complete 截断。
        access_size: usize,
    },
    /// MMIO write 等 host ack（v1 暂未严格 ack；保留接口）。
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

/// DMA 速率限制状态（K-NEW-C）。窗口起点 + 窗口内字节数。
struct DmaRate {
    window_start: Instant,
    bytes_in_window: u64,
}

impl DmaRate {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            bytes_in_window: 0,
        }
    }

    /// 申请 `bytes` 配额。返回 true 表示允许。
    fn try_consume(&mut self, bytes: u64) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= DMA_RATE_WINDOW {
            self.window_start = now;
            self.bytes_in_window = 0;
        }
        let new_total = self.bytes_in_window.saturating_add(bytes);
        if new_total > DMA_RATE_LIMIT_BYTES_PER_SEC {
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
        Self {
            transport,
            state,
            in_flight: HashMap::new(),
            _deadman: DeadMan::new(),
            from_device,
            interrupts,
            guest_memory,
            consecutive_bad_frames: 0,
            dma_rate: DmaRate::new(),
            next_dma_seq: 1 << 63,
            stats,
            transport_swap,
        }
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
                    self.dma_rate = DmaRate::new();
                    // 注：next_dma_seq 故意不重置 —— 保持跨重连单调，便于日志关联。
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
                                self.state.store(DeviceState::Lost);
                                self.drain_in_flight();
                            }
                        }
                        Err(e) => {
                            tracing::warn!(CVM_ALLOWED, error = %e, "read_frame failed; transport dead, going Lost (awaiting refresh)");
                            self.state.store(DeviceState::Lost);
                            self.drain_in_flight();
                        }
                    }
                }
            }
        }
        self.drain_in_flight();
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
mod tests {
    use super::*;

    /// 构造一个测试用 transport_swap channel：sender 立刻 forget 保活，
    /// 让 worker 主循环（如果跑）不会因 sender drop 走 None arm。返回 receiver。
    fn test_swap_rx() -> Receiver<crate::prepared::BoxedTransport> {
        let (tx, rx) = mesh::channel::<crate::prepared::BoxedTransport>();
        std::mem::forget(tx);
        rx
    }

    #[test]
    fn dma_rate_allows_under_limit() {
        let mut r = DmaRate::new();
        assert!(r.try_consume(1024 * 1024));
        assert!(r.try_consume(1024 * 1024));
    }

    #[test]
    fn dma_rate_rejects_over_limit() {
        let mut r = DmaRate::new();
        assert!(r.try_consume(DMA_RATE_LIMIT_BYTES_PER_SEC));
        assert!(!r.try_consume(1));
    }

    #[test]
    fn dma_rate_resets_after_window() {
        let mut r = DmaRate::new();
        assert!(r.try_consume(DMA_RATE_LIMIT_BYTES_PER_SEC));
        // 不真等 1s — 直接构造已过期窗口
        r.window_start = Instant::now() - Duration::from_secs(2);
        assert!(r.try_consume(1024));
    }

    /// inflight 计数器：MmioReadResult 处理后从 in_flight map 移除 → counter 减；
    /// drain_in_flight 触发归零。
    ///
    /// 不依赖真 guest driver — 用直接构造的 DeferredRead/Write token 模拟
    /// device.rs → worker 的 DeviceRequest 流向（e2e 时 guest 无 driver 不
    /// 会触发，这里单元测试覆盖该路径）。
    #[test]
    fn inflight_counter_tracks_in_flight_map() {
        use chipset_device::io::deferred::defer_read;
        use futures::io::Cursor;
        use pal_async::DefaultPool;
        use pcie_remote_protocol::MmioReadResult as Mrr;
        use pcie_remote_protocol::to_openhcl::Body;

        DefaultPool::run_with(|_| async move {
            let cursor = Cursor::new(Vec::<u8>::new());
            let (_dev_tx, dev_rx) = mesh::channel::<DeviceRequest>();
            let state = SharedState::new(DeviceState::Live);
            let gm = GuestMemory::empty();
            let stats: SharedWorkerStats = Arc::new(WorkerStats::default());
            let stats_for_check = stats.clone();
            let swap_rx = test_swap_rx();
            let mut w = Worker::new(
                Box::new(cursor) as crate::prepared::BoxedTransport,
                state,
                dev_rx,
                Vec::new(),
                gm,
                stats,
                swap_rx,
            );

            // 手工把 2 个 InFlight 塞进 worker（模拟 device.rs 投递过来）。
            // _t1/_t2 是对应 host-side wait future；测试不 await 它们。
            let (_d1, _t1) = defer_read();
            let (_d2, _t2) = defer_read();
            w.in_flight.insert(
                10,
                InFlight::Read {
                    token: _d1,
                    access_size: 4,
                },
            );
            w.in_flight.insert(
                11,
                InFlight::Read {
                    token: _d2,
                    access_size: 8,
                },
            );
            // 模拟 device.rs 投递时 worker 主循环更新 stats：
            let cur = w.in_flight.len() as u64;
            w.stats.inflight_current.store(cur, Ordering::Relaxed);
            w.stats.inflight_peak.fetch_max(cur, Ordering::Relaxed);
            assert_eq!(stats_for_check.inflight_current.load(Ordering::Relaxed), 2);
            assert_eq!(stats_for_check.inflight_peak.load(Ordering::Relaxed), 2);

            // host 回 seq=10 → in_flight 减 1，counter 更新
            let reply1 = ToOpenhcl {
                seq: 10,
                body: Some(Body::MmioReadResult(Mrr { value: 0xdead })),
            };
            assert!(w.dispatch_inbound(reply1).await);
            assert_eq!(stats_for_check.inflight_current.load(Ordering::Relaxed), 1);
            // peak 保持 2（fetch_max 单调）
            assert_eq!(stats_for_check.inflight_peak.load(Ordering::Relaxed), 2);
            assert_eq!(stats_for_check.mmio_read_results.load(Ordering::Relaxed), 1);

            // drain（模拟 worker 进 Lost）→ inflight_current 归零
            w.drain_in_flight();
            assert_eq!(stats_for_check.inflight_current.load(Ordering::Relaxed), 0);
            // 但 peak 仍 2
            assert_eq!(stats_for_check.inflight_peak.load(Ordering::Relaxed), 2);
            assert!(w.in_flight.is_empty());
        });
    }

    /// 验证 InterruptFire dispatch 的边界：
    /// - 合法 msix_index 在范围内 → 不增 bad-frame 计数
    /// - 越界 msix_index → 增 bad-frame 计数
    ///
    /// 用 MsiTarget::disconnected() 构造 MsixEmulator；deliver() 是 no-op。
    /// 用 futures::io::Cursor 当 "transport"（async read/write 兼容）。
    /// 实际帧不通过 cursor 流，只直接调 dispatch_inbound。
    #[test]
    fn interrupt_fire_bounds() {
        use futures::io::Cursor;
        use pal_async::DefaultPool;
        use pci_core::capabilities::msix::MsixEmulator;
        use pci_core::msi::MsiTarget;
        use pcie_remote_protocol::InterruptFire;
        use pcie_remote_protocol::to_openhcl::Body;

        DefaultPool::run_with(|_| async move {
            let target = MsiTarget::disconnected();
            let (msix, _cap) = MsixEmulator::new(4, 2, &target);
            let interrupts = (0..2)
                .map(|i| msix.interrupt(i).unwrap())
                .collect::<Vec<_>>();

            // 内存 cursor 作 transport；本测试不真走 transport，只测 dispatch_inbound。
            let cursor = Cursor::new(Vec::<u8>::new());
            let (_dev_tx, dev_rx) = mesh::channel::<DeviceRequest>();
            let state = SharedState::new(DeviceState::Live);
            let gm = GuestMemory::empty();
            let stats: SharedWorkerStats = Arc::new(WorkerStats::default());
            let stats_for_check = stats.clone();
            let swap_rx = test_swap_rx();
            let mut w = Worker::new(
                Box::new(cursor) as crate::prepared::BoxedTransport,
                state,
                dev_rx,
                interrupts,
                gm,
                stats,
                swap_rx,
            );

            // msix_index = 0 valid。
            let ok_msg = ToOpenhcl {
                seq: 1,
                body: Some(Body::InterruptFire(InterruptFire { msix_index: 0 })),
            };
            assert!(w.dispatch_inbound(ok_msg).await);
            assert_eq!(w.consecutive_bad_frames, 0);
            assert_eq!(stats_for_check.interrupts_fired.load(Ordering::Relaxed), 1);

            // msix_index = 1 valid。
            let ok_msg2 = ToOpenhcl {
                seq: 2,
                body: Some(Body::InterruptFire(InterruptFire { msix_index: 1 })),
            };
            assert!(w.dispatch_inbound(ok_msg2).await);
            assert_eq!(w.consecutive_bad_frames, 0);
            assert_eq!(stats_for_check.interrupts_fired.load(Ordering::Relaxed), 2);
            assert_eq!(stats_for_check.interrupts_oob.load(Ordering::Relaxed), 0);

            // msix_index = 2 → out of bounds → bad-frame +1，仍 < 阈值。
            let oob = ToOpenhcl {
                seq: 3,
                body: Some(Body::InterruptFire(InterruptFire { msix_index: 2 })),
            };
            assert!(w.dispatch_inbound(oob).await);
            assert_eq!(w.consecutive_bad_frames, 1);
            assert_eq!(stats_for_check.interrupts_oob.load(Ordering::Relaxed), 1);
            assert_eq!(
                stats_for_check
                    .consecutive_bad_frames
                    .load(Ordering::Relaxed),
                1
            );

            // 累计到 MAX_BAD_FRAMES 应让 dispatch_inbound 返回 false。
            for i in 0..(MAX_BAD_FRAMES - 1) {
                let bad = ToOpenhcl {
                    seq: 100 + i as u64,
                    body: Some(Body::InterruptFire(InterruptFire { msix_index: 99 })),
                };
                let cont = w.dispatch_inbound(bad).await;
                if i < (MAX_BAD_FRAMES - 2) {
                    assert!(cont, "i={i} should continue");
                } else {
                    assert!(!cont, "i={i} should stop (达阈值)");
                }
            }
            // 总越界数：1 (msix_index=2) + (MAX_BAD_FRAMES-1) 个 99
            assert_eq!(
                stats_for_check.interrupts_oob.load(Ordering::Relaxed),
                1 + (MAX_BAD_FRAMES - 1) as u64
            );
        });
    }

    /// DMA ReadGpa bounds check: len=0 / len > MAX_DMA_BYTES 应被拒绝 +
    /// 计为 bad-frame；合法 len 走 GuestMemory.read_at（空 GuestMemory 上会
    /// 失败但不算 bad-frame，read_gpa_requests 仍 ++）。
    #[test]
    fn read_gpa_bounds_and_stats() {
        use futures::io::Cursor;
        use pal_async::DefaultPool;
        use pcie_remote_protocol::ReadGpaRequest;

        DefaultPool::run_with(|_| async move {
            let cursor = Cursor::new(Vec::<u8>::new());
            let (_dev_tx, dev_rx) = mesh::channel::<DeviceRequest>();
            let state = SharedState::new(DeviceState::Live);
            let gm = GuestMemory::empty();
            let stats: SharedWorkerStats = Arc::new(WorkerStats::default());
            let stats_for_check = stats.clone();
            let swap_rx = test_swap_rx();
            let mut w = Worker::new(
                Box::new(cursor) as crate::prepared::BoxedTransport,
                state,
                dev_rx,
                Vec::new(),
                gm,
                stats,
                swap_rx,
            );

            // len=0 → bounds reject + record_bad
            let req0 = ReadGpaRequest {
                token: 1,
                gpa: 0,
                len: 0,
            };
            let _ = w.handle_read_gpa(req0).await;
            assert_eq!(stats_for_check.read_gpa_requests.load(Ordering::Relaxed), 1);
            assert_eq!(
                stats_for_check
                    .consecutive_bad_frames
                    .load(Ordering::Relaxed),
                1
            );

            // len 太大 → bounds reject + record_bad
            let req_big = ReadGpaRequest {
                token: 2,
                gpa: 0,
                len: (MAX_DMA_BYTES + 1) as u32,
            };
            let _ = w.handle_read_gpa(req_big).await;
            assert_eq!(stats_for_check.read_gpa_requests.load(Ordering::Relaxed), 2);
            assert_eq!(
                stats_for_check
                    .consecutive_bad_frames
                    .load(Ordering::Relaxed),
                2
            );

            // 合法 len（4 字节）→ guest_memory.read_at 在空 mem 上失败 →
            // reply ok=false 不计 bad-frame；read_gpa_requests 仍 ++；
            // consecutive_bad_frames 归零
            let req_ok = ReadGpaRequest {
                token: 3,
                gpa: 0,
                len: 4,
            };
            let _ = w.handle_read_gpa(req_ok).await;
            assert_eq!(stats_for_check.read_gpa_requests.load(Ordering::Relaxed), 3);
            assert_eq!(
                stats_for_check
                    .consecutive_bad_frames
                    .load(Ordering::Relaxed),
                0
            );
        });
    }

    /// WriteGpa 同样 bounds 检查 + stats 计数。
    #[test]
    fn write_gpa_bounds_and_stats() {
        use futures::io::Cursor;
        use pal_async::DefaultPool;
        use pcie_remote_protocol::WriteGpaRequest;

        DefaultPool::run_with(|_| async move {
            let cursor = Cursor::new(Vec::<u8>::new());
            let (_dev_tx, dev_rx) = mesh::channel::<DeviceRequest>();
            let state = SharedState::new(DeviceState::Live);
            let gm = GuestMemory::empty();
            let stats: SharedWorkerStats = Arc::new(WorkerStats::default());
            let stats_for_check = stats.clone();
            let swap_rx = test_swap_rx();
            let mut w = Worker::new(
                Box::new(cursor) as crate::prepared::BoxedTransport,
                state,
                dev_rx,
                Vec::new(),
                gm,
                stats,
                swap_rx,
            );

            // 空 data → bounds reject + record_bad
            let req0 = WriteGpaRequest {
                token: 1,
                gpa: 0,
                data: vec![],
            };
            let _ = w.handle_write_gpa(req0).await;
            assert_eq!(
                stats_for_check.write_gpa_requests.load(Ordering::Relaxed),
                1
            );
            assert_eq!(
                stats_for_check
                    .consecutive_bad_frames
                    .load(Ordering::Relaxed),
                1
            );

            // data 超大 → bounds reject
            let req_big = WriteGpaRequest {
                token: 2,
                gpa: 0,
                data: vec![0u8; MAX_DMA_BYTES + 1],
            };
            let _ = w.handle_write_gpa(req_big).await;
            assert_eq!(
                stats_for_check.write_gpa_requests.load(Ordering::Relaxed),
                2
            );
            assert_eq!(
                stats_for_check
                    .consecutive_bad_frames
                    .load(Ordering::Relaxed),
                2
            );

            // 合法 → guest_memory.write_at fail (empty mem) → ok=false 不 bad
            let req_ok = WriteGpaRequest {
                token: 3,
                gpa: 0,
                data: vec![0xff; 8],
            };
            let _ = w.handle_write_gpa(req_ok).await;
            assert_eq!(
                stats_for_check.write_gpa_requests.load(Ordering::Relaxed),
                3
            );
            assert_eq!(
                stats_for_check
                    .consecutive_bad_frames
                    .load(Ordering::Relaxed),
                0
            );
        });
    }

    /// 验证 DmaRate 与 dma_rate_limit_rejects 计数器配合：64 个 64KB
    /// 全允许（== 4 MiB << 64 MiB/s）；同窗口再 1024 个 → 第 1025 起拒。
    /// 间接也是 K-NEW-C e2e stress 模式的 unit-test 等价物。
    #[test]
    fn dma_rate_counter_increments_on_reject() {
        use futures::io::Cursor;
        use pal_async::DefaultPool;
        use pcie_remote_protocol::ReadGpaRequest;

        DefaultPool::run_with(|_| async move {
            let cursor = Cursor::new(Vec::<u8>::new());
            let (_dev_tx, dev_rx) = mesh::channel::<DeviceRequest>();
            let state = SharedState::new(DeviceState::Live);
            let gm = GuestMemory::empty();
            let stats: SharedWorkerStats = Arc::new(WorkerStats::default());
            let stats_for_check = stats.clone();
            let swap_rx = test_swap_rx();
            let mut w = Worker::new(
                Box::new(cursor) as crate::prepared::BoxedTransport,
                state,
                dev_rx,
                Vec::new(),
                gm,
                stats,
                swap_rx,
            );

            // 发 1100 个 64KB ReadGpa：1024 个允许（占满 64 MiB/s）+ 76 个被拒
            for i in 0..1100u64 {
                let req = ReadGpaRequest {
                    token: i,
                    gpa: 0,
                    len: 65536,
                };
                let _ = w.handle_read_gpa(req).await;
            }
            assert_eq!(
                stats_for_check.read_gpa_requests.load(Ordering::Relaxed),
                1100
            );
            // 1100 - 1024 = 76 被 rate limit 拒绝
            assert_eq!(
                stats_for_check
                    .dma_rate_limit_rejects
                    .load(Ordering::Relaxed),
                76
            );
        });
    }
}
