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

// 注：本实现用 SDK 分配的 raw DMA token 直接作 HashMap key 路由完成回调；
// 不再做 token 高位 tagging（早期设计想用 tag 标 op 类别，实测 raw token
// 已唯一，多此一举）。

/// Pending IO command 等 DMA 完成。
pub(super) struct PendingIo {
    sq_id: u16,
    cid: u16,
    sq_head: u16,
    cq_id: u16,
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

/// NVMe Controller 主结构 — 实现 `PcieDevice`。
pub struct NvmeController {
    // ----- Backing file -----
    file: File,
    total_lba: u64,

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
    /// AsyncEventRequest 已收到、待 fire 的 CID/SQ context FIFO。
    /// 当有事件触发时弹一条，构造 CQE 带 event info → post 到 admin CQ。
    /// 元组：(cid, sq_id, sq_head, cq_id)。
    pub(super) aen_pending: std::collections::VecDeque<(u16, u16, u16, u16)>,

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

impl NvmeController {
    /// `backing_file`：必须存在；其大小决定 namespace 容量（向下 round 到 512 倍数）。
    pub fn open(backing_file_path: &str, vid: u16, ssvid: u16) -> anyhow::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(backing_file_path)?;
        let size = file.metadata()?.len();
        let total_lba = size >> SECTOR_SHIFT;
        if total_lba == 0 {
            return Err(anyhow::anyhow!(
                "backing file too small (< 512 bytes): {backing_file_path}"
            ));
        }
        tracing::info!(
            backing_file_path,
            size,
            total_lba,
            "NVMe controller: backing file opened"
        );
        Ok(Self {
            file,
            total_lba,
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

    /// **Phase F** — 触发 AEN (Async Event Notification)。
    ///
    /// NVMe spec § 5.2：当 controller 发生 async 事件（health critical /
    /// namespace change / log page available / firmware activate），从
    /// 之前 driver 投入的 AsyncEventRequest pending 队列弹一条 (cid, sq,
    /// head, cq)，构造 CQE 携 cdw0 = `[type:8 | info:8 | log_id:8 | rsvd:8]`，
    /// post 到对应 CQ + raise interrupt。Driver 看到 CQE 后会读对应 log
    /// page，然后再投新的 AER。
    ///
    /// 当前没有自然事件源（无温度传感器/无 namespace 添加），此函数提
    /// 供基础设施 + tests 用；外部代码可调 `fire_aen(0x02, 0x00, 0x02)`
    /// 模拟 SMART critical。返 true = AER 已 fire；false = 无 pending AER
    /// 可弹（事件丢弃 — driver 下次投 AER 不会重发，与真硬件一致）。
    #[allow(dead_code)]
    pub(super) fn fire_aen(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        aen_type: u8,
        aen_info: u8,
        log_id: u8,
    ) -> bool {
        let Some((cid, sq_id, sq_head, cq_id)) = self.aen_pending.pop_front() else {
            tracing::debug!(
                aen_type,
                aen_info,
                log_id,
                "fire_aen: no pending AER, event dropped"
            );
            return false;
        };
        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
        let mut cqe = Cqe::success(cid, sq_id, sq_head, phase);
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
    fn build_error_info_log(&self, bytes: usize) -> Vec<u8> {
        // Spec：每 entry 64 字节，list 长度 = ELPE+1 (Identify Controller
        // .elpe，我们当前 = 0 → 1 entry)。NUMDL 给的 bytes 通常 ≥ 64。
        let mut buf = vec![0u8; bytes];
        // Entry 0 全 0 表示 "no error logged yet"，spec allowed。
        let _ = &mut buf; // 显式 mark used
        buf
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
        let units_read = (self.stat_lba_read / 1000) as u128;
        let units_written = (self.stat_lba_written / 1000) as u128;
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

    /// **Phase C** — Log Page 0x03 Firmware Slot Information。
    ///
    /// NVMe spec § 5.16.1.3，512 字节固定。AFI bit 2:0 = 当前激活槽，
    /// bits 6:4 = 下次启动激活槽。FRS[N] = 8 字节 ASCII FW revision。
    fn build_fw_slot_info_log(&self, bytes: usize) -> Vec<u8> {
        let mut buf = vec![0u8; bytes.max(512)];
        // AFI: 当前激活槽 = 1, 下次启动激活槽 = 1
        buf[0] = 0x11; // bits 2:0 = 1, bits 6:4 = 1
        // FRS[0] (offset 8-15): firmware revision string (ASCII)
        let fr = b"v2.0    ";
        buf[8..16].copy_from_slice(fr);
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

    fn tick(&mut self, _ctx: &mut DeviceCtx<'_>) {
        // 没事可做；NVMe 是 reactive
    }

    fn on_dma_complete(&mut self, ctx: &mut DeviceCtx<'_>, token: u64, ok: bool, data: Vec<u8>) {
        if !ok {
            tracing::warn!(token, "DMA failed");
            // 清相关 pending（IO 或 fetch）
            self.pending_fetches.remove(&token);
            if let Some(p) = self.pending_ios.remove(&token) {
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
                    let res = self
                        .file
                        .seek(SeekFrom::Start(lba * SECTOR_SIZE))
                        .and_then(|_| self.file.write_all(&data));
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
                        let res = self
                            .file
                            .seek(SeekFrom::Start(accum.lba * SECTOR_SIZE))
                            .and_then(|_| self.file.write_all(&full));
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
                        // 借用 PendingIo 共用字段 sq_id/cid/sq_head/cq_id
                        let (sq_id, cid, sq_head, cq_id) = {
                            let op = &self.prp_list_ops[&op_id];
                            (op.sq_id, op.cid, op.sq_head, op.cq_id)
                        };
                        self.pending_ios.insert(
                            tok,
                            PendingIo {
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
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
                        let res = self
                            .file
                            .seek(SeekFrom::Start(op.lba * SECTOR_SIZE))
                            .and_then(|_| self.file.write_all(&full));
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
                    // PRP list 页到达 (Read 路径)。Parse + 把 backing file
                    // 数据 dma_write 到 PRP1 + list 中每个 GPA 页。
                    let entries = parse_prp_list(&data);
                    // PRP list 解析结果 + 当前 op 关键字段 — 提取出来一次性
                    // 取走（避免后续多次借 self）。
                    type ReadPrpListInfo = (u32, u64, u32, u64, Vec<u64>, Vec<u8>);
                    let info: Option<ReadPrpListInfo> =
                        self.prp_list_ops.get_mut(&op_id).map(|op| {
                            let take = (op.total_pages - 1) as usize;
                            let list: Vec<u64> = entries.into_iter().take(take).collect();
                            // PRP1 数据已在 dispatch_io 时读入 data_pages[0]
                            let prp1_buf = op.data_pages[0]
                                .take()
                                .unwrap_or_else(|| vec![0u8; NVME_PAGE_SIZE as usize]);
                            (
                                op.total_pages,
                                op.lba,
                                op.num_blocks,
                                op.prp1_gpa,
                                list,
                                prp1_buf,
                            )
                        });
                    let Some((total_pages, lba, nlb, prp1_gpa, list, prp1_buf)) = info else {
                        tracing::warn!(op_id, "ReadPrpListFetch unknown op_id");
                        return;
                    };
                    if let Some(op) = self.prp_list_ops.get_mut(&op_id) {
                        op.list_entries = Some(list.clone());
                    }
                    let (sq_id, cid, sq_head, cq_id) = {
                        let op = &self.prp_list_ops[&op_id];
                        (op.sq_id, op.cid, op.sq_head, op.cq_id)
                    };
                    // **Step 2a**: dma_write PRP1 数据（page idx 0）。
                    // 注：dispatch_io Read 分支已把 file→buf 读入 prp1_buf
                    // 缓存在 data_pages[0]，这里发起 dma_write。完成回调
                    // (PendingOp::NvmReadPrpListData{op_id, page_idx:0}) +
                    // 其它 list 页完成回调汇总后 post CQE。
                    let tok_prp1 = ctx.dma_write(prp1_gpa, prp1_buf);
                    self.pending_ios.insert(
                        tok_prp1,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            op: PendingOp::NvmReadPrpListData { op_id, page_idx: 0 },
                        },
                    );
                    // **Step 2b**: 读 backing file 剩余页，dma_write 到 list GPA。
                    let total_bytes = nlb as u64 * SECTOR_SIZE;
                    for (i, gpa) in list.iter().enumerate() {
                        let page_idx = (i + 1) as u32;
                        let page_off = page_idx as u64 * NVME_PAGE_SIZE;
                        let page_bytes = (total_bytes - page_off).min(NVME_PAGE_SIZE) as usize;
                        let mut buf = vec![0u8; page_bytes];
                        if let Err(e) = self
                            .file
                            .seek(SeekFrom::Start(lba * SECTOR_SIZE + page_off))
                            .and_then(|_| std::io::Read::read_exact(&mut self.file, &mut buf))
                        {
                            tracing::warn!(error = %e, op_id, page_idx, "ReadPrpList file read failed");
                            let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
                            let cqe =
                                Cqe::error(cid, sq_id, sq_head, phase, sc::DATA_TRANSFER_ERROR, 0);
                            self.prp_list_ops.remove(&op_id);
                            self.post_cqe(ctx, cq_id, cqe);
                            return;
                        }
                        let tok = ctx.dma_write(*gpa, buf);
                        self.pending_ios.insert(
                            tok,
                            PendingIo {
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
                                op: PendingOp::NvmReadPrpListData { op_id, page_idx },
                            },
                        );
                    }
                    let _ = total_pages; // 文档可读性
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
/// 尾部全零 entry 视为终止。Phase E v1 不处理 list chaining（最后一个
/// entry == 下一个 PRP list 页指针）；MDTS=5 = 32 page 远小于 1 page list
/// 容量（512 entry），暂不会触发。
fn parse_prp_list(data: &[u8]) -> Vec<u64> {
    let mut out = Vec::new();
    for chunk in data.chunks_exact(8) {
        let v = u64::from_le_bytes(chunk.try_into().unwrap());
        if v == 0 {
            break;
        }
        out.push(v);
    }
    out
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
        let c = NvmeController::open(path.to_str().unwrap(), 0x1414, 0).unwrap();
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
        // data_units_read @ 32..48: 12345/1000 = 12
        let units_read = u128::from_le_bytes(buf[32..48].try_into().unwrap());
        assert_eq!(units_read, 12);
        // data_units_written @ 48..64: 67890/1000 = 67
        let units_written = u128::from_le_bytes(buf[48..64].try_into().unwrap());
        assert_eq!(units_written, 67);
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

    /// Phase E：parse_prp_list 在尾部 0 处终止 + 正确解析 LE u64。
    #[test]
    fn prp_list_parses_until_zero() {
        let mut bytes = vec![0u8; 64];
        bytes[..8].copy_from_slice(&0x1000_u64.to_le_bytes());
        bytes[8..16].copy_from_slice(&0x2000_u64.to_le_bytes());
        bytes[16..24].copy_from_slice(&0x3000_u64.to_le_bytes());
        // bytes[24..32] = 0 → 终止
        bytes[32..40].copy_from_slice(&0xdead_u64.to_le_bytes()); // 应被忽略
        let entries = parse_prp_list(&bytes);
        assert_eq!(entries, vec![0x1000, 0x2000, 0x3000]);
    }

    /// Phase F：AEN queue 行为 — push 多次，弹出顺序 FIFO。
    /// 注：fire_aen 需要 DeviceCtx 才能 dma_write CQE，这里只测 queue 状态。
    #[test]
    fn aen_queue_fifo_order() {
        let mut c = make_ctrl_with_tmp("aen");
        c.aen_pending.push_back((1, 0, 0, 0));
        c.aen_pending.push_back((2, 0, 0, 0));
        c.aen_pending.push_back((3, 0, 0, 0));
        assert_eq!(c.aen_pending.len(), 3);
        assert_eq!(c.aen_pending.pop_front().unwrap().0, 1);
        assert_eq!(c.aen_pending.pop_front().unwrap().0, 2);
        assert_eq!(c.aen_pending.pop_front().unwrap().0, 3);
        assert!(c.aen_pending.is_empty());
    }
}
