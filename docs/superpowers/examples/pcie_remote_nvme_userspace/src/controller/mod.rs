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
//! Phase H2 暴露了 4 个 IO queue (IO_QUEUE_CAP)；当前 dispatch 模型是
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
mod enable;
mod io;
mod logs;
mod mmio;
mod reservation;

use crate::cmd::*;
use crate::regs::*;
use pcie_remote_userspace_sdk::*;
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

/// **Phase H2** — controller 最多授予 driver 的 IO queue 对数（SQ+CQ）。
/// 真硬件常 8-128；教学 4 足够展示并发模型，每队列独立 dispatch。
pub(super) const IO_QUEUE_CAP: u16 = 4;

// 注：本实现用 SDK 分配的 raw DMA token 直接作 HashMap key 路由完成回调；
// 不再做 token 高位 tagging（早期设计想用 tag 标 op 类别，实测 raw token
// 已唯一，多此一举）。

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
    NvmWriteDmaRead { lba: u64, num_blocks: u32 },
    /// 双 PRP Write：PRP1 段 DMA-read 完成 → 填到 `dual_prp_writes[op_id].prp1_data`，
    /// 两段都到了就触发 dispatch_dual_prp。**op_id 与 PRP2 共用**：靠
    /// `dual_prp_writes[op_id]` 中的状态决定何时写盘 + 发 CQE。
    NvmWriteDualPrp { op_id: u64, is_prp1: bool },
    /// 等 DMA-write 数据到 PRP1 完成 → success CQE。
    /// （NVM Read 路径 + Admin Identify / Get Log Page 共用入口）
    /// `num_blocks` = NVM Read 时 LBA 数；admin（Identify/Log）= 0，
    /// 用于 SMART 统计区分（Phase F：只算真 IO，不算 admin 元数据）。
    NvmReadDmaWrite { num_blocks: u32 },
    /// **H4 修复** — 双 PRP Read 的"非记账段" sibling（成功时静默；失败时
    /// 经过通用 DMA-fail 路径 post error CQE）。设计：tok2 走
    /// `NvmReadDmaWrite { num_blocks: nlb }` 负责真正 success CQE +
    /// counter；tok1 走此变体只为捕获失败。
    NvmReadDualPrpSiblingHalf,
    /// **Phase E** — NVM Write with PRP list (> 2 page)。
    /// Step 1: DMA-read PRP list page itself（4 KiB u64 数组）。
    NvmWritePrpListFetch { op_id: u64 },
    /// **Phase E** — NVM Write with PRP list, Step 2: per-page data DMA-read。
    /// `page_idx` 是 PRP 中第几个数据页（PRP1=0，PRP list[0]=1，list[1]=2，…）。
    NvmWritePrpListData { op_id: u64, page_idx: u32 },
    /// **Phase E** — NVM Read with PRP list, Step 1: fetch PRP list 数组本身。
    NvmReadPrpListFetch { op_id: u64 },
    /// **Phase E** — NVM Read with PRP list, Step 2: per-page data DMA-write。
    /// `page_idx` 是 PRP 中第几个数据页。
    NvmReadPrpListData { op_id: u64, page_idx: u32 },
    /// **Phase H3** — NVM Compare：DMA-read host buffer 完成后与 backing
    /// LBA 对比。`lba/num_blocks` 用于 file seek+read；对比失败返
    /// COMPARE_FAILURE (SC 0x85, SCT=0x02 Media/Data Integrity)。
    /// 单 PRP 路径（≤ 1 page）。
    NvmCompareSinglePrp { lba: u64, num_blocks: u32 },
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
    AdminFwDownloadChunk { offset_bytes: u32 },
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
    NvmCompareDualPrp { op_id: u64, is_prp1: bool },
    /// **Phase K2** — Compare PRP list (> 2 page)：先 fetch list 页本身。
    NvmComparePrpListFetch { op_id: u64 },
    /// **Phase K2** — Compare PRP list per-page data DMA-read。
    NvmComparePrpListData { op_id: u64, page_idx: u32 },
    /// **Phase K9** — Set Features 0x81 Host Identifier DMA-read 完成。
    /// cdw11 bit 0 EXHID = 1 → 16 byte HOSTID；= 0 → 8 byte。
    AdminSetHostIdentifier { exhid: bool },
    /// **Phase S4** — NS Attachment SEL Attach/Detach 的 Controller List
    /// (4 KiB DMA-read) 完成回调。`sel`: 0=Attach 1=Detach。读完后解析
    /// NumIDs + cntlid 列表，若包含本 controller cntlid (=1)，更新目标
    /// NSID 的 attached 状态。
    AdminNsAttachmentList { sel: u8 },
    /// **Phase K4a** — PI Write 完成回调：DMA-read 完成后按 LBA 切 4KiB
    /// data，每 LBA 计算 T10 DIF tuple，interleave 写到 backing file
    /// (data + 8B tuple per LBA)。支持任意 nlb（≤ MDTS）。
    /// 限制：单 PRP 路径（bytes ≤ NVME_PAGE_SIZE = 4 KiB = 1 LBA when
    /// lbads=12）。多 LBA 需 dual-PRP / PRP list — 留 K4c。
    NvmWritePi { lba: u64, num_blocks: u32 },
    /// **Phase K4b** — PI Read sibling 占位（per-LBA file read + verify
    /// 在 dispatch 时同步完成，DMA-write 数据回 PRP1 在 PendingIo 路径）。
    NvmReadPiDmaWrite { num_blocks: u32 },
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
    NvmWritePiMulti { op_id: u64, page_idx: u32 },
    /// **Phase K4c-list** — PI Write PRP-list fetch：PRP2 指向的 list 页
    /// 已 DMA-read 到 data；解析 u64 array → 逐页 DMA-read 数据。
    NvmWritePiListFetch { op_id: u64 },
    /// **Phase K4c-list** — PI Read PRP-list fetch：list 页 DMA-read 完成；
    /// 解析 u64 array → 把 backing 已 verified 的 data per-page DMA-write
    /// 到 host 各页。
    NvmReadPiListFetch { op_id: u64 },
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
    NvmCopyFetchRanges { sdlba: u64, num_ranges: u32 },
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

/// **Phase H4 + K1** — 单个 namespace 状态。
///
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
    #[allow(dead_code)] // Phase K4 真 PI/4K IO 路径会用
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
    pub(super) namespaces: HashMap<u32, Namespace>,

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
    /// SQ ID → queue。Admin = ID 0；IO = ID 1..
    sqs: HashMap<u16, SubmissionQueue>,
    /// CQ ID → queue。
    cqs: HashMap<u16, CompletionQueue>,

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
    /// **Phase K2** — Compare > 1 page 累积（dual PRP / PRP list）。
    pub(super) compare_ops: HashMap<u64, CompareAccum>,
    /// **Phase K4c** — 多 LBA PI Write 累积。op_id → 全 data + 完成进度。
    pub(super) pi_writes: HashMap<u64, PiWriteAccum>,
    /// **Phase K4c-list** — 多 LBA PI Read PRP-list 累积器。op_id → 已
    /// verified data + DMA-write to host 进度。
    pub(super) pi_reads: HashMap<u64, PiReadAccum>,
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

    // ----- Phase F: AEN (Async Event Notification) -----
    /// AsyncEventRequest 已收到、待 fire 的 admin context FIFO。
    /// 当有事件触发时弹一条，构造 CQE 带 event info → post 到 admin CQ。
    /// 元组：(cid, sq_id, cq_id)；sq_head 不存（fire 时取实时值，
    /// 避免 stale sqhd 触发 spec § 4.6.1.4 单调违规 — Phase F H2 修复）。
    pub(super) aen_pending: std::collections::VecDeque<(u16, u16, u16)>,

    /// **Phase H1** — Set Features 写过的 cdw11 值，Get Features 时回填。
    /// fid → cdw11。NVMe spec § 5.21.1：driver 通过 Set 配置 controller
    /// 行为，必须能 Get 回。少量 fid (0x07 NumberOfQueues / 0x06 VWC) 由
    /// controller 强约束返实际值（不简单回填 stored）；其余 fid 走 stored
    /// 路径。Set 后立即生效（spec 'persistent across reset' bit 默认 0，
    /// 所以 reset 时清掉）。
    pub(super) features: std::collections::HashMap<u8, u32>,
    /// **Phase H2** — Driver 通过 Set Features 0x07 请求的 IO queue 数；
    /// controller 在 enable() 时实际授予 max(requested, IO_QUEUE_CAP) 个 SQ/CQ。
    /// 默认 4 SQ + 4 CQ，体现多 queue 并发模型。请求大于 cap 被限制到 cap。
    pub(super) granted_io_queues: u16,
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

    // ----- Phase K6: Doorbell Buffer Config -----
    /// driver 提供的 shadow doorbell buffer GPA（PRP1）+ event idx buffer
    /// (PRP2)。我们存下来但不做 polling（vsock 模型 MMIO 已是事件源）。
    #[allow(dead_code)]
    pub(super) doorbell_shadow_gpa: u64,
    #[allow(dead_code)]
    pub(super) doorbell_event_idx_gpa: u64,
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
        let mut namespaces: HashMap<u32, Namespace> = HashMap::new();
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
            cap: build_cap(64),
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
            sqs: HashMap::new(),
            cqs: HashMap::new(),
            pending_fetches: HashMap::new(),
            pending_ios: HashMap::new(),
            dual_prp_writes: HashMap::new(),
            next_op_id: 1,
            prp_list_ops: HashMap::new(),
            compare_ops: HashMap::new(),
            pi_writes: HashMap::new(),
            pi_reads: HashMap::new(),
            pending_fused: HashMap::new(),
            sqe_inbox: Vec::new(),
            stat_host_reads: 0,
            stat_host_writes: 0,
            stat_lba_read: 0,
            stat_lba_written: 0,
            power_on_instant: std::time::Instant::now(),
            stat_num_err_log_entries: 0,
            aen_pending: std::collections::VecDeque::new(),
            features: std::collections::HashMap::new(),
            granted_io_queues: IO_QUEUE_CAP,
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

    /// 计算 doorbell offset 是 SQ 还是 CQ + queue id。
    /// NVMe 1.4 § 3.1.7：doorbell 数组从 BAR0 + 0x1000 起，stride = 2^(2+CAP.DSTRD)。
    /// CAP.DSTRD=0 → stride=4 bytes。
    /// 序列：SQ0TDBL, CQ0HDBL, SQ1TDBL, CQ1HDBL, ...
    /// offset = 0x1000 + (2*qid + (0 if SQ else 1)) * 4
    fn parse_doorbell(offset: u64) -> Option<(bool, u16)> {
        if offset < 0x1000 {
            return None;
        }
        let idx = (offset - 0x1000) / 4;
        let is_cq = (idx & 1) != 0;
        let qid = (idx / 2) as u16;
        Some((!is_cq, qid)) // returns (is_sq, qid)
    }

    /// SQyTDBL 写入：driver 通告新 SQE。host 立即 DMA-read 新 entries。
    /// 支持 wrap：[old_tail..size) + [0..new_tail) 拆两段独立 DMA fetch。
    fn on_sq_tail_doorbell(&mut self, ctx: &mut DeviceCtx<'_>, sq_id: u16, new_tail: u32) {
        // 先校验：spec 要求 0 ≤ new_tail < size；越界视为 driver bug，
        // 设 CSTS.CFS 让 driver 见到 fatal 状态。在写 sq.tail 前校验，
        // 否则脏 state 已经在 SQ 中持久化。
        let (base_gpa, size, old_tail) = {
            let Some(sq) = self.sqs.get(&sq_id) else {
                tracing::warn!(sq_id, "SQ tail doorbell to unknown SQ");
                return;
            };
            if new_tail >= sq.size {
                tracing::error!(
                    sq_id,
                    new_tail,
                    size = sq.size,
                    "SQ doorbell out of range; setting CSTS.CFS"
                );
                self.csts |= csts::CFS;
                return;
            }
            (sq.base_gpa, sq.size, sq.tail)
        };
        // 校验通过后才写 sq.tail。
        self.sqs.get_mut(&sq_id).unwrap().tail = new_tail;
        if old_tail == new_tail {
            return;
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
    }

    /// CQyHDBL 写入：driver 通告已处理多少 CQE。host 仅用于流控（v1 不做）。
    fn on_cq_head_doorbell(&mut self, cq_id: u16, new_head: u32) {
        if let Some(cq) = self.cqs.get_mut(&cq_id) {
            cq.head = new_head;
            tracing::debug!(cq_id, new_head, "CQ head doorbell");
        }
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
            let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0);
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
                            0,
                        );
                        self.post_cqe(ctx, cq_id, cqe1);
                    }
                }
                1 => {
                    // FUSE_FIRST — 必须是 Compare (NVMe spec 唯一定义的 fused pair)
                    if opc != nvm_opc::COMPARE {
                        tracing::warn!(opc, "Fused FIRST is not Compare → INVALID_FIELD");
                        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
                        let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0);
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
                            0,
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
                        let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0);
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
                            Cqe::error(first.cid(), sq_id, first_head, phase, sc::INVALID_FIELD, 0),
                        );
                        let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0);
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
                            Cqe::error(first.cid(), sq_id, first_head, phase, sc::INVALID_FIELD, 0),
                        );
                        let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0);
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
                    let bytes = first_nlb as u64 * SECTOR_SIZE;
                    if bytes > NVME_PAGE_SIZE {
                        tracing::warn!(
                            slba = first_slba,
                            nlb = first_nlb,
                            bytes,
                            "Fused C+W > 1 page not supported"
                        );
                        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
                        self.post_cqe(
                            ctx,
                            cq_id,
                            Cqe::error(first.cid(), sq_id, first_head, phase, sc::INVALID_FIELD, 0),
                        );
                        let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0);
                        self.post_cqe(ctx, cq_id, cqe);
                        return;
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
                    let cqe = Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0);
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
                    0,
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
                    0,
                ),
            );
            return;
        }
        // DMA-read host Compare 数据 → NvmCompareSinglePrpFused 完成回调
        let bytes = nlb as u64 * SECTOR_SIZE;
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
        // M5：type 只占 3 bit，> 7 是 caller bug → 早 fail。
        debug_assert!(aen_type < 8, "AEN type must be < 8 (spec § 5.2 Figure 174)");
        let Some((cid, sq_id, cq_id)) = self.aen_pending.pop_front() else {
            tracing::debug!(
                aen_type,
                aen_info,
                log_id,
                "fire_aen: no pending AER, event dropped"
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
        let bytes = num_blocks as u64 * SECTOR_SIZE;
        let mut host_data = host_prp1;
        if let Some(extra) = host_prp2 {
            host_data.extend_from_slice(&extra);
        }
        host_data.truncate(bytes as usize);
        let mut backing_buf = vec![0u8; bytes as usize];
        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
        let Some(ns) = self.namespaces.get_mut(&nsid) else {
            return Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_NAMESPACE, 0);
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
        match ns.read_at(&mut backing_buf, lba * SECTOR_SIZE) {
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
                    self.push_error_log(sq_id, cid, (sc::COMPARE_FAILURE as u16) << 1, lba, nsid);
                    Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::COMPARE_FAILURE,
                        sc::SCT_MEDIA_DATA_INTEGRITY,
                    )
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, nsid, lba, "Compare: backing read failed");
                self.stat_num_err_log_entries += 1;
                self.push_error_log(sq_id, cid, (sc::DATA_TRANSFER_ERROR as u16) << 1, lba, nsid);
                Cqe::error(cid, sq_id, sq_head, phase, sc::DATA_TRANSFER_ERROR, 0)
            }
        }
    }

    /// 帮助函数：DMA-write `data` 到 `gpa`，完成后构造 success CQE 提交。
    /// 用 PendingOp::NvmReadDmaWrite 通用入口（Identify 也走这条）。
    fn dma_write_then_complete(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        gpa: u64,
        data: Vec<u8>,
        cid: u16,
        sq_id: u16,
        sq_head: u16,
        cq_id: u16,
    ) {
        let tok = ctx.dma_write(gpa, data);
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
}

impl PcieDevice for NvmeController {
    fn describe(&self) -> DeviceDescribe {
        DeviceDescribe {
            vendor_id: self.vid as u32,
            // PCI Device ID = 0xC0DE — matches OpenHCL noop convention 便于
            // ohcldiag-dev / guest 区分本设备实例。
            device_id: 0xc0de,
            class_code: 0x01_08_02, // Mass Storage / NVMe (PCIe class 01.08.02)
            revision: 1,
            subsystem_vendor: self.ssvid as u32,
            subsystem_device: 0,
            bars: vec![BarInfo {
                index: 0,
                size: BAR0_SIZE,
                // NVMe spec 不强求 64-bit BAR；用 32-bit 简化 cfg space。
                // BAR0 = MMIO 32 不消耗 BAR1，省 PCIe BAR slots。
                kind: BarKind::Mmio32 as i32,
                prefetchable: false,
            }],
            msix_count: self.msix_count as u32,
            capabilities: vec![],
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
            for (&cq_id, cq) in self.cqs.iter_mut() {
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

/// **Phase U-followup** — 让 vfio-user backend 拿到 BAR0/MSI-X 描述。
///
/// 仅在编译 `--cfg vfio_user` 时生效，避免给 OpenHCL 路径增加无用的
/// dep。当前直接 inline 实现（不走 cfg gate），因为 trait 只暴露常量
/// 查询，开销 0。
impl pcie_vfio_user_sdk::Regions for NvmeController {
    fn bar0_size(&self) -> u64 {
        crate::regs::BAR0_SIZE
    }
    fn msix_count(&self) -> u32 {
        self.msix_count as u32
    }
}

#[cfg(test)]
mod tests;
