// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe Controller —— `PcieDevice` 实现。
//!
//! 三个核心 state machine：
//! 1. **Enable**：CC.EN 0→1 → 从 ASQ/ACQ 注册的 GPA 启 admin loop；CC.EN
//!    1→0 → 等所有 outstanding DMA flush + 清状态 + CSTS.RDY=0。
//! 2. **Submission**：SQyTDBL 写入 → 发 DMA read SQ entries → 收 entries
//!    后逐条 dispatch_command。
//! 3. **Completion**：command 完成 → 构造 CQE → DMA write 到 CQ →
//!    fire_interrupt。
//!
//! DMA token 设计：高 8 bits = 操作分类（FetchSqEntries/FetchPrpData/
//! WriteData/WriteCqe），低 56 bits = 上下文（SQ ID + slot index）。
//! 用 token 直接路由 `on_dma_complete` 回到对应处理函数，避免 host 端
//! 维护 token → context map（O(1) match）。
//!
//! ## Phase M3 — Multi-queue 并发模型
//!
//! Phase H2 暴露了 4 个 IO queue (IO_QUEUE_SLOT_CAPACITY)；当前 dispatch 模型是
//! **per-controller 单 worker 线程**串行处理所有 SQE：
//! - SDK 的 `run` loop 单线程 select transport / tick
//! - 每个 SQyTDBL 写入触发独立的 DMA-read，多 queue 的 fetches 可
//!   并发在 wire (vsock) 上飞行；但完成时 `on_dma_complete` 仍单线程
//!   处理 → SQE dispatch 是串行
//! - 真并发收益只在 backing file IO 的并发性（多文件并行 read/write
//!   是 OS-level 并发）
//!
//! **真正并发需要的改造**（留 future）：
//! - 把 dispatch_sqe 改成 enqueue 到 per-SQ inbox + spawn N worker future
//! - backing file IO 改 async (tokio::fs)，让 file op 并行 await
//! - completion 回调 cross-thread → 需要 Mutex/atomic / channel 通信
//! - 性能换复杂度比换 ~10x 教学清晰度，目前 trade-off 保单线程；真要
//!   benchmark 可对比独立的 'multi-thread fork'。
//!
//! 教学：当前模型已足以展示 NVMe 协议正确性 + spec 全部命令。多 queue
//! 体现在 driver 视角（4 SQ 可独立投 cmd，不互相 stall 等待 CQE）。

mod admin;
mod completion;
pub mod discovery_log;
mod enable;
mod io;
mod logs;
mod mmio;
mod reservation;

use crate::cmd::*;
use crate::regs::*;
use pcie_device_core::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// 单 namespace 总容量（LBA 数）= 文件长度 / 512。
pub(super) const SECTOR_SHIFT: u32 = 9;
pub(super) const SECTOR_SIZE: u64 = 1 << SECTOR_SHIFT;

// ───── 编译期槽位上限（预留存储容量，DenseMap Vec 大小；非运行时广告值）─────

/// **预留 IO queue 槽位容量**（编译期）。`sqs`/`cqs` 的 [`DenseMap`] 各预留
/// `此值 + 1`（admin qid 0 + IO 1..=此值）个槽。这是**存储上限**，不是本次运行
/// 广告/授予的队列数——后者是运行时的 [`io_queue_pairs`](NvmeController::io_queue_pairs)
/// （≤ 本上限）。256 远超真硬件常见 64，给运行时模拟留足空间。
pub const IO_QUEUE_SLOT_CAPACITY: u16 = 256;

/// queue id 上界（admin 0 + IO 1..=IO_QUEUE_SLOT_CAPACITY）→ DenseMap slot 数 = 此 +1。
pub(super) const MAX_QID: u16 = IO_QUEUE_SLOT_CAPACITY;

/// **预留 namespace 槽位容量**（编译期）。namespaces 的 [`DenseMap`] 预留
/// `此值 + 1` 个槽（nsid 1..=此值）。这是**存储上限**；本次运行模拟的 NS 容量是
/// 运行时的 [`mnan`](NvmeController::mnan)（≤ 本上限，广告为 Identify Controller MNAN）。
pub const NAMESPACE_SLOT_CAPACITY: u32 = 8;

// ───── 运行时模拟上限（spec 名；≤ 上述编译期槽位容量；CLI 可配）─────

/// **2026-06-09** — 队列深度（MQES = Maximum Queue Entries Supported，单 SQ/CQ 最大
/// entry 数）的默认 / 范围。运行时经 [`NvmeController::set_max_queue_entries`] 调
/// （CLI flag）模拟不同档位设备。MQES 是 CAP 的 **0-based 16-bit** 字段（存 N-1，
/// ≤ 0xFFFF），故 entry 数 ∈ [1, 65536]；spec **不要求** 2 的幂。
pub const DEFAULT_MAX_QUEUE_ENTRIES: u32 = 128;
/// 队列深度可配下限（2 = 最小有意义深度）。
pub const MIN_MAX_QUEUE_ENTRIES: u32 = 2;
/// 队列深度可配上限 = MQES 字段满值（0xFFFF + 1 = 65536 entries）。
pub const MAX_MAX_QUEUE_ENTRIES: u32 = 65536;

// 注：本实现用 SDK 分配的 raw DMA token 直接作 HashMap key 路由完成回调；
// 不再做 token 高位 tagging（早期设计想用 tag 标 op 类别，实测 raw token
// 已唯一，多此一举）。

/// **2026-06-09** — 密集小整数键的稀疏映射，用 `Vec<Option<V>>` 直接索引。
///
/// 用于 SQ/CQ（u16 qid 0..=256）与 namespace（u32 nsid 1..=8）：键是密集小整数，
/// 直接索引 O(1) + cache-friendly，无 HashMap 哈希开销。API 镜像 `HashMap` 的常用
/// 子集（`get/get_mut/insert/remove/contains_key/clear`），故调用点零改动；`keys`/
/// `iter`/`iter_mut` 返 owned 键（slot 下标反推）。
///
/// 容量在 `new(max_key)` 固定（slot 数 = max_key+1）；`insert` 越界静默忽略
/// （caller 须先 bound-check，如 Create IO Queue 校验 qid ≤ IO_QUEUE_SLOT_CAPACITY）。
pub(super) struct DenseMap<K: SlotKey, V> {
    slots: Vec<Option<V>>,
    _k: core::marker::PhantomData<K>,
}

/// `DenseMap` 的键：可与 `usize` slot 下标互转的密集小整数。
pub(super) trait SlotKey: Copy {
    fn to_index(self) -> usize;
    fn from_index(i: usize) -> Self;
}
impl SlotKey for u16 {
    fn to_index(self) -> usize {
        self as usize
    }
    fn from_index(i: usize) -> Self {
        i as u16
    }
}
impl SlotKey for u32 {
    fn to_index(self) -> usize {
        self as usize
    }
    fn from_index(i: usize) -> Self {
        i as u32
    }
}

impl<K: SlotKey, V> DenseMap<K, V> {
    /// 建容量 = `max_key + 1` 的空映射（slot 0..=max_key）。
    pub(super) fn new(max_key: usize) -> Self {
        let mut slots = Vec::with_capacity(max_key + 1);
        slots.resize_with(max_key + 1, || None);
        Self {
            slots,
            _k: core::marker::PhantomData,
        }
    }
    pub(super) fn get(&self, k: &K) -> Option<&V> {
        self.slots.get(k.to_index()).and_then(|o| o.as_ref())
    }
    pub(super) fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        self.slots.get_mut(k.to_index()).and_then(|o| o.as_mut())
    }
    /// 镜像 `HashMap::insert`：返回被替换的旧值（若有）。越界（caller 未 bound-check）
    /// 静默忽略并返 `None` —— 生产校验在 caller（Create IO Queue qid gate）。
    pub(super) fn insert(&mut self, k: K, v: V) -> Option<V> {
        match self.slots.get_mut(k.to_index()) {
            Some(slot) => slot.replace(v),
            None => {
                // **review M-1** — 越界 insert 是 caller 漏 bound-check 的信号。
                // release 静默（不 panic 在线上），debug/test 立即命中 root cause，
                // 避免变成"success + 幽灵 entry"的远端症状（见 H-1）。
                debug_assert!(
                    false,
                    "DenseMap::insert 越界 key idx={}（caller 须先 bound-check）",
                    k.to_index()
                );
                None
            }
        }
    }
    pub(super) fn remove(&mut self, k: &K) -> Option<V> {
        self.slots.get_mut(k.to_index()).and_then(|o| o.take())
    }
    pub(super) fn contains_key(&self, k: &K) -> bool {
        self.get(k).is_some()
    }
    pub(super) fn clear(&mut self) {
        for s in &mut self.slots {
            *s = None;
        }
    }
    /// 已占用 slot 的键（升序，因 Vec 按下标）。返 owned 键（非 HashMap 的 `&K`）。
    pub(super) fn keys(&self) -> impl Iterator<Item = K> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.as_ref().map(|_| K::from_index(i)))
    }
    /// (key, &mut V) 升序迭代。返 owned 键。
    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = (K, &mut V)> + '_ {
        self.slots
            .iter_mut()
            .enumerate()
            .filter_map(|(i, o)| o.as_mut().map(|v| (K::from_index(i), v)))
    }
    /// 已占用 slot 数。
    pub(super) fn len(&self) -> usize {
        self.slots.iter().filter(|o| o.is_some()).count()
    }
    /// (key, &V) 升序迭代。返 owned 键。
    pub(super) fn iter(&self) -> impl Iterator<Item = (K, &V)> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.as_ref().map(|v| (K::from_index(i), v)))
    }
}

// 镜像 `HashMap` 的 `Index<&K>`：键不存在 panic（与 HashMap 行为一致）。
impl<K: SlotKey, V> core::ops::Index<&K> for DenseMap<K, V> {
    type Output = V;
    fn index(&self, k: &K) -> &V {
        self.get(k).expect("no entry found for key in DenseMap")
    }
}
impl<K: SlotKey, V> core::ops::IndexMut<&K> for DenseMap<K, V> {
    fn index_mut(&mut self, k: &K) -> &mut V {
        self.get_mut(k).expect("no entry found for key in DenseMap")
    }
}

/// **Phase S6** — Reservation Notification Log entry (NVMe spec § 5.16.1.20)。
///
/// log_page_type 取值（spec Figure 162）：
///   0 = Empty log page
///   1 = Reservation Preempted
///   2 = Reservation Released
///   3 = Reservation Registration Preempted
#[derive(Debug, Clone, Copy)]
pub(super) struct ReservationNotification {
    pub log_page_count: u64,
    pub log_page_type: u8,
    pub nsid: u32,
}

/// **Phase V8b reviewer C-2** — `nvme_install_admin_cq` 错误类型。
///
/// caller（V2Session::accept_and_handshake_shared）拿到 `MismatchedParams` 必须
/// hard-fail 该条 conn 的握手，不允许静默继续使用旧 CQ；否则后续 sentinel-based
/// 路径（`gpa >= CQ_BASE_GPA` 判 CQE bytes vs data write）会错位 → 协议越权。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminCqInstallError {
    /// admin CQ (cq_id=0) 已存在但 base_gpa 或 size 与新请求不一致。
    MismatchedParams {
        /// 已 install 的 base GPA。
        old_base_gpa: u64,
        /// 已 install 的 qsize。
        old_size: u32,
        /// 新请求的 base GPA。
        new_base_gpa: u64,
        /// 新请求的 qsize。
        new_size: u32,
    },
}

impl std::fmt::Display for AdminCqInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdminCqInstallError::MismatchedParams {
                old_base_gpa,
                old_size,
                new_base_gpa,
                new_size,
            } => write!(
                f,
                "nvme_install_admin_cq: admin CQ already exists with DIFFERENT params: \
                 old=(base={old_base_gpa:#x}, size={old_size}) vs new=(base={new_base_gpa:#x}, size={new_size})"
            ),
        }
    }
}

impl std::error::Error for AdminCqInstallError {}

impl NvmeController {
    /// **Phase S6** — push 一条 Reservation Notification 到 ring buffer。
    /// 保留末 32 条；log_page_count 单调递增。
    pub(super) fn push_reservation_notification(&mut self, log_page_type: u8, nsid: u32) {
        self.reservation_notification_count = self.reservation_notification_count.wrapping_add(1);
        let entry = ReservationNotification {
            log_page_count: self.reservation_notification_count,
            log_page_type,
            nsid,
        };
        if self.reservation_notification_log.len() >= 32 {
            self.reservation_notification_log.pop_front();
        }
        self.reservation_notification_log.push_back(entry);
        tracing::debug!(
            log_page_type,
            nsid,
            count = self.reservation_notification_count,
            "Reservation Notification queued"
        );
    }

    /// **Phase S7** — 切换 controller ANA state；递增 change_count 并发
    /// AEN type=0x02 (Notice) info=0x03 (ANA Change) log=0x0C，让 driver
    /// 重新读 Log 0x0C。new_state 取值：0x01 Optimized / 0x02 Non-Optimized
    /// / 0x03 Inaccessible / 0x04 Persistent Loss。返 true = state 真变。
    ///
    /// 当前仅 tests 调用；未来可挂到 vsock disconnect / re-handshake 事件，
    /// 让 driver 自动 multipath fail-over。
    #[allow(dead_code)]
    pub(super) fn set_ana_state(&mut self, ctx: &mut DeviceCtx<'_>, new_state: u8) -> bool {
        if !(0x01..=0x04).contains(&new_state) {
            return false;
        }
        if self.ana_state == new_state {
            return false;
        }
        self.ana_state = new_state;
        self.ana_change_count = self.ana_change_count.wrapping_add(1);
        tracing::info!(
            new_state,
            change_count = self.ana_change_count,
            "ANA state changed"
        );
        // 发 ANA Change Notice (NVMe spec § 5.2 Figure 174):
        //   type 0x02 Notice / info 0x03 ANA Change / log 0x0C ANA
        let _ = self.fire_aen(ctx, 0x02, 0x03, 0x0C);
        true
    }
}

/// Pending IO command 等 DMA 完成。
pub(super) struct PendingIo {
    sq_id: u16,
    cid: u16,
    sq_head: u16,
    cq_id: u16,
    /// **Phase H4** — 本 IO 所属 NSID。完成回调据此选 namespaces[nsid] 操作。
    /// Admin DMA-write Identify / Get Log Page 等无 NS 关联 = 0（特殊值，
    /// completion 看到 0 不查 namespace）。
    pub(super) nsid: u32,
    /// Read 路径：远端 DMA 完成后我们已读出数据，把 data 写到 LBA file，
    /// 然后构造 success CQE。
    /// Write 路径：DMA write 完成后构造 success CQE。
    op: PendingOp,
}

pub(super) enum PendingOp {
    /// 等 PRP1 内的 data DMA-read 完成 → 写文件 → success CQE。
    /// (单 PRP Write 路径)
    NvmWriteDmaRead {
        lba: u64,
        num_blocks: u32,
    },
    /// 双 PRP Write：PRP1 段 DMA-read 完成 → 填到 `dual_prp_writes[op_id].prp1_data`，
    /// 两段都到了就触发 dispatch_dual_prp。**op_id 与 PRP2 共用**：靠
    /// `dual_prp_writes[op_id]` 中的状态决定何时写盘 + 发 CQE。
    NvmWriteDualPrp {
        op_id: u64,
        is_prp1: bool,
    },
    /// 等 DMA-write 数据到 PRP1 完成 → success CQE。
    /// （NVM Read 路径 + Admin Identify / Get Log Page 共用入口）
    /// `num_blocks` = NVM Read 时 LBA 数；admin（Identify/Log）= 0，
    /// 用于 SMART 统计区分（Phase F：只算真 IO，不算 admin 元数据）。
    NvmReadDmaWrite {
        num_blocks: u32,
    },
    /// **H4 修复** — 双 PRP Read 的"非记账段" sibling（成功时静默；失败时
    /// 经过通用 DMA-fail 路径 post error CQE）。设计：tok2 走
    /// `NvmReadDmaWrite { num_blocks: nlb }` 负责真正 success CQE +
    /// counter；tok1 走此变体只为捕获失败。
    NvmReadDualPrpSiblingHalf,
    /// **Phase E** — NVM Write with PRP list (> 2 page)。
    /// Step 1: DMA-read PRP list page itself（4 KiB u64 数组）。
    NvmWritePrpListFetch {
        op_id: u64,
    },
    /// **Phase E** — NVM Write with PRP list, Step 2: per-page data DMA-read。
    /// `page_idx` 是 PRP 中第几个数据页（PRP1=0，PRP list[0]=1，list[1]=2，…）。
    NvmWritePrpListData {
        op_id: u64,
        page_idx: u32,
    },
    /// **Phase E** — NVM Read with PRP list, Step 1: fetch PRP list 数组本身。
    NvmReadPrpListFetch {
        op_id: u64,
    },
    /// **Phase E** — NVM Read with PRP list, Step 2: per-page data DMA-write。
    /// `page_idx` 是 PRP 中第几个数据页。
    NvmReadPrpListData {
        op_id: u64,
        page_idx: u32,
    },
    /// **Phase R2** — SGL Segment（PSDT=10）：embedded SGL1 指向的 segment 页
    /// DMA-read 完成 → parse descriptor + 累积 fragment plan。`is_last` 标记本段
    /// 经 Last Segment(true) 还是 Segment(false) 到达：false 时末位 descriptor 是
    /// continuation → 递归 fetch 下一段（R2b chain）。
    NvmSglFetch {
        op_id: u64,
        is_last: bool,
    },
    /// **Phase R2** — SGL 数据 fragment 传输完成（READ = scatter dma_write 到
    /// host / WRITE = gather dma_read 自 host）。`frag_idx` 索引 `SglOp.frags`。
    NvmSglData {
        op_id: u64,
        frag_idx: u32,
    },
    /// **Phase H3** — NVM Compare：DMA-read host buffer 完成后与 backing
    /// LBA 对比。`lba/num_blocks` 用于 file seek+read；对比失败返
    /// COMPARE_FAILURE (SC 0x85, SCT=0x02 Media/Data Integrity)。
    /// 单 PRP 路径（≤ 1 page）。
    NvmCompareSinglePrp {
        lba: u64,
        num_blocks: u32,
    },
    /// **Phase O3** — Fused Compare (single-PRP) 真 atomic chain：Compare
    /// 完成后**若 pass** → dispatch the captured Write SQE；若 fail → abort
    /// Write with COMPARE_FAILURE，不写盘。spec § 6.2 atomic CAS 语义。
    NvmCompareSinglePrpFused {
        lba: u64,
        num_blocks: u32,
        /// 待执行的 Write SQE（Compare 完成后真 dispatch）
        write_sqe: Sqe,
        write_sq_head: u16,
        write_sq_id: u16,
    },
    /// **Phase H5** — Firmware Image Download chunk DMA-read 完成。
    /// `offset_bytes` = byte offset into fw_download_buf。
    AdminFwDownloadChunk {
        offset_bytes: u32,
    },
    /// **Phase H6 + P1 (PTPL)** — Reservation 命令 DMA-read 完成。
    /// `rrega/racqa/rrela` 是 spec cdw10 bits 2:0（Register/Acquire/Release
    /// Action）；`rtype` 是 cdw10 bits 15:8 reservation type。
    /// `cptpl` = cdw10 bits 31:30（Register only；00=不变 / 01=clear /
    /// 11=set）触发 controller 把 reservation state 持久化到 sidecar 文件，
    /// reset / re-open 时自动 reload。统一变体让完成回调按 op_kind 分流。
    NvmReservationCmd {
        op_kind: ReservationKind,
        action: u8,
        rtype: u8,
        cptpl: u8,
    },
    /// **Phase K3** — NS Management Create：DMA-read 完成后 parse 4 KiB
    /// NS Identify 结构（含 NSZE/NCAP/FLBAS/DPS），分配新 NSID + RAM
    /// backing。Create 选 SEL=0，新 NSID 在 CQE.cdw0 返。
    AdminNsCreate,
    /// **Phase K2** — Compare with dual PRP (≤ 2 page)。两段 DMA-read
    /// 共用 op_id 累积，全到齐后合并 → byte-compare backing。
    NvmCompareDualPrp {
        op_id: u64,
        is_prp1: bool,
    },
    /// **Phase K2** — Compare PRP list (> 2 page)：先 fetch list 页本身。
    NvmComparePrpListFetch {
        op_id: u64,
    },
    /// **Phase K2** — Compare PRP list per-page data DMA-read。
    NvmComparePrpListData {
        op_id: u64,
        page_idx: u32,
    },
    /// **Phase K9** — Set Features 0x81 Host Identifier DMA-read 完成。
    /// cdw11 bit 0 EXHID = 1 → 16 byte HOSTID；= 0 → 8 byte。
    AdminSetHostIdentifier {
        exhid: bool,
    },
    /// **Phase S4** — NS Attachment SEL Attach/Detach 的 Controller List
    /// (4 KiB DMA-read) 完成回调。`sel`: 0=Attach 1=Detach。读完后解析
    /// NumIDs + cntlid 列表，若包含本 controller cntlid (=1)，更新目标
    /// NSID 的 attached 状态。
    AdminNsAttachmentList {
        sel: u8,
    },
    /// **Phase K4a** — PI Write 完成回调：DMA-read 完成后按 LBA 切 4KiB
    /// data，每 LBA 计算 T10 DIF tuple，interleave 写到 backing file
    /// (data + 8B tuple per LBA)。支持任意 nlb（≤ MDTS）。
    /// 限制：单 PRP 路径（bytes ≤ NVME_PAGE_SIZE = 4 KiB = 1 LBA when
    /// lbads=12）。多 LBA 需 dual-PRP / PRP list — 留 K4c。
    NvmWritePi {
        lba: u64,
        num_blocks: u32,
    },
    /// **B6b-2/B6b-4（separate metadata，PRACT=0）** — separate-buffer PI Write 的
    /// 子-DMA：host data 经 PRP（多 LBA 时第 0 块 PRP1、第 1 块 PRP2）、host PI tuple
    /// 经 MPTR（一条 N×8 字节）分别 DMA-read。N 条 data + 1 条 meta 都到齐后
    /// `SepMetaWriteAccum` finalize：verify-all-then-store-all（原子）。
    /// `page_idx` 标识本条 data 属于第几块（0..num_blocks）。
    SepMetaWriteData {
        op_id: u64,
        page_idx: u32,
    },
    SepMetaWriteMeta {
        op_id: u64,
    },
    /// **B6b-3** — separate PI Read 的一条 DMA-write（data→PRP 或 tuple→MPTR）完成。
    /// 两条都完成后 SepMetaReadAccum.remaining→0 post success CQE。
    SepMetaReadDone {
        op_id: u64,
    },
    /// **Phase K4b** — PI Read sibling 占位（per-LBA file read + verify
    /// 在 dispatch 时同步完成，DMA-write 数据回 PRP1 在 PendingIo 路径）。
    NvmReadPiDmaWrite {
        num_blocks: u32,
    },
    /// **Phase L1+ (reviewer H1 修复)** — ZNS Zone Append 完成回调。
    ///
    /// Append 与 Write 不同：driver 不知 WP，controller 决定落点。所以必须
    /// 在 success CQE 把 `assigned_lba` 通过 cdw0/cdw1 返回（ZNS CS § 3.2.4）。
    /// 失败时还要回滚之前预占的 WP / state（否则 zone 永久错位）。
    ///
    /// 为此区别于 `NvmWriteDmaRead`：
    /// - `zone_idx`: 命中的 zone（用于 rollback / state transition）
    /// - `assigned_lba`: WP-based 落点（success 时返回 driver）
    /// - `prev_wp` / `prev_state`: rollback 用的旧值
    /// - `num_blocks`: 计数用
    NvmZoneAppend {
        zone_idx: usize,
        assigned_lba: u64,
        prev_wp: u64,
        prev_state: ZoneState,
        num_blocks: u32,
    },
    /// **Phase K4c** — 多 LBA PI Write 数据段。op_id 索引 `pi_writes`
    /// 累积器；page_idx = 本回调对应 `received[page_idx*4096..]`
    /// （单 PRP=0；dual-PRP 0/1；PRP-list 0/1/2/.../N-1）。
    NvmWritePiMulti {
        op_id: u64,
        page_idx: u32,
    },
    /// **Phase K4c-list** — PI Write PRP-list fetch：PRP2 指向的 list 页
    /// 已 DMA-read 到 data；解析 u64 array → 逐页 DMA-read 数据。
    NvmWritePiListFetch {
        op_id: u64,
    },
    /// **Phase K4c-list** — PI Read PRP-list fetch：list 页 DMA-read 完成；
    /// 解析 u64 array → 把 backing 已 verified 的 data per-page DMA-write
    /// 到 host 各页。
    NvmReadPiListFetch {
        op_id: u64,
    },
    /// **Phase K4c-list** — PI Read PRP-list per-page DMA-write 完成。
    /// 全 page 完成 → post success CQE。`page_idx` 仅用于 trace/debug
    /// （完成路径只增 pages_done）。
    NvmReadPiListData {
        op_id: u64,
        #[allow(dead_code)]
        page_idx: u32,
    },
    /// **Phase O1** — Simple Copy 范围表 DMA-read 完成。完成后 controller
    /// 解析 32-byte range descriptors → 按 src→dst 顺序 backing read+write。
    /// 不再有 host DMA：copy 全在 controller 侧 backing。
    NvmCopyFetchRanges {
        sdlba: u64,
        num_ranges: u32,
    },
}

/// **Phase K4c** — 多 LBA PI Write 累积器。每个 DMA-read 完成填一段
/// `received`；`pages_done == pages_total` 时按 LBA 切 4096-byte data，
/// per-LBA compute PiTuple + interleave 4104-byte blocks 写到 backing。
pub(super) struct PiWriteAccum {
    pub(super) nsid: u32,
    pub(super) slba: u64,
    pub(super) num_blocks: u32,
    #[allow(dead_code)] // useful for diagnostics; received.len() 同义
    pub(super) data_bytes_total: usize,
    /// 全 LBA data，已按 page_idx 顺序填入。
    pub(super) received: Vec<u8>,
    pub(super) pages_done: u32,
    pub(super) pages_total: u32,
    pub(super) sq_id: u16,
    pub(super) cid: u16,
    pub(super) sq_head: u16,
    pub(super) cq_id: u16,
    /// **Phase K4c-list** — PRP-list 路径用：PRP1 已 dispatch（page 0），
    /// 仍待 fetch list 页解析后再 dispatch page 1..N 的标志。
    pub(super) prp_list_pending: bool,
}

/// **Phase K4c-list** — 多 LBA PI Read 累积器。dispatch 时 backing 已 verify
/// 完成 + data 提取到 `data_only`；按 PRP 三档拆 + 逐页 DMA-write 到 host。
pub(super) struct PiReadAccum {
    pub(super) nsid: u32,
    pub(super) num_blocks: u32,
    pub(super) data_only: Vec<u8>,
    pub(super) pages_done: u32,
    pub(super) pages_total: u32,
    pub(super) sq_id: u16,
    pub(super) cid: u16,
    pub(super) sq_head: u16,
    pub(super) cq_id: u16,
}

/// 双 PRP Write 累积：两段 DMA-read 任一先到都填到 prp1_data/prp2_data；
/// 两段都到时合并写盘 + 发 CQE。op_id 独立于 SDK DMA token，由
/// `NvmeController::next_op_id` 单调分配，**两个 PendingOp::NvmWriteDualPrp
/// 共用同一 op_id**，乱序到达也正确处理；只要双 PRP DMA 都失败才不发 CQE
/// （由 `on_dma_complete` 失败路径专门处理）。
pub(super) struct WriteAccum {
    sq_id: u16,
    cid: u16,
    sq_head: u16,
    cq_id: u16,
    /// **Phase H4** — 目标 NSID
    nsid: u32,
    lba: u64,
    num_blocks: u32,
    prp1_data: Option<Vec<u8>>,
    prp2_data: Option<Vec<u8>>,
}

/// **Phase K2** — Compare 双 PRP 或 PRP list 累积（与 WriteAccum 同结构
/// 但语义不同：累积 host 数据后与 backing 比较，不写盘）。
pub(super) struct CompareAccum {
    pub(super) sq_id: u16,
    pub(super) cid: u16,
    pub(super) sq_head: u16,
    pub(super) cq_id: u16,
    pub(super) nsid: u32,
    pub(super) lba: u64,
    pub(super) num_blocks: u32,
    /// dual-PRP 路径：prp1_data + prp2_data；PRP-list 路径：
    /// data_pages[0..total_pages] 全填 (None = 未到达)。
    pub(super) prp1_data: Option<Vec<u8>>,
    pub(super) prp2_data: Option<Vec<u8>>,
    /// PRP-list 路径用；single/dual-PRP 路径长度 0 不用。
    pub(super) data_pages: Vec<Option<Vec<u8>>>,
    pub(super) total_pages: u32,
    pub(super) pages_done: u32,
    pub(super) list_entries: Option<Vec<u64>>,
}

/// **Phase E** — PRP-list IO 累积（Write 或 Read，> 2 page）。
///
/// NVMe spec § 4.4 PRP layout：当 transfer > 2 page，PRP1 = 第一页，
/// PRP2 = 指向 PRP list 页（4 KiB，512 个 u64 page pointer）。
///
/// 状态机：
/// 1. dispatch_io 入口：分配 op_id + 入 prp_list_ops；DMA-read PRP1
///    （writes：fetch data 到 prp1_data）/ dma_write PRP1（reads：从
///    backing 读 data DMA 到 host）。同时 DMA-read PRP list 页本身。
/// 2. PRP list 到达：parse u64 数组，存 list_entries；分别 issue 每页
///    sub-DMA。
/// 3. 每页 sub-DMA 完成：填 data_pages[idx]；全部到齐 →（Write）合并
///    写文件 / （Read）只需 ack done。
/// 4. 全部 sub-DMA 完成 → post CQE。
pub(super) struct PrpListOp {
    pub(super) sq_id: u16,
    pub(super) cid: u16,
    pub(super) sq_head: u16,
    pub(super) cq_id: u16,
    /// **Phase H4** — 目标 NSID
    pub(super) nsid: u32,
    pub(super) lba: u64,
    pub(super) num_blocks: u32,
    /// true = Write (host→device data flow)；false = Read。
    /// 暂未读取（PendingOp 变体已区分 read/write 路径），保留供未来
    /// fault-injection 测试 / 真错误路径区分。
    #[allow(dead_code)]
    pub(super) is_write: bool,
    /// PRP1 GPA — Read 路径 step-2 需要 dma_write 数据回 PRP1。
    pub(super) prp1_gpa: u64,
    /// PRP list 页本身的数据（512 个 u64 GPA pointer），lazy 填充。
    pub(super) list_entries: Option<Vec<u64>>,
    /// 总共多少数据页（PRP1 + PRP list 中条目）。
    pub(super) total_pages: u32,
    /// 已完成的数据 sub-DMA 数。
    pub(super) pages_done: u32,
    /// （仅 Write）每页 data buffer，filled by sub-DMA-read 完成。
    /// 索引 0 = PRP1 数据；1..N = PRP list[0..N-1] 数据。
    pub(super) data_pages: Vec<Option<Vec<u8>>>,
}

/// **Phase R2b** — SGL segment chain fetch 段数上限（防恶意 driver 构造
/// Segment 自环导致无限 DMA-read）。教学路径：单段 ≤ 1 page = 256 descriptor，
/// 64 段足够任何合法 MDTS 传输（远超真实驱动用量）。
pub(super) const MAX_SGL_SEGMENTS: u32 = 64;

/// **Phase R2** — SGL Segment 数据路径累积器（PSDT=10）。
///
/// 镜像 `PrpListOp`，但 SGL 数据是任意 (address, length) fragment 的
/// scatter-gather，**无法**映射成单 (prp1, prp2)，需平行机件：
/// - WALK 阶段：DMA-read embedded SGL1 指向的 segment 页 → parse descriptor
///   →（R2b：末位 continuation descriptor → 递归 fetch 下一段）→ 累积
///   fragment plan（每片记 host address + 在数据流中的偏移 + 长度）。
/// - TRANSFER 阶段：walk 完成后逐 fragment DMA：
///   * READ (controller→host)：`data` 已从 backing 读好，按 fragment 切片
///     `dma_write` 到各 host 地址。
///   * WRITE (host→controller)：按 fragment `dma_read` 填 `data`，全到齐后
///     一次性写 backing。
pub(super) struct SglOp {
    pub(super) sq_id: u16,
    pub(super) cid: u16,
    pub(super) sq_head: u16,
    pub(super) cq_id: u16,
    pub(super) nsid: u32,
    pub(super) lba: u64,
    pub(super) num_blocks: u32,
    /// true = WRITE (host→device)，false = READ。
    pub(super) is_write: bool,
    /// per-NS 扇区字节（1<<lbads）。WRITE 完成写 backing 时算偏移用。
    pub(super) sector_bytes: u64,
    /// 期望总传输字节 = num_blocks * sector_bytes。fragment 覆盖须正好等于它。
    pub(super) expected_bytes: u64,
    /// READ：backing 已读好的数据（待按 fragment scatter）。
    /// WRITE：gather buffer（按 fragment dma_read 填，全到齐写 backing）。
    pub(super) data: Vec<u8>,
    /// segment walk 累积的数据 fragment（按 SGL 顺序，已计算 stream 偏移）。
    pub(super) frags: Vec<SglPlanFrag>,
    /// walk 中数据流累积偏移（下一个 fragment 在 `data` 流中的起点）。
    pub(super) walk_offset: u64,
    /// **Phase R2b** — 已 fetch 的 segment 段数（chain 自环上限保护，见
    /// `MAX_SGL_SEGMENTS`）。
    pub(super) walk_segments: u32,
    /// TRANSFER 阶段已完成的数据 fragment DMA 数。
    pub(super) transfers_done: u32,
    /// TRANSFER 阶段需完成的数据 fragment DMA 总数（walk 完成后 set）。
    pub(super) transfers_total: u32,
}

/// **Phase R2** — 单个 SGL 数据 fragment 的传输计划（walk 阶段构建）。
#[derive(Clone, Copy)]
pub(super) struct SglPlanFrag {
    /// host GPA（Data Block 目标 / 源）。
    pub(super) address: u64,
    /// 在 `SglOp.data` 流中的起点字节偏移。
    pub(super) stream_offset: u64,
    /// 本 fragment 字节数。
    pub(super) length: u32,
}

/// **B6b-2/B6b-4（separate metadata，PRACT=0）** — separate-buffer PI Write 累积器。
/// host 的 data（经 PRP，N 条子-DMA）与 PI tuple（经 MPTR，1 条 N×8 字节）分别 DMA-read
/// 到达；N 条 data + meta 都到齐后 **verify-all-then-store-all**：先逐块 verify host PI vs
/// data（per pi_type），**任一失败则全不落盘（原子）**；全通过才 interleave [tuple][data]
/// （per pi_first）逐块存盘。N≤2（dual-PRP）；N>2（PRP-list data）作为最终扩展 defer。
pub(super) struct SepMetaWriteAccum {
    pub(super) sq_id: u16,
    pub(super) cid: u16,
    pub(super) sq_head: u16,
    pub(super) cq_id: u16,
    pub(super) nsid: u32,
    /// 起始 LBA（slba）；第 i 块落在 `lba + i`。
    pub(super) lba: u64,
    /// 本命令的 LBA 数（nlb，1..=2；N>2 走 PRP-list，待续）。
    pub(super) num_blocks: u32,
    /// host data，按 page_idx 填（len=num_blocks）。第 i 项 = 第 i 块的纯 data（无 tuple）。
    pub(super) data_pages: Vec<Option<Vec<u8>>>,
    /// 尚未到达的 data 子-DMA 数（初值 num_blocks，每条 SepMetaWriteData 减 1）。
    pub(super) data_remaining: u32,
    /// host PI tuple，N×8 字节（MPTR 一条 DMA-read 填）。第 i 个 tuple 在 `meta[i*8..i*8+8]`。
    pub(super) meta: Option<Vec<u8>>,
}

/// **B6b-3/B6b-4（separate metadata，PRACT=0）** — separate-buffer PI Read 累积器。
/// dispatch 时已从 backing 读 N 个 interleaved block + verify-all stored PI + 发 N+1 条
/// DMA-write（N×data→PRP[1/2]、tuple concat→MPTR）；全部完成（remaining→0）后 post
/// success CQE。`num_blocks` 仅供 stat 计数（LBA 读数）。
pub(super) struct SepMetaReadAccum {
    pub(super) sq_id: u16,
    pub(super) cid: u16,
    pub(super) sq_head: u16,
    pub(super) cq_id: u16,
    /// 尚未完成的 DMA-write 数（初值 N data + 1 meta = num_blocks + 1）。
    pub(super) remaining: u32,
    /// 本命令的 LBA 数（stat 用）。
    pub(super) num_blocks: u32,
}

// ═══════════════════════ A1（Abort, spec § 5.1）═══════════════════════
//
// Abort 命令按 (SQID, CID) 定位一条 in-flight 命令并中止。本 controller 的
// in-flight 异步命令有两类追踪：
//  1. **单-DMA 命令**（plain Write/Read）：仅在 `pending_ios`（token→PendingIo）。
//  2. **累积器命令**（dual-PRP / PI-multi / PRP-list / SGL / Compare）：一个累积器
//     （6 张表之一）+ **多条** `pending_ios` 子-DMA 条目（都带命令的 sqid/cid）。
// 每个累积器都带 (sq_id, cid)（定位键）+ (cq_id, sq_head)（构造被中止命令 CQE）。
// 下面的 trait + 泛型 `abort_scan` 处理这 6 张累积器表；`pending_ios` 的子-DMA 条目
// 由 `try_abort_inflight` 直接 sweep（避免 partial-abort）；fused FIRST 等 SECOND
// （pending_fused，keyed by sq_id，cid 在 Sqe 内）形状不同，单独处理。

/// async-pending 累积器的 Abort 定位能力（A1）。
trait AbortableOp {
    /// (sq_id, cid) —— 与 Abort cdw10 的 SQID|CID 匹配。
    fn abort_match(&self) -> (u16, u16);
    /// (cq_id, sq_head) —— 给被中止命令 post COMMAND_ABORT_REQUESTED 用。
    fn abort_target(&self) -> (u16, u16);
}

/// 6 个累积器的字段名一致（sq_id/cid/cq_id/sq_head），用 macro 消除重复 impl。
/// （PendingIo 的子-DMA 条目改由 `try_abort_inflight` 直接 sweep，不走 abort_scan。）
macro_rules! impl_abortable_op {
    ($t:ty) => {
        impl AbortableOp for $t {
            fn abort_match(&self) -> (u16, u16) {
                (self.sq_id, self.cid)
            }
            fn abort_target(&self) -> (u16, u16) {
                (self.cq_id, self.sq_head)
            }
        }
    };
}
impl_abortable_op!(WriteAccum);
impl_abortable_op!(CompareAccum);
impl_abortable_op!(PiWriteAccum);
impl_abortable_op!(PiReadAccum);
impl_abortable_op!(PrpListOp);
impl_abortable_op!(SglOp);
impl_abortable_op!(SepMetaWriteAccum);
impl_abortable_op!(SepMetaReadAccum);

/// 在一张 DMA-pending 表里找 (sqid, cid) 匹配的累积器，命中则移除并返其
/// (cq_id, sq_head)。移除后该 op 在飞的 DMA completion 会走 unknown-token
/// 静默忽略（与 `disable()` 清表同一机制），不会重复完成。
fn abort_scan<T: AbortableOp>(
    map: &mut HashMap<u64, T>,
    sqid: u16,
    cid: u16,
) -> Option<(u16, u16)> {
    let tok = map
        .iter()
        .find(|(_, e)| e.abort_match() == (sqid, cid))
        .map(|(&t, _)| t)?;
    Some(map.remove(&tok).unwrap().abort_target())
}

/// **Phase H4 + K1** — 单个 namespace 状态。
/// 每 NS 有独立 backing file + 容量 + LBA 格式 + Protection Information
/// 配置。spec § 1.6 "An NSID maps to one namespace"。
pub(super) struct Namespace {
    pub(super) file: File,
    /// **Phase M2** — Lazy mmap of `file`（整文件映射，长度 = file_size）。
    /// 首次 `read_at` / `write_at` 时建立；之后 hot-path 跳过 syscall，
    /// 直接 memcpy from/to mmap slice。失败回退 file IO。
    ///
    /// 何时 invalidate (重要)：**任何 in-process file size 变化都必须先
    /// drop 这个 mmap**，否则 Linux mmap 越界访问 = SIGBUS。具体：
    /// - Format NVM SES=1/2：admin.rs set_len(0) 前先 `ns.mmap = None`，
    ///   完成后 try_mmap_file 重建（Reviewer C-2 修复）
    /// - NS Management Delete：drop Namespace 自动 drop mmap
    /// - 未来 NS Resize：必须 drop+rebuild
    ///
    /// Format SES=0 不改 size，mmap 仍 valid。
    pub(super) mmap: Option<memmap2::MmapMut>,
    /// LBA 数（按当前 lbads + meta_size 计算 = file_size / block_bytes）。
    pub(super) total_lba: u64,
    /// Backing 文件路径 — 仅供日志 / Identify Namespace 扩展用。
    #[allow(dead_code)]
    pub(super) path: String,
    /// **Phase K1** — LBA Data Size shift。9=512B / 12=4096B。Format NVM
    /// 时由 LBAF index 决定。spec § 5.17.2.1 LBAF[N].LBADS。
    pub(super) lbads: u8,
    /// **Phase K1** — Metadata bytes per LBA。0 = 无 metadata (PI 不能开)；
    /// 8 = PI 占用整个 metadata。spec § 5.17.2.1 LBAF[N].MS。
    pub(super) meta_size: u8,
    /// **Phase K1** — Protection Information Type (DPS bits 2:0)。
    /// 0=none / 1=T10 DIF Type 1 / 2=Type 2 / 3=Type 3。spec § 8.3.1。
    pub(super) pi_type: u8,
    /// **Phase K1** — PI in first 8 bytes of metadata (DPS bit 3)。
    /// true = first，false = last。spec § 5.17.2.1。
    pub(super) pi_first: bool,
    /// **Phase H6 + K9** — 已注册的 host 列表。每 entry =
    /// (rkey, hostid_lo, hostid_hi) — 完整 16-byte HOSTID (spec § 5.21.1.27
    /// Set Features 0x81 Host Identifier；driver 用 Get Features 0x81
    /// 读出 controller 自己生成的 ID 或 driver 给出 ID)。
    /// 我们简化：(hostid_lo=0, hostid_hi=0) 默认 = "anonymous host"，仅
    /// 用 rkey 区分；K9 driver 若调 Set Features 0x81 设置真 HOSTID 我们
    /// 把它和 rkey 一起记。
    pub(super) registrants: Vec<(u64, u64, u64)>,
    /// 当前 reservation 持有者的 key + type。
    pub(super) reservation: Option<(u64, u8)>,
    /// **Phase J reviewer M3 修复** — Reservation Status 'GEN' 字段，
    /// 单调递增（即使 unregister 也 +1），driver 用此感知 state 变化。
    /// spec § 6.14。
    pub(super) reservation_gen: u32,
    /// **Phase P1** — Persist Through Power Loss 标志。Register cdw10 bits
    /// 31:30 = 11 时 set，01 时 clear。set 后每次 Register/Acquire/Release
    /// 完成都把 registrants + reservation + gen 写到 sidecar `.ptpl` 文件；
    /// open() 时若 sidecar 存在则 reload，模拟 power-loss 恢复。
    pub(super) ptpl: bool,
    /// **Phase S1** — Namespace Write Protection State (spec § 8.19)。
    /// 0=NoWP / 1=WP / 2=WP-until-power-cycle / 3=Permanent。Set Features
    /// 0x84 写；Format/Write/DSM/Copy 路径检查。Permanent (3) 不可降级。
    pub(super) nswp: u8,
    /// **Phase S4** — Namespace Attachment state（NVMe spec § 5.20）。
    /// true = NSID attached 到本 controller (cntlid=1)；false = detached
    /// (IO 拒 INVALID_NAMESPACE)。NS Attachment opcode 0x15 维护。
    /// 默认 true：open() 时所有 NS 已 attached。
    pub(super) attached: bool,
    /// **Phase L1** — Zoned Namespace 状态（None = 普通 NVM NS，Some = ZNS）。
    /// ZNS NS 的 CSI=0x02，Identify NS CNS=0x05 返 ZNS-specific 字段；
    /// Read/Write 必须遵循 SWR（Sequential Write Required）。
    pub(super) zns: Option<ZnsState>,
    /// **B6b（separate metadata，spec § 8.3 / FLBAS.inband_metadata）** — metadata
    /// 布局：true = extended LBA（内联，MSET=1，host buffer data+meta 连续 / PRACT=1
    /// 时 controller 自动插）；false = separate buffer（MSET=0，metadata 走独立 MPTR
    /// buffer）。仅 meta_size>0 时有意义。默认 true（既有内联 NS）。Format MSET 设置；
    /// Identify NS FLBAS bit4 = (meta_size>0 && meta_inline)。
    pub(super) meta_inline: bool,
    /// **D（test-only fault injection）** — 强制 `flush()` 返 Err，用来测
    /// shutdown-flush 失败 → CSTS.CFS 路径（真 file sync_all 难在单测里失败）。
    /// production 恒 false（仅 `#[cfg(test)]` 置位）。
    pub(super) force_flush_err: bool,
    /// **NS-not-ready spec-completeness（NAMESPACE_NOT_READY 0x82）** — NS 已 attach
    /// 但 media 尚未初始化、当前不可用（NSTAT.NRDY=1）。true 时对该 NS 的所有 IO
    /// 返 NAMESPACE_NOT_READY（io.rs dispatch 早期门），且 Identify NS CNS 0x08 的
    /// NSTAT.NRDY 跟随此位（广告与门一致）。教学触发：bin `--not-ready-nsid` 让指定
    /// NS 开机即 not-ready，模拟"media 初始化未完成"；Format NVM 初始化该 NS 后清此位
    /// （→ ready）。spec § generic status 0x82：DNR=0，driver 重试。默认 false（既有
    /// NS 开机即 ready）。
    pub(super) not_ready: bool,
}

/// **Phase L1** — ZNS (Zoned Namespace) 状态（spec ZNS CS § 4）。
pub(super) struct ZnsState {
    /// 单 zone 容纳 LBA 数（典型 256 MiB / 512 = 524288 LBA，本教学小
    /// 化为 256 LBA = 128 KiB）。
    pub(super) zone_size: u64,
    /// 单 zone 可写 LBA 数（≤ zone_size；spec 留 capacity 给 metadata）。
    pub(super) zone_capacity: u64,
    /// Max Active Zones / Max Open Zones 限制（0 = 无限）。
    /// **Reviewer H2 修复** — Zone Mgmt Send Open + Append (隐式 Open) 现在
    /// 真 enforce 这些限制 (spec ZNS § 2.2)。当前 default = 0 (无限)；未来
    /// 接入 Identify NS ZNS-CS 字段 MAR/MOR 上报本值给 driver（TODO L1c）。
    pub(super) max_open: u32,
    pub(super) max_active: u32,
    /// per-zone 状态。zones[i] 对应 LBA 范围 [i*zone_size, (i+1)*zone_size)。
    pub(super) zones: Vec<Zone>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ZoneState {
    /// Empty — WP = start, never written
    Empty,
    /// Implicit Open — 写后未 explicit close/finish
    ImplicitOpen,
    /// Explicit Open — 通过 Zone Mgmt Send Open 转入
    ExplicitOpen,
    /// Closed — 写后被 close，但未 finish；后续可重新 open
    Closed,
    /// Full — WP == zone_capacity，不可再 Write
    Full,
    /// Read Only / Offline — Zone Mgmt Send Reset/Offline 触发
    /// (ReadOnly 留 future faulted-media simulation 路径)
    #[allow(dead_code)]
    ReadOnly,
    Offline,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Zone {
    /// Write Pointer，相对 zone 起点 (0..=zone_capacity)。Full 时 = capacity。
    pub(super) write_pointer: u64,
    pub(super) state: ZoneState,
}

impl Namespace {
    /// 一个 LBA 在 backing file 占多少字节（含 metadata，T10 PI 时
    /// = data + meta）。
    #[inline]
    pub(super) fn block_bytes(&self) -> u64 {
        (1u64 << self.lbads) + self.meta_size as u64
    }
    /// 仅数据部分字节数（不含 metadata）。
    #[inline]
    pub(super) fn data_bytes(&self) -> u64 {
        1u64 << self.lbads
    }
    /// metadata 字节数 per LBA（0 = 无 metadata）。
    #[inline]
    #[allow(dead_code)]
    pub(super) fn meta_bytes(&self) -> u64 {
        self.meta_size as u64
    }
    /// 当前是否启用 PI（pi_type != 0）。
    #[inline]
    pub(super) fn pi_enabled(&self) -> bool {
        self.pi_type != 0
    }

    /// **Reviewer H3 + Phase M2** — 位置无关的 read（不依赖 file cursor）。
    ///
    /// 当 mmap available 时走零拷贝（`memcpy from mmap slice`），节省一次
    /// kernel→user copy。否则 fallback `pread`/`seek_read`。
    ///
    /// 安全：mmap slice 与 file 后端一致，但 mmap 写入由 OS 异步 flush；
    /// FLUSH cmd 强制 `mmap.flush()` 保证持久化（spec § 5.10 Flush）。
    pub(super) fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        // Fast path：mmap available + 范围在内 → 直接 memcpy
        if let Some(mmap) = self.mmap.as_ref()
            && let Some(slice) = mmap.get(offset as usize..).and_then(|s| s.get(..buf.len()))
        {
            buf.copy_from_slice(slice);
            return Ok(());
        }
        // Fallback：positional file IO（Format 后未重建 mmap 等场景）
        self.file_read_at(buf, offset)
    }

    /// **Reviewer H3 + Phase M2** — 位置无关的 write，与 `read_at` 对称。
    ///
    /// mmap 路径：直接写入 mmap slice（OS dirty page，FLUSH 时 msync）。
    /// 注：必须用 `&mut self` 因为 mmap slice 是 `&mut [u8]`。
    pub(super) fn write_at(&mut self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        // Fast path
        if let Some(mmap) = self.mmap.as_mut()
            && let Some(slice) = mmap
                .get_mut(offset as usize..)
                .and_then(|s| s.get_mut(..buf.len()))
        {
            slice.copy_from_slice(buf);
            return Ok(());
        }
        // Fallback
        Self::file_write_at_static(&self.file, buf, offset)
    }

    /// File IO fallback for read（Phase M2 之前的实现）。
    fn file_read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_exact_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = self
                    .file
                    .seek_read(&mut buf[filled..], offset + filled as u64)?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "EOF before fill",
                    ));
                }
                filled += n;
            }
            Ok(())
        }
    }

    /// File IO fallback for write（static so 不冲突 mmap `&mut self`）。
    fn file_write_at_static(file: &File, buf: &[u8], offset: u64) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            file.write_all_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut written = 0usize;
            while written < buf.len() {
                let n = file.seek_write(&buf[written..], offset + written as u64)?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "wrote 0 bytes",
                    ));
                }
                written += n;
            }
            Ok(())
        }
    }

    /// **Phase M2** — Flush mmap 已 dirty 的页到 disk（用于 NVM FLUSH cmd）。
    /// `mmap.flush()` 内部对 Unix = `msync(MS_SYNC)`，Windows = `FlushViewOfFile`
    /// + `FlushFileBuffers`。失败时回退 `file.sync_all()`。
    pub(super) fn flush(&self) -> std::io::Result<()> {
        if self.force_flush_err {
            // D（test-only）：强制失败，验 shutdown CSTS.CFS 路径。
            return Err(std::io::Error::other("forced flush error (test)"));
        }
        if let Some(mmap) = self.mmap.as_ref() {
            return mmap.flush();
        }
        self.file.sync_all()
    }
}

/// **Phase M2** — 尝试 mmap 整个 backing file。失败（如 /tmp 不支持 mmap、
/// 文件被独占等）返 None，调用方走 file IO 回退。
///
/// # Safety
///
/// `MmapMut::map_mut(file)` 是 `unsafe`：要求 file 在 mmap 生命周期内不能
/// 被任何路径同步 write 或 truncate（page contents 可能不一致 / SIGBUS）。
/// 教学示例：
/// - **外部进程** — backing file 只由本 controller 进程持有；
/// - **内部** — NvmeController 单线程访问；任何**改变 file size** 的路径
///   (Format SES=1/2 set_len) **必须先 `ns.mmap = None`** 释放映射，
///   完成 truncate 后再 try_mmap_file 重建。Reviewer C-2 修复后此契约
///   在 admin.rs::Format NVM 路径已 enforce。
pub(super) fn try_mmap_file(file: &File) -> Option<memmap2::MmapMut> {
    // SAFETY: backing file 仅由本进程持有；NvmeController 单线程访问；
    // 改 size 的路径已先 drop mmap。符合 memmap2::MmapMut::map_mut 安全前置。
    let mmap = unsafe { memmap2::MmapMut::map_mut(file) };
    match mmap {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::warn!(error = %e, "Phase M2: mmap failed, falling back to file IO");
            None
        }
    }
}

/// NVMe Controller 主结构 — 实现 `PcieDevice`。
pub struct NvmeController {
    // ----- Backing namespaces (NSID -> NS state) -----
    /// NSID → Namespace。spec § 1.6：NSID 0 reserved (controller broadcast)，
    /// NSID 1..N 数据 namespace，NSID 0xFFFF_FFFF = broadcast。本实现单
    /// controller，NS 从 1 起编号。Phase H4 之前只支持 NSID=1。
    pub(super) namespaces: DenseMap<u32, Namespace>,

    // ----- Controller registers -----
    cap: u64,
    vs: u32,
    intms: u32,
    intmc: u32,
    cc: u32,
    csts: u32,
    aqa: u32,
    asq: u64,
    acq: u64,
    /// **Phase Q5** — Boot Partition Read Select (BPRSEL) RW，driver 写
    /// 进来选要读的 boot partition + 起始 offset，controller 把对应 boot
    /// image 内容拷到 BPMBL 指向的 guest memory。
    pub(super) bprsel: u32,
    /// **Phase Q5** — Boot Partition Memory Buffer Location (BPMBL)
    /// 64-bit RW，driver 提供的 guest memory 缓冲区地址（4 KiB 对齐）。
    /// controller 把 boot image 用 DMA-write 到这里。
    pub(super) bpmbl: u64,

    state: CtrlState,

    // ----- Queues -----
    /// SQ ID → queue。Admin = ID 0；IO = ID 1..=IO_QUEUE_SLOT_CAPACITY。Vec 直接索引。
    sqs: DenseMap<u16, SubmissionQueue>,
    /// CQ ID → queue。
    cqs: DenseMap<u16, CompletionQueue>,

    // ----- DMA tracking -----
    /// fetch_sqe token → SQ id + slot index（host 侧已确认这批 entries 要 fetch）。
    pending_fetches: HashMap<u64, FetchCtx>,
    /// pending IO commands waiting on PRP DMA。token → PendingIo。
    pending_ios: HashMap<u64, PendingIo>,
    /// 双 PRP Write 累积：op_id（单调分配，独立于 DMA token）→ 两段 PRP buffer
    /// + cmd 上下文。两段都到达时合并写盘 + 发 CQE。
    dual_prp_writes: HashMap<u64, WriteAccum>,
    /// 双 PRP Write 的 op_id 计数器。
    next_op_id: u64,
    /// **Phase E** — PRP-list IO 累积（Write/Read > 2 page）。
    pub(super) prp_list_ops: HashMap<u64, PrpListOp>,
    /// **Phase R2** — SGL Segment IO 累积（PSDT=10，scatter-gather）。
    pub(super) sgl_ops: HashMap<u64, SglOp>,
    /// **Phase K2** — Compare > 1 page 累积（dual PRP / PRP list）。
    pub(super) compare_ops: HashMap<u64, CompareAccum>,
    /// **Phase K4c** — 多 LBA PI Write 累积。op_id → 全 data + 完成进度。
    pub(super) pi_writes: HashMap<u64, PiWriteAccum>,
    /// **Phase K4c-list** — 多 LBA PI Read PRP-list 累积器。op_id → 已
    /// verified data + DMA-write to host 进度。
    pub(super) pi_reads: HashMap<u64, PiReadAccum>,
    /// **B6b-2（separate metadata，PRACT=0）** — separate-buffer PI Write 累积器。
    /// op_id → 等 data(PRP) + meta(MPTR) 两条 DMA 到齐 → verify host PI → 存盘。
    pub(super) sep_meta_writes: HashMap<u64, SepMetaWriteAccum>,
    /// **B6b-3** — separate-buffer PI Read 累积器（等 data+meta 两条 DMA-write 完成）。
    pub(super) sep_meta_reads: HashMap<u64, SepMetaReadAccum>,
    /// **Phase O2** — Fused operation state：per-SQ 缓存 FUSE_FIRST 的
    /// SQE，等待紧接其后的 FUSE_SECOND。spec § 6.2 要求：
    /// (a) 两条必须连续在同一 SQ；(b) 都 fused-marked；(c) 都同 nsid。
    /// 不满足 → 两条都 abort 返 INVALID_FIELD。
    /// 当前只支持 Fused Compare-and-Write (spec 唯一定义的 fused pair)。
    pub(super) pending_fused: HashMap<u16, (Sqe, u16)>, // SQ ID → (first SQE, sq_head)
    /// 待 dispatch 的 SQE 队列（按 FIFO 顺序），dispatch 是 sync 逻辑但
    /// 触发 DMA 后异步完成。
    sqe_inbox: Vec<(u16, u16, Sqe)>, // (sq_id, sq_head_after_fetch, sqe)

    // ----- Phase F: SMART 统计 -----
    /// 累计完成的 host 读命令数。
    pub(super) stat_host_reads: u64,
    /// 累计完成的 host 写命令数。
    pub(super) stat_host_writes: u64,
    /// 累计读 LBA 数（512B 单位）。SMART log data_units_read 单位是
    /// 1000 × 512B sectors，发送时做除法换算。
    pub(super) stat_lba_read: u64,
    /// 累计写 LBA 数。
    pub(super) stat_lba_written: u64,
    /// Power-on 时刻（构造时记一次），SMART log 用作 power_on_hours。
    pub(super) power_on_instant: std::time::Instant,
    /// 累计错误命令数（CQE 携 non-zero status code 即计数）。
    pub(super) stat_num_err_log_entries: u64,

    // ----- Phase F / V8c: AEN (Async Event Notification) per-conn -----
    /// AsyncEventRequest 已收到、待 fire 的 admin context FIFO。
    /// 当有事件触发时弹一条，构造 CQE 带 event info → post 到 admin CQ。
    /// 元组：(cid, sq_id, cq_id, conn_id)；sq_head 不存（fire 时取实时值，
    /// 避免 stale sqhd 触发 spec § 4.6.1.4 单调违规 — Phase F H2 修复）。
    ///
    /// **Phase V8c (security-reviewer H-1)** — `conn_id` 字段把"AER 是 controller
    /// 全局 FIFO"改为 per-conn 路由：session A AER 投到 conn A，conn B 不会
    /// 误吃。0 = legacy 未关联（V6 路径，V8c 之前的测试 fixture）；非 0 =
    /// session 通过 `accept_and_handshake_shared` 分配的 `conn_id`。
    /// **Phase V8c** — `nvme_admin_dispatch_with_conn` 调用期间临时记录当前
    /// dispatch 的 `conn_id`，让 admin handler 内部 `aen_pending.push_back`
    /// 自动 stamp 正确 conn_id。**V8c reviewer H-1** — `_with_conn` 用
    /// prev/restore 而非清 0：支持嵌套调用 + 防 BC wrapper 静默覆盖。
    /// 0 = legacy (V6 路径 / 未带 conn_id)。
    pub(super) current_dispatch_conn_id: u32,
    pub(super) aen_pending: std::collections::VecDeque<(u16, u16, u16, u32)>,

    /// **Phase V7** — Discovery Log Page entries (spec § 5.16.1.20 / § 5.1.4)。
    /// session bin 启动时通过 `nvme_set_discovery_target` 注入；Discovery
    /// Get Log 0x70 直接序列化此 vec。空 vec = controller 不在 Discovery
    /// mode（admin Get Log 0x70 仍走默认 zeros 路径）。
    pub(super) discovery_portals: Vec<discovery_log::DiscoveryPortal>,
    /// **Phase V7** — Discovery generation counter；portals 集合每改一次
    /// 该 caller 应 ++（V7 教学版静态注入仅 init=0；TP4126 动态 registry V-followup）。
    pub(super) discovery_gen_ctr: u64,

    /// **Phase H1** — Set Features 写过的 cdw11 值，Get Features 时回填。
    /// fid → cdw11。NVMe spec § 5.21.1：driver 通过 Set 配置 controller
    /// 行为，必须能 Get 回。少量 fid (0x07 NumberOfQueues / 0x06 VWC) 由
    /// controller 强约束返实际值（不简单回填 stored）；其余 fid 走 stored
    /// 路径。Set 后立即生效（spec 'persistent across reset' bit 默认 0，
    /// 所以 reset 时清掉）。
    pub(super) features: std::collections::HashMap<u8, u32>,
    /// **D（persistent features，spec § 5.21.1 'Save' bit）** — Set Features 带
    /// SV=1（cdw10 bit 31）的 FID 值持久化到此 map：**跨 controller reset 保留**
    /// （disable 不清），enable 时回灌为 current；Get Features SEL=2(saved) 读它。
    /// 此前 SV 被忽略、features 跨 reset 全丢（教学边界，D 重审修正）。
    /// 注：教学版仅"跨 reset"持久（进程内），非真跨 power-cycle 写盘。
    pub(super) saved_features: std::collections::HashMap<u8, u32>,
    /// **Phase H2** — Driver 通过 Set Features 0x07 请求的 IO queue 数；
    /// controller 在 enable() 时实际授予 `min(requested, io_queue_pairs)` 个 SQ/CQ。
    /// 默认 = 运行时上限 [`Self::io_queue_pairs`]；请求大于上限被限制到上限。
    pub(super) granted_io_queues: u16,
    /// **2026-06-09 运行时模拟上限** — 本次运行愿意授予的 IO queue 对数上限
    /// （Set Features Number of Queues grant 封顶；Create IO Queue qid gate）。
    /// ∈ [1, [`IO_QUEUE_SLOT_CAPACITY`]]，默认 = 上限。模拟"N-queue 设备"。
    pub(super) io_queue_pairs: u16,
    /// **2026-06-09 运行时模拟上限** — 本次运行模拟的 namespace 容量上限
    /// （≤ [`NAMESPACE_SLOT_CAPACITY`]）。加载 NS 数（= NN = MNAN，Linux 要求
    /// MNAN≤NN）不得超过它。模拟"N-NS 设备"。
    pub(super) max_namespaces: u32,
    /// **Phase G** — 上次 AEN 触发时观测到的 stat_num_err_log_entries
    /// 快照；tick 中比较新值 → 自动 fire AEN type 0x00 Error。
    pub(super) aen_last_err_count: u64,

    // ----- Phase G: Device Self-Test 状态机 -----
    /// 正在进行的自检（None = idle）。NVMe spec § 5.11 + § 5.16.1.6。
    pub(super) self_test_in_progress: Option<SelfTestInProgress>,
    /// 最近一次完成（或被 abort）的自检结果快照。Log Page 0x06 读它。
    pub(super) self_test_last: Option<SelfTestCompleted>,

    // ----- Phase G: Error Information Log entry 真累积 -----
    /// 最近 64 条 error log entry（环形 FIFO，spec ELPE=63 → 64 entry）。
    /// 真追踪 fail 命令的 cid / sq_id / status_field / LBA。Get Log Page
    /// 0x01 时填入。
    pub(super) error_log: std::collections::VecDeque<ErrorLogEntry>,

    // ----- Phase H5: Firmware download / commit / activate 状态机 -----
    /// FW image 累积 buffer（FW Image Download cmd 分块写入）。
    /// 每个 Download cmd 带 cdw10=NUMD（4 KiB units，0-based）+
    /// cdw11=OFFSET（4 KiB units），写到 self.fw_download_buf[OFFSET..]。
    pub(super) fw_download_buf: Vec<u8>,
    /// 当前活跃 FW slot (1..7)。Commit 后切换。Identify Controller
    /// .frmw + Get Log Page 0x03 AFI 反映此值。
    pub(super) fw_active_slot: u8,
    /// 每个 slot 的 FW revision string（8 ASCII chars，spec § 5.16.1.3）。
    /// slot 0 未使用，1..7 真值。空字符串表示该 slot 未填。
    pub(super) fw_slot_revisions: [String; 8],
    /// 待下次启动激活的 slot（Commit Action=2 设置）；0 = 立即激活。
    pub(super) fw_next_active_slot: u8,

    // ----- Phase K5: Sanitize 状态机 -----
    /// 当前 Sanitize 进度（None = idle / 完成；Some = 进行中）。
    /// NVMe spec § 5.26。简化：用 tick 推进 percent_complete；完成时
    /// fire AEN type=0x02 info=0x05 'Sanitize Operation Completed'。
    pub(super) sanitize: Option<SanitizeState>,
    /// 最近一次 Sanitize 完成的 Sanitize Status Log 0x81 数据。
    pub(super) sanitize_last_status: u8, // 0=never, 1=success, 2=in-progress, 3=failed

    // ----- Phase K6 / DBBUF (shadow doorbells, spec § 5.7 + § 7.13) -----
    /// driver 提供的 shadow doorbell buffer GPA（PRP1）+ event idx buffer
    /// (PRP2)。非 0 表示 DBBUF **active** —— controller 不再信任 MMIO doorbell
    /// 的 value（可能 stale），而是 DMA-poll shadow buffer 拿真 tail/head，并写回
    /// event_idx 告诉 driver 何时该 ring 真 doorbell（Linux `nvme_dbbuf_need_event`）。
    /// 二者全 0 = DBBUF inactive，走纯 MMIO doorbell 路径（admin qid 0 永远走此路径，
    /// 因 Linux `nvme_dbbuf_init` 跳过 qid 0）。
    pub(super) doorbell_shadow_gpa: u64,
    pub(super) doorbell_event_idx_gpa: u64,
    /// in-flight shadow-poll DMA reads：token → 该 read 在轮询哪个队列。完成回调
    /// (`on_dma_complete`) 据此路由进 `handle_shadow_poll_complete`。
    pub(super) pending_shadow_polls: std::collections::HashMap<u64, ShadowPollCtx>,
    /// in-flight event_idx 写回的 token 集合；完成回调静默消费（无后续动作），
    /// 不让它们落到 unknown-token 警告路径。
    pub(super) pending_eventidx_writes: std::collections::HashSet<u64>,
    /// 当前有 poll 链在飞的 (qid, is_cq)。doorbell ring 仅是"唤醒"信号：若已有链
    /// 在 drain，它下一次 re-read 会读到最新 shadow，故跳过启动重复链（也避免重复
    /// fetch 同一批 SQE）。链终止时清除。
    pub(super) shadow_poll_inflight: std::collections::HashSet<(u16, bool)>,
    /// **HIGH-1（async transport 漏 ring 修复）** — "链在飞期间又来了一次 ring" 的
    /// 一次性待处理标记，按 (qid, is_cq) 记。
    ///
    /// 背景：OpenHCL 真异步 transport 下，doorbell-ring 帧（`start_shadow_*_poll`）
    /// 与 re-read 的 DmaCompletion 帧由同一 run-loop **逐条** dispatch，可能交错。
    /// 旧逻辑里 `start_shadow_*_poll` 见 inflight 即静默 return —— 若该 ring 恰落在
    /// "host 已服务 re-read 但 driver 尚未存 shadow" 的窗口内，这次唤醒被**丢弃**，
    /// 对应槽位要等下一次 ~5s tick 兜底才 fetch（延迟失速，非丢命令）。
    ///
    /// 修复：`start_shadow_*_poll` 发现链在飞时**置此标记**（而非静默 return）。链在
    /// settle 分支（`shadow == processed`）清 inflight 前先查：若标记置位 → 清标记并
    /// **再发一次 re-read**（续链）而非 settle。如此每个被丢的 ring 都转成"再 poll 一
    /// 次"，settle 窗口内的提交必被捕获。一次性语义：续链前先 `remove` 标记，故除非
    /// **新** ring 再次置位否则不会重复触发，不会无限续链。`disable()` 一并清除。
    pub(super) shadow_ring_pending: std::collections::HashSet<(u16, bool)>,
    /// 每个 IO SQ 最近一次**真 MMIO** doorbell 写入的原始 value（仅用于证明日志：
    /// 当 shadow tail 领先于此值，说明 driver 跳过了一次真 ring 而 controller 经
    /// shadow 追上 —— 即 DBBUF 被真正行使）。
    pub(super) last_mmio_sq_doorbell: std::collections::HashMap<u16, u32>,
    /// 每个 IO SQ 最近写回 driver 的 SQ event_idx 值；避免冗余写 + 防 poll 链
    /// 在 `shadow == processed` 分支无谓重写而打转。
    pub(super) last_eventidx_sq: std::collections::HashMap<u16, u32>,
    /// shadow tail 被发现领先于"最近真 MMIO doorbell"的累计次数。`> 0` 即证明
    /// Linux driver 真的跳过了 ring 而 controller 经 shadow 捕获到（DBBUF 真行使，
    /// 非静默退回 MMIO）。harness server.log + 单测据此断言。
    pub(super) dbbuf_shadow_ahead_count: u64,
    /// **Phase Q7** — Lockdown 命令禁用的 admin opcode 集合。
    /// dispatch_admin 进入前查；命中则返 COMMAND_PROHIBITED_BY_LOCKDOWN。
    pub(super) locked_admin_opcodes: std::collections::HashSet<u8>,
    /// **Phase Q8** — Cryptographic Erase generation counter。Format SES=2
    /// 时 +1，模拟 NS 加密 key 销毁。教学版没真加密；driver 通过 SMART
    /// log / FW 状态读 generation 变化感知 erase 发生。
    pub(super) crypto_gen: u32,

    /// **Phase S6** — Reservation Notification Log (Log Page 0x80, spec §
    /// 5.16.1.20)。每条 record：log page count (u64) + log_page_type (u8) +
    /// available_log_pages (u8) + reserved + nsid (u32) + reserved 48 byte
    /// = 64 byte。我们 keep 末 32 条作 ring buffer。
    /// 资源 release / preemption / regstration preempted 事件 push 一条。
    pub(super) reservation_notification_log:
        std::collections::VecDeque<crate::controller::ReservationNotification>,
    /// 自启动累积的 reservation notification 总数（也写入 log page count 字段）。
    pub(super) reservation_notification_count: u64,

    /// **Phase S7** — Asymmetric Namespace Access (ANA) 全 controller 单
    /// ANA group (groupid=1)，当前 state 字段。spec § 8.1 取值：
    ///   0x01 Optimized / 0x02 Non-Optimized / 0x03 Inaccessible /
    ///   0x04 Persistent Loss / 0x0F Change（瞬态）。默认 0x01。
    /// Log Page 0x0C ANA + change_count 都从这里读。
    pub(super) ana_state: u8,
    /// ANA log change count（spec § 5.16.1.13 'Change Count' 字段）。
    /// state 切换时 +1；driver poll Log 0x0C 时按此判定 stale。
    pub(super) ana_change_count: u64,

    // ----- Phase K8: Power States -----
    /// 当前 power state index (0..31)。Set Features 0x02 Power Management
    /// 修改；Identify Controller .psd[N] 描述每个 state（spec § 5.17.2.2）。
    pub(super) current_ps: u8,

    // ----- Phase M1: Interrupt Coalescing (spec § 5.21.1.8) -----
    /// AGGR_TIME (8 bit, 100 us units) — 中断聚合最大延迟。0 = no coalesce。
    pub(super) irq_aggr_time: u8,
    /// AGGR_THR (8 bit) — Aggregation Threshold (0-based, 实际 = 值 + 1)。
    pub(super) irq_aggr_threshold: u8,

    // ----- Phase K9: Host Identifier (spec § 5.21.1.27) -----
    /// driver 通过 Set Features 0x81 提供的 host ID。EXHID=0 → 8 byte，
    /// 高 64-bit = 0；EXHID=1 → 完整 16 byte。Get Features 0x81 回这两个。
    /// 新 Register 时若设置过则填到 Namespace.registrants[].hid_lo/hi。
    pub(super) host_id_lo: u64,
    pub(super) host_id_hi: u64,

    // ----- 配置 -----
    vid: u16,
    ssvid: u16,
    msix_count: u16,
}

struct FetchCtx {
    sq_id: u16,
    /// SQE 数量。
    count: u32,
    /// 起始 slot index（在 SQ 中）。
    start_slot: u32,
}

/// DBBUF shadow-poll 的 in-flight 上下文：一次 `dma_read(shadow doorbell, 4)` 在等
/// 完成时记录"它在轮询哪个队列、本链已自续到多深"。完成回调据 token 取回本结构后
/// 驱动 race-safe 轮询循环（见 `handle_shadow_poll_complete`）。
#[derive(Debug, Clone, Copy)]
pub(super) struct ShadowPollCtx {
    /// 被轮询的队列 id（IO 队列；qid 0 admin 不参与 DBBUF）。
    pub(super) qid: u16,
    /// false = SQ tail doorbell；true = CQ head doorbell。
    pub(super) is_cq: bool,
    /// **HIGH-2（host-thread liveness）** — 本轮询链的**自续深度**：本链至今发出的
    /// shadow re-read（自我续接）总数。链起始（`start_shadow_*_poll`）的首读为 0；之后
    /// 每发一次自续 re-read（SQ advance 分支 re-read **或** HIGH-1 settle-续读）就 +1，
    /// **唯一**在链起始处重置 0。撞 `MAX_SHADOW_POLL_ITERS` → 置 CSTS.CFS 并收链。
    /// 这是**深度上限**而非环距离启发——见 `MAX_SHADOW_POLL_ITERS` 文档。
    pub(super) iters: u32,
}

/// **Phase H6** — Reservation 命令类别（分流完成回调）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReservationKind {
    Register,
    Acquire,
    Release,
}

/// **Phase G** — Device Self-Test 状态机（NVMe spec § 5.11 + § 5.16.1.6）。
///
/// **设计**：把"进行中"和"最后一次结果"分开，避免 G reviewer 发现的
/// 双 CRITICAL bug：
/// 1. 老设计 tick 完成后 `self_test = Some { stc:0, pct:100 }` 每秒都
///    满足 `elapsed >= total`，每秒重 fire AEN。
/// 2. abort 把 in-progress.last_result 设 0x09 但 tick 在下一秒会按
///    "完成"分支覆写 last_result = 0。
///
/// 新设计：
/// - `in_progress` 仅当真有自检跑时 Some；完成或 abort 立即 None。
/// - `last_completed` 记上一次结果快照（含 STC / result code / POH 当时
///   时刻）；tick 完成时一次性 transition + fire AEN。
///
/// 真硬件自检会跑分钟/小时级；本教学实现压成秒级（short = 5 s，
/// extended = 20 s）。
#[derive(Debug, Clone)]
pub(super) struct SelfTestInProgress {
    pub(super) started_at: std::time::Instant,
    /// Spec § 5.11 CDW10 bits 3:0 STC：1 = short，2 = extended。
    pub(super) stc: u8,
    pub(super) total_seconds: u32,
    pub(super) percent_complete: u8,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct SelfTestCompleted {
    /// 完成时的 STC (1=short, 2=extended)。
    pub(super) stc: u8,
    /// Spec § 5.16.1.6 Self-Test Result：0=ok, 0x09=aborted, …
    pub(super) result: u8,
    /// 完成时的 power-on-hours 快照（spec 要求 run-time POH，
    /// 不是 "读 log 此刻" POH）。
    pub(super) completed_at_poh: u64,
}

/// **Phase K5** — Sanitize 进行中状态（spec § 5.26）。
///
/// Sanitize Action (sanact)：
///   1 = Exit Failure mode
///   2 = Block Erase
///   3 = Overwrite
///   4 = Crypto Erase
/// 我们简化：所有 sanact ≥ 2 触发"擦写"（backing file truncate-then-zero）。
/// 教学时长压缩到 3 秒；真硬件分钟到小时级。
#[derive(Debug, Clone)]
pub(super) struct SanitizeState {
    pub(super) started_at: std::time::Instant,
    pub(super) sanact: u8,
    pub(super) total_seconds: u32,
    pub(super) percent_complete: u16,
}

/// **Phase G** — Error Information Log entry（NVMe spec § 5.16.1.1，64 字节）。
///
/// 真追踪 fail 命令 metadata，供 Get Log Page 0x01 序列化。当 CQE 发出
/// status code != 0 时 push 一条到 controller.error_log（环形 64 entry）。
#[derive(Debug, Clone, Copy)]
pub(super) struct ErrorLogEntry {
    /// 单调 error_count（spec offset 0..8）。重启后从 0 起。
    pub(super) error_count: u64,
    /// SQ id (spec offset 8..10)。
    pub(super) sq_id: u16,
    /// Command ID (spec offset 10..12)。
    pub(super) cid: u16,
    /// Status Field（spec offset 12..14）— 复用 CQE 的 SF 位 (bits 15:1)。
    pub(super) status_field: u16,
    /// Parameter Error Location (spec offset 14..16)。我们不细化 → 0。
    pub(super) param_loc: u16,
    /// LBA (spec offset 16..24)。NVM IO 失败时填 SLBA；admin 失败 = 0。
    pub(super) lba: u64,
    /// Namespace ID (spec offset 24..28)。
    pub(super) nsid: u32,
}

/// **Phase M1b** — Interrupt Coalescing 决策（spec § 5.21.1.8）。
///
/// 返回 `true` 表示本次 CQE 应立即 fire MSI-X；`false` 表示先 batch，等
/// 后续 CQE 凑齐 threshold 或 tick 路径检测到 AGGR_TIME 超时再 fire。
///
/// 抽成纯函数便于 unit test（避免 mock DeviceCtx）。
///
/// 规则（按 spec AGGR_THR/AGGR_TIME 0's-based 语义）：
/// 1. Admin CQ (id=0) 永远立即 fire — spec 要求 admin 延迟最小
/// 2. `pending >= AGGR_THR + 1`（0's-based：thr=0 → 阈值=1，立即；thr=3 → 4 触发）
///
/// AGGR_TIME 在 batch path 由 tick 单独处理（time flush）。
pub(crate) fn should_fire_irq(
    cq_id: u16,
    pending: u32,
    aggr_thr: u8,
    _aggr_time_100us: u8,
) -> bool {
    if cq_id == 0 {
        return true;
    }
    pending >= (aggr_thr as u32 + 1)
}

impl NvmeController {
    /// **Phase V2** — NVMe-oF property white-list（CAP/VS/CC/CSTS/NSSR）。
    /// Fabric spec § 3 限制 Property Get/Set 只能访问 BAR0 偏移 0x00/0x08/
    /// 0x14/0x1C/0x20；其它 reg（AQA/ASQ/ACQ/doorbell）由 Connect 命令携带
    /// 参数替代，不能通过 Property 路径访问。
    fn is_valid_property_offset(ofst: u32) -> bool {
        matches!(ofst, 0x00 | 0x08 | 0x14 | 0x1C | 0x20)
    }

    /// **Phase V2** — Fabric Property Get：narrow wrapper 调 mmio_read_impl
    /// 仅允许 NVMe-oF spec 白名单 offset，避免 nvme_of_tcp_target 等 caller
    /// 误读 doorbell 等不该跨 fabric 暴露的 reg。`size` ∈ {4, 8}。
    pub fn nvme_property_get(&mut self, ofst: u32, size: u32) -> Option<u64> {
        if !Self::is_valid_property_offset(ofst) || !matches!(size, 4 | 8) {
            return None;
        }
        Some(self.mmio_read_impl(0, ofst as u64, size))
    }

    /// **Phase V2** — Fabric Property Set 同窄 wrapper。
    pub fn nvme_property_set(
        &mut self,
        ctx: &mut pcie_device_core::DeviceCtx<'_>,
        ofst: u32,
        size: u32,
        value: u64,
    ) -> bool {
        if !Self::is_valid_property_offset(ofst) || !matches!(size, 4 | 8) {
            return false;
        }
        self.mmio_write_impl(ctx, 0, ofst as u64, size, value);
        true
    }

    /// **Phase V3** — NVMe-oF TCP target 用：直接派发一条 admin SQE 到
    /// controller 的 dispatch_admin，不经 PCIe MMIO doorbell / ASQ 路径。
    ///
    /// 调用方需自己提供一个已 set 好的 admin CQ (id=0)，让 controller
    /// `post_cqe` 写入；返 `Some(Cqe)` 表示 controller 同步完成（cmd 如
    /// Set Features），caller 应自行决定怎么把它编码成 CapsuleResp。
    /// 返 `None` 表示异步：controller 已 `ctx.dma_write` 数据并把 token 入
    /// pending_ios；caller 需调 `nvme_admin_complete_dma` 让 controller
    /// 走 post_cqe 路径。
    pub fn nvme_admin_dispatch(
        &mut self,
        ctx: &mut pcie_device_core::DeviceCtx<'_>,
        sqe: crate::cmd::Sqe,
        cid: u16,
        cq_id: u16,
    ) -> Option<crate::cmd::Cqe> {
        // V8c BC：未带 conn_id 等价 conn_id=0（legacy 单 conn 路径）。
        self.nvme_admin_dispatch_with_conn(ctx, sqe, cid, cq_id, 0)
    }

    /// **Phase V8c (reviewer H-1)** — 带 `conn_id` 的 admin dispatch。dispatch
    /// 期间触发的 `aen_pending.push_back` 会带上本 conn_id；让
    /// `nvme_fire_aen_for_conn` 只 fire 该 conn 的 pending AER；conn drop 时
    /// `nvme_cleanup_conn_aers` 按 conn_id 抹掉所有 pending 防 leak。
    ///
    /// `current_dispatch_conn_id` 用 prev/restore 而非清 0，支持未来嵌套调用
    /// 也防 BC wrapper 静默覆盖 caller 已设值。
    pub fn nvme_admin_dispatch_with_conn(
        &mut self,
        ctx: &mut pcie_device_core::DeviceCtx<'_>,
        sqe: crate::cmd::Sqe,
        cid: u16,
        cq_id: u16,
        conn_id: u32,
    ) -> Option<crate::cmd::Cqe> {
        let prev = self.current_dispatch_conn_id;
        self.current_dispatch_conn_id = conn_id;
        let r = self.dispatch_admin(ctx, sqe, cid, /*sq_head*/ 0, cq_id);
        self.current_dispatch_conn_id = prev;
        r
    }

    /// **Phase V5a** — 与 [`nvme_admin_dispatch`] 同形的 IO 队列 dispatch
    /// 入口。session 在 IO CapsuleCmd 时调本 wrapper，传入 created IO SQ
    /// id (`sq_id`) 与对应 CQ id (`cq_id`)。返语义同 admin：`Some(Cqe)` =
    /// 同步完成；`None` = 异步（已 dma_read/dma_write，等 caller 投
    /// `nvme_admin_complete_dma`）。
    pub fn nvme_io_dispatch(
        &mut self,
        ctx: &mut pcie_device_core::DeviceCtx<'_>,
        sq_id: u16,
        sqe: crate::cmd::Sqe,
        cid: u16,
        cq_id: u16,
    ) -> Option<crate::cmd::Cqe> {
        self.dispatch_io(ctx, sq_id, sqe, cid, /*sq_head*/ 0, cq_id)
    }

    /// **2026-06-09 纯 4K** — 查指定 NSID 当前激活 LBA Format 的 `lbads`
    /// （扇区字节 = `1 << lbads`：9 = 512B、12 = 4 KiB）。`None` 表示 NSID
    /// 不存在。NVMe-oF TCP session 用它把合成 PRP 的页边界 / nlb 上限 /
    /// chunk 大小改成 per-NS 扇区感知（否则 512B 假设会在 4K NS 上撕裂
    /// dual-PRP 边界 → 数据 corruption）。Format 改 lbads 后下条 IO 即读到
    /// 新值（controller Format handler 已更新 ns.lbads）。
    pub fn ns_lbads(&self, nsid: u32) -> Option<u8> {
        self.namespaces.get(&nsid).map(|n| n.lbads)
    }

    /// **2026-06-09 fused C&W over fabric** — 原子 Compare-and-Write。
    ///
    /// 在**单次 `&mut self` 调用内**（无 await / 无锁释放）做 read backing →
    /// 比 `compare_data` → 相等才写 `write_data`，返 (Compare CQE, Write CQE)。
    /// fabric session 先经 R2T 把两个 host buffer 取齐再调本函数，故 read-compare
    /// -write 真原子（不像逐 DMA 捕获那样中途释放控制器锁让别的 conn 插队）。
    ///
    /// 自校验 spec § 6.2 fused 约束：FIRST=Compare(0x05)/SECOND=Write(0x01)、
    /// 两条 nsid/slba/nlb 对齐、plain NS、单 PRP（≤1 page）。**TOCTOU 守卫**：
    /// host buffer 长度必须正好 `nlb × (1<<lbads)`——若 `--allow-format` 下并发
    /// Format 在 session R2T 取数与本调用间改了 lbads，长度对不上即 abort（防
    /// 错扇区读写）。`sq_head` 写死 0（fabric 不用 doorbell head）。
    pub fn nvme_fused_cas(
        &mut self,
        sq_id: u16,
        cq_id: u16,
        compare_sqe: crate::cmd::Sqe,
        write_sqe: crate::cmd::Sqe,
        compare_data: &[u8],
        write_data: &[u8],
    ) -> (crate::cmd::Cqe, crate::cmd::Cqe) {
        use crate::cmd::Cqe;
        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
        let c_cid = compare_sqe.cid();
        let w_cid = write_sqe.cid();
        let both = |status: u16| {
            (
                Cqe::error(c_cid, sq_id, 0, phase, status),
                Cqe::error(w_cid, sq_id, 0, phase, status),
            )
        };

        // spec § 6.2：FIRST=Compare、SECOND=Write，且 nsid/slba/nlb 对齐。
        let nsid = compare_sqe.nsid;
        let slba = compare_sqe.cdw10 as u64 | ((compare_sqe.cdw11 as u64) << 32);
        let nlb = (compare_sqe.cdw12 & 0xffff) + 1;
        let w_slba = write_sqe.cdw10 as u64 | ((write_sqe.cdw11 as u64) << 32);
        let w_nlb = (write_sqe.cdw12 & 0xffff) + 1;
        if compare_sqe.opcode() != nvm_opc::COMPARE
            || write_sqe.opcode() != nvm_opc::WRITE
            || write_sqe.nsid != nsid
            || w_slba != slba
            || w_nlb != nlb
        {
            tracing::warn!(
                nsid,
                slba,
                "fused CAS: opcode/nsid/slba/nlb 不匹配 → INVALID_FIELD"
            );
            return both(sc::INVALID_FIELD);
        }

        // ── 全部 ns 操作收在一个借用作用域内，出来再更新 stats/建 CQE ──
        enum Outcome {
            Pass,
            Fail,
            Rej(u16),
            Io,
        }
        let outcome = if let Some(ns) = self.namespaces.get_mut(&nsid) {
            let is_plain =
                ns.meta_size == 0 && !ns.pi_enabled() && (ns.lbads == 9 || ns.lbads == 12);
            let sector = 1u64 << ns.lbads;
            let bytes = (nlb as u64 * sector) as usize;
            if !is_plain || bytes > NVME_PAGE_SIZE as usize {
                Outcome::Rej(sc::INVALID_FIELD)
            } else if compare_data.len() != bytes || write_data.len() != bytes {
                // TOCTOU：lbads 在 R2T 取数后被 Format 改 → 长度不符 → abort。
                tracing::warn!(
                    nsid,
                    expected = bytes,
                    got_c = compare_data.len(),
                    "fused CAS: host buffer 长度 != nlb×sector（Format 改了 lbads？）→ abort"
                );
                Outcome::Rej(sc::INVALID_FIELD)
            } else if slba
                .checked_add(nlb as u64)
                .is_none_or(|e| e > ns.total_lba)
            {
                Outcome::Rej(sc::LBA_OUT_OF_RANGE)
            } else {
                let mut backing = vec![0u8; bytes];
                match ns.read_at(&mut backing, slba * sector) {
                    Err(e) => {
                        tracing::warn!(error = %e, nsid, slba, "fused CAS: backing read fail");
                        Outcome::Io
                    }
                    Ok(()) => {
                        if compare_data == backing.as_slice() {
                            match ns.write_at(write_data, slba * sector) {
                                Ok(()) => Outcome::Pass,
                                Err(e) => {
                                    tracing::warn!(error = %e, nsid, slba, "fused CAS: write fail");
                                    Outcome::Io
                                }
                            }
                        } else {
                            Outcome::Fail
                        }
                    }
                }
            }
        } else {
            Outcome::Rej(sc::INVALID_NAMESPACE)
        };

        match outcome {
            Outcome::Pass => {
                self.stat_host_reads += 1;
                self.stat_lba_read += nlb as u64;
                self.stat_host_writes += 1;
                self.stat_lba_written += nlb as u64;
                tracing::info!(
                    nsid,
                    slba,
                    nlb,
                    "fused CAS: Compare PASS → Write committed (atomic)"
                );
                (
                    Cqe::success(c_cid, sq_id, 0, phase),
                    Cqe::success(w_cid, sq_id, 0, phase),
                )
            }
            Outcome::Fail => {
                self.stat_num_err_log_entries += 1;
                self.push_error_log(sq_id, c_cid, sc::sf_of(sc::COMPARE_FAILURE), slba, nsid);
                tracing::info!(
                    nsid,
                    slba,
                    "fused CAS: Compare FAIL → Write aborted (atomic)"
                );
                both(sc::COMPARE_FAILURE)
            }
            Outcome::Rej(status) => both(status),
            Outcome::Io => both(sc::DATA_TRANSFER_ERROR),
        }
    }

    /// **Phase V3** — 让 controller 处理一条 DMA 完成事件（caller 通常
    /// 是 V2Session 在 captured dma_write 全部 emit 完 C2HData 后，回调
    /// 一次 ok=true 触发 controller post_cqe）。
    pub fn nvme_admin_complete_dma(
        &mut self,
        ctx: &mut pcie_device_core::DeviceCtx<'_>,
        token: u64,
        ok: bool,
        data: Vec<u8>,
    ) {
        use pcie_device_core::PcieDevice as _;
        self.on_dma_complete(ctx, token, ok, data);
    }

    /// **Phase V3** — 给 V2Session 用：插入一个 admin CQ (cq_id=0) 让
    /// controller `post_cqe` 能写进去。`base_gpa` 是 caller 用来识别
    /// "这块 dma_write 是 CQE bytes" 的 sentinel；不真做 mmap。
    ///
    /// **review M3** — 若 cq_id=0 已存在则覆盖前 log warn（reconnect
    /// 场景常见，session 应该用 fresh controller，覆盖通常无害但值得提示）。
    ///
    /// **V3-polish (review L-3)** — warn 时打印 old/new base_gpa + qsize，
    /// 让维护者一眼看出"同 base 重装（无害）"vs"base 漂移（潜在 bug）"。
    /// **V8b** — multi-conn 共享 controller 后，第 2..N 条 conn 都会调
    /// `accept_and_handshake` → install admin CQ。修改为 idempotent：
    /// 若 cq_id=0 已存在且 `base_gpa` + `qsize` **同值**，silently no-op；
    /// 不同值返回 `Err(AdminCqInstallError::MismatchedParams)`（reviewer C-2）
    /// 让 caller hard-fail，不再静默拒覆盖 — 防止后续 V8 系列任何 const 改动
    /// 引入静默协议越权。
    ///
    /// **V8b reviewer M-1** — 同值判断仅看 `base_gpa` + `qsize`；其余 CQ
    /// 字段（`interrupt_vector` / `interrupt_enabled` / `tail` / `phase` /
    /// `head` / `pending_completions` / `last_fire`）由本函数固定 default
    /// 初始化，不属可变 input。caller（V2Session::accept_and_handshake_shared）
    /// 也固定传 `CQ_BASE_GPA` + `ADMIN_CQ_SIZE`，所以"same params"判断够用。
    /// 未来若暴露 `interrupt_vector` 等可变 input，需扩判断条件。
    pub fn nvme_install_admin_cq(
        &mut self,
        base_gpa: u64,
        qsize: u32,
    ) -> Result<(), AdminCqInstallError> {
        if let Some(old) = self.cqs.get(&0) {
            if old.base_gpa == base_gpa && old.size == qsize {
                // V8b idempotent fast-path — 同值重 install 不 warn 不动
                return Ok(());
            }
            return Err(AdminCqInstallError::MismatchedParams {
                old_base_gpa: old.base_gpa,
                old_size: old.size,
                new_base_gpa: base_gpa,
                new_size: qsize,
            });
        }
        let cq = crate::regs::CompletionQueue {
            base_gpa,
            size: qsize,
            tail: 0,
            phase: 1,
            head: 0,
            interrupt_vector: 0,
            interrupt_enabled: false,
            pending_completions: 0,
            last_fire: None,
        };
        self.cqs.insert(0, cq);
        Ok(())
    }

    /// **V-followup-interop-1** — 强制 (重新) install admin CQ；为 NVMe-oF
    /// fabric session 在 CC.EN 0→1 后 controller `enable()` 会把 cqs[0]
    /// 重置为 `self.acq = 0` (host fabric 路径不写 ACQ register)。session 端
    /// 通过本 API 把 cqs[0] base_gpa 拍回 CQ_BASE_GPA sentinel。
    ///
    /// 与 [`Self::nvme_install_admin_cq`] 区别：本函数永远成功并覆盖，不返
    /// `MismatchedParams`。
    pub fn nvme_force_install_admin_cq(&mut self, base_gpa: u64, qsize: u32) {
        let cq = crate::regs::CompletionQueue {
            base_gpa,
            size: qsize,
            tail: 0,
            phase: 1,
            head: 0,
            interrupt_vector: 0,
            interrupt_enabled: false,
            pending_completions: 0,
            last_fire: None,
        };
        self.cqs.insert(0, cq);
    }

    /// **V-followup-interop-2** — NVMe-oF fabric IO queue 自动 install。
    ///
    /// NVMe-oF spec § 3.6 IO queue 由 Fabric Connect (qid≥1) 创建，**不**
    /// 走 PCIe-only 的 admin `Create IO CQ` + `Create IO SQ` 双 cmd 流程。
    /// session 端 `handle_connect_async` 见 qid≥1 调本 API 一次性 install
    /// 该 qid 对应的 CQ + SQ；wire 端 IO cmd 接下来按 PCIe 同路径 dispatch。
    ///
    /// - `qid`: 队列 ID (≥ 1；admin 用 [`Self::nvme_force_install_admin_cq`])
    /// - `cq_base_gpa`: CQ sentinel GPA (session 端 `cq_sentinel(qid)`)
    /// - `qsize`: 队列槽数 (与 host 的 sqsize+1 等价；NVMe-oF Connect.sqsize
    ///   是 0-based)
    pub fn nvme_force_install_io_queue(&mut self, qid: u16, cq_base_gpa: u64, qsize: u32) {
        assert!(qid >= 1, "qid 0 是 admin，应用 nvme_force_install_admin_cq");
        let cq = crate::regs::CompletionQueue {
            base_gpa: cq_base_gpa,
            size: qsize,
            tail: 0,
            phase: 1,
            head: 0,
            interrupt_vector: 0,
            interrupt_enabled: false,
            pending_completions: 0,
            last_fire: None,
        };
        self.cqs.insert(qid, cq);
        let sq = crate::regs::SubmissionQueue {
            base_gpa: 0, // fabric 路径 SQ base 无意义 (cmd 直接走 PDU dispatch)
            size: qsize,
            head: 0,
            tail: 0,
            cq_id: qid, // fabric IO queue：SQ 与 CQ 同 qid (1:1 pairing)
        };
        self.sqs.insert(qid, sq);
    }

    /// **V8b** — 检查 admin CQ (cq_id=0) 是否已 install。
    /// session 端 multi-conn 共享时 cheap-check 避免重复 install。
    pub fn nvme_has_admin_cq(&self) -> bool {
        self.cqs.contains_key(&0)
    }

    /// **Phase V8b** — controller 端当前在用 / 留痕的最高 dma_read token。
    /// 多 conn 共享同一 controller 时，每条新 conn 的 `next_token` 起点不应
    /// 撞已注册 pending_ios 的 token（spec V8b plan §6 R-2 / reviewer M-3）。
    /// session 端在 `accept_and_handshake_shared` 起手用本 getter 计算新
    /// conn 的 token 起始（高水位 + 1）防 conn A 残留的 pending_ios 与
    /// conn B 起点撞 key。
    ///
    /// 返 0 = 当前 controller 无 pending dma_read（typical clean steady state）。
    /// 返非 0 = 当前 keys 最大值，caller 应 `+1` 作为新 conn 起点。
    pub fn nvme_token_high_water(&self) -> u64 {
        self.pending_ios.keys().max().copied().unwrap_or(0)
    }

    /// **Phase V3** — 让 caller 自己 post_cqe，用于把同步返的 `Some(Cqe)`
    /// 路径也走 controller 内部 post_cqe（统一异步/同步路径，让 CQ tail
    /// 推进与 wire-emit 一致）。
    ///
    /// **V3-polish (review L-2)** — 加 debug_assert 防 caller 漏调
    /// [`Self::nvme_install_admin_cq`]；漏调时 post_cqe 内部 warn no-op，
    /// session 后续 drain 找不到 CQE bytes → bail "did not produce CQE"
    /// 错位归因为 controller bug。tripwire 让 panic 直接命中 root cause。
    pub fn nvme_post_cqe(
        &mut self,
        ctx: &mut pcie_device_core::DeviceCtx<'_>,
        cqe: crate::cmd::Cqe,
    ) {
        debug_assert!(
            self.cqs.contains_key(&0),
            "nvme_post_cqe: caller must call nvme_install_admin_cq before dispatching admin cmds"
        );
        self.post_cqe(ctx, 0, cqe);
    }

    /// **Phase V6a** — controller 当前 pending AER 数量（host post 但未弹）。
    /// session 用来 cap 自己镜像的 pending_aers 容量 + AER 越界保护。
    /// read-only；不外泄 `aen_pending` 内部 VecDeque。
    pub fn nvme_pending_aer_count(&self) -> usize {
        self.aen_pending.len()
    }

    /// **Phase V8c** — 指定 `conn_id` 当前 pending AER 数。session 用以判断
    /// "是否该 sync_aer_mirror 对本 conn 做 truncate"。
    pub fn nvme_pending_aer_count_for_conn(&self, conn_id: u32) -> usize {
        self.aen_pending.iter().filter(|t| t.3 == conn_id).count()
    }

    /// **Phase V8c** — conn drop 时清理该 conn 在 controller 端残留的所有
    /// pending AER，防 leak / 让其它 conn 误 fire（H-1 + R-12）。返清理数。
    pub fn nvme_cleanup_conn_aers(&mut self, conn_id: u32) -> usize {
        let before = self.aen_pending.len();
        self.aen_pending.retain(|t| t.3 != conn_id);
        let cleaned = before - self.aen_pending.len();
        if cleaned > 0 {
            tracing::info!(conn_id, cleaned, "V8c cleanup_conn_aers");
        }
        cleaned
    }

    /// **Phase V8d** — 删除 IO SQ（qid ≥ 1）。spec § 7.6.1 ordering 要求先
    /// 删 SQ 后删 CQ；本函数仅删 SQ。返 true = 找到并删；false = qid 不存在
    /// 或是 admin (qid=0)，session 当 idempotent 处理。
    pub fn nvme_delete_io_sq(&mut self, qid: u16) -> bool {
        if qid == 0 {
            return false;
        }
        let removed = self.sqs.remove(&qid).is_some();
        if removed {
            tracing::debug!(qid, "V8d nvme_delete_io_sq");
        }
        removed
    }

    /// **Phase V8d** — 删除 IO CQ（qid ≥ 1）。caller 必须先 `nvme_delete_io_sq`
    /// 同 qid 防 spec § 7.6.1 ordering 违规（删了正在用的 CQ 让 SQ 悬挂）。
    pub fn nvme_delete_io_cq(&mut self, qid: u16) -> bool {
        if qid == 0 {
            return false;
        }
        let removed = self.cqs.remove(&qid).is_some();
        if removed {
            tracing::debug!(qid, "V8d nvme_delete_io_cq");
        }
        removed
    }

    /// **Phase V8d** — 列出所有 IO SQ id（qid ≥ 1，过滤掉 admin 0）。
    /// session Drop sweep 用以拿到要清的 qid 列表，避免漏。
    pub fn nvme_list_io_sqs(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self.sqs.keys().filter(|&q| q != 0).collect();
        v.sort_unstable();
        v
    }

    /// **Phase V8d** — 同 [`Self::nvme_list_io_sqs`] 但列 CQ。
    pub fn nvme_list_io_cqs(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self.cqs.keys().filter(|&q| q != 0).collect();
        v.sort_unstable();
        v
    }

    /// **Phase V6b** — 外部强制 fire 一条 AEN；返 true 表示 AER 已 fire
    /// （驱动 `aen_pending.pop_front` + post_cqe 已发 16B CQE 到 ctx.dma_write
    /// 哨值地址，caller 必须用 [`pcie_device_core::DeviceCtx`] 接
    /// 着上 `TcpAdminTransport` capture 那条 CQE write）。
    /// false 表示无 pending AER 可弹（事件按 spec drop）。
    ///
    /// `aen_type` < 8 (spec § 5.2 Figure 174 bits 2:0)。
    pub fn nvme_fire_aen(
        &mut self,
        ctx: &mut pcie_device_core::DeviceCtx<'_>,
        aen_type: u8,
        aen_info: u8,
        log_id: u8,
    ) -> bool {
        self.fire_aen(ctx, aen_type, aen_info, log_id)
    }

    /// **Phase V8c** — fire 指定 `conn_id` 的最早 pending AER。其它 conn 的
    /// AER 留在队列里不动。session V2 调本 wrapper 防 cross-conn AER 窃取。
    pub fn nvme_fire_aen_for_conn(
        &mut self,
        ctx: &mut pcie_device_core::DeviceCtx<'_>,
        aen_type: u8,
        aen_info: u8,
        log_id: u8,
        conn_id: u32,
    ) -> bool {
        self.fire_aen_for_conn(ctx, aen_type, aen_info, log_id, conn_id)
    }

    /// **Phase V6b** — 是否有待 fire 的 AEN event。session 主循环 cheap-check
    /// 避免空 drain 时分配 transport。
    ///
    /// 注意：当前 controller 没有真自然事件源（tick 内 SMART/Self-Test/
    /// Sanitize 仍依赖 mock），所以本函数返回值实际等价 "controller 有 AER
    /// pending + 测试代码已显式 fire_aen 或 tick 触发过"。返 true 不保证
    /// drain 一定有 CQE 输出（race window：caller 调本函数后另一 path 又消费）。
    /// session 应在 false 时跳过 drain，true 时 try drain 但容忍 0 结果。
    ///
    /// V6b 教学版策略：drain 函数自己内部用 `nvme_pending_aer_count` 决定是
    /// 否真 fire；本函数仅作"是否有 AER pending 可消费"的 fast hint。
    pub fn nvme_has_pending_aen_event(&self) -> bool {
        self.nvme_pending_aer_count() > 0
    }

    /// **Phase V7** — 配置 Discovery target portals。bin 启动 `--discovery-mode`
    /// 时调用。session `discovery_mode` flag 同步 derive (`nvme_is_discovery_mode`)；
    /// admin Get Log 0x70 即返此 vec 的 byte-exact 序列化；Identify Controller
    /// (CNS=0x01) 在 admin.rs 已加 CNTRLTYPE=0x02 + NN=0 patch (spec § 5.1.4 +
    /// § 5.17.2.1 byte 111)，让 Linux nvme-cli 把本 controller 当 Discovery
    /// Ctrl 处理。
    ///
    /// **V7c-fix (review R-2/H-1)** — CNTRLTYPE patch 已落 controller/admin.rs
    /// Identify CNS=0x01 分支；之前 doc 声称的"session 反向 patch"是 stale
    /// description，已删。
    ///
    /// 教学 V7 静态注入；TP4126 动态 registry V-followup 时再加 add/remove API。
    pub fn nvme_set_discovery_target(&mut self, portals: Vec<discovery_log::DiscoveryPortal>) {
        tracing::info!(
            count = portals.len(),
            "V7 set_discovery_target: controller 切 Discovery mode + Identify CNTRLTYPE=0x02 patch (admin.rs CNS=0x01 分支)"
        );
        self.discovery_portals = portals;
        self.discovery_gen_ctr = self.discovery_gen_ctr.wrapping_add(1);
    }

    /// **Phase V7** — read-only 检查 controller 是否已配 Discovery target。
    /// session `discovery_mode` 通过本函数初始化。
    pub fn nvme_is_discovery_mode(&self) -> bool {
        !self.discovery_portals.is_empty()
    }

    /// **Phase V7** — 给 session / test 读 portals 用于 Get Log 0x70 builder。
    pub fn nvme_discovery_portals(&self) -> &[discovery_log::DiscoveryPortal] {
        &self.discovery_portals
    }

    /// **Phase V7** — 当前 Discovery generation counter（spec § 5.16.1.20 GENCTR）。
    pub fn nvme_discovery_gen_ctr(&self) -> u64 {
        self.discovery_gen_ctr
    }
}

impl NvmeController {
    /// **2026-06-09** — 运行时设置队列深度（MQES = 单 SQ/CQ 最大 entry 数），
    /// 模拟不同档位设备。须在 controller enable（host 读 CAP）前调，通常 `open()`
    /// 之后立即调（CLI flag）。`entries` ∈ [2, 65536]；spec 不要求 2 的幂。
    pub fn set_max_queue_entries(&mut self, entries: u32) -> anyhow::Result<()> {
        if !(MIN_MAX_QUEUE_ENTRIES..=MAX_MAX_QUEUE_ENTRIES).contains(&entries) {
            return Err(anyhow::anyhow!(
                "max_queue_entries {entries} 非法：须 ∈ [{MIN_MAX_QUEUE_ENTRIES}, {MAX_MAX_QUEUE_ENTRIES}]（MQES 0-based 16-bit）"
            ));
        }
        self.cap = crate::regs::build_cap(entries);
        tracing::info!(mqes_entries = entries, "queue depth (MQES) 设置");
        Ok(())
    }

    /// **2026-06-09** — 运行时设置本次模拟的 **IO queue 对数上限**（Set Features
    /// Number of Queues 授予封顶 + Create IO Queue qid gate）。`pairs` ∈
    /// `[1, IO_QUEUE_SLOT_CAPACITY]`（不得超编译期槽位容量）。`open()` 后调。
    pub fn set_io_queue_pairs(&mut self, pairs: u16) -> anyhow::Result<()> {
        if pairs == 0 || pairs > IO_QUEUE_SLOT_CAPACITY {
            return Err(anyhow::anyhow!(
                "io_queue_pairs {pairs} 非法：须 ∈ [1, {IO_QUEUE_SLOT_CAPACITY}]（槽位容量）"
            ));
        }
        self.io_queue_pairs = pairs;
        // 重置默认授予数为新上限（host 仍按 Set Features 协商实际值）。
        self.granted_io_queues = pairs;
        tracing::info!(io_queue_pairs = pairs, "IO queue 对数上限设置");
        Ok(())
    }

    /// **2026-06-09** — 运行时设置本次模拟的 **namespace 容量上限**（≤
    /// `NAMESPACE_SLOT_CAPACITY`）。当前已加载 NS 数不得超过它（否则报错）。
    /// MNAN/NN 仍 = 实际加载数（Linux 要求 MNAN≤NN），本值只作模拟上限约束。
    pub fn set_max_namespaces(&mut self, max_ns: u32) -> anyhow::Result<()> {
        if max_ns == 0 || max_ns > NAMESPACE_SLOT_CAPACITY {
            return Err(anyhow::anyhow!(
                "max_namespaces {max_ns} 非法：须 ∈ [1, {NAMESPACE_SLOT_CAPACITY}]（槽位容量）"
            ));
        }
        let loaded = self.namespaces.len() as u32;
        if loaded > max_ns {
            return Err(anyhow::anyhow!(
                "已加载 {loaded} 个 namespace，超过模拟上限 max_namespaces {max_ns}"
            ));
        }
        self.max_namespaces = max_ns;
        tracing::info!(max_namespaces = max_ns, "namespace 容量上限设置");
        Ok(())
    }

    /// `backing_files`：每个文件成为一个 namespace（NSID 1, 2, ...）。
    /// 文件大小决定该 NS 容量（÷ 512 round down 到 LBA 数）。
    /// Phase H4：之前接受单 path string；现在 slice，至少 1 个。
    pub fn open(
        backing_files: &[String],
        vid: u16,
        ssvid: u16,
        zns_nsids: &[u32],
    ) -> anyhow::Result<Self> {
        if backing_files.is_empty() {
            return Err(anyhow::anyhow!("at least one --backing-file required"));
        }
        // **2026-06-09** — namespace 硬上限 NAMESPACE_SLOT_CAPACITY（nsid 1..=8）。
        if backing_files.len() > NAMESPACE_SLOT_CAPACITY as usize {
            return Err(anyhow::anyhow!(
                "too many --backing-file ({}): namespace 硬上限 {}",
                backing_files.len(),
                NAMESPACE_SLOT_CAPACITY
            ));
        }
        let mut namespaces: DenseMap<u32, Namespace> =
            DenseMap::new(NAMESPACE_SLOT_CAPACITY as usize);
        for (idx, path) in backing_files.iter().enumerate() {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?;
            let size = file.metadata()?.len();
            // Phase K1：默认 LBAF[0] = 512B no-meta no-PI（向后兼容）。
            // 真 PI 路径需 Format 切到 LBAF[1] + DPS=1 才启用。
            let lbads = 9u8;
            let meta_size = 0u8;
            let total_lba = size >> lbads;
            if total_lba == 0 {
                return Err(anyhow::anyhow!(
                    "backing file too small (< {} bytes): {path}",
                    1u64 << lbads
                ));
            }
            let nsid = (idx as u32) + 1;
            tracing::info!(
                nsid,
                path = %path,
                size,
                total_lba,
                lbads,
                meta_size,
                "NVMe controller: namespace registered"
            );
            // **Phase P1** — open() 时若 sidecar PTPL 存在则 reload
            // reservation state（模拟 power-loss survival）。
            let (gen_, reservation, registrants, ptpl) =
                match crate::controller::reservation::load_ptpl_sidecar(path) {
                    Some((g, r, regs)) => {
                        tracing::info!(
                            nsid,
                            n_reg = regs.len(),
                            "PTPL: reloaded reservation state from sidecar"
                        );
                        (g, r, regs, true)
                    }
                    None => (0, None, Vec::new(), false),
                };
            namespaces.insert(
                nsid,
                Namespace {
                    mmap: try_mmap_file(&file),
                    file,
                    total_lba,
                    path: path.clone(),
                    lbads,
                    meta_size,
                    pi_type: 0,
                    pi_first: true,
                    registrants,
                    reservation,
                    reservation_gen: gen_,
                    ptpl,
                    nswp: 0,
                    attached: true,
                    zns: None,
                    meta_inline: true,
                    force_flush_err: false,
                    not_ready: false,
                },
            );
        }
        // **Phase L1** — 把 zns_nsids 列表中的 NS 标记为 ZNS。
        // 教学短化：zone_size = 1 MiB 单位 LBA。**注意单位 = NS LBA**：
        // 默认 lbads=9 (512B) 时 1 MiB = 2048 LBA；若 Format 切到 lbads=12
        // (4 KiB) 则 zone 物理大小 = 2048 × 4 KiB = 8 MiB（zone_size 数字
        // 不变，物理字节 = zone_size × ns.block_bytes()）。
        const ZNS_ZONE_LBAS: u64 = 2048;
        for &nsid in zns_nsids {
            let Some(ns) = namespaces.get_mut(&nsid) else {
                tracing::warn!(nsid, "--zns-nsid 指定了不存在的 NSID");
                continue;
            };
            let total = ns.total_lba;
            let zone_size = ZNS_ZONE_LBAS.min(total.max(1));
            let n_zones = total.div_ceil(zone_size);
            let zones: Vec<Zone> = (0..n_zones)
                .map(|_| Zone {
                    write_pointer: 0,
                    state: ZoneState::Empty,
                })
                .collect();
            ns.zns = Some(ZnsState {
                zone_size,
                zone_capacity: zone_size,
                // 教学：默认无限制，避免简单 demo 触发 TOO_MANY_OPEN_ZONES。
                // 真硬件典型 MAR=14, MOR=14（NVMe ZNS 默认值，但 spec 无强约束）。
                max_open: 0,
                max_active: 0,
                zones,
            });
            tracing::info!(nsid, zone_size, n_zones, "ZNS NS enabled");
        }
        Ok(Self {
            namespaces,
            // 默认 MQES = 128 entries；运行时可经 set_max_queue_entries 调
            // （CLI flag）。SQ/CQ 是 host 分配的内存，逐条 dispatch，深度无本地
            // 数组限制；128 也消掉 Linux "queue_size 128 > sqsize 64 clamping" 警告。
            cap: build_cap(DEFAULT_MAX_QUEUE_ENTRIES),
            vs: VS_NVME_1_4,
            intms: 0,
            intmc: 0,
            cc: 0,
            csts: 0,
            aqa: 0,
            asq: 0,
            acq: 0,
            bprsel: 0,
            bpmbl: 0,
            state: CtrlState::Disabled,
            sqs: DenseMap::new(MAX_QID as usize),
            cqs: DenseMap::new(MAX_QID as usize),
            pending_fetches: HashMap::new(),
            pending_ios: HashMap::new(),
            dual_prp_writes: HashMap::new(),
            next_op_id: 1,
            prp_list_ops: HashMap::new(),
            sgl_ops: HashMap::new(),
            compare_ops: HashMap::new(),
            pi_writes: HashMap::new(),
            pi_reads: HashMap::new(),
            sep_meta_writes: HashMap::new(),
            sep_meta_reads: HashMap::new(),
            pending_fused: HashMap::new(),
            sqe_inbox: Vec::new(),
            stat_host_reads: 0,
            stat_host_writes: 0,
            stat_lba_read: 0,
            stat_lba_written: 0,
            power_on_instant: std::time::Instant::now(),
            stat_num_err_log_entries: 0,
            aen_pending: std::collections::VecDeque::new(),
            current_dispatch_conn_id: 0,
            discovery_portals: Vec::new(),
            discovery_gen_ctr: 0,
            features: std::collections::HashMap::new(),
            saved_features: std::collections::HashMap::new(),
            granted_io_queues: IO_QUEUE_SLOT_CAPACITY,
            // 运行时模拟上限默认 = 编译期槽位容量（即"不额外限制"）；CLI 可调低。
            io_queue_pairs: IO_QUEUE_SLOT_CAPACITY,
            max_namespaces: NAMESPACE_SLOT_CAPACITY,
            aen_last_err_count: 0,
            self_test_in_progress: None,
            self_test_last: None,
            error_log: std::collections::VecDeque::new(),
            fw_download_buf: Vec::new(),
            fw_active_slot: 1,
            fw_slot_revisions: {
                let mut s = std::array::from_fn(|_| String::new());
                s[1] = "v2.0    ".to_string(); // 默认 slot 1 已 factory-loaded
                s
            },
            fw_next_active_slot: 0,
            sanitize: None,
            sanitize_last_status: 0,
            doorbell_shadow_gpa: 0,
            doorbell_event_idx_gpa: 0,
            pending_shadow_polls: std::collections::HashMap::new(),
            pending_eventidx_writes: std::collections::HashSet::new(),
            shadow_poll_inflight: std::collections::HashSet::new(),
            shadow_ring_pending: std::collections::HashSet::new(),
            last_mmio_sq_doorbell: std::collections::HashMap::new(),
            last_eventidx_sq: std::collections::HashMap::new(),
            dbbuf_shadow_ahead_count: 0,
            locked_admin_opcodes: std::collections::HashSet::new(),
            crypto_gen: 0,
            reservation_notification_log: std::collections::VecDeque::new(),
            reservation_notification_count: 0,
            ana_state: 0x01, // Optimized
            ana_change_count: 1,
            current_ps: 0,
            irq_aggr_time: 0,
            irq_aggr_threshold: 0,
            host_id_lo: 0,
            host_id_hi: 0,
            vid,
            ssvid,
            msix_count: 4, // admin (vec 0) + IO (vec 1) + 2 spare
        })
    }

    /// **NS-not-ready spec-completeness（NAMESPACE_NOT_READY 0x82）** — 把 `nsids`
    /// 列表中的 NS 标记为 not-ready（`not_ready=true`）：其 IO 返 NAMESPACE_NOT_READY、
    /// Identify NS CNS 0x08 NSTAT.NRDY=1，直到 Format NVM 初始化该 NS 清此位。bin 经
    /// `--not-ready-nsid` 调用，模拟"media 初始化未完成"的冷启动 NS。不存在的 NSID 仅
    /// warn（与 `--zns-nsid` 同语义）。
    ///
    /// **为何用追加 setter 而非 `open()` 入参**：`open()` 已有 20+ 调用点（含 tests.rs
    /// 的 M1/M4 矩阵），改签名会全线破坏；setter 对既有调用点零回归（不调即全 ready）。
    pub fn set_namespaces_not_ready(&mut self, nsids: &[u32]) {
        for &nsid in nsids {
            match self.namespaces.get_mut(&nsid) {
                Some(ns) => {
                    ns.not_ready = true;
                    tracing::info!(nsid, "NS 标记为 not-ready（待 Format 初始化）");
                }
                None => tracing::warn!(nsid, "--not-ready-nsid 指定了不存在的 NSID"),
            }
        }
    }

    /// 计算 doorbell offset 是 SQ 还是 CQ + queue id。
    /// NVMe 1.4 § 3.1.7：doorbell 数组从 BAR0 + 0x1000 起，stride = 2^(2+CAP.DSTRD)。
    /// CAP.DSTRD=0 → stride=4 bytes。
    /// 序列：SQ0TDBL, CQ0HDBL, SQ1TDBL, CQ1HDBL, ...
    /// offset = 0x1000 + (2*qid + (0 if SQ else 1)) * 4
    ///
    /// **LOW-1（2026-06-10 reviewer）** — doorbell 区上界 = [`MSIX_TABLE_BAR0_OFFSET`]
    /// (0x2000)。doorbell 数组 [0x1000, 0x2000) 与其后的 MSI-X table 区
    /// [0x2000, 0x3000) 在同一 BAR0 内相邻。当前最大 doorbell（~0x1804，对应 256 队列档 = IO_QUEUE_SLOT_CAPACITY 上限）
    /// 远不及 0x2000，但若将来队列数上调致 doorbell 数组撑到 0x2000，无上界的
    /// `>= 0x1000` 会把落在 MSI-X table 区的访问误判成 doorbell。显式 gate 到
    /// `< MSIX_TABLE_BAR0_OFFSET`，让该别名永不发生（越界返 `None`）。
    fn parse_doorbell(offset: u64) -> Option<(bool, u16)> {
        // doorbell 合法区 = [0x1000, MSIX_TABLE_BAR0_OFFSET)；区外（含 MSI-X
        // table/PBA 区）一律非 doorbell。
        if !(0x1000..MSIX_TABLE_BAR0_OFFSET).contains(&offset) {
            return None;
        }
        let idx = (offset - 0x1000) / 4;
        let is_cq = (idx & 1) != 0;
        let qid = (idx / 2) as u16;
        Some((!is_cq, qid)) // returns (is_sq, qid)
    }

    /// SQyTDBL 写入：driver 通告新 SQE。
    ///
    /// **两条路径**：
    /// - **DBBUF active**（`doorbell_shadow_gpa != 0`）且 `sq_id != 0`（IO 队列）：
    ///   传入的 MMIO `value` 可能 **stale**（driver 经 shadow 多推了几条却按
    ///   `nvme_dbbuf_need_event` 跳过了真 ring，本次 ring 只是"晚到的唤醒"）。忽略
    ///   `value` 作 fetch 依据，转而 DMA-poll shadow doorbell 拿真 tail（见
    ///   `start_shadow_sq_poll` → race-safe 循环）。仅把 `value` 记成"最近真 MMIO
    ///   doorbell"供证明日志比对。
    /// - **DBBUF inactive 或 admin SQ（qid 0）**：保持原行为 —— 直接信任 `value`
    ///   做两段 wrap fetch（Linux `nvme_dbbuf_init` 跳过 qid 0，故 admin 永走此路径）。
    fn on_sq_tail_doorbell(&mut self, ctx: &mut DeviceCtx<'_>, sq_id: u16, new_tail: u32) {
        // **Quiesce（spec § 3.1.4.5）**：controller 未 operational 时不处理新命令——
        // 关机完成（CSTS.SHST=complete）或未 enable（CSTS.RDY=0）时忽略 SQ tail doorbell。
        // disable 路径已清 SQ，故 RDY 这条是 belt-and-suspenders；真正修的是 **shutdown
        // 后 SQ 仍在却不该再处理**（shutdown 不清队列，只置 SHST）。
        let shut_down = (self.csts & csts::SHST_MASK) == csts::SHST_COMPLETE;
        if self.csts & csts::RDY == 0 || shut_down {
            tracing::warn!(
                sq_id,
                new_tail,
                shut_down,
                "SQ doorbell while controller not operational (disabled / shut down) → ignored"
            );
            return;
        }
        // **DBBUF active + IO SQ** → shadow 是真相，MMIO value 仅作唤醒 + 证明比对。
        if self.dbbuf_active() && sq_id != 0 {
            if !self.sqs.contains_key(&sq_id) {
                tracing::warn!(sq_id, "SQ tail doorbell (DBBUF) to unknown SQ");
                return;
            }
            // 记录最近真 MMIO doorbell（裸 value）。当 shadow tail 之后被发现领先
            // 于此值，即证明 driver 跳过了一次真 ring 而 controller 经 shadow 追上。
            self.last_mmio_sq_doorbell.insert(sq_id, new_tail);
            self.start_shadow_sq_poll(ctx, sq_id);
            return;
        }
        // —— 纯 MMIO doorbell 路径（DBBUF inactive 或 admin qid 0）——
        self.advance_sq_to_tail(ctx, sq_id, new_tail);
    }

    /// **DBBUF 是否 active**：driver 已通过 Doorbell Buffer Config 提供 shadow +
    /// event_idx buffer（二者非 0）。inactive 时所有 shadow-poll 入口都是 no-op。
    pub(super) fn dbbuf_active(&self) -> bool {
        self.doorbell_shadow_gpa != 0 && self.doorbell_event_idx_gpa != 0
    }

    /// **共享 "把 SQ 推进到 tail T" 例程**（MMIO 路径与 shadow-poll 路径**共用**，
    /// 不重复 SQE fetch/dispatch 逻辑）。校验 `T < size`（越界 → CSTS.CFS）；把
    /// `sq.tail`（= host 已 issue fetch 到的位置 = processed_sq_tail）从旧值推进到
    /// `T`，对 `[old..T)` 两段 wrap fetch。`old == T` 时无事可做。
    ///
    /// 返回 `true` 表示推进成功（或已在目标，无需 fetch）；`false` 表示越界已置 CFS。
    fn advance_sq_to_tail(&mut self, ctx: &mut DeviceCtx<'_>, sq_id: u16, new_tail: u32) -> bool {
        let (base_gpa, size, old_tail) = {
            let Some(sq) = self.sqs.get(&sq_id) else {
                tracing::warn!(sq_id, "advance_sq_to_tail: unknown SQ");
                return false;
            };
            if new_tail >= sq.size {
                tracing::error!(
                    sq_id,
                    new_tail,
                    size = sq.size,
                    "SQ tail out of range; setting CSTS.CFS"
                );
                self.csts |= csts::CFS;
                return false;
            }
            (sq.base_gpa, sq.size, sq.tail)
        };
        // 校验通过后才写 sq.tail（processed_sq_tail）。
        self.sqs.get_mut(&sq_id).unwrap().tail = new_tail;
        if old_tail == new_tail {
            return true;
        }
        // 分两段：上半 [old_tail..end_of_q) + 下半 [0..new_tail)。
        // 非 wrap 时下半 count=0，跳过。
        let (first_count, second_count) = if new_tail > old_tail {
            (new_tail - old_tail, 0)
        } else {
            (size - old_tail, new_tail)
        };

        // 段 1：[old_tail .. old_tail + first_count)
        let bytes1 = first_count * SQE_BYTES as u32;
        let gpa1 = base_gpa + old_tail as u64 * SQE_BYTES;
        let tok1 = ctx.dma_read(gpa1, bytes1);
        self.pending_fetches.insert(
            tok1,
            FetchCtx {
                sq_id,
                count: first_count,
                start_slot: old_tail,
            },
        );
        tracing::debug!(
            sq_id,
            start_slot = old_tail,
            count = first_count,
            gpa = format_args!("{:#x}", gpa1),
            tok1,
            "SQ doorbell: fetch segment 1"
        );

        // 段 2：[0 .. second_count) — 仅 wrap 时
        if second_count > 0 {
            let bytes2 = second_count * SQE_BYTES as u32;
            let gpa2 = base_gpa;
            let tok2 = ctx.dma_read(gpa2, bytes2);
            self.pending_fetches.insert(
                tok2,
                FetchCtx {
                    sq_id,
                    count: second_count,
                    start_slot: 0,
                },
            );
            tracing::debug!(
                sq_id,
                start_slot = 0,
                count = second_count,
                gpa = format_args!("{:#x}", gpa2),
                tok2,
                "SQ doorbell: fetch segment 2 (wrap)"
            );
        }
        true
    }

    /// CQyHDBL 写入：driver 通告已处理多少 CQE（释放完成槽）。
    ///
    /// DBBUF active + IO CQ：MMIO value 可能 stale，转而 poll shadow CQ head 学真
    /// head 并写 CQ event_idx（见 `start_shadow_cq_poll`）。DBBUF inactive 时保持
    /// 原行为（直接存 `new_head`，仅作流控信息）。
    fn on_cq_head_doorbell(&mut self, ctx: &mut DeviceCtx<'_>, cq_id: u16, new_head: u32) {
        if let Some(cq) = self.cqs.get_mut(&cq_id) {
            cq.head = new_head;
            tracing::debug!(cq_id, new_head, "CQ head doorbell");
        } else {
            return;
        }
        if self.dbbuf_active() && cq_id != 0 {
            self.start_shadow_cq_poll(ctx, cq_id);
        }
    }

    // ======================= DBBUF shadow-doorbell polling =======================
    //
    // 背景（Linux drivers/nvme/host/pci.c）：广告 OACS Doorbell Buffer Config 后，
    // driver 给每个 IO 队列在 guest mem 维护 shadow doorbell（dbbuf_dbs）+ eventidx
    // (dbbuf_eis)。提交时它写 shadow tail、读 eventidx，**仅当** `need_event` 为真才
    // ring 真 MMIO doorbell（省 VM-exit）。故真 MMIO doorbell 的 value 可能 stale：
    // driver 已把 shadow 推得更远却跳过了 ring。controller **必须**读 shadow 拿真
    // tail/head，并写 eventidx 告诉 driver "下次提交超过我已消费的就 ring 我"。
    //
    // DSTRD=0（本 controller）：队列 qid 的 SQ doorbell 在 shadow buffer 字节偏移
    // `qid*8`，CQ doorbell 在 `qid*8+4`，各一个 LE u32。eventidx buffer 同布局。
    //
    // **transport 无同步读**（`dma_read` 是 token-异步，完成经 `on_dma_complete`），
    // 故轮询循环实现成跨完成回调的异步状态机：一次 shadow read 完成 → 推进 + 写
    // eventidx → **再发一次 shadow read** 闭合 race → … 直到 `shadow == processed`。
    // vfio-user adapter 在单次 pump 内 drain 所有（含回调里再发的）完成，OpenHCL 则
    // 真异步逐条回调 —— 两者都只靠 "token 终会被回调一次" 契约，循环收敛一致。

    /// DBBUF 上限：界定**单条 poll 链的自续深度**（本链发出的 shadow re-read 总数），
    /// **为 HOST-THREAD LIVENESS** 而设。
    ///
    /// vfio `dma_read` 是**同步**的：`vfio_user_transport` 在 wire 往返内联完成并把
    /// completion 立刻 `push_back` 到 `drain_dma_completions` 正在 drain 的同一队列。
    /// 于是 `handle_shadow_sq` 里每发一次自续 re-read，都会让**同一个** `while let
    /// Some(c) = pop_front()` 在**同一次** `pump_one` 内立刻取到它的 completion——一个
    /// 自喂的 re-read 循环会把 host 线程**永久**卡在一次 `drain_dma_completions` 调用里。
    /// 一个 **in-range 震荡**的 guest shadow（如 2↔4 反复，两者皆 < size；
    /// `advance_sq_to_tail` 把 `new_tail < old_tail` 当 wrap 仍前进）令 `shadow != processed`
    /// 步步成立、链无逃逸——正是触发此自喂的 case。
    ///
    /// 正确 driver 的链在 ~几十次 re-read 内 settle（自喂规模 = controller 往返期间并发
    /// 提交数，实测 harness ~14，**远** ≪ `1<<16`）→ **永不**误触。runaway（震荡/in-range
    /// garbage shadow）在此处撞顶 → 置 CFS + 收链（有限 ≤ 65536 次往返后停，而非无限
    /// hang）。**这是深度上限，不是环距离启发**（不引入 `fwd > size/2` 之类探测器——那是
    /// 脆弱启发，被本次修复刻意排除）。in-range 震荡正是经此 cap 收敛；真正的**越界**
    /// garbage（`shadow >= size`）另由 `handle_shadow_sq` 顶部守卫即时置 CFS。
    ///
    /// 互补强化（未在此处做，留作未来）：在 vfio transport 的 `drain_dma_completions`
    /// （`session.rs`）里**每次循环让出一次**，可在 runaway 期间保持 host 线程对外
    /// 响应——本 cap 保证有限终止，那个 yield 保证终止前不僵死。二者正交。
    const MAX_SHADOW_POLL_ITERS: u32 = 1 << 16;

    /// 启动一条 **SQ** shadow-poll 链（若该 (qid, SQ) 尚无链在飞）。doorbell ring 只是
    /// 唤醒：已有链在 drain 时其下一次 re-read 自会读到最新 shadow，故跳过启动重复链
    /// （同时避免重复 fetch 同批 SQE）。
    fn start_shadow_sq_poll(&mut self, ctx: &mut DeviceCtx<'_>, qid: u16) {
        if !self.dbbuf_active() || qid == 0 {
            return;
        }
        if !self.shadow_poll_inflight.insert((qid, false)) {
            // 已有 SQ 链在飞。**HIGH-1**：不静默丢这次唤醒 —— 置 "ring 待处理" 标记，
            // 让链在 settle 前再 re-read 一次，闭合 async transport 下 ring/re-read 帧
            // 交错导致的 settle-窗口漏 ring（详见 `shadow_ring_pending` 文档）。
            self.shadow_ring_pending.insert((qid, false));
            return;
        }
        // 新链：自续深度从 0 起算（首读不算 re-read；之后每次自续 +1）。
        self.issue_shadow_read(ctx, qid, false, 0);
    }

    /// 启动一条 **CQ** shadow-poll 链（若该 (qid, CQ) 尚无链在飞）。
    fn start_shadow_cq_poll(&mut self, ctx: &mut DeviceCtx<'_>, qid: u16) {
        if !self.dbbuf_active() || qid == 0 {
            return;
        }
        if !self.shadow_poll_inflight.insert((qid, true)) {
            // 已有 CQ 链在飞。**HIGH-1**：同 SQ，记一次性待处理 ring，settle 时补一读。
            self.shadow_ring_pending.insert((qid, true));
            return;
        }
        self.issue_shadow_read(ctx, qid, true, 0);
    }

    /// 发一次 shadow doorbell 的 4 字节 DMA-read 并登记 token → 轮询上下文，让
    /// `on_dma_complete` 路由回 `handle_shadow_poll_complete`。`iters` 透传本链**自续
    /// 深度**（chain start 处为 0，每次自续 +1；见 `MAX_SHADOW_POLL_ITERS`）。
    fn issue_shadow_read(&mut self, ctx: &mut DeviceCtx<'_>, qid: u16, is_cq: bool, iters: u32) {
        let off = qid as u64 * 8 + if is_cq { 4 } else { 0 };
        let gpa = self.doorbell_shadow_gpa + off;
        let token = ctx.dma_read(gpa, 4);
        self.pending_shadow_polls
            .insert(token, ShadowPollCtx { qid, is_cq, iters });
        tracing::trace!(
            qid,
            is_cq,
            iters,
            gpa = format_args!("{:#x}", gpa),
            token,
            "DBBUF: shadow doorbell read issued"
        );
    }

    /// 写一个 event_idx slot（LE u32）并登记 token 到 `pending_eventidx_writes`，
    /// 让其完成回调被静默消费（无后续动作）。
    fn write_eventidx(&mut self, ctx: &mut DeviceCtx<'_>, qid: u16, is_cq: bool, value: u32) {
        let off = qid as u64 * 8 + if is_cq { 4 } else { 0 };
        let gpa = self.doorbell_event_idx_gpa + off;
        let token = ctx.dma_write(gpa, value.to_le_bytes().to_vec());
        self.pending_eventidx_writes.insert(token);
        tracing::trace!(
            qid,
            is_cq,
            value,
            gpa = format_args!("{:#x}", gpa),
            token,
            "DBBUF: event_idx written"
        );
    }

    /// shadow-poll DMA-read 完成回调（由 `on_dma_complete` 顶部据 token 派发）。
    /// 解析 shadow value 后驱动 race-safe 循环：SQ 侧推进 + 写 eventidx + re-read，
    /// CQ 侧学 head + 写 eventidx（无需循环，详见各分支注释）。
    pub(super) fn handle_shadow_poll_complete(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        p: ShadowPollCtx,
        ok: bool,
        data: Vec<u8>,
    ) {
        let ShadowPollCtx { qid, is_cq, iters } = p;
        // 读失败或字节不足 → 不推进，清 inflight（下次 doorbell / tick 再起链）。
        // **HIGH-1**：一并清漏 ring 标记 —— 链已死，标记无处续读；下次 ring/tick 重起。
        if !ok || data.len() < 4 {
            tracing::warn!(
                qid,
                is_cq,
                ok,
                got = data.len(),
                "DBBUF: shadow read failed/short; ending poll chain"
            );
            self.shadow_poll_inflight.remove(&(qid, is_cq));
            self.shadow_ring_pending.remove(&(qid, is_cq));
            return;
        }
        let shadow = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if is_cq {
            self.handle_shadow_cq(ctx, qid, shadow);
        } else {
            self.handle_shadow_sq(ctx, qid, shadow, iters);
        }
    }

    /// **SQ** shadow 轮询单步 + race-safe 循环推进。
    ///
    /// `processed` = `sq.tail`（host 已 issue fetch 到的位置）。`iters` = 本链**自续深度**
    /// （HIGH-2 host-thread-liveness 上限的载体；见 `MAX_SHADOW_POLL_ITERS`）。
    /// - `shadow != processed`：driver 提交了我们尚未 fetch 的 SQE。推进 fetch 到
    ///   shadow（`advance_sq_to_tail`）、写 eventidx=shadow、**再 re-read** 闭合
    ///   "读后-提交" race（一次自续：`iters` += 1）。
    /// - `shadow == processed`：已追平。确保 eventidx==processed（driver 下次提交即
    ///   ring）。**HIGH-1**：settle 前查 `shadow_ring_pending`——若链在飞期间漏过一次
    ///   ring，清标记并**续一次 re-read** 而非停（把丢的 ring 转成"再 poll 一次"，覆盖
    ///   settle 窗口内的提交；同样是一次自续：`iters` += 1）；否则**停止**（不 re-read）。
    ///   因终止时 `eventidx==processed==shadow`，driver 下次提交 old==eventidx →
    ///   `need_event` 必真 → 必 ring，race 已闭合。
    ///
    /// **HIGH-2（host-thread liveness 上限）**：vfio `dma_read` 同步，自喂的 re-read
    /// 循环会把 host 线程卡在一次 `drain_dma_completions` 内（见 `MAX_SHADOW_POLL_ITERS`）。
    /// 上限界定**本链自续深度**：每次自续 re-read（advance 分支 **与** settle-续读两处）
    /// 前先 `next_iters = iters + 1`；撞 `MAX_SHADOW_POLL_ITERS` → 置 CSTS.CFS + 收链
    /// （与越界路径同款 fail-safe）。**深度上限非环距离启发**——in-range 震荡（shadow 在
    /// 小窗口打转、`advance_sq_to_tail` 把回退当 wrap 仍前进、`shadow != processed` 步步
    /// 成立）正是经此 cap 在有限 ≤ 65536 步后收敛，而非无限 hang。脏 shadow 的另一道防线
    /// 是下方 `shadow >= size` 的**越界守卫**：任何越界值即时置 CSTS.CFS 并收链。
    fn handle_shadow_sq(&mut self, ctx: &mut DeviceCtx<'_>, qid: u16, shadow: u32, iters: u32) {
        let Some(processed) = self.sqs.get(&qid).map(|s| s.tail) else {
            // SQ 在轮询途中消失（disable/delete）→ 收链。
            self.shadow_poll_inflight.remove(&(qid, false));
            self.shadow_ring_pending.remove(&(qid, false));
            return;
        };
        let size = self.sqs.get(&qid).map(|s| s.size).unwrap_or(0);
        // shadow 越界（driver bug / 脏 shadow）：置 CFS，收链。
        if size == 0 || shadow >= size {
            tracing::error!(
                qid,
                shadow,
                size,
                "DBBUF: shadow SQ tail out of range; setting CSTS.CFS"
            );
            self.csts |= csts::CFS;
            self.shadow_poll_inflight.remove(&(qid, false));
            self.shadow_ring_pending.remove(&(qid, false));
            return;
        }

        if shadow != processed {
            // —— 证明：shadow 领先于最近真 MMIO doorbell ⇒ driver 跳过了一次真 ring，
            //    controller 经 shadow 追上（DBBUF 被真正行使，非静默退回 MMIO）。仅当
            //    确有"最近 MMIO doorbell"可比（Some）且 shadow 与之不同才计数 —— 避免
            //    tick 在首个真 ring 之前发起的 poll 把"领先于 None(0)"误记为跳过。
            if let Some(last_mmio) = self
                .last_mmio_sq_doorbell
                .get(&qid)
                .copied()
                .filter(|&m| shadow != m)
            {
                self.dbbuf_shadow_ahead_count += 1;
                tracing::info!(
                    qid,
                    shadow_tail = shadow,
                    processed,
                    last_mmio_doorbell = last_mmio,
                    ahead_count = self.dbbuf_shadow_ahead_count,
                    "DBBUF shadow poll: SQ advanced via shadow AHEAD of last MMIO doorbell \
                     (driver skipped a real ring; controller caught it)"
                );
            }
            // 推进 fetch 到 shadow（processed := shadow）。越界已在上面拦，故必 true。
            // **LOW-1**：用 debug_assert! 锚定返回值，使未来若改动 advance 的越界守卫而
            // 意外让 processed/eventidx 失步时在 test 立即响（而非静默 desync）。
            debug_assert!(
                self.advance_sq_to_tail(ctx, qid, shadow),
                "advance_sq_to_tail 应成功：shadow 已在上面校验 < size"
            );
            // 写 eventidx = shadow（= 新 processed）：告诉 driver "提交超过 shadow 才 ring"。
            self.write_eventidx(ctx, qid, false, shadow);
            self.last_eventidx_sq.insert(qid, shadow);
            // **HIGH-2（host-thread liveness）**：本步发一次自续 re-read → 自续深度 +1。
            // 撞顶（runaway：in-range 震荡 / garbage in-range shadow 步步使 shadow != processed）
            // → 置 CFS + 收链，bound 同步 vfio 自喂为有限 ≤ 65536 次往返而非无限 hang。
            // 正确 driver 的链在 ~几十次内 settle（≪ 1<<16）→ 永不误触。
            let next_iters = iters + 1;
            if next_iters >= Self::MAX_SHADOW_POLL_ITERS {
                tracing::error!(
                    qid,
                    iters = next_iters,
                    shadow,
                    "DBBUF: SQ poll chain hit self-continuation DEPTH cap (runaway shadow); \
                     setting CSTS.CFS"
                );
                self.csts |= csts::CFS;
                self.shadow_poll_inflight.remove(&(qid, false));
                self.shadow_ring_pending.remove(&(qid, false));
                return;
            }
            // **re-read 闭合 race**：若 driver 在 "我读 shadow ~ 我写 eventidx" 窗口内又
            // 提交，shadow 会再次领先 processed，下一次完成回调会再推进。
            self.issue_shadow_read(ctx, qid, false, next_iters);
        } else {
            // 已追平。确保 eventidx==processed（仅在变化时写，免无谓 DMA + 防打转）。
            if self.last_eventidx_sq.get(&qid).copied() != Some(processed) {
                self.write_eventidx(ctx, qid, false, processed);
                self.last_eventidx_sq.insert(qid, processed);
            }
            // **HIGH-1**：settle 前查"链在飞期间漏 ring"标记。若置位 → 清标记并**再 re-read
            // 一次**（续链）而非 settle —— 把每个被丢的 ring 转成"再 poll 一次"，使落在
            // settle 窗口内的提交必被下一读捕获。一次性：标记已 remove，故除非**新** ring
            // 再置位否则不续链（无限链不可能）。
            if self.shadow_ring_pending.remove(&(qid, false)) {
                tracing::trace!(
                    qid,
                    processed,
                    "DBBUF: settle 见漏 ring 标记 → 续一次 re-read（HIGH-1）"
                );
                // 续链：inflight 仍持有（未 remove）。这是一次自续 re-read → 自续深度 +1
                // （**不**重置——重置只在 chain start）；同样受 host-thread-liveness 上限
                // 约束（settle/advance 交替的 runaway 也被 bound）。
                let next_iters = iters + 1;
                if next_iters >= Self::MAX_SHADOW_POLL_ITERS {
                    tracing::error!(
                        qid,
                        iters = next_iters,
                        "DBBUF: SQ poll chain hit self-continuation DEPTH cap at settle-continue; \
                         setting CSTS.CFS"
                    );
                    self.csts |= csts::CFS;
                    self.shadow_poll_inflight.remove(&(qid, false));
                    self.shadow_ring_pending.remove(&(qid, false));
                    return;
                }
                self.issue_shadow_read(ctx, qid, false, next_iters);
                return;
            }
            self.shadow_poll_inflight.remove(&(qid, false));
            tracing::trace!(qid, processed, "DBBUF: SQ poll chain settled");
        }
    }

    /// **CQ** shadow 轮询单步。post_cqe 从不在 CQ-full 上阻塞（fire-and-forget），故
    /// CQ shadow 跳过 ≠ 挂死，仅"晚学到 head"。因此 CQ 侧**无需** race 循环：读 shadow
    /// head → 更新 `cq.head` → 写 CQ eventidx = head（driver 据此在推进 head 时 ring，
    /// 让 controller 及时学到释放的完成槽）→ 停。
    ///
    /// **HIGH-1**：CQ 也有"链在飞期间漏 ring"问题（`start_shadow_cq_poll` 见 inflight 时
    /// 置 `shadow_ring_pending`）。停止前查标记：若置位 → 清标记并**再读一次** CQ shadow
    /// （同 SQ 的一次性续读），把被丢的 CQ ring 转成"再学一次 head"而非等下一 tick。
    fn handle_shadow_cq(&mut self, ctx: &mut DeviceCtx<'_>, qid: u16, shadow: u32) {
        let size = self.cqs.get(&qid).map(|c| c.size).unwrap_or(0);
        if size == 0 || shadow >= size {
            tracing::error!(
                qid,
                shadow,
                size,
                "DBBUF: shadow CQ head out of range; ending poll chain"
            );
            self.shadow_poll_inflight.remove(&(qid, true));
            self.shadow_ring_pending.remove(&(qid, true));
            return;
        }
        if let Some(cq) = self.cqs.get_mut(&qid) {
            cq.head = shadow;
        }
        // eventidx = 当前 head：driver 推进 head 越过此值即 ring（need_event 语义）。
        self.write_eventidx(ctx, qid, true, shadow);
        // **HIGH-1**：漏 ring 标记 → 续一次 CQ 读（一次性，标记已 remove）而非 settle。
        if self.shadow_ring_pending.remove(&(qid, true)) {
            tracing::trace!(
                qid,
                head = shadow,
                "DBBUF: CQ settle 见漏 ring 标记 → 续一次 re-read（HIGH-1）"
            );
            self.issue_shadow_read(ctx, qid, true, 0); // CQ 不参与深度上限（同步 drain 下不自喂）
            return;
        }
        self.shadow_poll_inflight.remove(&(qid, true));
        tracing::trace!(qid, head = shadow, "DBBUF: CQ poll chain settled");
    }

    /// Dispatch SQE 单条命令。可能立即完成（构造 CQE 发出去）或入 pending（等
    /// PRP DMA）。`head_after_this` = controller 已 fetch 到的下一条 SQE 位置
    /// （CQE.sqhd 字段），由 `on_fetched_sqes` 按批内 index 单调推算给出。
    fn dispatch_sqe(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        sq_id: u16,
        head_after_this: u16,
        sqe: Sqe,
    ) {
        let cid = sqe.cid();
        let opc = sqe.opcode();
        let fuse = sqe.fuse();
        tracing::debug!(
            sq_id,
            cid,
            opc = format_args!("{:#x}", opc),
            fuse,
            head_after_this,
            "dispatch SQE"
        );
        let sq_head = head_after_this;
        let cq_id = self.sqs.get(&sq_id).map(|s| s.cq_id).unwrap_or(0);
        let is_admin = sq_id == 0;

        // **Phase O2** — Fused operation handling (spec § 6.2)。
        // Admin SQ 不支持 fused（spec §6.2 "fused operations are not supported
        // on Admin Submission Queue"）—— Reviewer H-5 (7轮)：admin SQ 上 fuse
        // != 0 必须直接 INVALID_FIELD，不能 silently 走 normal admin dispatch。
        if is_admin && fuse != 0 {
            tracing::warn!(fuse, "Fused operation on Admin SQ → INVALID_FIELD");
            let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
            let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD);
            self.post_cqe(ctx, cq_id, cqe);
            return;
        }
        if !is_admin {
            match fuse {
                0 => {
                    // 正常命令 — 但若 SQ 上有未配对的 FUSE_FIRST，spec 要求两条
                    // 都 abort INVALID_FIELD（"fused 第二条必须紧跟第一条"）。
                    if let Some((stranded, stranded_head)) = self.pending_fused.remove(&sq_id) {
                        tracing::warn!(
                            sq_id,
                            stranded_cid = stranded.cid(),
                            "Fused: FIRST without matching SECOND, aborting"
                        );
                        let phase1 = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
                        let cqe1 = Cqe::error(
                            stranded.cid(),
                            sq_id,
                            stranded_head,
                            phase1,
                            sc::INVALID_FIELD,
                        );
                        self.post_cqe(ctx, cq_id, cqe1);
                    }
                }
                1 => {
                    // FUSE_FIRST — 必须是 Compare (NVMe spec 唯一定义的 fused pair)
                    if opc != nvm_opc::COMPARE {
                        tracing::warn!(opc, "Fused FIRST is not Compare → INVALID_FIELD");
                        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
                        let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD);
                        self.post_cqe(ctx, cq_id, cqe);
                        return;
                    }
                    // 如果同 SQ 上已有 stranded FIRST → 那条也 abort
                    if let Some((stranded, stranded_head)) = self.pending_fused.remove(&sq_id) {
                        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
                        let cqe = Cqe::error(
                            stranded.cid(),
                            sq_id,
                            stranded_head,
                            phase,
                            sc::INVALID_FIELD,
                        );
                        self.post_cqe(ctx, cq_id, cqe);
                    }
                    self.pending_fused.insert(sq_id, (sqe, sq_head));
                    return; // 等 SECOND
                }
                2 => {
                    // FUSE_SECOND — 必须是 Write，且 nsid/slba/nlb 必须与 FIRST 匹配
                    let Some((first, first_head)) = self.pending_fused.remove(&sq_id) else {
                        tracing::warn!("Fused SECOND without FIRST → INVALID_FIELD");
                        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
                        let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD);
                        self.post_cqe(ctx, cq_id, cqe);
                        return;
                    };
                    if opc != nvm_opc::WRITE {
                        tracing::warn!(opc, "Fused SECOND is not Write → INVALID_FIELD");
                        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
                        // Abort 两条
                        self.post_cqe(
                            ctx,
                            cq_id,
                            Cqe::error(first.cid(), sq_id, first_head, phase, sc::INVALID_FIELD),
                        );
                        let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD);
                        self.post_cqe(ctx, cq_id, cqe);
                        return;
                    }
                    let first_nsid = first.nsid;
                    let first_slba = first.cdw10 as u64 | ((first.cdw11 as u64) << 32);
                    let first_nlb = (first.cdw12 & 0xffff) as u32 + 1;
                    let second_nsid = sqe.nsid;
                    let second_slba = sqe.cdw10 as u64 | ((sqe.cdw11 as u64) << 32);
                    let second_nlb = (sqe.cdw12 & 0xffff) as u32 + 1;
                    if first_nsid != second_nsid
                        || first_slba != second_slba
                        || first_nlb != second_nlb
                    {
                        tracing::warn!(
                            "Fused C&W mismatch: nsid/slba/nlb differ between FIRST and SECOND"
                        );
                        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
                        self.post_cqe(
                            ctx,
                            cq_id,
                            Cqe::error(first.cid(), sq_id, first_head, phase, sc::INVALID_FIELD),
                        );
                        let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD);
                        self.post_cqe(ctx, cq_id, cqe);
                        return;
                    }
                    // 把 FIRST 当独立 Compare dispatch；它的完成 (compare_finalize
                    // 等) 会决定 Compare 成败。如果 Compare succeeds → 再 dispatch
                    // SECOND (Write)；如果 fail → cancel SECOND，返 SC=0x85
                    // COMPARE_FAILURE。本教学路径简化：因 Compare 单 PRP 路径
                    // 是同步 finalize 返 Cqe，无法 cleanly chain；所以这里只
                    // 演示 fused-pair detection + post 'Compare aborted because
                    // **Phase O3** — Fused Compare-and-Write 真 atomic chain。
                    // Compare（FIRST）执行的同时把 Write（SECOND）的整条 SQE
                    // 存进 NvmCompareSinglePrpFused PendingOp；Compare 完成
                    // 时按结果决定是否真 dispatch Write（pass = dispatch；
                    // fail = post COMPARE_FAILURE for Write 不写盘）。
                    // 限制：教学路径只支持单 PRP Compare（≤ 1 page = 8 LBA at
                    // 512B）。超过此尺寸 → abort 两条 INVALID_FIELD（spec 允许
                    // controller 不支持任意尺寸的 fused）。
                    // **2026-06-09 纯 4K** — fused Compare 仅支持 plain NS（512B
                    // LBAF[0] / 纯 4K LBAF[2]，无 meta/PI）且单 PRP（≤ 1 page）。
                    // 按 per-NS 扇区算字节：4K 时 1 LBA = 4096 = 1 page 仍 OK。
                    // 非 plain（如 PI 交错格式）或 > 1 page → 两条 INVALID_FIELD
                    // （spec 允许 controller 不支持任意尺寸/格式的 fused）。
                    // ns 不存在时不在此拒，交由 dispatch_fused_compare_write 返
                    // INVALID_NAMESPACE，保持错误码语义。
                    if let Some((is_plain, sector_bytes)) = self.ns(first_nsid).map(|ns| {
                        (
                            ns.meta_size == 0
                                && !ns.pi_enabled()
                                && (ns.lbads == 9 || ns.lbads == 12),
                            1u64 << ns.lbads,
                        )
                    }) {
                        let bytes = first_nlb as u64 * sector_bytes;
                        if !is_plain || bytes > NVME_PAGE_SIZE {
                            tracing::warn!(
                                slba = first_slba,
                                nlb = first_nlb,
                                bytes,
                                is_plain,
                                "Fused C+W: non-plain NS 或 > 1 page not supported"
                            );
                            let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
                            self.post_cqe(
                                ctx,
                                cq_id,
                                Cqe::error(
                                    first.cid(),
                                    sq_id,
                                    first_head,
                                    phase,
                                    sc::INVALID_FIELD,
                                ),
                            );
                            let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD);
                            self.post_cqe(ctx, cq_id, cqe);
                            return;
                        }
                    }
                    // Build NvmCompareSinglePrpFused op: 走 Compare 的 DMA-read
                    // 流程，但 PendingOp 变体携 write_sqe 让完成路径区分。
                    let prp1 = first.prp1;
                    self.dispatch_fused_compare_write(
                        ctx,
                        sq_id,
                        first,
                        first.cid(),
                        first_head,
                        first_slba,
                        first_nlb,
                        prp1,
                        sqe,
                        sq_head,
                        cq_id,
                    );
                    return;
                }
                _ => {
                    tracing::warn!(fuse, "Reserved fuse value → INVALID_FIELD");
                    let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
                    let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD);
                    self.post_cqe(ctx, cq_id, cqe);
                    return;
                }
            }
        }

        let cqe_result = if is_admin {
            self.dispatch_admin(ctx, sqe, cid, sq_head, cq_id)
        } else {
            self.dispatch_io(ctx, sq_id, sqe, cid, sq_head, cq_id)
        };

        if let Some(cqe) = cqe_result {
            self.post_cqe(ctx, cq_id, cqe);
        }
    }

    // dispatch_admin moved to controller/admin.rs (H6 reviewer split)

    // dispatch_io moved to controller/io.rs (H6 split)

    /// **Phase O3** — Fused Compare-and-Write dispatch helper（spec § 6.2
    /// atomic CAS）。Compare 完成后按结果决定 Write 是否执行：
    /// - 数据 == backing → dispatch Write 走正常 NvmWriteDmaRead
    /// - 数据 != backing → post COMPARE_FAILURE 给 Compare CID +
    ///   post 同 SC 给 Write CID（spec § 6.2：fused 两条都需个别 CQE）
    ///
    /// nsid/slba/nlb 已在 dispatcher 校验过对齐，调用方只需提供 Write SQE。
    #[allow(clippy::too_many_arguments)]
    fn dispatch_fused_compare_write(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        sq_id: u16,
        compare_sqe: Sqe,
        compare_cid: u16,
        compare_sq_head: u16,
        slba: u64,
        nlb: u32,
        prp1: u64,
        write_sqe: Sqe,
        write_sq_head: u16,
        cq_id: u16,
    ) {
        // 基础校验复用普通 IO 路径的 ns 判断
        let nsid = compare_sqe.nsid;
        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
        if self.ns(nsid).is_none() {
            self.post_cqe(
                ctx,
                cq_id,
                Cqe::error(
                    compare_cid,
                    sq_id,
                    compare_sq_head,
                    phase,
                    sc::INVALID_NAMESPACE,
                ),
            );
            self.post_cqe(
                ctx,
                cq_id,
                Cqe::error(
                    write_sqe.cid(),
                    sq_id,
                    write_sq_head,
                    phase,
                    sc::INVALID_NAMESPACE,
                ),
            );
            return;
        }
        // **NS-not-ready spec-completeness（reviewer HIGH-1）** — fused C&W 在 dispatch_sqe
        // 的 fuse==2 分支直接进本函数，**绕过 dispatch_io 的 not-ready 门**。若不在此再设门，
        // not-ready NS 上的 fused C&W 会让 Compare 半边照常读盘返 success/COMPARE_FAILURE、
        // 仅 Write 半边经 dispatch_io 才被拦 → driver 看到非原子、不自洽结果。故此处对 not-ready
        // NS 的 fused 两条都返 NAMESPACE_NOT_READY，与 io.rs 门一致（同 INVALID_NAMESPACE 双-CQE 模式）。
        if self.ns(nsid).map(|n| n.not_ready).unwrap_or(false) {
            self.post_cqe(
                ctx,
                cq_id,
                Cqe::error(
                    compare_cid,
                    sq_id,
                    compare_sq_head,
                    phase,
                    sc::NAMESPACE_NOT_READY,
                ),
            );
            self.post_cqe(
                ctx,
                cq_id,
                Cqe::error(
                    write_sqe.cid(),
                    sq_id,
                    write_sq_head,
                    phase,
                    sc::NAMESPACE_NOT_READY,
                ),
            );
            return;
        }
        // DMA-read host Compare 数据 → NvmCompareSinglePrpFused 完成回调
        // **2026-06-09 纯 4K** — 按 per-NS 扇区算字节，与 fused completion
        // (completion.rs NvmCompareSinglePrpFused) 的 sector 对称，避免
        // 512-vs-4096 长度不一致导致 4K 上 Compare 恒 fail。
        let sector = self.ns(nsid).map_or(SECTOR_SIZE, |n| 1u64 << n.lbads);
        let bytes = nlb as u64 * sector;
        let tok = ctx.dma_read(prp1, bytes as u32);
        self.pending_ios.insert(
            tok,
            PendingIo {
                sq_id,
                cid: compare_cid,
                sq_head: compare_sq_head,
                cq_id,
                nsid,
                op: PendingOp::NvmCompareSinglePrpFused {
                    lba: slba,
                    num_blocks: nlb,
                    write_sqe,
                    write_sq_head,
                    write_sq_id: sq_id,
                },
            },
        );
        tracing::info!(
            sq_id,
            compare_cid,
            write_cid = write_sqe.cid(),
            slba,
            nlb,
            "Fused C+W: dispatched as atomic chain (Compare → Write)"
        );
    }

    /// 分配单调递增的 op_id（独立于 SDK DMA token），用于关联多段
    /// DMA 完成回调（双 PRP / PRP list 等）。
    pub(super) fn alloc_op_id(&mut self) -> u64 {
        let id = self.next_op_id;
        self.next_op_id = self.next_op_id.wrapping_add(1);
        id
    }

    /// **Phase H4** — NSID 校验：返 Some(&mut Namespace) 或 None（NSID
    /// 不存在）。dispatch_io 必须先校验，spec § 6.1：invalid NSID 返
    /// SC=0x0b "Invalid Namespace or Format"。NSID 0xFFFF_FFFF 是
    /// broadcast 仅 admin 命令允许（如 Format All），IO 命令不允许。
    pub(super) fn ns_mut(&mut self, nsid: u32) -> Option<&mut Namespace> {
        if nsid == 0 || nsid == 0xFFFF_FFFF {
            return None;
        }
        self.namespaces.get_mut(&nsid)
    }
    /// 只读版（用于 LBA 边界校验等不需 mut 的场景）。
    pub(super) fn ns(&self, nsid: u32) -> Option<&Namespace> {
        if nsid == 0 || nsid == 0xFFFF_FFFF {
            return None;
        }
        self.namespaces.get(&nsid)
    }

    /// **Phase F** — 触发 AEN (Async Event Notification)。
    ///
    /// NVMe spec § 5.2：当 controller 发生 async 事件（health critical /
    /// namespace change / log page available / firmware activate），从
    /// 之前 driver 投入的 AsyncEventRequest pending 队列弹一条，构造 CQE
    /// 携 cdw0 = `[type:3 | rsvd:5 | info:8 | log_id:8 | rsvd:8]`，post
    /// 到对应 CQ + raise interrupt。Driver 看到 CQE 后会读对应 log page，
    /// 然后再投新的 AER。
    ///
    /// **spec § 5.2 Figure 174 Async Event Type 值**（H1 修正错误注释）：
    /// - 0x00 = Error
    /// - 0x01 = **SMART/Health Status**（非 0x02）
    /// - 0x02 = Notice
    /// - 0x06 = I/O Command Set Specific
    /// - 0x07 = Vendor Specific
    ///
    /// **H2 修复**：CQE.sqhd 必须反映 controller **当前**实时 SQ head
    /// （spec § 4.6.1.4 sqhd 单调要求）。不能用 driver 投 AER 时的 stale
    /// head，否则 driver 会观测到 sqhd 倒退而进入 fatal 状态。
    ///
    /// 当前没有自然事件源（无温度传感器/无 namespace 添加），此函数提
    /// 供基础设施 + tests 用；外部代码可调 `fire_aen(0x01, 0x00, 0x02)`
    /// 模拟 SMART critical。返 true = AER 已 fire；false = 无 pending AER
    /// 可弹（事件丢弃 — driver 下次投 AER 不会重发，与真硬件一致）。
    pub(super) fn fire_aen(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        aen_type: u8,
        aen_info: u8,
        log_id: u8,
    ) -> bool {
        // V6 路径仍 fire 任意 conn 的 head（legacy 行为，BC）；V8c 路径
        // 用 `fire_aen_for_conn` 只 fire 指定 conn。
        self.fire_aen_inner(ctx, aen_type, aen_info, log_id, /*filter*/ None)
    }

    /// **Phase V8c** — fire 指定 `conn_id` 的最早 pending AER。其它 conn 的
    /// AER 留在队列里不动；返 true = fire 成功；false = 该 conn 无 pending AER。
    pub(super) fn fire_aen_for_conn(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        aen_type: u8,
        aen_info: u8,
        log_id: u8,
        conn_id: u32,
    ) -> bool {
        self.fire_aen_inner(ctx, aen_type, aen_info, log_id, Some(conn_id))
    }

    fn fire_aen_inner(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        aen_type: u8,
        aen_info: u8,
        log_id: u8,
        conn_id_filter: Option<u32>,
    ) -> bool {
        // M5：type 只占 3 bit，> 7 是 caller bug → 早 fail。
        debug_assert!(aen_type < 8, "AEN type must be < 8 (spec § 5.2 Figure 174)");
        // V8c：filter present 时找第一个 conn_id 匹配项；否则 pop_front。
        let popped = if let Some(want) = conn_id_filter {
            let pos = self.aen_pending.iter().position(|t| t.3 == want);
            pos.and_then(|i| self.aen_pending.remove(i))
        } else {
            self.aen_pending.pop_front()
        };
        let Some((cid, sq_id, cq_id, _conn_id)) = popped else {
            tracing::debug!(
                aen_type,
                aen_info,
                log_id,
                conn_id_filter,
                "fire_aen: no pending AER (matching conn), event dropped"
            );
            return false;
        };
        // H2：用 controller 当前实时 sq_head（admin SQ id=0），避免 stale
        // sqhd 触发 spec § 4.6.1.4 单调违规。
        let sq_head_now = self.sqs.get(&sq_id).map(|s| s.head as u16).unwrap_or(0);
        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
        let mut cqe = Cqe::success(cid, sq_id, sq_head_now, phase);
        // spec § 5.2 CDW0 layout: bits 2:0 = Async Event Type, bits 15:8 =
        // Async Event Info, bits 23:16 = Log Page Identifier。
        cqe.cdw0 = (aen_type as u32 & 0x7) | ((aen_info as u32) << 8) | ((log_id as u32) << 16);
        // 复制 packed 字段到本地变量再 fmt（packed struct field 取引用 UB）。
        let cdw0_local = cqe.cdw0;
        tracing::info!(cid, cdw0 = format_args!("{:#x}", cdw0_local), "AEN: firing");
        self.post_cqe(ctx, cq_id, cqe);
        true
    }

    /// **Phase G** — push error log entry（CQE 携 non-zero status 时调）。
    /// 环形 buffer，最多 64 entry（spec ELPE=63）。
    ///
    /// Phase J 重构后唯一保留在 mod.rs 的 log 相关方法（需 `&mut self`，
    /// 5 个纯读 builder 已搬到 controller/logs.rs）。
    pub(super) fn push_error_log(
        &mut self,
        sq_id: u16,
        cid: u16,
        status_field: u16,
        lba: u64,
        nsid: u32,
    ) {
        const ELPE_MAX: usize = 64;
        let count = self.stat_num_err_log_entries; // 调 push 时已 ++
        self.error_log.push_back(ErrorLogEntry {
            error_count: count,
            sq_id,
            cid,
            status_field,
            param_loc: 0,
            lba,
            nsid,
        });
        while self.error_log.len() > ELPE_MAX {
            self.error_log.pop_front();
        }
    }

    /// **Phase K2** — Compare finalize：把 host 数据（prp1 + 可选 prp2）
    /// 拼起来，读 backing 对比，构造 success / COMPARE_FAILURE CQE。
    /// dual-PRP 路径 host_prp2 = Some；PRP-list 路径 host_prp1 = 完整
    /// data，host_prp2 = None。
    #[allow(clippy::too_many_arguments)]
    fn compare_finalize(
        &mut self,
        nsid: u32,
        lba: u64,
        num_blocks: u32,
        host_prp1: Vec<u8>,
        host_prp2: Option<Vec<u8>>,
        cid: u16,
        sq_id: u16,
        sq_head: u16,
        cq_id: u16,
    ) -> Cqe {
        // **2026-06-09 纯 4K** — 按 per-NS 扇区算字节/偏移。
        let sector = 1u64 << self.namespaces.get(&nsid).map_or(9u8, |n| n.lbads);
        let bytes = num_blocks as u64 * sector;
        let mut host_data = host_prp1;
        if let Some(extra) = host_prp2 {
            host_data.extend_from_slice(&extra);
        }
        host_data.truncate(bytes as usize);
        let mut backing_buf = vec![0u8; bytes as usize];
        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
        let Some(ns) = self.namespaces.get_mut(&nsid) else {
            return Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_NAMESPACE);
        };
        // **Reviewer M4** — Compare 路径目前仅对非 PI NS 有效（dispatch
        // 层已拒绝 PI Compare）。如果未来放宽这条 gate，此 assert 会让
        // bug 立刻显形而不是静默按 4096 vs 4104 byte 比较全 fail。
        debug_assert!(
            !ns.pi_enabled(),
            "compare_finalize: PI NS not supported (dispatch should reject)"
        );
        // **Phase M2** — read_at 走 mmap 零拷贝（read 路径 immutable，
        // 多个 reader 并发安全）
        match ns.read_at(&mut backing_buf, lba * sector) {
            Ok(()) => {
                if host_data == backing_buf {
                    self.stat_host_reads += 1;
                    self.stat_lba_read += num_blocks as u64;
                    tracing::debug!(nsid, lba, num_blocks, "Compare multi-PRP OK");
                    Cqe::success(cid, sq_id, sq_head, phase)
                } else {
                    let mismatch_at = host_data
                        .iter()
                        .zip(backing_buf.iter())
                        .position(|(a, b)| a != b)
                        .unwrap_or(0);
                    tracing::warn!(
                        nsid,
                        lba,
                        num_blocks,
                        mismatch_at,
                        "Compare multi-PRP FAILURE"
                    );
                    self.stat_num_err_log_entries += 1;
                    self.push_error_log(sq_id, cid, sc::sf_of(sc::COMPARE_FAILURE), lba, nsid);
                    Cqe::error(cid, sq_id, sq_head, phase, sc::COMPARE_FAILURE)
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, nsid, lba, "Compare: backing read failed");
                self.stat_num_err_log_entries += 1;
                self.push_error_log(sq_id, cid, sc::sf_of(sc::DATA_TRANSFER_ERROR), lba, nsid);
                Cqe::error(cid, sq_id, sq_head, phase, sc::DATA_TRANSFER_ERROR)
            }
        }
    }

    /// 帮助函数：把 `data` 经 **spec § 4.1.1 PRP** DMA-write 回 host，完成后提交
    /// success CQE。Identify / Get Log Page / Get Features（admin）+ Zone Report /
    /// Reservation Report（io.rs，可 > 8 KiB）都走这条。
    ///
    /// **P1（2026-06-10）—— 修 latent silent corruption**：原实现把整个 `data`
    /// 连续写到单个 `prp1`，无视 PRP2。但 host 的 PRP1/PRP2 页 GPA **不保证连续**
    /// （Linux/Windows 驱动常给非连续页）→ > 4 KiB 数据的 page 1 会被写到
    /// `prp1 + 4096` 而非 PRP2 指向的 GPA，silent 写错地址。
    ///
    /// 现按 buf 大小分流（复用 IO read 的 dual-PRP 双 token 机件，零新状态）：
    /// - **≤ 1 page**：单 PRP1 写（原行为，字节级不变 —— Identify(4K)/SMART 等回归安全）。
    /// - **2 page**：page0→PRP1（`NvmReadDualPrpSiblingHalf`，success no-op）+
    ///   page1→PRP2（`NvmReadDmaWrite{num_blocks:0}`，success post CQE）。completer
    ///   是后发的 tok2 → transport in-order completion 下它触发时 sibling 已落，两页
    ///   齐才 post CQE。DMA-fail 由 `on_dma_complete` 顶部 `!ok` 分支统一处理
    ///   （含移除 sibling 防其后到 post success 覆盖 error）。
    /// - **> 2 page（P2）**：PRP2 是 PRP **list** 页指针 → 复用 IO read 的 device→host
    ///   list 机件（`PrpListOp` + `NvmReadPrpList{Fetch,Data}`）：DMA-read list 页 → parse →
    ///   逐页 DMA-write（page0→PRP1，page1..→list entry）→ 全到齐 post CQE。单 list 页容
    ///   513 数据页（~2 MiB）；超过需 list chaining（未实现）→ 回退连续写 PRP1（status quo）。
    fn dma_write_then_complete(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        prp1: u64,
        prp2: u64,
        data: Vec<u8>,
        cid: u16,
        sq_id: u16,
        sq_head: u16,
        cq_id: u16,
    ) {
        if data.len() as u64 <= NVME_PAGE_SIZE {
            let tok = ctx.dma_write(prp1, data);
            self.pending_ios.insert(
                tok,
                PendingIo {
                    sq_id,
                    cid,
                    sq_head,
                    cq_id,
                    nsid: 0, // admin payload，无 NS 关联
                    op: PendingOp::NvmReadDmaWrite { num_blocks: 0 },
                },
            );
            return;
        }
        if data.len() as u64 > 2 * NVME_PAGE_SIZE {
            // > 2 page：PRP2 = PRP **list** 页指针。**P2（2026-06-10）**——复用 IO read 的
            // device→host PRP-list 机件（`PrpListOp` + `NvmReadPrpList{Fetch,Data}`，零新
            // 代码路径）：把 buf 按页切进 `data_pages` → 入 op → DMA-read list 页（在 prp2）
            // → 完成 arm parse list + 逐页 DMA-write（page0→PRP1，page1..→list entry）→ 全
            // 到齐 post CQE。
            //
            // **容量**：单 PRP list 页装 `NVME_PAGE_SIZE/8 = 512` 个 entry → 513 数据页
            // （~2 MiB）。**C1② chaining**：超过时 NvmReadPrpListFetch 跟随 chain pointer
            // 跨多 list 页 walk（完成路径处理），dispatch 侧只需把 buf 按页切好入 op、
            // DMA-read 第一张 list 页。data buffer 已由 caller 按需分配（无 OOM 风险）。
            //
            // **激活条件（forward scaffolding，§30）**：chaining 在此 device→host 活路径上
            // **已就绪**，但只有当 payload 真 > 2 MiB（>513 页）时才触发。当前所有 caller
            // 都把 payload 卡在 ≤ 2 MiB（Get Log Page admin / Zone+Reservation Report io，
            // 防御性策略；IO Read 受 MDTS=128 KiB 限），故生产路径暂不触发——单测
            // `prp_list_chaining_device_to_host` 直驱本函数验证。届时抬高 MDTS（IO Read
            // >2 MiB）或放开 report cap 即自动启用，无需改本机件。
            let total_pages = data.len().div_ceil(NVME_PAGE_SIZE as usize) as u32;
            // 按页切 buf（page0 = PRP1 数据，page1.. = list entry 数据）。
            let mut data_pages: Vec<Option<Vec<u8>>> = Vec::with_capacity(total_pages as usize);
            for i in 0..total_pages as usize {
                let off = i * NVME_PAGE_SIZE as usize;
                let end = ((i + 1) * NVME_PAGE_SIZE as usize).min(data.len());
                data_pages.push(Some(data[off..end].to_vec()));
            }
            let op_id = self.alloc_op_id();
            self.prp_list_ops.insert(
                op_id,
                PrpListOp {
                    sq_id,
                    cid,
                    sq_head,
                    cq_id,
                    nsid: 0,
                    lba: 0,
                    num_blocks: 0, // admin/report payload —— 不计 SMART host-read
                    is_write: false,
                    prp1_gpa: prp1,
                    list_entries: None,
                    total_pages,
                    pages_done: 0,
                    data_pages,
                },
            );
            // DMA-read PRP list 页（在 prp2）；到达后 NvmReadPrpListFetch 解析 + per-page 写。
            let tok = ctx.dma_read(prp2, NVME_PAGE_SIZE as u32);
            self.pending_ios.insert(
                tok,
                PendingIo {
                    sq_id,
                    cid,
                    sq_head,
                    cq_id,
                    nsid: 0,
                    op: PendingOp::NvmReadPrpListFetch { op_id },
                },
            );
            return;
        }
        let half = NVME_PAGE_SIZE as usize;
        let (b1, b2) = data.split_at(half);
        // page0 → PRP1（sibling 半，success no-op）。
        let tok1 = ctx.dma_write(prp1, b1.to_vec());
        self.pending_ios.insert(
            tok1,
            PendingIo {
                sq_id,
                cid,
                sq_head,
                cq_id,
                nsid: 0,
                op: PendingOp::NvmReadDualPrpSiblingHalf,
            },
        );
        // page1 → PRP2（completer，success post CQE）。后发 → in-order 下最后完成。
        let tok2 = ctx.dma_write(prp2, b2.to_vec());
        self.pending_ios.insert(
            tok2,
            PendingIo {
                sq_id,
                cid,
                sq_head,
                cq_id,
                nsid: 0,
                op: PendingOp::NvmReadDmaWrite { num_blocks: 0 },
            },
        );
    }

    /// 把 CQE 写入指定 CQ：DMA-write 16 字节 → fire interrupt（或 batch）。
    ///
    /// **Phase M1b** — Interrupt Coalescing (spec § 5.21.1.8)：
    /// - AGGR_THR (0-based) → 每 (thr+1) 个 CQE 累积 fire 一次
    /// - AGGR_TIME (100 us 单位) → 距 last_fire 超时强制 fire
    ///
    /// 若 driver Set Features 0x08 时 thr=0 + time=0 → 退化为
    /// "fire-on-every-CQE"（原行为）。Admin CQ (cq_id=0) 不参与
    /// coalescing — spec 要求 admin 延迟最小。
    /// **A1（Abort, spec § 5.1）** — 尝试中止 (sqid, cid) 命名的 in-flight 命令。
    /// 返回 true=已中止 / false=未找到（已完成 / 从未提交 / 已同步执行完）。
    ///
    /// **多-DMA 命令的完整清理（A1-fix）**：accumulator 类命令（dual-PRP /
    /// PI-multi / PRP-list / SGL）在 `pending_ios` 里有**多条**子-DMA 条目（都带命令
    /// 的 sqid/cid）+ 一个累积器。必须**全部**清掉：先移累积器，再 sweep 掉
    /// `pending_ios` 里该命令的所有子-DMA 条目，只 post **一条** CQE。否则只移一条
    /// 子-DMA、留下累积器 + 其余子-DMA = partial-abort（累积器永不集齐 → 泄漏）。
    /// 单-DMA 命令（NvmWriteDmaRead / NvmReadDmaWrite 等）只有 pending_ios 条目，
    /// 由 sweep 取其 target。被移条目对应的在飞 DMA completion 后续走 unknown-token
    /// 静默忽略（同 `disable()`），杜绝重复完成。
    ///
    /// **教学边界**：只中止真正"在飞"的异步命令（等 host DMA）。已 fetch 但还在
    /// sqe_inbox 待同步 dispatch 的命令转瞬即完，不在此中止——spec § 5.1 允许对
    /// 已开始执行的命令返 dw0 bit0=1（Could Not Abort）。
    pub(super) fn try_abort_inflight(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        sqid: u16,
        cid: u16,
    ) -> bool {
        // 1) 先扫累积器表（每个 remove 累积器并返其 (cq_id, sq_head)）。
        let acc_target = abort_scan(&mut self.dual_prp_writes, sqid, cid)
            .or_else(|| abort_scan(&mut self.compare_ops, sqid, cid))
            .or_else(|| abort_scan(&mut self.pi_writes, sqid, cid))
            .or_else(|| abort_scan(&mut self.pi_reads, sqid, cid))
            .or_else(|| abort_scan(&mut self.prp_list_ops, sqid, cid))
            .or_else(|| abort_scan(&mut self.sgl_ops, sqid, cid))
            .or_else(|| abort_scan(&mut self.sep_meta_writes, sqid, cid))
            .or_else(|| abort_scan(&mut self.sep_meta_reads, sqid, cid));
        // 2) sweep `pending_ios`：移除该命令的所有（子-）DMA 条目；若无累积器
        //    （单-DMA 命令），从中取 target。
        let mut po_target: Option<(u16, u16)> = None;
        self.pending_ios.retain(|_, p| {
            if p.sq_id == sqid && p.cid == cid {
                po_target.get_or_insert((p.cq_id, p.sq_head));
                false
            } else {
                true
            }
        });
        // 3) 累积器 target 优先（其 sq_head 与子-DMA 一致）；否则单-DMA target；
        //    再否则 fused FIRST 等 SECOND（keyed by sq_id，cid 在缓存的 Sqe 内）。
        let target = acc_target.or(po_target).or_else(|| {
            let hit = self
                .pending_fused
                .get(&sqid)
                .is_some_and(|(fsqe, _)| fsqe.cid() == cid);
            if !hit {
                return None;
            }
            let (_, sq_head) = self.pending_fused.remove(&sqid).unwrap();
            let cq_id = self.sqs.get(&sqid).map(|s| s.cq_id)?;
            Some((cq_id, sq_head))
        });
        let Some((cq_id, sq_head)) = target else {
            return false;
        };
        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
        let cqe = Cqe::error(cid, sqid, sq_head, phase, sc::COMMAND_ABORT_REQUESTED);
        self.post_cqe(ctx, cq_id, cqe);
        true
    }

    fn post_cqe(&mut self, ctx: &mut DeviceCtx<'_>, cq_id: u16, cqe: Cqe) {
        let aggr_thr = self.irq_aggr_threshold;
        let aggr_time_100us = self.irq_aggr_time;
        let Some(cq) = self.cqs.get_mut(&cq_id) else {
            tracing::warn!(cq_id, "post_cqe: unknown CQ");
            return;
        };
        let slot = cq.tail;
        let gpa = cq.base_gpa + slot as u64 * CQE_BYTES;
        let bytes = cqe.as_bytes().to_vec();
        let iv = cq.interrupt_vector;
        let iv_enabled = cq.interrupt_enabled;
        // advance tail; flip phase on wrap
        cq.tail = (cq.tail + 1) % cq.size;
        if cq.tail == 0 {
            cq.phase ^= 1;
        }
        // 累计未通知 CQE
        cq.pending_completions = cq.pending_completions.saturating_add(1);
        tracing::debug!(cq_id, slot, gpa = format_args!("{:#x}", gpa), "post CQE");
        ctx.dma_write_fire_and_forget(gpa, bytes);
        if !iv_enabled {
            return;
        }
        let must_fire = should_fire_irq(cq_id, cq.pending_completions, aggr_thr, aggr_time_100us);
        if must_fire {
            cq.pending_completions = 0;
            cq.last_fire = Some(std::time::Instant::now());
            ctx.fire_interrupt(iv as u32);
        } else if cq.last_fire.is_none() {
            // 第一条 pending：记 timestamp 供 tick 检超时
            cq.last_fire = Some(std::time::Instant::now());
        }
    }

    /// 解析 fetched SQE bytes 并把它们入 sqe_inbox 等 dispatch。
    fn on_fetched_sqes(&mut self, ctx_token: u64, data: Vec<u8>) {
        let Some(fctx) = self.pending_fetches.remove(&ctx_token) else {
            tracing::warn!(ctx_token, "fetched SQE: unknown token");
            return;
        };
        let expected_bytes = fctx.count as usize * SQE_BYTES as usize;
        if data.len() != expected_bytes {
            tracing::error!(
                got = data.len(),
                expected = expected_bytes,
                "fetched SQE: byte count mismatch; setting CSTS.CFS (fatal status)"
            );
            // 让 driver 可见 fatal status 而不是 silent drop。
            self.csts |= csts::CFS;
            return;
        }
        // advance SQ head（host 已 fetch 完，head = start_slot + count）。
        let sq_size = self.sqs.get(&fctx.sq_id).map(|s| s.size).unwrap_or(1);
        if let Some(sq) = self.sqs.get_mut(&fctx.sq_id) {
            sq.head = (fctx.start_slot + fctx.count) % sq.size;
        }
        // 拆成单条 SQE，**每条带单调推进的 head_after_this**（CQE.sqhd 必须
        // 反映 "controller 已 fetch 到的下一条 SQE 位置"；同批多条 cmd 不能
        // 全部报 batch 末尾的 head）。
        for i in 0..fctx.count {
            let off = i as usize * SQE_BYTES as usize;
            let slice = &data[off..off + SQE_BYTES as usize];
            let head_after_this = ((fctx.start_slot + i + 1) % sq_size) as u16;
            match Sqe::read_from_bytes(slice) {
                Ok(sqe) => {
                    self.sqe_inbox.push((fctx.sq_id, head_after_this, sqe));
                }
                Err(_) => {
                    tracing::warn!("fetched SQE: cast failed (alignment?); skipping");
                }
            }
        }
    }

    /// **真 QEMU 11 vfio-user guest e2e（2026-06-10）** — 合成标准 PCI MSI-X
    /// capability body（cap_id + next_ptr 之后的 10 字节，由 adapter 拼链表指针）。
    ///
    /// 布局（PCI 3.0 spec § 6.8.2 MSI-X Capability）：
    /// - Message Control（u16）：bit[10:0] = Table Size = N-1（N = `msix_count`）；
    ///   bit15 MSI-X Enable / bit14 Function Mask 由 QEMU/guest 经 cfg 写控制，
    ///   初值 0。
    /// - Table Offset/BIR（u32）：bit[2:0] = BIR（=0 → BAR0）；bit[31:3] = table
    ///   在该 BAR 内的 8 字节对齐 offset（= [`MSIX_TABLE_BAR0_OFFSET`]）。
    /// - PBA Offset/BIR（u32）：同上，offset = [`MSIX_PBA_BAR0_OFFSET`]。
    ///
    /// QEMU 自行 overlay MSI-X table 内存区（server 不服务 table 读写），仅把 PBA
    /// 读 forward 给 server（controller mmio_read 对该区返 0 = 无 pending）。
    fn msix_capability(&self) -> Capability {
        debug_assert!(self.msix_count >= 1, "MSI-X table size 须 ≥ 1");
        // Table Size 字段 = N-1（spec 编码）。msix_count 受 u16 约束、远小于 2048，
        // 故 N-1 必落在 bit[10:0]（最大 2047）。`saturating_sub` 防御 msix_count==0
        // 的 underflow（调用点已 gate msix_count>0，此处仅双保险）。
        let table_size_field = self.msix_count.saturating_sub(1) & 0x07FF;
        let message_control: u16 = table_size_field; // EN=0 / Mask=0 初值
        // BIR=0（BAR0），低 3 位即 BIR；offset 已 8 字节对齐故低 3 位本就为 0，
        // 直接用 offset 值即 `offset | BIR(0)`。
        debug_assert_eq!(
            MSIX_TABLE_BAR0_OFFSET & 0x7,
            0,
            "table offset 须 8 字节对齐"
        );
        debug_assert_eq!(MSIX_PBA_BAR0_OFFSET & 0x7, 0, "PBA offset 须 8 字节对齐");
        let table_off_bir: u32 = MSIX_TABLE_BAR0_OFFSET as u32; // BIR=0
        let pba_off_bir: u32 = MSIX_PBA_BAR0_OFFSET as u32; // BIR=0
        let mut raw = Vec::with_capacity(10);
        raw.extend_from_slice(&message_control.to_le_bytes());
        raw.extend_from_slice(&table_off_bir.to_le_bytes());
        raw.extend_from_slice(&pba_off_bir.to_le_bytes());
        Capability {
            cap_id: MSIX_CAP_ID,
            raw,
        }
    }
}

impl PcieDevice for NvmeController {
    fn describe(&self) -> DeviceDescribe {
        DeviceDescribe {
            vendor_id: self.vid,
            // PCI Device ID = 0xC0DE — matches OpenHCL noop convention 便于
            // ohcldiag-dev / guest 区分本设备实例。
            device_id: 0xc0de,
            class_code: 0x01_08_02, // Mass Storage / NVMe (PCIe class 01.08.02)
            revision: 1,
            subsystem_vendor: self.ssvid,
            subsystem_device: 0,
            bars: vec![BarLayout {
                index: 0,
                size: BAR0_SIZE,
                // NVMe spec 不强求 64-bit BAR；用 32-bit 简化 cfg space。
                // BAR0 = MMIO 32 不消耗 BAR1，省 PCIe BAR slots。
                kind: BarKind::Mmio32,
                prefetchable: false,
            }],
            msix_count: self.msix_count as u32,
            // **真 QEMU 11 vfio-user guest e2e 修复（2026-06-10）** — 必须在 config
            // space 暴露 MSI-X capability，否则 QEMU `vfio_pci_add_capabilities` 找不到
            // MSI-X cap → `vdev->msix=NULL` → guest nvme 驱动 `pci_alloc_irq_vectors`
            // 拿不到向量 → `nvme_probe` 返 -EINVAL（-22），设备永不 enumerate。
            // 仅广告 `msix_count`（vfio-user `GET_IRQ_INFO` 接口）**不够**——cfg-space
            // cap 链表才是 QEMU 解析 table/PBA 布局的来源。OpenHCL 路径由 VTL2 shim
            // 据 `msix_count` 自行合成此 cap，故那条路径不需要本字段；vfio-user 无
            // shim，QEMU 直读我们合成的 cfg space，故这里必须补上。
            // msix_count==0（无 MSI-X 设备）则不合成该 cap（合规：无向量不应广告 cap）。
            capabilities: if self.msix_count > 0 {
                vec![self.msix_capability()]
            } else {
                vec![]
            },
            cfg_write_side_effect_offsets: vec![],
        }
    }

    // **Reviewer M-5 (Phase Q11)** — MMIO 读写实现在 controller/mmio.rs，
    // 让 mod.rs 不背 ~130 行 BAR0 寄存器布局代码。
    fn mmio_read(&mut self, bar: u32, offset: u64, size: u32) -> u64 {
        self.mmio_read_impl(bar, offset, size)
    }

    fn mmio_write(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        bar: u32,
        offset: u64,
        size: u32,
        value: u64,
    ) {
        self.mmio_write_impl(ctx, bar, offset, size, value);
    }

    fn reset(&mut self, kind: u32) {
        tracing::info!(kind, "NVMe: PCIe reset");
        self.disable();
    }

    fn tick(&mut self, ctx: &mut DeviceCtx<'_>) {
        // **Phase G (rev. CRITICAL fix)** — self-test 进度推进 + 完成 transition
        // 边沿一次性 fire AEN（避免老设计每秒重 fire）。
        if let Some(st) = self.self_test_in_progress.as_mut() {
            let elapsed = st.started_at.elapsed().as_secs();
            // 用整数除避免 f64 round edge case (LOW reviewer 建议)
            let pct = ((elapsed * 100) / st.total_seconds.max(1) as u64).min(100) as u8;
            if pct != st.percent_complete {
                st.percent_complete = pct;
                tracing::debug!(pct, stc = st.stc, "Self-Test progress");
            }
            if elapsed >= st.total_seconds as u64 {
                // 完成：从 in_progress transition 到 last。take() 保证
                // 下次 tick 这个分支不会再触发。
                let done = self.self_test_in_progress.take().unwrap();
                let poh = self.power_on_instant.elapsed().as_secs() / 3600;
                self.self_test_last = Some(SelfTestCompleted {
                    stc: done.stc,
                    result: 0, // 0 = completed without error（教学：backing 永远 ok）
                    completed_at_poh: poh,
                });
                tracing::info!(stc = done.stc, "Self-Test completed (result=0)");
                // AEN Notice (type=0x02) info=0x01 'Device Self-test Completed'
                // log_id=0x06 Self-Test Log（spec § 5.2 Figure 174）。
                // fire_aen 返 false 表示 driver 未投 AER → 事件按 spec 丢弃，
                // 不重试。
                let _ = self.fire_aen(ctx, 0x02, 0x01, 0x06);
            }
        }
        // AEN: 新出现的 error 触发 type=0x00 Error。
        if self.stat_num_err_log_entries > self.aen_last_err_count {
            self.aen_last_err_count = self.stat_num_err_log_entries;
            // info=0x00 reserved/generic，log_id=0x01 Error Information Log
            // 同样：fire 失败不重试（spec § 5.2 允许 drop）。
            let _ = self.fire_aen(ctx, 0x00, 0x00, 0x01);
        }
        // **Phase K5** — Sanitize 进度推进 + 完成 transition fire AEN。
        if let Some(sn) = self.sanitize.as_mut() {
            let elapsed = sn.started_at.elapsed().as_secs();
            // SPROG (spec § 5.16.1.18) 是 0..=65535 范围（不是 0..100）
            let pct = ((elapsed * 65535) / sn.total_seconds.max(1) as u64).min(65535) as u16;
            sn.percent_complete = pct;
            if elapsed >= sn.total_seconds as u64 {
                let sanact = sn.sanact;
                self.sanitize = None;
                self.sanitize_last_status = 1; // success
                tracing::info!(sanact, "Sanitize completed");
                // AEN Notice (type=0x02) info=0x05 'Sanitize Completed'
                // log_id=0x81 Sanitize Status Log（spec § 5.2 Figure 174）。
                let _ = self.fire_aen(ctx, 0x02, 0x05, 0x81);
            }
        }
        // **Phase M1b** — Interrupt Coalescing 超时 flush
        // (spec § 5.21.1.8 Interrupt Coalescing)：若 driver 设置 AGGR_TIME
        // 且当前有 pending CQE 已超时未 fire → 强制 fire。
        // AGGR_TIME 单位 100 us；0 = 关闭时间维度，仅按 threshold 触发。
        let aggr_time_100us = self.irq_aggr_time;
        if aggr_time_100us > 0 {
            let timeout = std::time::Duration::from_micros(aggr_time_100us as u64 * 100);
            let now = std::time::Instant::now();
            // 先收集 (cq_id, iv) 避免 borrow 冲突
            let mut to_fire: Vec<(u16, u16)> = Vec::new();
            for (cq_id, cq) in self.cqs.iter_mut() {
                if cq_id == 0 || !cq.interrupt_enabled || cq.pending_completions == 0 {
                    continue;
                }
                let Some(last) = cq.last_fire else { continue };
                if now.duration_since(last) >= timeout {
                    to_fire.push((cq_id, cq.interrupt_vector));
                    cq.pending_completions = 0;
                    cq.last_fire = Some(now);
                }
            }
            for (cq_id, iv) in to_fire {
                tracing::trace!(cq_id, iv, "IRQ coalesce: time flush");
                ctx.fire_interrupt(iv as u32);
            }
        }
        // **DBBUF 安全网（spec § 7.13）** — doorbell-ring 路径是主路（poll 仅在被唤醒
        // 时跑，延迟有界）；tick 这条是兜底：覆盖 *完全跳过* ring 的极端场景（driver
        // 的 eventidx 已让它对某次提交不 ring，而我们因故没在 ring 路径起链）以及 CQ
        // 侧 head 推进的周期学习。inflight guard 让"已有链在飞"时近乎零成本；DBBUF
        // inactive 时下面 `dbbuf_active()` 直接短路，tick 不付任何 DBBUF 代价。
        if self.dbbuf_active() {
            // 收集 IO 队列 id（避开遍历时 &self 与 &mut self 冲突）。admin qid 0 不参与。
            let sq_ids: Vec<u16> = self.sqs.keys().filter(|&q| q != 0).collect();
            for qid in sq_ids {
                self.start_shadow_sq_poll(ctx, qid);
            }
            let cq_ids: Vec<u16> = self.cqs.keys().filter(|&q| q != 0).collect();
            for qid in cq_ids {
                self.start_shadow_cq_poll(ctx, qid);
            }
        }
    }

    /// **H-3 修复** — DMA 完成派发委托到 `controller/completion.rs` 中的
    /// `on_dma_complete_impl`，让 mod.rs 不背 ~1600 行 IO 完成路径代码。
    fn on_dma_complete(&mut self, ctx: &mut DeviceCtx<'_>, token: u64, ok: bool, data: Vec<u8>) {
        self.on_dma_complete_impl(ctx, token, ok, data);
    }
}

/// Parse a PRP list page (4 KiB = 512 u64 entries) into Vec<u64>。
///
/// **M2 修复**：之前 "尾部全零终止" 与 spec § 4.4 不符 — spec 要求 caller
/// 按 transfer size 自行算精确 entry 数；GPA = 0 是合法地址（嵌入式 BIOS
/// 可能把 RAM mapped 从 0 起），不能视为终止。caller 用
/// `total_pages - 1` 精确截，本函数返回所有 entry 不截断。
///
/// Phase E v1 不处理 list chaining（最后一个 entry == 下一个 PRP list 页
/// 指针）；MDTS=5 = 32 page 远小于 1 page list 容量（512 entry），暂不会触发。
pub(super) fn parse_prp_list(data: &[u8]) -> Vec<u64> {
    data.chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().expect("chunks_exact(8) guarantees 8 bytes")))
        .collect()
}

// **Phase W1 (ADR-010)** — 原 `impl vfio_user_transport::Regions for NvmeController`
// 已删除：vfio-user backend 现统一从中立 `describe()` 派生 BAR0/MSI-X/config，
// 不再要求 controller 额外实现 vfio-user 专属 trait（消除"描述模型分叉"）。

#[cfg(test)]
mod tests;

/// **DBBUF（shadow doorbells, spec § 5.7 + § 7.13）单测** —— 用中立
/// `pcie_device_core::CaptureTransport` 驱动 shadow 读/写，断言：
/// 1. `need_event` 镜像（与 Linux `nvme_dbbuf_need_event` 同公式）+ 我们写的
///    event_idx 选值确实让 driver 在"提交超过已消费"时才 ring；
/// 2. poll-advances-SQ：shadow 领先时 controller 经 shadow fetch 到真 tail；
/// 3. submit-during-window race：写 event_idx 后的 **re-read** 捕获窗口内的新提交
///    （**revert 锚点** —— 去掉 re-read 此用例红）；
/// 4. CQ 学 head + 写 CQ event_idx；
/// 5. DBBUF inactive 时退回纯 MMIO 路径（行为不变）；stale MMIO value 被忽略。
#[cfg(test)]
mod dbbuf_tests {
    use super::*;
    use crate::regs::{SQE_BYTES, SubmissionQueue, csts};
    use pcie_device_core::{CaptureTransport, DeviceCtx, TransportEvent};

    const SHADOW_GPA: u64 = 0x10_0000;
    const EVENTIDX_GPA: u64 = 0x20_0000;
    const SQ1_BASE: u64 = 0x2_0000;
    const SQ_DEPTH: u32 = 64;

    fn mk() -> NvmeController {
        let path = std::env::temp_dir().join(format!(
            "nvme_dbbuf_test_{}_{:?}.img",
            std::process::id(),
            std::thread::current().id()
        ));
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(4 * 1024 * 1024).unwrap();
        drop(f);
        NvmeController::open(&[path.to_str().unwrap().to_string()], 0x1414, 0, &[]).unwrap()
    }

    /// enable + 装一条 IO SQ(qid=1) + 激活 DBBUF。
    fn mk_dbbuf() -> NvmeController {
        let mut c = mk();
        c.csts |= csts::RDY;
        c.sqs.insert(
            1,
            SubmissionQueue {
                base_gpa: SQ1_BASE,
                size: SQ_DEPTH,
                head: 0,
                tail: 0,
                cq_id: 1,
            },
        );
        c.doorbell_shadow_gpa = SHADOW_GPA;
        c.doorbell_event_idx_gpa = EVENTIDX_GPA;
        c
    }

    /// 在 `cap.events()[from..]` 里找最后一条对 `gpa` 的 DmaWrite，返回其 4 字节 LE 值。
    fn last_eventidx_write(cap: &CaptureTransport, from: usize, gpa: u64) -> Option<u32> {
        cap.events()[from..].iter().rev().find_map(|e| match e {
            TransportEvent::DmaWrite { gpa: g, data, .. } if *g == gpa && data.len() == 4 => {
                Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
            }
            _ => None,
        })
    }

    /// 找 `cap.events()[from..]` 里**首条**对 `gpa` 的 DmaRead，返回其 token。
    fn first_read_token(cap: &CaptureTransport, from: usize, gpa: u64) -> Option<u64> {
        cap.events()[from..].iter().find_map(|e| match e {
            TransportEvent::DmaRead { gpa: g, token, len } if *g == gpa && *len == 4 => {
                Some(*token)
            }
            _ => None,
        })
    }

    /// 统计 `cap.events()[from..]` 里对 `gpa` 的 4-byte DmaRead 次数（shadow 读次数）。
    fn count_shadow_reads(cap: &CaptureTransport, from: usize, gpa: u64) -> usize {
        cap.events()[from..]
            .iter()
            .filter(|e| {
                matches!(e, TransportEvent::DmaRead { gpa: g, len, .. } if *g == gpa && *len == 4)
            })
            .count()
    }

    // ---------------- 1) need_event 镜像 + event_idx 选值正确性 ----------------

    /// 与 Linux `nvme_dbbuf_need_event` 逐字节同公式（unsigned 16-bit wrap）。
    fn need_event(event_idx: u16, new: u16, old: u16) -> bool {
        (new.wrapping_sub(event_idx).wrapping_sub(1)) < (new.wrapping_sub(old))
    }

    #[test]
    fn need_event_matches_kernel_formula_examples() {
        // driver 从 old→new 提交；event_idx 是 controller 写回的值。
        // 我们写 event_idx = processed（已消费到的 tail）。
        // 关键不变式：当 controller 追平（event_idx == old == processed），driver 下一次
        // 提交 new>old 必 ring。
        assert!(need_event(5, 6, 5), "old==event_idx, 提交一条 → 必 ring");
        assert!(need_event(5, 8, 5), "old==event_idx, 提交多条 → 必 ring");
        // driver 已领先 controller（old > event_idx），再提交可能跳过 ring —— 这正是
        // 我们必须靠 re-read 追平后才停的原因。
        assert!(
            !need_event(5, 7, 6),
            "old(6) > event_idx(5) 且新值未越过窗口 → driver 跳过 ring"
        );
        // wrap：event_idx 接近 u16::MAX，new 绕回。
        assert!(
            need_event(65535, 0, 65535),
            "wrap 边界：old==event_idx → ring"
        );
    }

    /// 证明我们的 event_idx 选值（= processed_sq_tail）在 controller 追平后让 driver
    /// 对**任何**新提交都 ring（即 race 已闭合，不会有"提交了却不 ring"的悬空命令）。
    #[test]
    fn settled_eventidx_forces_ring_on_any_new_submit() {
        let processed: u16 = 10; // controller 已消费到 10，写 event_idx=10
        // 队列深度 N 最多容纳 N-1 条 outstanding（tail==head 表示空，不表示满），故
        // 有效新提交量 delta ∈ [1, N-1]；delta==N 即绕回同一 index（净 0 条，不算提交）。
        for delta in 1..SQ_DEPTH as u16 {
            let new = (processed + delta) % SQ_DEPTH as u16;
            // 追平时 driver 的 old == processed（它上次提交也到这），event_idx==processed。
            assert!(
                need_event(processed, new, processed),
                "追平后提交到 {new}（delta={delta}）必 ring（event_idx==old==processed）"
            );
        }
    }

    // ---------------- 2) poll-advances-SQ via shadow ----------------

    #[test]
    fn sq_poll_advances_via_shadow_and_writes_eventidx() {
        let mut c = mk_dbbuf();
        let mut cap = CaptureTransport::with_start_token(0x1000);

        // driver ring 真 MMIO doorbell，但 value=0 是 STALE（它已把 shadow 推到 4
        // 却因 need_event 跳过了把真值写进 MMIO）。
        let pre = cap.events().len();
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_sq_tail_doorbell(&mut ctx, 1, 0); // stale MMIO value=0 → 应被忽略
        }
        // 应发起一次 shadow SQ 读（SQ offset = qid*8；qid=1 → 8）。
        let sq_shadow_gpa = SHADOW_GPA + 8;
        let tok = first_read_token(&cap, pre, sq_shadow_gpa).expect("应发起 shadow SQ 读");

        // 完成 shadow 读：真 tail=4（领先 stale MMIO 0 与 processed 0）。
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_dma_complete_impl(&mut ctx, tok, true, 4u32.to_le_bytes().to_vec());
        }
        // 证明计数：shadow(4) 领先 last_mmio(0) → +1。
        assert_eq!(
            c.dbbuf_shadow_ahead_count, 1,
            "shadow 领先 MMIO → ahead_count 自增（DBBUF 真行使）"
        );
        // 应据 shadow=4 发起 SQE fetch（从 processed=0 到 4，4 条，gpa=SQ1_BASE）。
        let fetched = cap.events()[pre..].iter().any(|e| {
            matches!(e, TransportEvent::DmaRead { gpa, len, .. }
                if *gpa == SQ1_BASE && *len == 4 * SQE_BYTES as u32)
        });
        assert!(fetched, "应据 shadow=4 fetch 4 条 SQE");
        // processed 推进到 4。
        assert_eq!(
            c.sqs.get(&1).unwrap().tail,
            4,
            "sq.tail(processed) 推进到 shadow=4"
        );
        // 写回 event_idx = 4（SQ offset = qid*8；qid=1 → 8）。
        let ei_gpa = EVENTIDX_GPA + 8;
        assert_eq!(
            last_eventidx_write(&cap, pre, ei_gpa),
            Some(4),
            "event_idx 写回 = processed(4)"
        );
        // 推进分支后应**再发一次** shadow 读（re-read 闭合 race）。
        let reread_token = {
            // 找推进后第二次对 shadow 的读。
            let reads: Vec<u64> = cap.events()[pre..]
                .iter()
                .filter_map(|e| match e {
                    TransportEvent::DmaRead { gpa, token, len }
                        if *gpa == sq_shadow_gpa && *len == 4 =>
                    {
                        Some(*token)
                    }
                    _ => None,
                })
                .collect();
            assert!(reads.len() >= 2, "推进后应 re-read shadow（闭合 race）");
            reads[1]
        };

        // 完成 re-read：shadow 仍是 4（无新提交）→ 链应终止（不再 re-read）。
        let pre2 = cap.events().len();
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_dma_complete_impl(&mut ctx, reread_token, true, 4u32.to_le_bytes().to_vec());
        }
        // 终止：不应再有新的 shadow 读。
        assert_eq!(
            count_shadow_reads(&cap, pre2, sq_shadow_gpa),
            0,
            "shadow==processed → 链终止，不再 re-read"
        );
        // inflight 已清。
        assert!(
            !c.shadow_poll_inflight.contains(&(1, false)),
            "链终止后 inflight 清除"
        );
    }

    // ---------------- 3) submit-during-window race（revert 锚点）----------------

    /// **核心 race 用例 / revert 锚点**：controller 读 shadow=4、推进、写 event_idx=4
    /// 后 re-read；在 re-read 完成时 driver 已又提交到 6（窗口内提交，因 event_idx
    /// 让它跳过了 ring）。re-read 必须捕获 6 并继续推进 —— 否则命令 4..6 永不 fetch
    /// → 真 driver 挂死。去掉 `handle_shadow_sq` 的 re-read（step 4）此用例立刻红。
    #[test]
    fn sq_poll_reread_catches_submit_during_window() {
        let mut c = mk_dbbuf();
        let mut cap = CaptureTransport::with_start_token(0x2000);
        let sq_shadow_gpa = SHADOW_GPA + 8;
        let ei_gpa = EVENTIDX_GPA + 8;

        let pre = cap.events().len();
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_sq_tail_doorbell(&mut ctx, 1, 0);
        }
        let tok = first_read_token(&cap, pre, sq_shadow_gpa).unwrap();
        // 第一次 shadow 读 → 4。
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_dma_complete_impl(&mut ctx, tok, true, 4u32.to_le_bytes().to_vec());
        }
        assert_eq!(c.sqs.get(&1).unwrap().tail, 4);
        // 取 re-read token。
        let reread = cap.events()[pre..]
            .iter()
            .filter_map(|e| match e {
                TransportEvent::DmaRead { gpa, token, len }
                    if *gpa == sq_shadow_gpa && *len == 4 =>
                {
                    Some(*token)
                }
                _ => None,
            })
            .nth(1)
            .expect("应有 re-read");

        // **窗口内 driver 又提交到 6** → re-read 完成时读到 6（不是 4）。
        let pre_catch = cap.events().len();
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_dma_complete_impl(&mut ctx, reread, true, 6u32.to_le_bytes().to_vec());
        }
        // 必须继续推进到 6（否则命令 4..6 丢失 → 挂死）。
        assert_eq!(
            c.sqs.get(&1).unwrap().tail,
            6,
            "re-read 捕获窗口内提交(6)并继续推进 —— race 闭合的关键"
        );
        // 应 fetch 4..6（2 条）。
        let fetched_4_6 = cap.events()[pre_catch..].iter().any(|e| {
            matches!(e, TransportEvent::DmaRead { gpa, len, .. }
                if *gpa == SQ1_BASE + 4 * SQE_BYTES && *len == 2 * SQE_BYTES as u32)
        });
        assert!(fetched_4_6, "应 fetch SQE 槽位 4..6");
        // event_idx 推进到 6。
        assert_eq!(last_eventidx_write(&cap, pre_catch, ei_gpa), Some(6));
        // 又一次 re-read（因又推进了）。
        assert!(
            count_shadow_reads(&cap, pre_catch, sq_shadow_gpa) >= 1,
            "再次推进后再 re-read"
        );
        assert_eq!(
            c.dbbuf_shadow_ahead_count, 2,
            "两次 shadow 领先 → ahead_count=2"
        );
    }

    // ---------------- 4) CQ 学 head + 写 CQ event_idx ----------------

    #[test]
    fn cq_poll_learns_head_and_writes_eventidx() {
        let mut c = mk_dbbuf();
        // 装一条 IO CQ(qid=1)。
        c.cqs.insert(
            1,
            crate::regs::CompletionQueue {
                base_gpa: 0x3_0000,
                size: SQ_DEPTH,
                tail: 0,
                phase: 1,
                head: 0,
                interrupt_vector: 1,
                interrupt_enabled: true,
                pending_completions: 0,
                last_fire: None,
            },
        );
        let mut cap = CaptureTransport::with_start_token(0x3000);
        let cq_shadow_gpa = SHADOW_GPA + 8 + 4; // CQ offset = qid*8+4（qid=1 → 12）
        let cq_ei_gpa = EVENTIDX_GPA + 8 + 4;

        let pre = cap.events().len();
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_cq_head_doorbell(&mut ctx, 1, 0); // stale MMIO head
        }
        let tok = first_read_token(&cap, pre, cq_shadow_gpa).expect("应发起 shadow CQ 读");
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_dma_complete_impl(&mut ctx, tok, true, 7u32.to_le_bytes().to_vec());
        }
        // 学到真 head=7。
        assert_eq!(c.cqs.get(&1).unwrap().head, 7, "经 shadow 学到真 CQ head=7");
        // 写 CQ event_idx = 7。
        assert_eq!(
            last_eventidx_write(&cap, pre, cq_ei_gpa),
            Some(7),
            "CQ event_idx 写回 = head(7)"
        );
        // CQ 侧无 re-read 循环 → 仅一次 shadow 读。
        assert_eq!(
            count_shadow_reads(&cap, pre, cq_shadow_gpa),
            1,
            "CQ 侧单次读（无 race 循环）"
        );
        assert!(
            !c.shadow_poll_inflight.contains(&(1, true)),
            "CQ 链终止清 inflight"
        );
    }

    // ---------------- 5) inactive → 纯 MMIO 路径不变；stale value 被忽略 ----------------

    #[test]
    fn dbbuf_inactive_uses_plain_mmio_fetch() {
        let mut c = mk();
        c.csts |= csts::RDY;
        c.sqs.insert(
            1,
            SubmissionQueue {
                base_gpa: SQ1_BASE,
                size: SQ_DEPTH,
                head: 0,
                tail: 0,
                cq_id: 1,
            },
        );
        // DBBUF 未激活（两 GPA 仍 0）。
        assert!(!c.dbbuf_active());
        let mut cap = CaptureTransport::with_start_token(0x4000);
        let pre = cap.events().len();
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_sq_tail_doorbell(&mut ctx, 1, 3); // MMIO value=3 → 直接信任
        }
        // 应直接 fetch 3 条 SQE（从 0 到 3），**无** shadow 读。
        let direct_fetch = cap.events()[pre..].iter().any(|e| {
            matches!(e, TransportEvent::DmaRead { gpa, len, .. }
                if *gpa == SQ1_BASE && *len == 3 * SQE_BYTES as u32)
        });
        assert!(direct_fetch, "inactive：直接按 MMIO value=3 fetch");
        assert_eq!(
            c.sqs.get(&1).unwrap().tail,
            3,
            "inactive：sq.tail = MMIO value"
        );
        // 没有任何对 shadow 区的读。
        assert_eq!(
            count_shadow_reads(&cap, pre, SHADOW_GPA + 8),
            0,
            "inactive：不碰 shadow"
        );
    }

    /// DBBUF active 时 `on_sq_tail_doorbell` 的 MMIO value 仅作"最近 MMIO doorbell"
    /// 记录，**不**作 fetch 依据；真相全凭 shadow。
    #[test]
    fn active_ignores_stale_mmio_value_uses_shadow() {
        let mut c = mk_dbbuf();
        let mut cap = CaptureTransport::with_start_token(0x5000);
        let sq_shadow_gpa = SHADOW_GPA + 8;
        let pre = cap.events().len();
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            // 故意传一个"错"的 MMIO value=2；shadow 才是真值。
            c.on_sq_tail_doorbell(&mut ctx, 1, 2);
        }
        // 记录了 last_mmio=2。
        assert_eq!(c.last_mmio_sq_doorbell.get(&1).copied(), Some(2));
        // 但 **没有** 按 value=2 直接 fetch（active 路径只发 shadow 读）。
        let bad_direct = cap.events()[pre..].iter().any(|e| {
            matches!(e, TransportEvent::DmaRead { gpa, len, .. }
                if *gpa == SQ1_BASE && *len == 2 * SQE_BYTES as u32)
        });
        assert!(!bad_direct, "active：绝不按 stale MMIO value 直接 fetch");
        // sq.tail 仍 0（要等 shadow 读完成才推进）。
        assert_eq!(
            c.sqs.get(&1).unwrap().tail,
            0,
            "active：未读 shadow 前 processed 不动"
        );
        // 完成 shadow 读 = 5 → 按 5（非 2）推进。
        let tok = first_read_token(&cap, pre, sq_shadow_gpa).unwrap();
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_dma_complete_impl(&mut ctx, tok, true, 5u32.to_le_bytes().to_vec());
        }
        assert_eq!(
            c.sqs.get(&1).unwrap().tail,
            5,
            "按 shadow=5 推进（证 shadow 是真相）"
        );
    }

    /// inflight guard：一条链在飞时，重复 doorbell / tick 不再起新链（避免重复 fetch）。
    #[test]
    fn inflight_guard_suppresses_duplicate_chain() {
        let mut c = mk_dbbuf();
        let mut cap = CaptureTransport::with_start_token(0x6000);
        let sq_shadow_gpa = SHADOW_GPA + 8;
        let pre = cap.events().len();
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_sq_tail_doorbell(&mut ctx, 1, 0); // 起链（inflight set）
            // 链尚未完成（shadow 读未回）。再来一次 doorbell + tick：
            c.on_sq_tail_doorbell(&mut ctx, 1, 0);
            c.tick(&mut ctx);
        }
        // 仅 1 次 shadow 读（后两次被 inflight guard 抑制）。
        assert_eq!(
            count_shadow_reads(&cap, pre, sq_shadow_gpa),
            1,
            "inflight 时不起重复链"
        );
    }

    /// disable 清空所有 DBBUF in-flight 状态（含 GPA、inflight、缓存）。
    #[test]
    fn disable_clears_dbbuf_state() {
        let mut c = mk_dbbuf();
        c.shadow_poll_inflight.insert((1, false));
        c.shadow_ring_pending.insert((1, false));
        c.last_mmio_sq_doorbell.insert(1, 3);
        c.last_eventidx_sq.insert(1, 3);
        c.pending_shadow_polls.insert(
            99,
            ShadowPollCtx {
                qid: 1,
                is_cq: false,
                iters: 0,
            },
        );
        c.disable();
        assert_eq!(c.doorbell_shadow_gpa, 0);
        assert_eq!(c.doorbell_event_idx_gpa, 0);
        assert!(!c.dbbuf_active());
        assert!(c.shadow_poll_inflight.is_empty());
        assert!(c.shadow_ring_pending.is_empty());
        assert!(c.pending_shadow_polls.is_empty());
        assert!(c.pending_eventidx_writes.is_empty());
        assert!(c.last_mmio_sq_doorbell.is_empty());
        assert!(c.last_eventidx_sq.is_empty());
    }

    // ---------------- 6) HIGH-1：async transport 下 settle 窗口漏 ring 修复 ----------------

    /// 取 `pending_shadow_polls` 中 token 最大者（= 最近一次 issue 的 re-read）的上下文，
    /// 用于在直接调 `handle_shadow_sq` 的循环里把链状态 `iters`（自续深度）回喂下一步。
    fn latest_shadow_chain_ctx(c: &NvmeController) -> Option<ShadowPollCtx> {
        c.pending_shadow_polls
            .iter()
            .max_by_key(|(tok, _)| **tok)
            .map(|(_, ctx)| *ctx)
    }

    /// **HIGH-1 核心 race / revert 锚点**：OpenHCL 真异步 transport 下，doorbell-ring 帧
    /// 与 re-read 的 DmaCompletion 帧逐条交错。可达漏 ring 链（reviewer 步骤 1-5）：
    ///
    /// 1. controller 已推进到 P 并发出 re-read R（eventidx=P，inflight 持有）。
    /// 2. host 服务 R 时 driver 尚未存 shadow → R 完成将携带 shadow==processed==P。
    /// 3. driver 提交槽 P：shadow:=P+1、读 eventidx=P、need_event(P,P+1,P)=true → **ring**。
    /// 4. ring 帧先被 dispatch → `start_shadow_sq_poll` 见链在飞 → **置 pending 标记**
    ///    （旧代码静默 return → ring 丢失）。
    /// 5. R 完成 → `handle_shadow_sq` 见 shadow(P)==processed(P) → settle 分支：**因
    ///    pending 标记置位 → 续一次 re-read** 而非 settle（旧代码直接 settle → 槽 P 要等
    ///    下一次 ~5s tick 才 fetch = 延迟失速）。续读捕获 P+1 → 槽 P 被 fetch。
    ///
    /// **revert-verify**：去掉修复（settle 分支不查 pending / start 见 inflight 静默 return）
    /// 后，第 5 步直接 settle 不续读 → 续读断言失败、槽 P 不被 fetch → 本用例红。
    #[test]
    fn high1_dropped_ring_during_settle_window_is_recovered() {
        const P: u32 = 4;
        let mut c = mk_dbbuf();
        let mut cap = CaptureTransport::with_start_token(0x7000);
        let sq_shadow_gpa = SHADOW_GPA + 8;

        // —— 步骤 1：把 controller 推进到 processed=P 并令 re-read R 在飞 ——
        // doorbell（DBBUF 下 MMIO value 仅作唤醒）。
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_sq_tail_doorbell(&mut ctx, 1, 0);
        }
        let first_tok = first_read_token(&cap, 0, sq_shadow_gpa).expect("应起 shadow 读");
        // 完成首读 = P → 推进到 P、写 eventidx=P、发 re-read R。
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_dma_complete_impl(&mut ctx, first_tok, true, P.to_le_bytes().to_vec());
        }
        assert_eq!(c.sqs.get(&1).unwrap().tail, P, "processed 推进到 P");
        assert!(
            c.shadow_poll_inflight.contains(&(1, false)),
            "re-read R 在飞，inflight 持有"
        );
        // 取 R 的 token（推进后第 2 次 shadow 读）。
        let reread_r = cap.events()[0..]
            .iter()
            .filter_map(|e| match e {
                TransportEvent::DmaRead { gpa, token, len }
                    if *gpa == sq_shadow_gpa && *len == 4 =>
                {
                    Some(*token)
                }
                _ => None,
            })
            .nth(1)
            .expect("应有 re-read R");

        // —— 步骤 4：ring 帧先于 R 的完成被 dispatch（链在飞）→ 置 pending 标记 ——
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_sq_tail_doorbell(&mut ctx, 1, P); // driver ring（value 在 DBBUF 下被忽略）
        }
        assert!(
            c.shadow_ring_pending.contains(&(1, false)),
            "链在飞期间的 ring 被记成 pending（而非静默丢弃）"
        );

        // —— 步骤 5：R 完成，观察到 shadow==processed==P（driver 尚未让 host 看到 P+1）——
        let pre_settle = cap.events().len();
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_dma_complete_impl(&mut ctx, reread_r, true, P.to_le_bytes().to_vec());
        }
        // 修复：settle 窗口见 pending → **续一次 re-read** 而非 settle。
        assert_eq!(
            count_shadow_reads(&cap, pre_settle, sq_shadow_gpa),
            1,
            "settle 窗口的漏 ring → 续一次 re-read（HIGH-1 修复）"
        );
        assert!(
            !c.shadow_ring_pending.contains(&(1, false)),
            "pending 一次性消费：续读后清除"
        );
        assert!(
            c.shadow_poll_inflight.contains(&(1, false)),
            "续读后链仍在飞（未 settle）"
        );
        // 取续读 token。
        let reread_r2 = latest_shadow_chain_ctx(&c)
            .map(|_| {
                cap.events()[pre_settle..]
                    .iter()
                    .find_map(|e| match e {
                        TransportEvent::DmaRead { gpa, token, len }
                            if *gpa == sq_shadow_gpa && *len == 4 =>
                        {
                            Some(*token)
                        }
                        _ => None,
                    })
                    .expect("续读 token")
            })
            .expect("续读应已登记到 pending_shadow_polls");

        // —— 续读完成：driver 的 P+1 现已可见 → 推进到 P+1、**fetch 槽 P** ——
        let pre_fetch = cap.events().len();
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_dma_complete_impl(&mut ctx, reread_r2, true, (P + 1).to_le_bytes().to_vec());
        }
        assert_eq!(
            c.sqs.get(&1).unwrap().tail,
            P + 1,
            "续读捕获窗口内提交(P+1)并推进 —— 槽 P 不再被搁置到下一 tick"
        );
        // 断言槽 P 的 SQE 被 fetch（1 条，gpa = SQ1_BASE + P*SQE_BYTES）。
        let fetched_slot_p = cap.events()[pre_fetch..].iter().any(|e| {
            matches!(e, TransportEvent::DmaRead { gpa, len, .. }
                if *gpa == SQ1_BASE + P as u64 * SQE_BYTES && *len == SQE_BYTES as u32)
        });
        assert!(
            fetched_slot_p,
            "槽 P 的 SQE 被 fetch（HIGH-1：漏 ring 已转成续读捕获）"
        );
    }

    /// HIGH-1 CQ 侧对位：链在飞期间漏的 CQ ring 在 settle 时续一次 CQ 读（学到新 head），
    /// 而非等下一 tick。
    #[test]
    fn high1_cq_dropped_ring_recovered() {
        let mut c = mk_dbbuf();
        c.cqs.insert(
            1,
            crate::regs::CompletionQueue {
                base_gpa: 0x3_0000,
                size: SQ_DEPTH,
                tail: 0,
                phase: 1,
                head: 0,
                interrupt_vector: 1,
                interrupt_enabled: true,
                pending_completions: 0,
                last_fire: None,
            },
        );
        let mut cap = CaptureTransport::with_start_token(0x7800);
        let cq_shadow_gpa = SHADOW_GPA + 8 + 4;

        // 起 CQ 链。
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_cq_head_doorbell(&mut ctx, 1, 0);
        }
        let tok = first_read_token(&cap, 0, cq_shadow_gpa).expect("应起 CQ shadow 读");
        // 链在飞期间又来一次 CQ ring → 置 pending。
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_cq_head_doorbell(&mut ctx, 1, 0);
        }
        assert!(
            c.shadow_ring_pending.contains(&(1, true)),
            "CQ 链在飞期间的 ring 记成 pending"
        );
        // 完成首读 head=3 → settle 见 pending → 续一次 CQ 读。
        let pre = cap.events().len();
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.on_dma_complete_impl(&mut ctx, tok, true, 3u32.to_le_bytes().to_vec());
        }
        assert_eq!(c.cqs.get(&1).unwrap().head, 3, "学到 head=3");
        assert_eq!(
            count_shadow_reads(&cap, pre, cq_shadow_gpa),
            1,
            "CQ 漏 ring → settle 时续一次 CQ 读（HIGH-1）"
        );
        assert!(
            !c.shadow_ring_pending.contains(&(1, true)),
            "CQ pending 一次性消费"
        );
    }

    // ---------------- 7) HIGH-2：自续深度上限（host-thread liveness）----------------

    /// **HIGH-2 / live-cap 锚点**：上限界定**单条链的自续深度**（本链发出的 re-read 总数），
    /// 为同步 vfio drain 的 host-thread liveness 而设。两段证"深度 ≪ cap 的健康负载不误触"：
    /// (a) **单条 modest-深度（≪ cap）链**做 1023 步**前进**推进（size=1024，不 wrap），
    ///     回喂 `iters`，断言每步 `iters == step_tail`（**live 增量锚点**：自续深度逐步 +1）
    ///     且全程 CFS 未置（1023 ≪ 1<<16 → 不撞顶）。**revert-verify** — 删掉 advance 分支的
    ///     `iters += 1`（cap 退化），`iters` 恒 0 → `iters == step_tail` 断言（step≥1 时）立刻红。
    /// (b) 跨**多条短链**累计推进数 **远超** `MAX_SHADOW_POLL_ITERS`（1<<16），断言 CFS 始终
    ///     未置 —— 证**每条链在 chain start 重置深度**，故"持续高负载 over time"无论总量多大
    ///     都不触 cap（这是 per-chain 深度上限下"sustained load 不误触"的真正保证）。
    #[test]
    fn high2_sustained_progress_never_trips_cfs() {
        // —— (a) 单条 modest 链：size=1024，前进 1..=1023 不 wrap，深度逐步 +1 ——
        let mut c = mk();
        c.csts |= csts::RDY;
        const BIG_DEPTH: u32 = 1024;
        c.sqs.insert(
            1,
            SubmissionQueue {
                base_gpa: SQ1_BASE,
                size: BIG_DEPTH,
                head: 0,
                tail: 0,
                cq_id: 1,
            },
        );
        c.doorbell_shadow_gpa = SHADOW_GPA;
        c.doorbell_event_idx_gpa = EVENTIDX_GPA;
        let mut cap = CaptureTransport::with_start_token(0x8000);
        c.shadow_poll_inflight.insert((1, false));
        let mut iters = 0u32;
        for step_tail in 1..BIG_DEPTH {
            {
                let mut ctx = DeviceCtx::new(&mut cap);
                c.handle_shadow_sq(&mut ctx, 1, step_tail, iters);
            }
            let cx = latest_shadow_chain_ctx(&c).expect("续读应登记");
            iters = cx.iters;
            // **live 增量锚点**：每发一次自续 re-read，自续深度 +1（step_tail 从 1 起，
            // 第 k 步发出第 k 次自续 → iters == k == step_tail）。
            assert_eq!(iters, step_tail, "自续深度逐步 +1（step_tail={step_tail}）");
            assert_eq!(c.csts & csts::CFS, 0, "深度 ≪ cap 全程不触 CFS");
            c.pending_shadow_polls.clear(); // 防 map 膨胀
        }

        // —— (b) 多条短链，总推进 > cap，仍不触 CFS（每条链 chain start 重置深度）——
        let mut c2 = mk_dbbuf();
        let mut cap2 = CaptureTransport::with_start_token(0x8400);
        let total_advances_target: u64 = (1u64 << 16) + 5_000;
        let mut advances: u64 = 0;
        while advances < total_advances_target {
            c2.sqs.get_mut(&1).unwrap().tail = 0;
            c2.shadow_poll_inflight.insert((1, false));
            let mut it = 0u32; // 新链：深度从 0 起（模拟 start_shadow_sq_poll 的 seed）
            for step_tail in 1..=4u32 {
                {
                    let mut ctx = DeviceCtx::new(&mut cap2);
                    c2.handle_shadow_sq(&mut ctx, 1, step_tail, it);
                }
                advances += 1;
                let cx = latest_shadow_chain_ctx(&c2).expect("续读应登记");
                it = cx.iters;
            }
            {
                let mut ctx = DeviceCtx::new(&mut cap2);
                c2.handle_shadow_sq(&mut ctx, 1, 4, it); // settle
            }
            c2.pending_shadow_polls.clear();
        }
        assert!(advances > (1 << 16), "总推进确超 cap（{advances} > 65536）");
        assert_eq!(
            c2.csts & csts::CFS,
            0,
            "持续真进展（>65536 次，跨多链、每链深度重置）绝不触 CFS（HIGH-2）"
        );
    }

    /// **HIGH-2 (a)：realistic max-depth wrap SETTLES → 永不误触**。最深队列（`size =
    /// MAX_MAX_QUEUE_ENTRIES = 65536`）上，模拟 driver 每个 round-trip 提交一批 SQE
    /// （大步前进），跨**一两整圈** ring wrap，最后 `shadow == processed` settle。断言
    /// 全程 CFS CLEAR、自续深度始终 ≪ cap、链最终 settle（inflight 清除）。
    ///
    /// **为何关键**：证明真实重负载（max-depth 队列被反复 wrap）在**自续深度上限**下
    /// **不**误触——一条正确 driver 的链在 ~几十次 re-read 内 settle（这里 ~32 步跨 2 圈，
    /// 远 ≪ 1<<16）。深度上限只 bound **自喂不收敛**的 runaway（见配套的 oscillation 用例），
    /// 不碰会 settle 的健康 wrap 负载。
    #[test]
    fn high2_realistic_maxdepth_wrap_settles_never_trips_cfs() {
        let mut c = mk();
        c.csts |= csts::RDY;
        const MAXD: u32 = MAX_MAX_QUEUE_ENTRIES; // 65536
        c.sqs.insert(
            1,
            SubmissionQueue {
                base_gpa: SQ1_BASE,
                size: MAXD,
                head: 0,
                tail: 0,
                cq_id: 1,
            },
        );
        c.doorbell_shadow_gpa = SHADOW_GPA;
        c.doorbell_event_idx_gpa = EVENTIDX_GPA;
        let mut cap = CaptureTransport::with_start_token(0xA000);
        c.shadow_poll_inflight.insert((1, false));

        // 每 round-trip driver 推进 STRIDE 条（< size，故每步 shadow != processed = 真前进）。
        // 走 STEPS 步使总前向距离 ≈ STEPS*STRIDE 跨过 ~2 整圈（2*size = 131072）。
        const STRIDE: u32 = 4096;
        const STEPS: u32 = 2 * MAXD / STRIDE + 4; // ~36 步：越过 2 整圈
        let mut iters = 0u32;
        let mut prev: u32 = 0;
        for step in 0..STEPS {
            let shadow = (prev + STRIDE) % MAXD;
            {
                let mut ctx = DeviceCtx::new(&mut cap);
                c.handle_shadow_sq(&mut ctx, 1, shadow, iters);
            }
            assert_eq!(
                c.csts & csts::CFS,
                0,
                "健康 wrap 负载不得触 CFS（step={step} shadow={shadow}）"
            );
            let cx = latest_shadow_chain_ctx(&c).expect("续读应登记");
            iters = cx.iters;
            // 自续深度逐步 +1，但始终 ≪ cap（这条链 ~36 步就跨完 2 圈）。
            assert_eq!(iters, step + 1, "自续深度逐步 +1（step={step}）");
            assert!(
                iters < NvmeController::MAX_SHADOW_POLL_ITERS,
                "健康链深度始终 ≪ cap（iters={iters}）"
            );
            prev = shadow;
            c.pending_shadow_polls.clear(); // 防 map 膨胀
        }
        // 现在 driver 不再提交：喂 shadow == processed（= prev）→ settle 分支。
        // processed 此刻 == prev（上一步 advance 到 prev）。无 pending → 链 settle。
        assert_eq!(c.sqs.get(&1).unwrap().tail, prev, "processed 已追到 prev");
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.handle_shadow_sq(&mut ctx, 1, prev, iters);
        }
        assert_eq!(
            c.csts & csts::CFS,
            0,
            "跨 2 整圈后 settle，全程 CFS 仍 CLEAR（深度上限不误触健康 wrap）"
        );
        assert!(
            !c.shadow_poll_inflight.contains(&(1, false)),
            "shadow==processed 且无 pending → 链 settle（inflight 清除）"
        );
    }

    /// **HIGH-2 (b)：in-range OSCILLATING 链 → 在 cap 处 TRIP CFS + 收链**（核心 blocker
    /// 回归 + live-cap revert 锚点）。
    ///
    /// **blocker（reviewer 已确认、可达）**：vfio `dma_read` 同步——`handle_shadow_sq` 里
    /// 每发一次自续 re-read，同一次 `drain_dma_completions` 立刻取到它的 completion。一个
    /// in-range 震荡 guest shadow（2↔4，皆 < size；`advance_sq_to_tail` 把回退当 wrap 仍前进）
    /// 令 `shadow != processed` 步步成立 → 自喂 drain 循环**无逃逸** → 旧 dead cap 下 host
    /// 线程**永久** spin。新 live cap 在自续深度撞 `MAX_SHADOW_POLL_ITERS` 时置 CFS + 收链，
    /// 把它 bound 成有限 ≤ 65536 次往返后停。
    ///
    /// **合成驱动**（**关键**：TEST 自身**不**能 infinite-loop）：经 `handle_shadow_sq`
    /// 直接喂一段**有界**的震荡 shadow 序列、回喂自续深度，**不**依赖真同步 drain（那会
    /// 把测试挂死）。硬上界 `feed_cap = MAX + 8` 兜底——若链未在界内自终止（CFS）即 panic
    /// （= 探测到非终止）。
    ///
    /// **revert-verify**：把 advance 分支的 live `iters += 1`（cap 增量）删掉（cap 退化成
    /// dead），震荡链在所喂序列内**永不** trip → 下方 "CFS set" 断言失败（或经 `feed_cap`
    /// 兜底计数探测到非终止）；恢复增量 → 通过。
    #[test]
    fn high2_inrange_oscillating_chain_trips_cfs_at_cap() {
        let mut c = mk_dbbuf(); // SQ size = SQ_DEPTH(64)
        let mut cap = CaptureTransport::with_start_token(0xB000);
        c.shadow_poll_inflight.insert((1, false));

        // 震荡：2↔4（皆 < 64）。每步 shadow != processed → 永走 advance 分支 → 自续 +1。
        const OSC: [u32; 2] = [2, 4];
        // 链 start 已 seed 深度 0（issue read#0）；此处从首个 completion 起喂。
        let mut iters = 0u32;
        let mut tripped_at: Option<u32> = None;
        // **有界**喂入：最多 MAX+8 次（远超必触点 = 第 MAX 次）。链应在界内自终止。
        let feed_cap: u32 = NvmeController::MAX_SHADOW_POLL_ITERS + 8;
        for i in 0..feed_cap {
            let shadow = OSC[(i as usize) & 1];
            // 记录本次"进入 handle_shadow_sq 的 incoming iters"——撞顶发生在 incoming
            // iters == MAX-1（此时 next_iters == MAX → trip）。
            let incoming = iters;
            {
                let mut ctx = DeviceCtx::new(&mut cap);
                c.handle_shadow_sq(&mut ctx, 1, shadow, incoming);
            }
            if c.csts & csts::CFS != 0 {
                tripped_at = Some(incoming);
                break;
            }
            // 未 trip：取本链续读的自续深度回喂。
            let cx = latest_shadow_chain_ctx(&c).expect("未 trip 时应已发续读");
            iters = cx.iters;
            c.pending_shadow_polls.clear(); // 防 map 膨胀（不影响链状态，已回喂 iters）
        }

        // 必须在有界序列内 TRIP（否则 = 非终止 / cap dead → revert-verify 红）。
        let trip_incoming = tripped_at
            .expect("in-range 震荡链必在喂入界内 trip CFS（若未 trip = live cap 失效 / 非终止）");
        // **精确**：trip 发生在自续深度撞 cap 的那一步，即 incoming iters == MAX-1
        // （next_iters == MAX）。证"在 EXACTLY MAX_SHADOW_POLL_ITERS 次 re-read 处终止"。
        assert_eq!(
            trip_incoming,
            NvmeController::MAX_SHADOW_POLL_ITERS - 1,
            "trip 发生在第 MAX 次自续 re-read（incoming depth == MAX-1 → next == MAX）"
        );
        assert_ne!(
            c.csts & csts::CFS,
            0,
            "震荡 runaway 撞自续深度上限 → 置 CSTS.CFS"
        );
        // 撞顶后收链：inflight + pending 清除（与越界路径同款 fail-safe shutdown）。
        assert!(
            !c.shadow_poll_inflight.contains(&(1, false)),
            "trip 后收链（inflight 清除）"
        );
        assert!(
            !c.shadow_ring_pending.contains(&(1, false)),
            "trip 后 pending 标记一并清除"
        );
    }

    /// **HIGH-2 garbage guard**：自续深度上限只 bound **in-range** runaway（震荡/in-range
    /// garbage 步步 `shadow != processed` 自喂，见配套 oscillating 用例——在 cap 处 trip）。
    /// **越界** garbage（`shadow >= size`）则由 `handle_shadow_sq` 顶部的**越界守卫**单步
    /// 即时置 CSTS.CFS 并收链，不必等深度上限。本用例锚定该越界守卫仍生效——它与深度上限
    /// 互补，覆盖"一步即非法"的脏 shadow。
    ///
    /// **revert-verify**：若删掉 `handle_shadow_sq` 顶部的 `shadow >= size` 越界守卫，
    /// 本用例的 CFS 断言立刻红（越界值不再被拦）。
    #[test]
    fn high2_out_of_range_shadow_trips_cfs_garbage_guard() {
        let mut c = mk_dbbuf(); // SQ size = SQ_DEPTH(64)
        let mut cap = CaptureTransport::with_start_token(0x9000);
        c.shadow_poll_inflight.insert((1, false));
        // 越界 shadow（>= size=64）：driver bug / 脏 shadow / 恶意值。单步即应置 CFS 收链。
        let garbage = SQ_DEPTH; // == size → 越界（合法 tail ∈ [0, size)）
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            c.handle_shadow_sq(&mut ctx, 1, garbage, 0);
        }
        assert_ne!(
            c.csts & csts::CFS,
            0,
            "越界 shadow（garbage）→ 经 shadow>=size 越界守卫立即置 CFS"
        );
        // 触发后链已收：inflight + pending 清除。
        assert!(
            !c.shadow_poll_inflight.contains(&(1, false)),
            "garbage 触 CFS 后收链（inflight 清除）"
        );
        assert!(
            !c.shadow_ring_pending.contains(&(1, false)),
            "garbage 触 CFS 后 pending 标记一并清除"
        );
    }
}
