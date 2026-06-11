// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 后台 worker：持全双工 split 通道（writer + reader），把 device shim 的 MMIO
//! 请求翻译成 vfio-user `REGION_READ` / `REGION_WRITE` 帧发给 firmware（另一个
//! VTL2 进程），并按 `msg_id` 把 reply 路由回各自的 [`DeferredRead`]。
//!
//! 这是 `vm/devices/pcie_remote_device::worker` 的 **W6b 精简版**。相对模板删掉：
//! - `transport_swap` / hotplug 复活臂（Phase 1 不重连；重连是 W6d）；
//! - `DmaRate` / 速率限制（无 control-socket DMA；Phase 3 走 `dma_map` 零拷贝）；
//! - `ReadGpa` / `WriteGpa` / `reply_dma` / `guest_memory`（DMA 不经 control socket）；
//! - `MAX_BAD_FRAMES` / `record_bad`（W6b 更严：**单个 wire Err 即 Lost**）；
//! - `next_dma_seq` / `DeadMan`。
//!
//! 设计要点：
//! - worker 拥有 writer / reader / in-flight map / [`SharedState`] / `Vec<Interrupt>`
//!   （MSI-X 路由，resolver 注入）。
//! - 进 Lost 时**先 store Lost 再 drain**（让 device shim 一读到 Lost 即短路新请求，
//!   即便 inflight 还在 drain），镜像模板 `record_lost` 注释。
//! - 关闭信号经 `mesh::Receiver<()>` 接收。
//! - **单个 wire Err（writer 或 reader）→ 立即 Lost + drain**；不容忍累积坏帧。
//! - 一个**异常**的 reply（未知 msg_id / echo 不符 / payload 过短）是**宽容**的：
//!   只 bump `unknown_replies` 计数，对应 token（若有）`complete_error`，**不**进 Lost。
//!   只有真正的 wire-level Err 才致命。
//!
//! **顺序保证（H-1）**：同一寄存器的 MMIO write→read 顺序，由
//! ① worker **单线程 FIFO 消费 `from_device`** + ② writer **单 socket 顺序发帧** +
//! ③ firmware **单连接串行处理** 共同保证。device 的 write 立即 Ok（Task 1.4 不
//! defer）、read defer，但两者的 [`DeviceRequest`] 按 guest 访问顺序进 channel，
//! worker FIFO 取出后按序发帧，因此到 firmware 的次序与 guest 访问次序一致。

use crate::state::DeviceState;
use crate::state::SharedState;
use chipset_device::io::IoError;
use chipset_device::io::deferred::DeferredRead;
use cvm_tracing::CVM_ALLOWED;
use futures::FutureExt;
use futures::StreamExt;
use futures::select_biased;
use inspect::Inspect;
use mesh::Receiver;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use vfio_user_wire::proto::RegionAccessPayload;
use vfio_user_wire::proto::decode_payload;
use vmcore::interrupt::Interrupt;

/// 可观察的 worker 计数器（device.rs 也持 clone 用于 inspect 暴露）。
///
/// 全是 AtomicU64，**单写者不变量：仅 [`Worker::run`] 持的 worker task 写入**；
/// ohcldiag-dev 通过 Inspect 只读。所以 Relaxed ordering 足够——这是诊断数据，
/// 不影响 program correctness。
#[derive(Inspect, Default)]
pub struct WorkerStats {
    /// 成功 complete 的 MMIO read reply 数。
    pub mmio_read_results: AtomicU64,
    /// 触发过的 MSI-X 中断数（已调 `Interrupt::deliver()`）。
    ///
    /// 注：W6b 的中断走独立 eventfd-wait task（见 [`crate::irq`]），**不**经本
    /// worker 主循环；该字段保留给将来若把中断计数收口到 worker 时用，当前恒 0。
    pub interrupts_fired: AtomicU64,
    /// 当前 inflight read 数；`drain_in_flight` 后归零。
    pub inflight_current: AtomicU64,
    /// 历史 inflight 峰值（fetch_max 单调递增，仅写不重置）。
    pub inflight_peak: AtomicU64,
    /// 异常 reply 数（msg_id 不在 in_flight、echo 不符、payload 过短、error reply）。
    /// 宽容计数：不致命，仅诊断。
    pub unknown_replies: AtomicU64,
    /// 最近一次进入 Lost 的 unix 毫秒时间戳；0 = 从未 Lost。
    pub last_lost_at_ms: AtomicU64,
    /// 最近一次进入 Lost 的原因分类（位标记，见 [`lost_reason`]）。0 = 未发生过。
    pub last_lost_reason: AtomicU64,
}

/// 共享 stats 句柄。
pub type SharedWorkerStats = Arc<WorkerStats>;

/// [`WorkerStats::last_lost_reason`] 位标记。
pub mod lost_reason {
    /// writer 发帧返 Err（control socket write 死）。
    pub const WRITE_ERR: u64 = 1 << 0;
    /// reader 收帧返 Err（control socket read 死）。
    pub const READ_ERR: u64 = 1 << 1;
    /// worker 主循环正常退出（shutdown 信号 / `from_device` 全 drop）。
    /// 区别于异常 Lost：诊断时见此 bit 说明设备被有序卸载，非 transport 真死。
    pub const WORKER_EXIT: u64 = 1 << 2;
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

/// device shim → worker 的请求。
pub struct DeviceRequest {
    /// 请求种类。
    pub kind: ReqKind,
}

/// 请求种类：MMIO 读（defer）或写（fire-and-forget）。
pub enum ReqKind {
    /// MMIO read：defer→worker，`token` 在对应 reply 到达后 `complete`。
    MmioRead {
        /// BAR / region index。
        bar: u32,
        /// region 内 offset。
        offset: u64,
        /// 访问字节数（1/2/4/8）。
        size: usize,
        /// 由 device shim 创建并移交进来的 DeferredRead。
        token: DeferredRead,
    },
    /// MMIO write：fire-and-forget（device 立即 Ok，worker 发帧不插 in_flight）。
    MmioWrite {
        /// BAR / region index。
        bar: u32,
        /// region 内 offset。
        offset: u64,
        /// 要写的数据。
        data: Vec<u8>,
    },
}

/// 一个在途 MMIO read：等 firmware 回对应 `msg_id` 的 REGION_READ reply。
struct InFlightRead {
    /// 等回填的 DeferredRead。
    token: DeferredRead,
    /// 期望 reply echo 的 region（= 请求 bar），用于校验。
    region: u32,
    /// 期望 reply echo 的 offset，用于校验。
    offset: u64,
    /// 期望 reply echo 的 count（= 请求 size），用于校验。
    count: u32,
    /// 实际访问字节数（1/2/4/8），用于 `complete` 截断。
    size: usize,
}

/// 后台 worker（W6b 全双工精简版）。
pub struct Worker {
    /// 发请求帧（REGION_READ / REGION_WRITE）。
    writer: vfio_user_device::VfioUserWriter,
    /// 收 reply 帧。
    reader: vfio_user_device::VfioUserReader,
    /// 下一个分配的 msg_id（wrapping，从 1 起）。
    next_msg_id: u16,
    /// msg_id → 在途 read。
    in_flight: HashMap<u16, InFlightRead>,
    /// 与 device shim 共享的状态机。
    state: SharedState,
    /// device shim → worker 的请求 channel。
    from_device: Receiver<DeviceRequest>,
    /// MSI-X 中断向量（长度 = msix_count；resolver 注入）。
    ///
    /// W6b 中断实际走 [`crate::irq`] 独立 eventfd-wait task；此 Vec 持有
    /// `Interrupt` 句柄保活（供 irq task clone）。worker 主循环当前不直接用它，
    /// 故 `_` 前缀（与 `_irq_tasks` 一致：held-for-lifetime scaffolding，resolver
    /// Task 1.6 注入）。
    _interrupts: Vec<Interrupt>,
    /// eventfd-wait task 句柄，保活防 drop（resolver spawn 后存进来）。
    _irq_tasks: Vec<pal_async::task::Task<()>>,
    /// 可观察 stats（与 device.rs 共享 Arc，inspect 暴露）。
    stats: SharedWorkerStats,
}

impl Worker {
    /// 构造 Worker（next_msg_id 从 1 起，in_flight 空）。
    pub fn new(
        writer: vfio_user_device::VfioUserWriter,
        reader: vfio_user_device::VfioUserReader,
        state: SharedState,
        from_device: Receiver<DeviceRequest>,
        interrupts: Vec<Interrupt>,
        irq_tasks: Vec<pal_async::task::Task<()>>,
        stats: SharedWorkerStats,
    ) -> Self {
        Self {
            writer,
            reader,
            next_msg_id: 1,
            in_flight: HashMap::new(),
            state,
            from_device,
            _interrupts: interrupts,
            _irq_tasks: irq_tasks,
            stats,
        }
    }

    /// 分配下一个 msg_id（wrapping；跳过 0，便于日志区分"未设置"）。
    fn alloc_msg_id(&mut self) -> u16 {
        let id = self.next_msg_id;
        self.next_msg_id = self.next_msg_id.wrapping_add(1);
        if self.next_msg_id == 0 {
            self.next_msg_id = 1;
        }
        id
    }

    /// 主循环：直到 shutdown 信号 / `from_device` 全 drop / transport 出错。
    pub async fn run(mut self, mut shutdown: Receiver<()>) {
        loop {
            // Lost 期不 read（control socket 死，会立即返回 Err → busy-loop）；
            // 改为永远 pending，镜像模板 `read_inbound_or_pending`。
            let is_lost = matches!(self.state.load(), DeviceState::Lost);
            select_biased! {
                _ = shutdown.next().fuse() => {
                    tracing::info!(CVM_ALLOWED, "vfio_user_pci worker shutdown signal");
                    break;
                }
                req = self.from_device.next().fuse() => {
                    let Some(req) = req else {
                        // sender 全 drop（device unbind），worker 真正退出。
                        break;
                    };
                    match req.kind {
                        ReqKind::MmioRead { bar, offset, size, token } => {
                            // MMIO 访问尺寸严格 ∈ {1,2,4,8}：DeferredRead::complete 内部把
                            // 数据塞进 [0u8; 8]，size > 8 会 panic worker task → drain 失效、
                            // 设备挂死。在边界拒绝（不入 in_flight、不发帧），让本次 read 失败
                            // 但 worker 存活。device shim（Task 1.4）只会发合法尺寸，这里是
                            // 防御性兜底。
                            if !matches!(size, 1 | 2 | 4 | 8) {
                                tracing::warn!(
                                    CVM_ALLOWED,
                                    bar,
                                    offset,
                                    size,
                                    "vfio_user_pci: MMIO read size 非法（须 1/2/4/8），拒绝"
                                );
                                token.complete_error(IoError::InvalidRegister);
                                continue;
                            }
                            let id = self.alloc_msg_id();
                            self.in_flight.insert(
                                id,
                                InFlightRead { token, region: bar, offset, count: size as u32, size },
                            );
                            let cur = self.in_flight.len() as u64;
                            self.stats.inflight_current.store(cur, Ordering::Relaxed);
                            self.stats.inflight_peak.fetch_max(cur, Ordering::Relaxed);
                            // writer 发 REGION_READ 帧（立即返回，不等 reply）。
                            if let Err(e) = self
                                .writer
                                .send_region_read(id, bar, offset, size as u32)
                                .await
                            {
                                self.go_lost(lost_reason::WRITE_ERR, &e);
                            }
                        }
                        ReqKind::MmioWrite { bar, offset, data } => {
                            // 不插 in_flight（fire-and-forget）。
                            let id = self.alloc_msg_id();
                            if let Err(e) = self
                                .writer
                                .send_region_write(id, bar, offset, &data)
                                .await
                            {
                                self.go_lost(lost_reason::WRITE_ERR, &e);
                            }
                        }
                    }
                }
                inbound = recv_reply_or_pending(&mut self.reader, is_lost).fuse() => {
                    match inbound {
                        Ok(reply) => self.dispatch_reply(reply),
                        Err(e) => self.go_lost(lost_reason::READ_ERR, &e),
                    }
                }
            }
        }
        self.drain_in_flight();
        self.stats
            .last_lost_reason
            .store(lost_reason::WORKER_EXIT, Ordering::Relaxed);
        self.state.store(DeviceState::Lost);
    }

    /// 处理一帧 reply：按 `msg_id` 路由回对应 in_flight read。
    ///
    /// 宽容策略：异常 reply（error reply / 未知 msg_id / echo 不符 / payload 过短）
    /// 只 bump `unknown_replies` 并 `complete_error` 对应 token（若有），**不**进 Lost。
    fn dispatch_reply(&mut self, reply: vfio_user_wire::framing::WireMessage) {
        // packed Header 字段先 copy 到本地（E0793）。
        let msg_id = reply.header.msg_id;
        let flags = reply.header.flags();

        // error reply：把对应 in_flight read 当失败处理。
        if flags.is_error() {
            self.bump_unknown();
            if let Some(f) = self.in_flight.remove(&msg_id) {
                self.refresh_inflight_stat();
                tracing::warn!(
                    CVM_ALLOWED,
                    msg_id,
                    "vfio_user_pci: error reply for in-flight MMIO read"
                );
                f.token.complete_error(IoError::InvalidRegister);
            } else {
                tracing::warn!(
                    CVM_ALLOWED,
                    msg_id,
                    "vfio_user_pci: error reply with unknown msg_id (discarded)"
                );
            }
            return;
        }

        // 成功 reply：查 in_flight。
        let Some(f) = self.in_flight.remove(&msg_id) else {
            // write reply（无 in_flight）或未知 msg_id：丢弃 + 宽容计数。
            self.bump_unknown();
            return;
        };
        self.refresh_inflight_stat();

        // 解 echo RegionAccessPayload（前 16B）+ 校验 + 取数据段。
        if let Some(echo_bytes) = reply.payload.get(..16) {
            match decode_payload::<RegionAccessPayload>(echo_bytes) {
                Ok(echo) => {
                    // packed 字段先 copy。
                    let echo_region = echo.region;
                    let echo_offset = echo.offset;
                    let echo_count = echo.count;
                    let data_end = 16usize.saturating_add(f.count as usize);
                    let data = reply.payload.get(16..data_end);
                    if echo_region == f.region
                        && echo_offset == f.offset
                        && echo_count == f.count
                        && data.is_some_and(|d| d.len() >= f.size)
                    {
                        // safe：上面 is_some_and 已确保 Some + 长度足够。
                        let data = data.expect("checked Some above");
                        f.token.complete(&data[..f.size]);
                        self.stats.mmio_read_results.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    tracing::warn!(
                        CVM_ALLOWED,
                        msg_id,
                        "vfio_user_pci: REGION_READ reply echo/length mismatch"
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        CVM_ALLOWED,
                        msg_id,
                        "vfio_user_pci: REGION_READ reply payload decode failed"
                    );
                }
            }
        } else {
            tracing::warn!(
                CVM_ALLOWED,
                msg_id,
                "vfio_user_pci: REGION_READ reply payload too short for echo"
            );
        }
        // 任何 echo/长度问题：宽容（非致命），但本次 read 失败。
        self.bump_unknown();
        f.token.complete_error(IoError::InvalidRegister);
    }

    /// 集中处理 transport 死亡 → Lost：log + 记时间戳/原因 + **先 store Lost 再 drain**。
    ///
    /// 顺序很重要：store Lost 在 drain 之前，让 device shim 一读到 Lost 就短路新
    /// 请求（即便 inflight 还在 drain）—— 镜像模板 `record_lost` 注释。中间窗口可能
    /// 短暂出现 `state=Lost && inflight_current > 0`，inspect 消费者应视为合法 transient。
    fn go_lost(&mut self, reason_bit: u64, err: &anyhow::Error) {
        tracing::warn!(
            CVM_ALLOWED,
            error = %err,
            reason_bit,
            "vfio_user_pci: transport dead, going Lost"
        );
        self.stats
            .last_lost_at_ms
            .store(now_unix_ms(), Ordering::Relaxed);
        self.stats
            .last_lost_reason
            .store(reason_bit, Ordering::Relaxed);
        self.state.store(DeviceState::Lost);
        self.drain_in_flight();
    }

    /// drain 所有在途 read，各 `complete_error(NoResponse)`；归零 inflight_current。
    fn drain_in_flight(&mut self) {
        for (_, f) in self.in_flight.drain() {
            f.token.complete_error(IoError::NoResponse);
        }
        self.stats.inflight_current.store(0, Ordering::Relaxed);
    }

    /// 把 inflight_current 刷新到 map 当前长度。
    fn refresh_inflight_stat(&self) {
        self.stats
            .inflight_current
            .store(self.in_flight.len() as u64, Ordering::Relaxed);
    }

    /// bump 宽容计数（异常 reply）。
    fn bump_unknown(&self) {
        self.stats.unknown_replies.fetch_add(1, Ordering::Relaxed);
    }
}

/// Lost 期不实际 read（避免 dead-socket busy-loop）：永远 pending，由 `from_device`
/// / shutdown 决定下一步。否则正常 `recv_reply`。镜像模板 `read_inbound_or_pending`。
async fn recv_reply_or_pending(
    reader: &mut vfio_user_device::VfioUserReader,
    is_lost: bool,
) -> anyhow::Result<vfio_user_wire::framing::WireMessage> {
    if is_lost {
        std::future::pending::<()>().await;
        unreachable!()
    } else {
        reader.recv_reply().await
    }
}
