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
mod io;
mod logs;
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
    /// **Phase H5** — Firmware Image Download chunk DMA-read 完成。
    /// `offset_bytes` = byte offset into fw_download_buf。
    AdminFwDownloadChunk { offset_bytes: u32 },
    /// **Phase H6** — Reservation 命令 DMA-read 完成。
    /// `rrega/racqa/rrela` 是 spec cdw10 bits 2:0（Register/Acquire/Release
    /// Action）；`rtype` 是 cdw10 bits 15:8 reservation type。统一变体
    /// 让完成回调按 op_kind 分流。
    NvmReservationCmd {
        op_kind: ReservationKind,
        action: u8,
        rtype: u8,
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
                    registrants: Vec::new(),
                    reservation: None,
                    reservation_gen: 0,
                    zns: None,
                },
            );
        }
        // **Phase L1** — 把 zns_nsids 列表中的 NS 标记为 ZNS。
        // 教学短化：zone_size = 1 MiB = 2048 LBA at 512B sector，capacity
        // 与 size 相同（spec 允许 capacity < size 留 metadata 区域）。
        const ZNS_ZONE_LBAS: u64 = 2048; // 1 MiB at 512B sector
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

    /// CC 写入（每次 EN bit 变化都可能 enable/disable controller）。
    fn write_cc(&mut self, new_cc: u32) {
        let old_en = self.cc & cc::EN != 0;
        let new_en = new_cc & cc::EN != 0;
        self.cc = new_cc;
        if !old_en && new_en {
            self.enable();
        } else if old_en && !new_en {
            self.disable();
        }
    }

    fn enable(&mut self) {
        tracing::info!("NVMe: CC.EN 0→1 enabling controller");
        // AQA: bits 11:0 ASQS (admin SQ size minus 1), bits 27:16 ACQS。
        let asqs = (self.aqa & 0xfff) as u32 + 1;
        let acqs = ((self.aqa >> 16) & 0xfff) as u32 + 1;
        // 注册 admin SQ (ID 0)、admin CQ (ID 0)。
        self.sqs.insert(
            0,
            SubmissionQueue {
                base_gpa: self.asq,
                size: asqs,
                head: 0,
                tail: 0,
                cq_id: 0,
            },
        );
        self.cqs.insert(
            0,
            CompletionQueue {
                base_gpa: self.acq,
                size: acqs,
                tail: 0,
                phase: 1,
                head: 0,
                interrupt_vector: 0,
                interrupt_enabled: true,
                pending_completions: 0,
                last_fire: None,
            },
        );
        self.state = CtrlState::Ready;
        self.csts |= csts::RDY;
        tracing::info!(
            asqs,
            acqs,
            asq = format_args!("{:#x}", self.asq),
            acq = format_args!("{:#x}", self.acq),
            "NVMe: ready"
        );
    }

    fn disable(&mut self) {
        tracing::info!("NVMe: CC.EN 1→0 disabling controller");
        // 简化：不等 outstanding DMA flush（容易 dead-lock），直接清状态。
        // 真 NVMe driver 在 disable 前会 set CSTS.SHST→shutdown sequence,
        // 但 v1 简化处理：driver 通常容忍立即重置。
        //
        // **reviewer C2 mitigation**：清后 SDK in-flight DMA 完成时找不到
        // token → unknown-token warn 路径（mod.rs ok=true 分支末尾）。
        // op_id 单调递增 (next_op_id 跨 reset 不重置)，新 op 不会与旧
        // 完成回调撞 token / op_id；下面 debug_assert 让任何意外残留
        // 在 test mode 立即响。
        self.sqs.clear();
        self.cqs.clear();
        self.pending_fetches.clear();
        self.pending_ios.clear();
        self.dual_prp_writes.clear();
        self.prp_list_ops.clear();
        self.compare_ops.clear();
        self.pi_writes.clear();
        self.pending_fused.clear();
        self.sqe_inbox.clear();
        debug_assert!(self.pending_ios.is_empty());
        debug_assert!(self.dual_prp_writes.is_empty());
        debug_assert!(self.prp_list_ops.is_empty());
        debug_assert!(self.compare_ops.is_empty());
        debug_assert!(self.pi_writes.is_empty());
        debug_assert!(self.pending_fused.is_empty());
        // AEN queue 跨 reset 不保留 (NVMe spec § 5.2 "Implicit Aborts on Reset")
        self.aen_pending.clear();
        self.aen_last_err_count = self.stat_num_err_log_entries;
        // Phase G：self-test 跨 reset 撤回（spec § 5.11 "Reset terminates
        // any in-progress Device Self-test"）；error log + self_test_last
        // 跨 reset 保留（spec § 5.16.1.1 / § 5.16.1.6 持久化，仅 power-
        // cycle 清空）。
        self.self_test_in_progress = None;
        // Phase H1：features 跨 reset 不保留（spec § 5.21.1 'Save' bit
        // 默认 0；我们暂不实现 NVM Subsystem persistent）。
        self.features.clear();
        self.granted_io_queues = IO_QUEUE_CAP;
        // K5: sanitize 跨 reset 撤回（spec § 5.26 'Sanitize Operation
        // Aborts on Reset'），last_status 保留作 history
        self.sanitize = None;
        // K6: doorbell buffer 跨 reset 清（driver 重新配置）
        self.doorbell_shadow_gpa = 0;
        self.doorbell_event_idx_gpa = 0;
        // K8: power state 重置到 PS0
        self.current_ps = 0;
        // M1: interrupt coalescing 重置默认（无 coalesce）
        self.irq_aggr_time = 0;
        self.irq_aggr_threshold = 0;
        self.state = CtrlState::Disabled;
        self.csts &= !csts::RDY;
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
                    // mismatch' 时也 abort Write。
                    //
                    // 真正 atomic chain 需 PendingOp::NvmFusedCompareThenWrite
                    // 跟踪 pair；留作后续扩展。当前：dispatch 两条独立但顺序
                    // 保证（FIRST 先 dispatch + post CQE，然后 SECOND）。
                    // **Reviewer C-1 修正** — 当前 dispatch 不真做 atomic chain
                    // （Compare 的 single-PRP 路径返 None async；Write 在 Compare
                    // 完成前已 dispatch）。所以 IdentifyController.fuses=0 不
                    // advertise，但保留 dispatcher 路径让 driver / 测试能识别
                    // fused-pair 协议是 understood。真 atomic chain 需新
                    // PendingOp::NvmFusedCompareThenWrite 在 Compare 完成时
                    // 决定是否 dispatch Write（pass=dispatch，fail=COMPARE_FAILURE
                    // 给 Write 而非真 write）。Tracked as Phase O3 TODO。
                    tracing::warn!(
                        sq_id,
                        first_cid = first.cid(),
                        second_cid = cid,
                        slba = first_slba,
                        nlb = first_nlb,
                        "Fused C+W dispatched sequentially (NOT atomic; fuses=0 advertise)"
                    );
                    if let Some(cqe) =
                        self.dispatch_io(ctx, sq_id, first, first.cid(), first_head, cq_id)
                    {
                        self.post_cqe(ctx, cq_id, cqe);
                    }
                    if let Some(cqe) = self.dispatch_io(ctx, sq_id, sqe, cid, sq_head, cq_id) {
                        self.post_cqe(ctx, cq_id, cqe);
                    }
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

    fn mmio_read(&mut self, bar: u32, offset: u64, size: u32) -> u64 {
        if bar != 0 {
            return 0;
        }
        // CAP 是 64-bit register；driver 可以一次 8 字节读全 CAP，或分两次
        // 4 字节读 lo/hi。处理两种情况。
        let val = match (offset, size) {
            (0x00, 8) => self.cap,
            (0x00, 4) => self.cap & 0xffff_ffff,
            (0x04, 4) => self.cap >> 32,
            (0x08, _) => self.vs as u64,
            (0x0c, _) => self.intms as u64,
            (0x10, _) => self.intmc as u64,
            (0x14, _) => self.cc as u64,
            (0x1c, _) => self.csts as u64,
            (0x24, _) => self.aqa as u64,
            // ASQ/ACQ 同理可 8-byte 读
            (0x28, 8) => self.asq,
            (0x28, 4) => self.asq & 0xffff_ffff,
            (0x2c, 4) => self.asq >> 32,
            (0x30, 8) => self.acq,
            (0x30, 4) => self.acq & 0xffff_ffff,
            (0x34, 4) => self.acq >> 32,
            // **Phase L3** — CMB / BPINFO / PMR 寄存器 RO 全 0 (capability
            // 已声明不支持；driver 读到 0 知道不可用，spec-conformant 行为)
            (0x38, _) => 0,             // CMBLOC
            (0x3c, _) => 0,             // CMBSZ
            (0x40, _) => 0,             // BPINFO
            (0xe00, _) => 0,            // PMRCAP
            (0xe04, _) => 0,            // PMRCTL
            (0xe08, _) => 0,            // PMRSTS
            (o, _) if o >= 0x1000 => 0, // doorbell reads return 0 (write-only)
            _ => {
                tracing::debug!(offset, size, "MMIO read: unknown offset");
                0
            }
        };
        tracing::debug!(
            offset = format_args!("{:#x}", offset),
            size,
            value = format_args!("{:#x}", val),
            "MMIO read"
        );
        val
    }

    fn mmio_write(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        bar: u32,
        offset: u64,
        size: u32,
        value: u64,
    ) {
        if bar != 0 {
            return;
        }
        tracing::debug!(
            offset = format_args!("{:#x}", offset),
            size,
            value = format_args!("{:#x}", value),
            "MMIO write"
        );
        match offset {
            0x0c => self.intms |= value as u32,    // mask set
            0x10 => self.intms &= !(value as u32), // mask clear (INTMC sets bits to clear)
            0x14 => self.write_cc(value as u32),
            0x24 => self.aqa = value as u32,
            0x28 => {
                // ASQ low 32
                self.asq = (self.asq & !0xffff_ffff) | (value & 0xffff_ffff);
            }
            0x2c => {
                self.asq = (self.asq & 0xffff_ffff) | (value << 32);
            }
            0x30 => {
                self.acq = (self.acq & !0xffff_ffff) | (value & 0xffff_ffff);
            }
            0x34 => {
                self.acq = (self.acq & 0xffff_ffff) | (value << 32);
            }
            o if o >= 0x1000 => {
                // doorbell 写**必须** 4 字节 access；其它尺寸视为 driver bug
                // 直接忽略（不应该按 8/2/1 字节写 doorbell）。
                if size != 4 {
                    tracing::warn!(
                        offset = format_args!("{:#x}", o),
                        size,
                        "doorbell write with non-4-byte size; ignored"
                    );
                    return;
                }
                if let Some((is_sq, qid)) = Self::parse_doorbell(o) {
                    if is_sq {
                        self.on_sq_tail_doorbell(ctx, qid, value as u32);
                    } else {
                        self.on_cq_head_doorbell(qid, value as u32);
                    }
                }
            }
            _ => {}
        }
        // 处理 inbox SQE（dispatch_sqe 可能 mutably borrow self → 借出再回填）
        let inbox = std::mem::take(&mut self.sqe_inbox);
        for (sq_id, head, sqe) in inbox {
            self.dispatch_sqe(ctx, sq_id, head, sqe);
        }
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

    fn on_dma_complete(&mut self, ctx: &mut DeviceCtx<'_>, token: u64, ok: bool, data: Vec<u8>) {
        // **Reviewer H-3 TODO** — 此方法 1020 行，未来应按 PendingOp variant
        // 拆成多个 `fn complete_*` helpers 入 `controller/completion.rs`。
        // 当前保留单一方法是因为：(a) 多数 variant 复用相同 phase/cq/post_cqe
        // 引用；(b) refactor 风险大需大量改动；(c) 现有 25 个测试已锁定行为。
        // 已抽出的 helpers：advance_zns_wp / check_zns_write / check_zns_read /
        // apply_zsa / check_zsa_transition / should_fire_irq / build_zone_report。
        if !ok {
            tracing::warn!(token, "DMA failed");
            // 清相关 pending（IO 或 fetch）
            self.pending_fetches.remove(&token);
            if let Some(p) = self.pending_ios.remove(&token) {
                // **C1 修复** — 多段 op (dual PRP / PRP list) 必须清掉
                // sibling 的 pending_ios + 累积器，并保证只 post 一次
                // error CQE。否则 sibling 完成时会进 unknown-op_id 分支
                // warn + leak，driver 还可能收两次 error CQE 违反 NVMe
                // spec § 4.6.1 "one CQE per command"。
                match p.op {
                    PendingOp::NvmWriteDualPrp { op_id, .. } => {
                        self.pending_ios.retain(|_, q| {
                            !matches!(q.op,
                                PendingOp::NvmWriteDualPrp { op_id: o, .. } if o == op_id)
                        });
                        self.dual_prp_writes.remove(&op_id);
                    }
                    PendingOp::NvmWritePrpListFetch { op_id }
                    | PendingOp::NvmWritePrpListData { op_id, .. }
                    | PendingOp::NvmReadPrpListFetch { op_id }
                    | PendingOp::NvmReadPrpListData { op_id, .. } => {
                        self.pending_ios.retain(|_, q| match q.op {
                            PendingOp::NvmWritePrpListFetch { op_id: o }
                            | PendingOp::NvmWritePrpListData { op_id: o, .. }
                            | PendingOp::NvmReadPrpListFetch { op_id: o }
                            | PendingOp::NvmReadPrpListData { op_id: o, .. } => o != op_id,
                            _ => true,
                        });
                        self.prp_list_ops.remove(&op_id);
                    }
                    PendingOp::NvmCompareDualPrp { op_id, .. }
                    | PendingOp::NvmComparePrpListFetch { op_id }
                    | PendingOp::NvmComparePrpListData { op_id, .. } => {
                        // **Phase K2** — Compare 多段 sibling cleanup (同 C1 修复)
                        self.pending_ios.retain(|_, q| match q.op {
                            PendingOp::NvmCompareDualPrp { op_id: o, .. }
                            | PendingOp::NvmComparePrpListFetch { op_id: o }
                            | PendingOp::NvmComparePrpListData { op_id: o, .. } => o != op_id,
                            _ => true,
                        });
                        self.compare_ops.remove(&op_id);
                    }
                    PendingOp::NvmWritePiMulti { op_id, .. } => {
                        // **Phase K4c** — 多 LBA PI Write sibling cleanup
                        self.pending_ios.retain(|_, q| {
                            !matches!(q.op,
                                PendingOp::NvmWritePiMulti { op_id: o, .. } if o == op_id)
                        });
                        self.pi_writes.remove(&op_id);
                    }
                    PendingOp::NvmReadDualPrpSiblingHalf => {
                        // sibling tok2 (NvmReadDmaWrite) 还在 pending_ios 中；
                        // 移除避免它后续到达时给 driver post success CQE
                        // 覆盖本 error。
                        self.pending_ios.retain(|_, q| {
                            !matches!(
                                q.op,
                                PendingOp::NvmReadDmaWrite { .. }
                                    if q.cid == p.cid && q.sq_id == p.sq_id
                            )
                        });
                    }
                    PendingOp::NvmZoneAppend {
                        zone_idx,
                        prev_wp,
                        prev_state,
                        num_blocks,
                        ..
                    } => {
                        // **Reviewer H1 + M-1** — Append DMA-fail：尝试回滚预占
                        // 的 WP/state。只在 WP 尚未被更新 Append 覆盖时回滚（即
                        // 当前 WP 恰好 = prev_wp + num_blocks）。否则更晚的
                        // Append 已预占，rollback 会破坏其 assigned_lba —
                        // 这种情况留 hole（spec 允许 zone gap，driver 通过
                        // Read SLBA 看到 zero data 推断）。
                        if let Some(ns) = self.namespaces.get_mut(&p.nsid)
                            && let Some(zns) = ns.zns.as_mut()
                            && let Some(zone) = zns.zones.get_mut(zone_idx)
                        {
                            let expected_wp = prev_wp + num_blocks as u64;
                            if zone.write_pointer == expected_wp {
                                zone.write_pointer = prev_wp;
                                zone.state = prev_state;
                            } else {
                                tracing::warn!(
                                    zone_idx,
                                    cur_wp = zone.write_pointer,
                                    expected = expected_wp,
                                    "Append DMA-fail: newer reservation present, leaving hole"
                                );
                            }
                        }
                    }
                    _ => {}
                }
                self.stat_num_err_log_entries += 1;
                // Phase G：push 进 error_log 环形 buffer，Get Log Page 0x01 用。
                // sc=DATA_TRANSFER_ERROR (0x04) 在 SF bits 8..1 = 0x04 << 1。
                // 区分 NVM IO vs admin：admin SQ id=0 时 nsid=0xFFFF_FFFF
                // 表示 "not applicable"（reviewer M3 修复）。LBA 暂用 0；
                // 真要追踪需在 PendingOp 变体存 SLBA。
                let sf = (sc::DATA_TRANSFER_ERROR as u16) << 1;
                let nsid = if p.sq_id == 0 { 0xFFFF_FFFF } else { 1 };
                self.push_error_log(p.sq_id, p.cid, sf, 0, nsid);
                let cq = self.cqs.get(&p.cq_id);
                let phase = cq.map(|c| c.phase).unwrap_or(1);
                let cqe = Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR, 0);
                self.post_cqe(ctx, p.cq_id, cqe);
            }
            return;
        }
        // 1) fetch_sqe completion
        if self.pending_fetches.contains_key(&token) {
            self.on_fetched_sqes(token, data);
            // 处理新入的 SQE
            let inbox = std::mem::take(&mut self.sqe_inbox);
            for (sq_id, head, sqe) in inbox {
                self.dispatch_sqe(ctx, sq_id, head, sqe);
            }
            return;
        }
        // 2) IO completion
        if let Some(p) = self.pending_ios.remove(&token) {
            match p.op {
                PendingOp::NvmWriteDmaRead { lba, num_blocks } => {
                    let bytes = num_blocks as u64 * SECTOR_SIZE;
                    let first8 = if data.len() >= 8 {
                        u64::from_le_bytes(data[..8].try_into().unwrap_or([0; 8]))
                    } else {
                        0
                    };
                    tracing::debug!(
                        lba,
                        num_blocks,
                        data_len = data.len(),
                        expected = bytes,
                        first8 = format_args!("{:#x}", first8),
                        "NVM Write DMA-read complete"
                    );
                    if data.len() != bytes as usize {
                        tracing::warn!(
                            got = data.len(),
                            want = bytes,
                            "NVM Write DMA-read byte mismatch"
                        );
                    }
                    // **Phase H4 + M2** — 用 p.nsid 选 NS，走 write_at（mmap
                    // 零拷贝 fast path）。Admin DMA-write 是 NvmReadDmaWrite
                    // { num_blocks: 0 } 不走这里。
                    let res = if let Some(ns) = self.namespaces.get_mut(&p.nsid) {
                        ns.write_at(&data, lba * SECTOR_SIZE)
                    } else {
                        Err(std::io::Error::other(format!("unknown NSID {}", p.nsid)))
                    };
                    // 注：不 per-IO fsync（H5）；driver 用 NVM FLUSH (opc 0x00)
                    // 显式拿持久化承诺；Identify Controller VWC=1 已声明
                    // volatile write cache，driver 会主动发 FLUSH。
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = match res {
                        Ok(()) => {
                            // Phase F：真 IO 写完成 → 计数。
                            self.stat_host_writes += 1;
                            self.stat_lba_written += num_blocks as u64;
                            // **Reviewer M-2 + H-1** — 落到 ZNS NS 时推进 WP
                            // + 状态 transition（共享 io::advance_zns_wp）。
                            if let Some(ns) = self.namespaces.get_mut(&p.nsid) {
                                crate::controller::io::advance_zns_wp(ns, lba, num_blocks);
                            }
                            Cqe::success(p.cid, p.sq_id, p.sq_head, phase)
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, lba, num_blocks, "NVM Write file write failed");
                            self.stat_num_err_log_entries += 1;
                            self.push_error_log(
                                p.sq_id,
                                p.cid,
                                (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                lba,
                                1,
                            );
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR, 0)
                        }
                    };
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmReadDmaWrite { num_blocks } => {
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    // Phase F：num_blocks > 0 表示真 NVM Read；= 0 是 admin
                    // Identify/Log Page，不计入 SMART host read。
                    if num_blocks > 0 {
                        self.stat_host_reads += 1;
                        self.stat_lba_read += num_blocks as u64;
                    }
                    let cqe = Cqe::success(p.cid, p.sq_id, p.sq_head, phase);
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmReadPiDmaWrite { num_blocks } => {
                    // **Phase K4b** — PI Read 已 verify + dma_write data 完成，
                    // 与 NvmReadDmaWrite 等价（计数）。
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    self.stat_host_reads += 1;
                    self.stat_lba_read += num_blocks as u64;
                    let cqe = Cqe::success(p.cid, p.sq_id, p.sq_head, phase);
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmWritePi { lba, num_blocks } => {
                    // **Phase K4a** — DMA-read 4 KiB data 完成；compute PI
                    // tuple + interleave 写到 backing file (block_bytes=4104)。
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let nsid = p.nsid;
                    let cqe = if let Some(ns) = self.namespaces.get_mut(&nsid) {
                        let pi_type = ns.pi_type;
                        let pi_first = ns.pi_first;
                        let block_bytes = ns.block_bytes() as usize;
                        let data_bytes = ns.data_bytes() as usize;
                        if data.len() != data_bytes {
                            tracing::warn!(
                                got = data.len(),
                                want = data_bytes,
                                "PI Write DMA-read length mismatch"
                            );
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR, 0)
                        } else {
                            let tuple = crate::pi::PiTuple::compute(&data, lba, pi_type);
                            let tuple_bytes = tuple.to_bytes();
                            // Interleave: pi_first → [tuple(8)][data(4096)]
                            //             else      → [data(4096)][tuple(8)]
                            let mut block = vec![0u8; block_bytes];
                            if pi_first {
                                block[0..8].copy_from_slice(&tuple_bytes);
                                block[8..8 + data_bytes].copy_from_slice(&data);
                            } else {
                                block[0..data_bytes].copy_from_slice(&data);
                                block[data_bytes..data_bytes + 8].copy_from_slice(&tuple_bytes);
                            }
                            // **Phase M2** — write_at 走 mmap 零拷贝
                            let res = ns.write_at(&block, lba * block_bytes as u64);
                            match res {
                                Ok(()) => {
                                    self.stat_host_writes += 1;
                                    self.stat_lba_written += num_blocks as u64;
                                    tracing::debug!(
                                        nsid,
                                        lba,
                                        pi_type,
                                        pi_first,
                                        guard = tuple.guard,
                                        ref_tag = tuple.ref_tag,
                                        "PI Write OK"
                                    );
                                    Cqe::success(p.cid, p.sq_id, p.sq_head, phase)
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, nsid, lba, "PI Write backing fail");
                                    self.stat_num_err_log_entries += 1;
                                    self.push_error_log(
                                        p.sq_id,
                                        p.cid,
                                        (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                        lba,
                                        nsid,
                                    );
                                    Cqe::error(
                                        p.cid,
                                        p.sq_id,
                                        p.sq_head,
                                        phase,
                                        sc::DATA_TRANSFER_ERROR,
                                        0,
                                    )
                                }
                            }
                        }
                    } else {
                        Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_NAMESPACE, 0)
                    };
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmReadDualPrpSiblingHalf => {
                    // **H4**：成功路径无 op — counter + success CQE 全由
                    // tok2 (NvmReadDmaWrite) 处理；这里只是消化 token
                    // 让生命周期闭合。失败路径在 on_dma_complete 顶部 ok==
                    // false 分支统一 post error CQE（参 mod.rs DMA fail）。
                    tracing::trace!(token, "dual-PRP Read sibling half ok (no-op)");
                }
                PendingOp::NvmZoneAppend {
                    zone_idx,
                    assigned_lba,
                    prev_wp,
                    prev_state,
                    num_blocks,
                } => {
                    // **Phase L1 + reviewer H1 修复** — ZNS Zone Append 完成回调。
                    // 1. DMA-read 数据落盘到 assigned_lba 处
                    // 2. 成功 → success CQE 携 assigned_lba（dw0=low32, dw1=hi32）
                    //    （ZNS CS § 3.2.4 要求）
                    // 3. 失败 → 回滚 WP/state 到 prev_wp/prev_state，post error CQE
                    let bytes = num_blocks as u64 * SECTOR_SIZE;
                    let nsid = p.nsid;
                    let res = if let Some(ns) = self.namespaces.get_mut(&nsid) {
                        // **Phase M2** — write_at 走 mmap 零拷贝
                        ns.write_at(&data[..bytes as usize], assigned_lba * SECTOR_SIZE)
                    } else {
                        Err(std::io::Error::other(format!("unknown NSID {}", nsid)))
                    };
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = match res {
                        Ok(()) => {
                            self.stat_host_writes += 1;
                            self.stat_lba_written += num_blocks as u64;
                            tracing::debug!(
                                nsid,
                                zone_idx,
                                assigned_lba,
                                num_blocks,
                                "Zone Append OK"
                            );
                            let mut c = Cqe::success(p.cid, p.sq_id, p.sq_head, phase);
                            // ZNS spec：CQE dw0/dw1 = assigned LBA（little-endian
                            // 双字拼成 64-bit）。driver 用这个值后续 Read。
                            c.cdw0 = (assigned_lba & 0xFFFF_FFFF) as u32;
                            c.cdw1 = (assigned_lba >> 32) as u32;
                            c
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, nsid, zone_idx, assigned_lba,
                                "Zone Append backing failed; rolling back WP/state");
                            // **Reviewer M-1** — 同上 DMA-fail 路径：仅在
                            // 当前 WP 仍 = prev_wp + num_blocks 时回滚，否则
                            // 留 hole 避免破坏更晚 reservation 的 assigned_lba。
                            if let Some(ns) = self.namespaces.get_mut(&nsid)
                                && let Some(zns) = ns.zns.as_mut()
                                && let Some(zone) = zns.zones.get_mut(zone_idx)
                            {
                                let expected_wp = prev_wp + num_blocks as u64;
                                if zone.write_pointer == expected_wp {
                                    zone.write_pointer = prev_wp;
                                    zone.state = prev_state;
                                } else {
                                    tracing::warn!(
                                        zone_idx,
                                        cur_wp = zone.write_pointer,
                                        expected = expected_wp,
                                        "leaving hole (newer reservation present)"
                                    );
                                }
                            }
                            self.stat_num_err_log_entries += 1;
                            self.push_error_log(
                                p.sq_id,
                                p.cid,
                                (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                assigned_lba,
                                nsid,
                            );
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR, 0)
                        }
                    };
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmWritePiMulti { op_id, page_idx } => {
                    // **Phase K4c + Reviewer H-3 (7轮)** — 多 LBA PI Write
                    // 数据段到达。把数据 copy 到 accumulator.received 中
                    // 对应 page；所有 page 到齐时按 LBA 切 4096 + compute
                    // PI + interleave 写文件。
                    let Some(accum) = self.pi_writes.get_mut(&op_id) else {
                        tracing::warn!(op_id, "NvmWritePiMulti: unknown op_id");
                        return;
                    };
                    let off = page_idx as usize * NVME_PAGE_SIZE as usize;
                    // Bounds-check defensively：page_idx 越界 / data 长度异常
                    // 都立即 abort 整个 op，避免 silent corruption
                    if off >= accum.received.len() || off + data.len() > accum.received.len() {
                        tracing::error!(
                            op_id,
                            page_idx,
                            data_len = data.len(),
                            cap = accum.received.len(),
                            "K4c PI Write: page out of bounds; aborting op"
                        );
                        let accum = self.pi_writes.remove(&op_id).unwrap();
                        let phase = self.cqs.get(&accum.cq_id).map(|c| c.phase).unwrap_or(1);
                        let cqe = Cqe::error(
                            accum.cid,
                            accum.sq_id,
                            accum.sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
                            0,
                        );
                        self.post_cqe(ctx, accum.cq_id, cqe);
                        // 顺便清 sibling tokens（防止 silently 完成）
                        self.pending_ios.retain(|_, q| {
                            !matches!(q.op,
                                PendingOp::NvmWritePiMulti { op_id: o, .. } if o == op_id)
                        });
                        return;
                    }
                    accum.received[off..off + data.len()].copy_from_slice(&data);
                    accum.pages_done += 1;
                    if accum.pages_done < accum.pages_total {
                        return; // 还有 page 未到，等下一次完成
                    }
                    // 所有 page 到齐 — 拿 accumulator 出来处理
                    let accum = self.pi_writes.remove(&op_id).unwrap();
                    let nsid = accum.nsid;
                    let cq_id = accum.cq_id;
                    let cq = self.cqs.get(&cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = if let Some(ns) = self.namespaces.get_mut(&nsid) {
                        let pi_type = ns.pi_type;
                        let pi_first = ns.pi_first;
                        let block_bytes = ns.block_bytes() as usize;
                        let data_bytes = ns.data_bytes() as usize;
                        let mut ok = true;
                        let mut last_err: Option<std::io::Error> = None;
                        for i in 0..accum.num_blocks as usize {
                            let lba = accum.slba + i as u64;
                            let data_chunk = &accum.received[i * data_bytes..(i + 1) * data_bytes];
                            let tuple = crate::pi::PiTuple::compute(data_chunk, lba, pi_type);
                            let tuple_bytes = tuple.to_bytes();
                            let mut block = vec![0u8; block_bytes];
                            if pi_first {
                                block[0..8].copy_from_slice(&tuple_bytes);
                                block[8..8 + data_bytes].copy_from_slice(data_chunk);
                            } else {
                                block[0..data_bytes].copy_from_slice(data_chunk);
                                block[data_bytes..data_bytes + 8].copy_from_slice(&tuple_bytes);
                            }
                            if let Err(e) = ns.write_at(&block, lba * block_bytes as u64) {
                                ok = false;
                                last_err = Some(e);
                                break;
                            }
                        }
                        if ok {
                            self.stat_host_writes += 1;
                            self.stat_lba_written += accum.num_blocks as u64;
                            tracing::debug!(
                                nsid,
                                slba = accum.slba,
                                num_blocks = accum.num_blocks,
                                "K4c multi-LBA PI Write OK"
                            );
                            Cqe::success(accum.cid, accum.sq_id, accum.sq_head, phase)
                        } else {
                            let e = last_err.unwrap();
                            tracing::warn!(error = %e, nsid, slba = accum.slba, "K4c PI Write fail");
                            self.stat_num_err_log_entries += 1;
                            self.push_error_log(
                                accum.sq_id,
                                accum.cid,
                                (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                accum.slba,
                                nsid,
                            );
                            Cqe::error(
                                accum.cid,
                                accum.sq_id,
                                accum.sq_head,
                                phase,
                                sc::DATA_TRANSFER_ERROR,
                                0,
                            )
                        }
                    } else {
                        Cqe::error(
                            accum.cid,
                            accum.sq_id,
                            accum.sq_head,
                            phase,
                            sc::INVALID_NAMESPACE,
                            0,
                        )
                    };
                    self.post_cqe(ctx, cq_id, cqe);
                }
                PendingOp::NvmCopyFetchRanges { sdlba, num_ranges } => {
                    // **Phase O1** — Simple Copy ranges 已到达；解析 32-byte
                    // descriptors → per-range backing read + write 到 sdlba。
                    // 全 controller 侧 backing，无 host DMA。
                    let nsid = p.nsid;
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let expected = num_ranges as usize * 32;
                    if data.len() < expected {
                        tracing::warn!(
                            got = data.len(),
                            want = expected,
                            "COPY range list DMA short read"
                        );
                        let cqe = Cqe::error(
                            p.cid,
                            p.sq_id,
                            p.sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
                            0,
                        );
                        self.post_cqe(ctx, p.cq_id, cqe);
                        return;
                    }
                    // Parse ranges & total destination NLB
                    let mut ranges: Vec<(u64, u32)> = Vec::with_capacity(num_ranges as usize);
                    let mut dst_total: u64 = 0;
                    for i in 0..num_ranges as usize {
                        let base = i * 32;
                        let slba = u64::from_le_bytes(data[base..base + 8].try_into().unwrap());
                        let nlb = u16::from_le_bytes(data[base + 16..base + 18].try_into().unwrap())
                            as u32
                            + 1; // 0-based
                        ranges.push((slba, nlb));
                        dst_total += nlb as u64;
                    }
                    let cqe = if let Some(ns) = self.namespaces.get_mut(&nsid) {
                        let sector = SECTOR_SIZE;
                        // **Reviewer H-2** — checked_add 防 sdlba/slba 来自
                        // driver / 恶意输入溢出 u64 后绕过 bounds 检查。
                        let ok_bounds = sdlba
                            .checked_add(dst_total)
                            .is_some_and(|e| e <= ns.total_lba)
                            && ranges.iter().all(|&(slba, nlb)| {
                                slba.checked_add(nlb as u64)
                                    .is_some_and(|e| e <= ns.total_lba)
                            });
                        if !ok_bounds {
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::LBA_OUT_OF_RANGE, 0)
                        } else {
                            // Per-range: read backing → write to sdlba offset
                            let mut dst_off_lba = sdlba;
                            let mut copy_err: Option<std::io::Error> = None;
                            for &(slba, nlb) in &ranges {
                                let bytes = nlb as usize * sector as usize;
                                let mut buf = vec![0u8; bytes];
                                if let Err(e) = ns.read_at(&mut buf, slba * sector) {
                                    copy_err = Some(e);
                                    break;
                                }
                                if let Err(e) = ns.write_at(&buf, dst_off_lba * sector) {
                                    copy_err = Some(e);
                                    break;
                                }
                                dst_off_lba += nlb as u64;
                            }
                            if let Some(e) = copy_err {
                                tracing::warn!(error = %e, nsid, sdlba, "COPY backing IO fail");
                                self.stat_num_err_log_entries += 1;
                                self.push_error_log(
                                    p.sq_id,
                                    p.cid,
                                    (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                    sdlba,
                                    nsid,
                                );
                                Cqe::error(
                                    p.cid,
                                    p.sq_id,
                                    p.sq_head,
                                    phase,
                                    sc::DATA_TRANSFER_ERROR,
                                    0,
                                )
                            } else {
                                // 计入 host_reads + host_writes (NVMe spec § 5.16.1.2
                                // SMART 把 Copy 既算 read 也算 write，因为 backing
                                // 真的双程 IO 了)
                                self.stat_host_reads += 1;
                                self.stat_host_writes += 1;
                                self.stat_lba_read += dst_total;
                                self.stat_lba_written += dst_total;
                                tracing::debug!(
                                    nsid,
                                    sdlba,
                                    dst_total,
                                    num_ranges,
                                    "COPY OK (controller-side backing)"
                                );
                                Cqe::success(p.cid, p.sq_id, p.sq_head, phase)
                            }
                        }
                    } else {
                        Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_NAMESPACE, 0)
                    };
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::AdminSetHostIdentifier { exhid } => {
                    // **Phase K9** — host_id DMA-read 完成，存到 controller。
                    if data.len() >= 8 {
                        self.host_id_lo = u64::from_le_bytes(data[0..8].try_into().unwrap());
                    }
                    if exhid && data.len() >= 16 {
                        self.host_id_hi = u64::from_le_bytes(data[8..16].try_into().unwrap());
                    } else if !exhid {
                        self.host_id_hi = 0;
                    }
                    tracing::info!(
                        exhid,
                        lo = format_args!("{:#x}", self.host_id_lo),
                        hi = format_args!("{:#x}", self.host_id_hi),
                        "Set Features Host Identifier"
                    );
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = Cqe::success(p.cid, p.sq_id, p.sq_head, phase);
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::AdminFwDownloadChunk { offset_bytes } => {
                    // **Phase H5** — FW chunk DMA-read 完成，写入累积 buffer
                    let off = offset_bytes as usize;
                    let end = off + data.len();
                    if end > self.fw_download_buf.len() {
                        self.fw_download_buf.resize(end, 0);
                    }
                    self.fw_download_buf[off..end].copy_from_slice(&data);
                    tracing::debug!(
                        offset_bytes,
                        len = data.len(),
                        total_buf = self.fw_download_buf.len(),
                        "FW Download chunk applied"
                    );
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = Cqe::success(p.cid, p.sq_id, p.sq_head, phase);
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::AdminNsCreate => {
                    // **Phase K3** — NS Mgmt Create：parse 4 KiB Identify NS：
                    //   NSZE @ 0..8 (LBA count)
                    //   NCAP @ 8..16
                    //   FLBAS @ 26 (LBAF index bits 3:0)
                    //   DPS @ 29 (PI type bits 2:0 + bit 3 first/last)
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let mut cqe = if data.len() < 30 {
                        Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_FIELD, 0)
                    } else {
                        let nsze = u64::from_le_bytes(data[0..8].try_into().unwrap());
                        let flbas = data[26] & 0x0f;
                        let dps = data[29];
                        let pi_type = dps & 0x07;
                        let pi_first = (dps & 0x08) == 0;
                        if nsze == 0 || flbas > 1 || pi_type > 1 {
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_FIELD, 0)
                        } else {
                            let new_lbads = if flbas == 0 { 9u8 } else { 12u8 };
                            let new_meta_size = if flbas == 0 { 0u8 } else { 8u8 };
                            // 分配新 NSID（next available）+ 创 temp file backing
                            let new_nsid = (1..=u32::MAX)
                                .find(|n| !self.namespaces.contains_key(n))
                                .unwrap_or(0);
                            if new_nsid == 0 {
                                Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INTERNAL_ERROR, 0)
                            } else {
                                let dir = std::env::temp_dir();
                                let path = dir.join(format!(
                                    "nvme_ns_{}_{}.img",
                                    std::process::id(),
                                    new_nsid
                                ));
                                let block = (1u64 << new_lbads) + new_meta_size as u64;
                                let total_bytes = nsze * block;
                                let create_result = std::fs::OpenOptions::new()
                                    .read(true)
                                    .write(true)
                                    .create(true)
                                    .truncate(true)
                                    .open(&path)
                                    .and_then(|f| {
                                        f.set_len(total_bytes)?;
                                        Ok(f)
                                    });
                                match create_result {
                                    Ok(file) => {
                                        let path_str = path.to_string_lossy().into_owned();
                                        tracing::info!(
                                            new_nsid,
                                            nsze,
                                            new_lbads,
                                            new_meta_size,
                                            pi_type,
                                            path = %path_str,
                                            "NS Mgmt Create OK"
                                        );
                                        self.namespaces.insert(
                                            new_nsid,
                                            Namespace {
                                                mmap: try_mmap_file(&file),
                                                file,
                                                total_lba: nsze,
                                                path: path_str,
                                                lbads: new_lbads,
                                                meta_size: new_meta_size,
                                                pi_type,
                                                pi_first,
                                                registrants: Vec::new(),
                                                reservation: None,
                                                reservation_gen: 0,
                                                zns: None,
                                            },
                                        );
                                        let mut c = Cqe::success(p.cid, p.sq_id, p.sq_head, phase);
                                        c.cdw0 = new_nsid;
                                        c
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "NS Mgmt Create: file alloc failed");
                                        Cqe::error(
                                            p.cid,
                                            p.sq_id,
                                            p.sq_head,
                                            phase,
                                            sc::INTERNAL_ERROR,
                                            0,
                                        )
                                    }
                                }
                            }
                        }
                    };
                    let _ = &mut cqe; // suppress unused_mut if no future mutation
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmCompareDualPrp { op_id, is_prp1 } => {
                    // **Phase K2** — 双 PRP Compare：两段 DMA-read 累积 →
                    // 合并 → byte-compare backing。
                    let ready = if let Some(accum) = self.compare_ops.get_mut(&op_id) {
                        if is_prp1 {
                            accum.prp1_data = Some(data);
                        } else {
                            accum.prp2_data = Some(data);
                        }
                        accum.prp1_data.is_some() && accum.prp2_data.is_some()
                    } else {
                        tracing::warn!(op_id, "Compare dual-PRP unknown op_id");
                        false
                    };
                    if ready {
                        let accum = self.compare_ops.remove(&op_id).unwrap();
                        let cqe = self.compare_finalize(
                            accum.nsid,
                            accum.lba,
                            accum.num_blocks,
                            accum.prp1_data.unwrap(),
                            Some(accum.prp2_data.unwrap()),
                            accum.cid,
                            accum.sq_id,
                            accum.sq_head,
                            accum.cq_id,
                        );
                        self.post_cqe(ctx, accum.cq_id, cqe);
                    }
                }
                PendingOp::NvmComparePrpListFetch { op_id } => {
                    // **Phase K2** — PRP list 页到达，parse + issue per-page
                    // DMA-read。
                    let entries = parse_prp_list(&data);
                    let info: Option<(u32, u32, Vec<u64>)> =
                        self.compare_ops.get_mut(&op_id).map(|op| {
                            let take = (op.total_pages - 1) as usize;
                            let list: Vec<u64> = entries.into_iter().take(take).collect();
                            op.list_entries = Some(list.clone());
                            (op.total_pages, op.num_blocks, list)
                        });
                    let Some((total_pages, num_blocks, list)) = info else {
                        tracing::warn!(op_id, "Compare PRP list fetch unknown op_id");
                        return;
                    };
                    let (sq_id, cid, sq_head, cq_id, nsid) = {
                        let op = &self.compare_ops[&op_id];
                        (op.sq_id, op.cid, op.sq_head, op.cq_id, op.nsid)
                    };
                    let total_bytes = num_blocks as u64 * SECTOR_SIZE;
                    for (i, gpa) in list.iter().enumerate() {
                        let page_idx = (i + 1) as u32;
                        let want_bytes = if page_idx == total_pages - 1 {
                            let last = total_bytes - (page_idx as u64) * NVME_PAGE_SIZE;
                            last as u32
                        } else {
                            NVME_PAGE_SIZE as u32
                        };
                        let tok = ctx.dma_read(*gpa, want_bytes);
                        self.pending_ios.insert(
                            tok,
                            PendingIo {
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
                                nsid,
                                op: PendingOp::NvmComparePrpListData { op_id, page_idx },
                            },
                        );
                    }
                }
                PendingOp::NvmComparePrpListData { op_id, page_idx } => {
                    // **Phase K2** — 单页数据到达 → 填 data_pages[page_idx]，
                    // 全到齐合并 + compare。
                    let done_all = if let Some(op) = self.compare_ops.get_mut(&op_id) {
                        op.data_pages[page_idx as usize] = Some(data);
                        op.pages_done += 1;
                        op.pages_done == op.total_pages
                    } else {
                        tracing::warn!(op_id, page_idx, "Compare PRP list data unknown");
                        false
                    };
                    if done_all {
                        let op = self.compare_ops.remove(&op_id).unwrap();
                        let total_bytes = op.num_blocks as u64 * SECTOR_SIZE;
                        let mut full = Vec::with_capacity(total_bytes as usize);
                        for b in op.data_pages.iter().flatten() {
                            full.extend_from_slice(b);
                        }
                        full.truncate(total_bytes as usize);
                        let cqe = self.compare_finalize(
                            op.nsid,
                            op.lba,
                            op.num_blocks,
                            full,
                            None,
                            op.cid,
                            op.sq_id,
                            op.sq_head,
                            op.cq_id,
                        );
                        self.post_cqe(ctx, op.cq_id, cqe);
                    }
                }
                PendingOp::NvmReservationCmd {
                    op_kind,
                    action,
                    rtype,
                } => {
                    // **Phase H6** — reservation cmd 数据已 DMA-read 到 `data`
                    // (16 byte 或 8 byte)；按 op_kind 修 ns.reservation 状态。
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = self.apply_reservation_cmd(
                        p.nsid, op_kind, action, rtype, &data, p.cid, p.sq_id, p.sq_head, phase,
                    );
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmCompareSinglePrp { lba, num_blocks } => {
                    // **Phase H3** — host buffer 已 DMA-read 到 `data`；
                    // 读 backing file 对应 LBA 范围 → byte-compare。
                    let bytes = num_blocks as u64 * SECTOR_SIZE;
                    let mut backing_buf = vec![0u8; bytes as usize];
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = if let Some(ns) = self.namespaces.get_mut(&p.nsid) {
                        // **Phase M2** — read_at 走 mmap 零拷贝
                        match ns.read_at(&mut backing_buf, lba * SECTOR_SIZE) {
                            Ok(()) => {
                                if data == backing_buf {
                                    tracing::debug!(
                                        lba,
                                        num_blocks,
                                        "Compare success (data == backing)"
                                    );
                                    // 真 IO 读完成 → counter（Compare 也算读 host）
                                    self.stat_host_reads += 1;
                                    self.stat_lba_read += num_blocks as u64;
                                    Cqe::success(p.cid, p.sq_id, p.sq_head, phase)
                                } else {
                                    // 找第一个差异位置便于教学日志
                                    let mismatch_at = data
                                        .iter()
                                        .zip(backing_buf.iter())
                                        .position(|(a, b)| a != b)
                                        .unwrap_or(0);
                                    tracing::warn!(
                                        lba,
                                        num_blocks,
                                        mismatch_at,
                                        "Compare FAILURE: host vs backing mismatch"
                                    );
                                    self.stat_num_err_log_entries += 1;
                                    self.push_error_log(
                                        p.sq_id,
                                        p.cid,
                                        (sc::COMPARE_FAILURE as u16) << 1,
                                        lba,
                                        1,
                                    );
                                    Cqe::error(
                                        p.cid,
                                        p.sq_id,
                                        p.sq_head,
                                        phase,
                                        sc::COMPARE_FAILURE,
                                        sc::SCT_MEDIA_DATA_INTEGRITY,
                                    )
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, lba, "Compare: backing read failed");
                                self.stat_num_err_log_entries += 1;
                                self.push_error_log(
                                    p.sq_id,
                                    p.cid,
                                    (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                    lba,
                                    1,
                                );
                                Cqe::error(
                                    p.cid,
                                    p.sq_id,
                                    p.sq_head,
                                    phase,
                                    sc::DATA_TRANSFER_ERROR,
                                    0,
                                )
                            }
                        }
                    } else {
                        tracing::warn!(nsid = p.nsid, "Compare: unknown NSID at completion");
                        Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_NAMESPACE, 0)
                    };
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmWriteDualPrp { op_id, is_prp1 } => {
                    // 任一段到达：填入 accum 对应槽；两段都到时 dispatch 写盘。
                    // 乱序到达自动处理（C1 reviewer 指出 PRP2 先到的 data
                    // corruption + leak 在此被一并解决）。
                    let ready = if let Some(accum) = self.dual_prp_writes.get_mut(&op_id) {
                        if is_prp1 {
                            accum.prp1_data = Some(data);
                        } else {
                            accum.prp2_data = Some(data);
                        }
                        accum.prp1_data.is_some() && accum.prp2_data.is_some()
                    } else {
                        tracing::warn!(op_id, is_prp1, "DualPrp completion for unknown op_id");
                        false
                    };
                    if ready {
                        // 两段都已到达；移出 accum + 合并写文件 + 发 CQE。
                        let accum = self.dual_prp_writes.remove(&op_id).unwrap();
                        let mut full = accum.prp1_data.unwrap();
                        full.extend_from_slice(&accum.prp2_data.unwrap());
                        let bytes = accum.num_blocks as u64 * SECTOR_SIZE;
                        tracing::debug!(
                            lba = accum.lba,
                            num_blocks = accum.num_blocks,
                            full_len = full.len(),
                            expected = bytes,
                            op_id,
                            "NVM Write dual-PRP: both segments ready, writing"
                        );
                        let res = if let Some(ns) = self.namespaces.get_mut(&accum.nsid) {
                            // **Phase M2** — write_at 走 mmap 零拷贝
                            ns.write_at(&full, accum.lba * SECTOR_SIZE)
                        } else {
                            Err(std::io::Error::other(format!(
                                "unknown NSID {}",
                                accum.nsid
                            )))
                        };
                        // 注：不再 per-IO sync_data（H5）；driver 用 NVM FLUSH
                        // (opcode 0x00) 拿持久化承诺，spec-compliant 行为。
                        let cq = self.cqs.get(&accum.cq_id);
                        let phase = cq.map(|c| c.phase).unwrap_or(1);
                        let cqe = match res {
                            Ok(()) => {
                                // Phase F：dual-PRP Write 完成 → 计数。
                                self.stat_host_writes += 1;
                                self.stat_lba_written += accum.num_blocks as u64;
                                // **Reviewer H-2** — ZNS WP 推进（同 NvmWriteDmaRead）
                                if let Some(ns) = self.namespaces.get_mut(&p.nsid) {
                                    crate::controller::io::advance_zns_wp(
                                        ns,
                                        accum.lba,
                                        accum.num_blocks,
                                    );
                                }
                                Cqe::success(accum.cid, accum.sq_id, accum.sq_head, phase)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    lba = accum.lba,
                                    num_blocks = accum.num_blocks,
                                    "NVM Write dual-PRP file write failed"
                                );
                                self.stat_num_err_log_entries += 1;
                                self.push_error_log(
                                    accum.sq_id,
                                    accum.cid,
                                    (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                    accum.lba,
                                    1,
                                );
                                Cqe::error(
                                    accum.cid,
                                    accum.sq_id,
                                    accum.sq_head,
                                    phase,
                                    sc::DATA_TRANSFER_ERROR,
                                    0,
                                )
                            }
                        };
                        self.post_cqe(ctx, accum.cq_id, cqe);
                    }
                }
                PendingOp::NvmWritePrpListFetch { op_id } => {
                    // **Phase E**：PRP list 页 (4 KiB = 512 个 u64) 到达。
                    // Parse 后为 list 中每个 entry dispatch 一个 sub-DMA
                    // (dma_read 数据页到 controller)。
                    let entries = parse_prp_list(&data);
                    let needed: Option<(u32, Vec<u64>)> = self
                        .prp_list_ops
                        .get_mut(&op_id)
                        .map(|op| (op.total_pages, entries.clone()))
                        .map(|(tot, ent)| {
                            let take = (tot - 1) as usize; // PRP1 已 dispatch
                            (tot, ent.into_iter().take(take).collect())
                        });
                    let Some((total_pages, list)) = needed else {
                        tracing::warn!(op_id, "PRP list fetch for unknown op_id");
                        return;
                    };
                    if let Some(op) = self.prp_list_ops.get_mut(&op_id) {
                        op.list_entries = Some(list.clone());
                    }
                    // Issue sub-DMA reads for each list entry (page_idx 1..total)
                    for (i, gpa) in list.iter().enumerate() {
                        let page_idx = (i + 1) as u32;
                        let want_bytes = if page_idx == total_pages - 1 {
                            // 末页可能不满 4 KiB
                            let total_bytes =
                                self.prp_list_ops[&op_id].num_blocks as u64 * SECTOR_SIZE;
                            let last = total_bytes - (page_idx as u64) * NVME_PAGE_SIZE;
                            last as u32
                        } else {
                            NVME_PAGE_SIZE as u32
                        };
                        let tok = ctx.dma_read(*gpa, want_bytes);
                        // 借用 PendingIo 共用字段 sq_id/cid/sq_head/cq_id/nsid
                        let (sq_id, cid, sq_head, cq_id, nsid) = {
                            let op = &self.prp_list_ops[&op_id];
                            (op.sq_id, op.cid, op.sq_head, op.cq_id, op.nsid)
                        };
                        self.pending_ios.insert(
                            tok,
                            PendingIo {
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
                                nsid,
                                op: PendingOp::NvmWritePrpListData { op_id, page_idx },
                            },
                        );
                    }
                }
                PendingOp::NvmWritePrpListData { op_id, page_idx } => {
                    // **Phase E**：单个数据页 DMA-read 到达。填入 data_pages，
                    // 全部齐 → 合并写文件 → CQE。
                    let done_all = if let Some(op) = self.prp_list_ops.get_mut(&op_id) {
                        op.data_pages[page_idx as usize] = Some(data);
                        op.pages_done += 1;
                        op.pages_done == op.total_pages
                    } else {
                        tracing::warn!(op_id, page_idx, "PRP list data completion unknown");
                        false
                    };
                    if done_all {
                        let op = self.prp_list_ops.remove(&op_id).unwrap();
                        // 合并 data_pages → 一段连续 buffer
                        let total_bytes = op.num_blocks as u64 * SECTOR_SIZE;
                        let mut full = Vec::with_capacity(total_bytes as usize);
                        for page in op.data_pages.iter() {
                            if let Some(b) = page {
                                full.extend_from_slice(b);
                            } else {
                                tracing::warn!(op_id, "PRP list page missing on complete");
                            }
                        }
                        full.truncate(total_bytes as usize);
                        tracing::debug!(
                            op_id,
                            lba = op.lba,
                            num_blocks = op.num_blocks,
                            full_len = full.len(),
                            "NVM Write PRP-list: all pages ready, writing"
                        );
                        let res = if let Some(ns) = self.namespaces.get_mut(&op.nsid) {
                            // **Phase M2** — write_at 走 mmap 零拷贝
                            ns.write_at(&full, op.lba * SECTOR_SIZE)
                        } else {
                            Err(std::io::Error::other(format!("unknown NSID {}", op.nsid)))
                        };
                        let cq = self.cqs.get(&op.cq_id);
                        let phase = cq.map(|c| c.phase).unwrap_or(1);
                        let cqe = match res {
                            Ok(()) => {
                                // Phase F：PRP-list Write 完成 → 计数。
                                self.stat_host_writes += 1;
                                self.stat_lba_written += op.num_blocks as u64;
                                // **Reviewer H-2** — ZNS WP 推进
                                if let Some(ns) = self.namespaces.get_mut(&op.nsid) {
                                    crate::controller::io::advance_zns_wp(
                                        ns,
                                        op.lba,
                                        op.num_blocks,
                                    );
                                }
                                Cqe::success(op.cid, op.sq_id, op.sq_head, phase)
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, lba = op.lba, "PRP-list write failed");
                                self.stat_num_err_log_entries += 1;
                                self.push_error_log(
                                    op.sq_id,
                                    op.cid,
                                    (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                    op.lba,
                                    1,
                                );
                                Cqe::error(
                                    op.cid,
                                    op.sq_id,
                                    op.sq_head,
                                    phase,
                                    sc::DATA_TRANSFER_ERROR,
                                    0,
                                )
                            }
                        };
                        self.post_cqe(ctx, op.cq_id, cqe);
                    }
                }
                PendingOp::NvmReadPrpListFetch { op_id } => {
                    // PRP list 页到达 (Read 路径)。Parse + dma_write 已 cached
                    // 的 per-page data 到 PRP1 + list 中每个 GPA。
                    //
                    // **C3 修复后**：dispatch_io 已按页切分 data_pages，这里
                    // 不再 re-read file，直接 take 每页 buffer dma_write。
                    let entries = parse_prp_list(&data);
                    // 提取 op 关键字段 + take 全部页 buffer
                    type ReadPrpListFetchInfo = (u32, u64, Vec<u64>, Vec<Vec<u8>>);
                    let info: Option<ReadPrpListFetchInfo> =
                        self.prp_list_ops.get_mut(&op_id).map(|op| {
                            // M2 修复：parse_prp_list 现在不再 0 终止；用
                            // total_pages-1 精确截 list 长度。
                            let take = (op.total_pages - 1) as usize;
                            let list: Vec<u64> = entries.into_iter().take(take).collect();
                            // Take 全部页（含 PRP1 = idx 0）；data_pages 移空
                            let pages: Vec<Vec<u8>> = op
                                .data_pages
                                .iter_mut()
                                .map(|p| p.take().unwrap_or_default())
                                .collect();
                            (op.total_pages, op.prp1_gpa, list, pages)
                        });
                    let Some((total_pages, prp1_gpa, list, pages)) = info else {
                        tracing::warn!(op_id, "ReadPrpListFetch unknown op_id");
                        return;
                    };
                    if let Some(op) = self.prp_list_ops.get_mut(&op_id) {
                        op.list_entries = Some(list.clone());
                    }
                    let (sq_id, cid, sq_head, cq_id, nsid) = {
                        let op = &self.prp_list_ops[&op_id];
                        (op.sq_id, op.cid, op.sq_head, op.cq_id, op.nsid)
                    };
                    // 不变量校验：list.len() + 1 (PRP1) == total_pages
                    debug_assert_eq!(
                        list.len() as u32 + 1,
                        total_pages,
                        "PRP list size != total_pages-1"
                    );
                    // **Step 2a**: dma_write PRP1 数据（page idx 0）
                    let mut pages_iter = pages.into_iter();
                    let prp1_buf = pages_iter.next().unwrap_or_default();
                    let tok_prp1 = ctx.dma_write(prp1_gpa, prp1_buf);
                    self.pending_ios.insert(
                        tok_prp1,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmReadPrpListData { op_id, page_idx: 0 },
                        },
                    );
                    // **Step 2b**: dma_write 剩余页到 list 中的 GPA
                    for (i, gpa) in list.iter().enumerate() {
                        let page_idx = (i + 1) as u32;
                        let page_buf = pages_iter.next().unwrap_or_default();
                        let tok = ctx.dma_write(*gpa, page_buf);
                        self.pending_ios.insert(
                            tok,
                            PendingIo {
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
                                nsid,
                                op: PendingOp::NvmReadPrpListData { op_id, page_idx },
                            },
                        );
                    }
                }
                PendingOp::NvmReadPrpListData { op_id, page_idx } => {
                    // 一个数据页 dma_write 完成（PRP1 或 list 中某页）。
                    let done_all = if let Some(op) = self.prp_list_ops.get_mut(&op_id) {
                        op.pages_done += 1;
                        // Read 路径：PRP1 + list 中 (total_pages-1) 个 = total_pages 个 dma_write。
                        op.pages_done >= op.total_pages
                    } else {
                        tracing::warn!(op_id, page_idx, "ReadPrpListData unknown op_id");
                        false
                    };
                    let _ = data; // Read dma_write 完成 data 为空
                    if done_all {
                        let op = self.prp_list_ops.remove(&op_id).unwrap();
                        tracing::debug!(
                            op_id,
                            lba = op.lba,
                            num_blocks = op.num_blocks,
                            "NVM Read PRP-list: all pages dma'd, posting CQE"
                        );
                        let cq = self.cqs.get(&op.cq_id);
                        let phase = cq.map(|c| c.phase).unwrap_or(1);
                        // Phase F：PRP-list Read 完成 → 计数。
                        self.stat_host_reads += 1;
                        self.stat_lba_read += op.num_blocks as u64;
                        let cqe = Cqe::success(op.cid, op.sq_id, op.sq_head, phase);
                        self.post_cqe(ctx, op.cq_id, cqe);
                    }
                }
            }
            return;
        }
        tracing::debug!(token, "DMA completion for unknown token (likely 2nd PRP)");
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
fn parse_prp_list(data: &[u8]) -> Vec<u64> {
    data.chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().expect("chunks_exact(8) guarantees 8 bytes")))
        .collect()
}

#[cfg(test)]
mod tests;
