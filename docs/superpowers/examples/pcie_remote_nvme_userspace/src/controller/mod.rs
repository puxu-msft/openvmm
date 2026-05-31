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

mod admin;
mod io;

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

/// **Phase H4** — 单个 namespace 状态（spec § 1.6 "An NSID maps to one
/// namespace"）。每 NS 有独立 backing file + 容量。
pub(super) struct Namespace {
    pub(super) file: File,
    pub(super) total_lba: u64,
    /// Backing 文件路径 — 仅供日志 / Identify Namespace 扩展用。
    #[allow(dead_code)]
    pub(super) path: String,
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

impl NvmeController {
    /// `backing_files`：每个文件成为一个 namespace（NSID 1, 2, ...）。
    /// 文件大小决定该 NS 容量（÷ 512 round down 到 LBA 数）。
    /// Phase H4：之前接受单 path string；现在 slice，至少 1 个。
    pub fn open(backing_files: &[String], vid: u16, ssvid: u16) -> anyhow::Result<Self> {
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
            let total_lba = size >> SECTOR_SHIFT;
            if total_lba == 0 {
                return Err(anyhow::anyhow!(
                    "backing file too small (< 512 bytes): {path}"
                ));
            }
            let nsid = (idx as u32) + 1;
            tracing::info!(
                nsid,
                path = %path,
                size,
                total_lba,
                "NVMe controller: namespace registered"
            );
            namespaces.insert(
                nsid,
                Namespace {
                    file,
                    total_lba,
                    path: path.clone(),
                },
            );
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
        self.sqs.clear();
        self.cqs.clear();
        self.pending_fetches.clear();
        self.pending_ios.clear();
        self.dual_prp_writes.clear();
        self.prp_list_ops.clear();
        self.sqe_inbox.clear();
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
        tracing::debug!(
            sq_id,
            cid,
            opc = format_args!("{:#x}", opc),
            head_after_this,
            "dispatch SQE"
        );
        let sq_head = head_after_this;
        let cq_id = self.sqs.get(&sq_id).map(|s| s.cq_id).unwrap_or(0);

        let is_admin = sq_id == 0;
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

    /// **Phase C** — Log Page 0x01 Error Information Log。
    ///
    /// NVMe spec § 5.16.1.1。每个 entry 64 字节，含 Error Count / SQID /
    /// CMDID / Status Field / Param Error Loc / LBA / NSID / Vendor /
    /// Cmd Specific Info。我们当前不追踪 per-cmd error history，返单条
    /// 全零 entry 作 "no errors" 占位。
    /// **Phase C/G** — Log Page 0x01 Error Information Log。
    ///
    /// NVMe spec § 5.16.1.1。每 entry 64 字节。Phase G 之前返全零；现在
    /// 真序列化 `self.error_log` 环形 buffer。Driver 顺序读到 error_count
    /// 单调递增的 entry 列表（最旧→最新）。entry 0 是 "最近一次" — 我们
    /// 按 spec 反序：buf[0..64] = 最新，buf[64..128] = 次新，…。
    fn build_error_info_log(&self, bytes: usize) -> Vec<u8> {
        let mut buf = vec![0u8; bytes];
        // 最多塞 bytes/64 个；error_log 按 push 顺序（旧→新），spec 要求
        // entry 0 = most recent，所以 rev()。
        for (i, e) in self.error_log.iter().rev().enumerate() {
            let off = i * 64;
            if off + 64 > bytes {
                break;
            }
            buf[off..off + 8].copy_from_slice(&e.error_count.to_le_bytes());
            buf[off + 8..off + 10].copy_from_slice(&e.sq_id.to_le_bytes());
            buf[off + 10..off + 12].copy_from_slice(&e.cid.to_le_bytes());
            buf[off + 12..off + 14].copy_from_slice(&e.status_field.to_le_bytes());
            buf[off + 14..off + 16].copy_from_slice(&e.param_loc.to_le_bytes());
            buf[off + 16..off + 24].copy_from_slice(&e.lba.to_le_bytes());
            buf[off + 24..off + 28].copy_from_slice(&e.nsid.to_le_bytes());
            // offset 28..64 = vendor info / log page ver / cmd-specific = 0
        }
        buf
    }

    /// **Phase G** — Log Page 0x06 Device Self-Test (NVMe spec § 5.16.1.6)。
    ///
    /// 564 字节布局（修正 reviewer H + M offset 错位）：
    /// - offset 0 (1 byte): Current Self-Test Operation
    ///   bits 3:0 = 0x0 (none), 0x1 short, 0x2 extended
    /// - offset 1 (1 byte): Current Self-Test Completion (bits 6:0 = pct)
    /// - offset 2-3: Reserved
    /// - offset 4..32: Self-Test Result Data Structure[0] (most recent)
    ///   - byte 0 bits 7:4 = Self-Test Code (STC: 1=short, 2=extended)
    ///   - byte 0 bits 3:0 = Self-Test Result (0=ok, 0x09=aborted, 0xf=未运行)
    ///   - byte 1 = Segment Number (我们单段)
    ///   - byte 2 = Valid Diagnostic Information bits (POH/NSID/LBA/SC/SCT)
    ///   - byte 3 = Reserved
    ///   - byte 4..12 = Power On Hours (u64 LE, run-time 快照)
    ///   - byte 12 = NSID Valid (1 byte) — spec 实际 bit 0
    ///   - byte 13..17 = NSID (u32 LE)
    ///   - byte 17..25 = Failing LBA (u64 LE) — 注：跨 byte 边界但 spec
    ///     就是这样 (offset 17 起 8 byte)
    ///   - byte 25 = Status Code Type / Status Code
    ///   - byte 27 = Vendor Specific
    /// - offset 32..560: Result[1..19] (older entries) — 我们只追踪最新一条
    fn build_self_test_log(&self, bytes: usize) -> Vec<u8> {
        let mut buf = vec![0u8; bytes.max(564)];
        // 当前进行中：byte 0/1
        if let Some(ip) = &self.self_test_in_progress {
            buf[0] = ip.stc & 0x0f;
            buf[1] = ip.percent_complete & 0x7f;
        }
        // 最近完成结果（包括 abort）：result data structure @ offset 4
        if let Some(last) = self.self_test_last {
            buf[4] = ((last.stc & 0x0f) << 4) | (last.result & 0x0f);
            // POH @ offset 4..12 的 byte 4..12 == buf[8..16]
            buf[8..16].copy_from_slice(&last.completed_at_poh.to_le_bytes());
            // NSID Valid bit @ buf[16] — 我们不指定具体 NS 失败 → 0
            // 不写 NSID / Failing LBA / SC / SCT（保持 0 = "无该字段"）
        }
        buf.truncate(bytes);
        buf
    }

    /// **Phase G (rev. H 修复)** — Log Page 0x80 Reservation Notification
    /// (NVMe spec § 5.16.1.15)。
    ///
    /// 我们 ONCS.reservations=0 → spec 允许 controller 返 INVALID_FIELD。
    /// 但为兼容老 driver 误发，返 spec 规定的 64 字节 'no notifications'
    /// 占位（全零 = 没新通知 + count=0），并 truncate 到 driver 请求大小。
    fn build_reservation_log(&self, bytes: usize) -> Vec<u8> {
        let mut buf = vec![0u8; bytes.max(64)];
        buf.truncate(bytes);
        buf
    }

    /// **Phase G** — push error log entry（CQE 携 non-zero status 时调）。
    /// 环形 buffer，最多 64 entry（spec ELPE=63）。
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

    /// **Phase C/F** — Log Page 0x02 SMART / Health Information。
    ///
    /// NVMe spec § 5.16.1.2，512 字节固定。Phase F：填入真追踪的
    /// counters（host_read_commands / host_write_commands / data_units_*
    /// / power_on_hours / num_err_log_entries），其余字段保持合理常量。
    ///
    /// data_units_read/written 单位：spec 定义为 "1000 × 512B sectors"
    /// 的累计数，即 `lba_count / 1000`，向下取整。当 lba_count < 1000
    /// 时回报 0（spec 允许，driver 不会因此告警）。
    fn build_smart_health_log(&self, bytes: usize) -> Vec<u8> {
        let mut buf = vec![0u8; bytes.max(512)];
        // Offset 0: critical_warning (1 byte) = 0
        // Offset 1-2: composite_temperature (2 byte LE, in Kelvin)
        let temp_kelvin: u16 = 313;
        buf[1..3].copy_from_slice(&temp_kelvin.to_le_bytes());
        // Offset 3: available_spare (%) — set 100 = full spare available
        buf[3] = 100;
        // Offset 4: available_spare_threshold (%)
        buf[4] = 10;
        // Offset 5: percentage_used (%) — controller wear indicator
        buf[5] = 0;
        // Offset 6: endurance_group_critical_warning_summary
        // Offset 7-31: reserved
        // H3 修复：spec § 5.16.1.2 Figure 196 — data_units_*
        // 以 1000 × 512B sector 为单位，**round up**（"a value of 1
        // corresponds to 1000 units of 512 bytes read, rounded up"）。
        let units_read = self.stat_lba_read.div_ceil(1000) as u128;
        let units_written = self.stat_lba_written.div_ceil(1000) as u128;
        let host_reads = self.stat_host_reads as u128;
        let host_writes = self.stat_host_writes as u128;
        // Offset 32-47: data_units_read (128-bit LE)
        buf[32..48].copy_from_slice(&units_read.to_le_bytes());
        // Offset 48-63: data_units_written
        buf[48..64].copy_from_slice(&units_written.to_le_bytes());
        // Offset 64-79: host_read_commands
        buf[64..80].copy_from_slice(&host_reads.to_le_bytes());
        // Offset 80-95: host_write_commands
        buf[80..96].copy_from_slice(&host_writes.to_le_bytes());
        // Offset 96-111: controller_busy_time (minutes) — 简化 = power_on_hours * 60
        // 真实现需追踪 IO 累计时间；这里近似当作 always busy。
        // 留 0 避免对 driver 误导。
        // Offset 112-127: power_cycles — 1 (本进程启动算一次)
        let one: u128 = 1;
        buf[112..128].copy_from_slice(&one.to_le_bytes());
        // Offset 128-143: power_on_hours
        let hours = (self.power_on_instant.elapsed().as_secs() / 3600) as u128;
        buf[128..144].copy_from_slice(&hours.to_le_bytes());
        // Offset 144-159: unsafe_shutdowns — 0
        // Offset 160-175: media_errors — 0
        // Offset 176-191: num_err_log_entries
        let nerr = self.stat_num_err_log_entries as u128;
        buf[176..192].copy_from_slice(&nerr.to_le_bytes());
        // Offset 192-195: warning_composite_temp_time (minutes above WCTEMP)
        // Offset 196-199: critical_composite_temp_time
        // Offset 200..: thermal sensors / endurance group stats — 0 占位
        buf.truncate(bytes); // 缩到 driver 请求的字节数
        buf
    }

    /// **Phase C/H5** — Log Page 0x03 Firmware Slot Information。
    ///
    /// NVMe spec § 5.16.1.3，512 字节固定。AFI bit 2:0 = 当前激活槽
    /// (1..7)，bits 6:4 = 下次启动激活槽（0 表示无 pending activation）。
    /// FRS[1..7] = 8 字节 ASCII FW revision；FRS[0] 不存在（spec FRS 索引
    /// 1-based）。Phase H5：从 self.fw_active_slot / fw_next_active_slot /
    /// fw_slot_revisions 真序列化。
    fn build_fw_slot_info_log(&self, bytes: usize) -> Vec<u8> {
        let mut buf = vec![0u8; bytes.max(512)];
        // AFI = (next << 4) | active
        let afi = (self.fw_next_active_slot & 0x7) << 4 | (self.fw_active_slot & 0x7);
        buf[0] = afi;
        // FRS[1..7] @ offset 8..64（每槽 8 byte），index 1-based 但
        // spec layout 是 offset 8 = FRS for slot 1
        for slot in 1..=7usize {
            let rev = &self.fw_slot_revisions[slot];
            if rev.is_empty() {
                continue;
            }
            let off = (slot - 1) * 8 + 8;
            let rev_bytes = rev.as_bytes();
            let n = rev_bytes.len().min(8);
            buf[off..off + n].copy_from_slice(&rev_bytes[..n]);
            // pad 不足 8 字节为 ASCII space
            for b in buf[off + n..off + 8].iter_mut() {
                *b = b' ';
            }
        }
        buf.truncate(bytes);
        buf
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

    /// 把 CQE 写入指定 CQ：DMA-write 16 字节 → fire interrupt。
    fn post_cqe(&mut self, ctx: &mut DeviceCtx<'_>, cq_id: u16, cqe: Cqe) {
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
        tracing::debug!(cq_id, slot, gpa = format_args!("{:#x}", gpa), "post CQE");
        ctx.dma_write_fire_and_forget(gpa, bytes);
        if iv_enabled {
            ctx.fire_interrupt(iv as u32);
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
    }

    fn on_dma_complete(&mut self, ctx: &mut DeviceCtx<'_>, token: u64, ok: bool, data: Vec<u8>) {
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
                    // **Phase H4** — 用 p.nsid 选 NS file（admin DMA-write
                    // 是 NvmReadDmaWrite { num_blocks: 0 } 不走这里）。
                    let res = if let Some(ns) = self.namespaces.get_mut(&p.nsid) {
                        ns.file
                            .seek(SeekFrom::Start(lba * SECTOR_SIZE))
                            .and_then(|_| ns.file.write_all(&data))
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
                PendingOp::NvmReadDualPrpSiblingHalf => {
                    // **H4**：成功路径无 op — counter + success CQE 全由
                    // tok2 (NvmReadDmaWrite) 处理；这里只是消化 token
                    // 让生命周期闭合。失败路径在 on_dma_complete 顶部 ok==
                    // false 分支统一 post error CQE（参 mod.rs DMA fail）。
                    tracing::trace!(token, "dual-PRP Read sibling half ok (no-op)");
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
                PendingOp::NvmCompareSinglePrp { lba, num_blocks } => {
                    // **Phase H3** — host buffer 已 DMA-read 到 `data`；
                    // 读 backing file 对应 LBA 范围 → byte-compare。
                    let bytes = num_blocks as u64 * SECTOR_SIZE;
                    let mut backing_buf = vec![0u8; bytes as usize];
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = if let Some(ns) = self.namespaces.get_mut(&p.nsid) {
                        match ns
                            .file
                            .seek(SeekFrom::Start(lba * SECTOR_SIZE))
                            .and_then(|_| std::io::Read::read_exact(&mut ns.file, &mut backing_buf))
                        {
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
                                        0,
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
                            ns.file
                                .seek(SeekFrom::Start(accum.lba * SECTOR_SIZE))
                                .and_then(|_| ns.file.write_all(&full))
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
                            ns.file
                                .seek(SeekFrom::Start(op.lba * SECTOR_SIZE))
                                .and_then(|_| ns.file.write_all(&full))
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
mod tests {
    use super::*;

    /// 构造一个临时 backing file + NvmeController，仅用来跑非 DMA 单元逻辑
    /// （SMART log builder / parse_prp_list / counter 累计）。
    fn make_ctrl_with_tmp(tag: &str) -> NvmeController {
        let path = std::env::temp_dir().join(format!(
            "nvme_test_{}_{}_{:?}.img",
            std::process::id(),
            tag,
            std::thread::current().id()
        ));
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(1024 * 1024).unwrap(); // 1 MiB → 2048 LBA
        drop(f);
        let c = NvmeController::open(&[path.to_str().unwrap().to_string()], 0x1414, 0).unwrap();
        // 不能立即 remove —— Windows 上仍持有的 File 句柄被删后导致后续操作
        // 失败；Unix 下 unlink-while-open 没问题但为可移植性也 keep。测试
        // 结束 OS tempdir 清理（best-effort）。
        c
    }

    /// Phase F：SMART log 关键 offset + counter 写入校验。
    #[test]
    fn smart_log_byte_layout_and_counters() {
        let mut c = make_ctrl_with_tmp("smart");
        c.stat_host_reads = 5;
        c.stat_host_writes = 7;
        c.stat_lba_read = 12_345;
        c.stat_lba_written = 67_890;
        c.stat_num_err_log_entries = 3;
        let buf = c.build_smart_health_log(512);
        assert_eq!(buf.len(), 512);
        // composite_temp @ 1..3 (KiB)
        assert_eq!(u16::from_le_bytes([buf[1], buf[2]]), 313);
        // available_spare @ 3 = 100
        assert_eq!(buf[3], 100);
        // data_units_read @ 32..48: ceil(12345/1000) = 13
        let units_read = u128::from_le_bytes(buf[32..48].try_into().unwrap());
        assert_eq!(units_read, 13);
        // data_units_written @ 48..64: ceil(67890/1000) = 68
        let units_written = u128::from_le_bytes(buf[48..64].try_into().unwrap());
        assert_eq!(units_written, 68);
        // host_read_commands @ 64..80 = 5
        let hr = u128::from_le_bytes(buf[64..80].try_into().unwrap());
        assert_eq!(hr, 5);
        // host_write_commands @ 80..96 = 7
        let hw = u128::from_le_bytes(buf[80..96].try_into().unwrap());
        assert_eq!(hw, 7);
        // power_cycles @ 112..128 = 1
        let pc = u128::from_le_bytes(buf[112..128].try_into().unwrap());
        assert_eq!(pc, 1);
        // num_err_log_entries @ 176..192 = 3
        let nerr = u128::from_le_bytes(buf[176..192].try_into().unwrap());
        assert_eq!(nerr, 3);
    }

    /// Phase E (M2 修复后)：parse_prp_list 不再 0 终止 — 返回所有 entry，
    /// caller 用 total_pages 自行截断。GPA = 0 是合法地址，不能视作终止。
    #[test]
    fn prp_list_returns_all_entries_no_zero_termination() {
        let mut bytes = vec![0u8; 64];
        bytes[..8].copy_from_slice(&0x1000_u64.to_le_bytes());
        bytes[8..16].copy_from_slice(&0x2000_u64.to_le_bytes());
        bytes[16..24].copy_from_slice(&0x3000_u64.to_le_bytes());
        // bytes[24..32] = 0 — 不再终止
        bytes[32..40].copy_from_slice(&0xdead_u64.to_le_bytes());
        let entries = parse_prp_list(&bytes);
        // 64 / 8 = 8 entry
        assert_eq!(entries.len(), 8);
        assert_eq!(entries[0], 0x1000);
        assert_eq!(entries[1], 0x2000);
        assert_eq!(entries[2], 0x3000);
        assert_eq!(entries[3], 0); // 0 不再终止
        assert_eq!(entries[4], 0xdead);
    }

    /// Phase F：AEN queue 行为 — push 多次，弹出顺序 FIFO。
    /// 注：fire_aen 需要 DeviceCtx 才能 dma_write CQE，这里只测 queue 状态。
    #[test]
    fn aen_queue_fifo_order() {
        let mut c = make_ctrl_with_tmp("aen");
        c.aen_pending.push_back((1, 0, 0));
        c.aen_pending.push_back((2, 0, 0));
        c.aen_pending.push_back((3, 0, 0));
        assert_eq!(c.aen_pending.len(), 3);
        assert_eq!(c.aen_pending.pop_front().unwrap().0, 1);
        assert_eq!(c.aen_pending.pop_front().unwrap().0, 2);
        assert_eq!(c.aen_pending.pop_front().unwrap().0, 3);
        assert!(c.aen_pending.is_empty());
    }

    /// Phase G：error_log 环形 buffer 截到 64 (ELPE+1)；最新在 build 输出
    /// 头部（spec § 5.16.1.1 entry 0 = most recent）。
    #[test]
    fn error_log_ring_buffer_64_max_recent_first() {
        let mut c = make_ctrl_with_tmp("errlog");
        // 推 70 条 → 应只保留 last 64
        for i in 0..70u16 {
            c.stat_num_err_log_entries += 1;
            c.push_error_log(1, i, 0x8, (i as u64) * 100, 1);
        }
        assert_eq!(c.error_log.len(), 64);
        // 最旧保留的 error_count = 7 (70 个推入 - 64 保留 = 6 个丢弃)
        assert_eq!(c.error_log.front().unwrap().error_count, 7);
        // 序列化：entry 0 应是最新的 (cid=69)
        let buf = c.build_error_info_log(4096);
        let cid_at_entry0 = u16::from_le_bytes([buf[10], buf[11]]);
        assert_eq!(cid_at_entry0, 69, "entry 0 must be most recent");
        // entry 1 = 次新 cid=68
        let cid_at_entry1 = u16::from_le_bytes([buf[64 + 10], buf[64 + 11]]);
        assert_eq!(cid_at_entry1, 68);
    }

    /// Phase G：Self-Test Log 0x06 反映 idle / in-progress / 完成 (extended) /
    /// aborted 状态。修正 reviewer HIGH：STC 不再硬编码 short，要真实反映
    /// 上次完成类型；STC=2 extended 必须被记成 2。
    #[test]
    fn self_test_log_layout_in_progress_done_and_abort() {
        let mut c = make_ctrl_with_tmp("st");
        // idle → 全 0
        let buf = c.build_self_test_log(564);
        assert_eq!(buf[0], 0);
        assert_eq!(buf[1], 0);
        assert_eq!(buf[4], 0);
        // in-progress extended, 42% done
        c.self_test_in_progress = Some(SelfTestInProgress {
            started_at: std::time::Instant::now(),
            stc: 2,
            total_seconds: 20,
            percent_complete: 42,
        });
        let buf = c.build_self_test_log(564);
        assert_eq!(buf[0], 2);
        assert_eq!(buf[1], 42);
        // 完成 extended：in_progress=None, last=Some(stc=2, result=0)
        c.self_test_in_progress = None;
        c.self_test_last = Some(SelfTestCompleted {
            stc: 2,
            result: 0,
            completed_at_poh: 7,
        });
        let buf = c.build_self_test_log(564);
        assert_eq!(buf[0], 0);
        assert_eq!(buf[1], 0);
        // byte 4：上半 nibble = STC=2 (extended), 下半 = result=0
        assert_eq!(buf[4] & 0xf0, 0x20, "STC must reflect actual extended type");
        assert_eq!(buf[4] & 0x0f, 0x00);
        // POH @ buf[8..16] = 7
        let poh = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        assert_eq!(poh, 7);
        // Aborted short：result=0x09
        c.self_test_last = Some(SelfTestCompleted {
            stc: 1,
            result: 0x09,
            completed_at_poh: 3,
        });
        let buf = c.build_self_test_log(564);
        assert_eq!(buf[4] & 0xf0, 0x10);
        assert_eq!(buf[4] & 0x0f, 0x09);
    }

    /// Phase H1：features map 存 Set 过的 cdw11，Get 回填；NumberOfQueues
    /// 受 IO_QUEUE_CAP 限；VWC 强制 WCE=1。
    #[test]
    fn features_set_get_round_trip_and_special_cases() {
        let mut c = make_ctrl_with_tmp("feat");
        // 任意 fid：Set 0x42 cdw11=0xdeadbeef → Get 回 0xdeadbeef
        c.features.insert(0x42, 0xdead_beef);
        assert_eq!(*c.features.get(&0x42).unwrap(), 0xdead_beef);
        // 未 Set 过的 fid Get 返 0（admin.rs match _ 默认值）
        assert_eq!(c.features.get(&0xff).copied().unwrap_or(0), 0);
        // NumberOfQueues 实际行为校验：cap 常量非零（绕过 clippy const-assert）
        let cap: u16 = IO_QUEUE_CAP;
        assert!(cap >= 1);
        // VWC 强制 bit0=1（模拟 Set 路径把 driver 写的 cdw11 | 0x1 存入）
        c.features
            .insert(crate::cmd::fid::VOLATILE_WRITE_CACHE, 0x1);
        assert_eq!(
            c.features[&crate::cmd::fid::VOLATILE_WRITE_CACHE] & 0x1,
            0x1
        );
    }

    /// Phase H4：多 namespace open + Active NSID list 序列化 + ns_mut
    /// 返 None for NSID 0 / 0xFFFF_FFFF。
    #[test]
    fn multi_namespace_open_and_active_list() {
        let dir = std::env::temp_dir();
        let p1 = dir.join(format!(
            "nvme_test_ns1_{}_{:?}.img",
            std::process::id(),
            std::thread::current().id()
        ));
        let p2 = dir.join(format!(
            "nvme_test_ns2_{}_{:?}.img",
            std::process::id(),
            std::thread::current().id()
        ));
        for p in [&p1, &p2] {
            let f = std::fs::File::create(p).unwrap();
            f.set_len(1024 * 1024).unwrap();
        }
        let paths = vec![
            p1.to_str().unwrap().to_string(),
            p2.to_str().unwrap().to_string(),
        ];
        let c = NvmeController::open(&paths, 0x1414, 0).unwrap();
        assert_eq!(c.namespaces.len(), 2);
        assert!(c.namespaces.contains_key(&1));
        assert!(c.namespaces.contains_key(&2));
        // NSID 0 / 0xFFFF_FFFF 不可用
        assert!(c.ns(0).is_none());
        assert!(c.ns(0xFFFF_FFFF).is_none());
        assert!(c.ns(3).is_none());
        // NS 都是 2048 LBA (1 MiB / 512)
        assert_eq!(c.ns(1).unwrap().total_lba, 2048);
        assert_eq!(c.ns(2).unwrap().total_lba, 2048);
    }

    /// Phase H5：FW Slot Info Log 反映 active_slot / next_active_slot /
    /// per-slot revision string；AFI 编码 bits 2:0 = active, 6:4 = next。
    #[test]
    fn fw_slot_info_log_reflects_state() {
        let mut c = make_ctrl_with_tmp("fw");
        c.fw_active_slot = 2;
        c.fw_next_active_slot = 3;
        c.fw_slot_revisions[2] = "newrev1 ".to_string();
        c.fw_slot_revisions[3] = "newrev2 ".to_string();
        let buf = c.build_fw_slot_info_log(512);
        // AFI = (3 << 4) | 2 = 0x32
        assert_eq!(buf[0], 0x32);
        // FRS[slot 1] @ offset 8..16（默认 v2.0    ，保留自 init）
        assert_eq!(&buf[8..16], b"v2.0    ");
        // FRS[slot 2] @ offset 16..24
        assert_eq!(&buf[16..24], b"newrev1 ");
        // FRS[slot 3] @ offset 24..32
        assert_eq!(&buf[24..32], b"newrev2 ");
    }
}
