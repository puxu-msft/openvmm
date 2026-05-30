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

use crate::cmd::*;
use crate::regs::*;
use pcie_remote_userspace_sdk::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// 单 namespace 总容量（LBA 数）= 文件长度 / 512。
const SECTOR_SHIFT: u32 = 9;
const SECTOR_SIZE: u64 = 1 << SECTOR_SHIFT;

// 注：本实现用 SDK 分配的 raw DMA token 直接作 HashMap key 路由完成回调；
// 不再做 token 高位 tagging（早期设计想用 tag 标 op 类别，实测 raw token
// 已唯一，多此一举）。

/// Pending IO command 等 DMA 完成。
struct PendingIo {
    sq_id: u16,
    cid: u16,
    sq_head: u16,
    cq_id: u16,
    /// Read 路径：远端 DMA 完成后我们已读出数据，把 data 写到 LBA file，
    /// 然后构造 success CQE。
    /// Write 路径：DMA write 完成后构造 success CQE。
    op: PendingOp,
}

enum PendingOp {
    /// 等 PRP1 内的 data DMA-read 完成 → 写文件 → success CQE。
    /// (单 PRP Write 路径)
    NvmWriteDmaRead { lba: u64, num_blocks: u32 },
    /// 双 PRP Write：PRP1 段 DMA-read 完成 → 填到 `dual_prp_writes[op_id].prp1_data`，
    /// 两段都到了就触发 dispatch_dual_prp。**op_id 与 PRP2 共用**：靠
    /// `dual_prp_writes[op_id]` 中的状态决定何时写盘 + 发 CQE。
    NvmWriteDualPrp { op_id: u64, is_prp1: bool },
    /// 等 DMA-write 数据到 PRP1 完成 → success CQE。(在 NVM Read 路径)
    NvmReadDmaWrite,
}

/// 双 PRP Write 累积：两段 DMA-read 任一先到都填到 prp1_data/prp2_data；
/// 两段都到时合并写盘 + 发 CQE。op_id 独立于 SDK DMA token，由
/// `NvmeController::next_op_id` 单调分配，**两个 PendingOp::NvmWriteDualPrp
/// 共用同一 op_id**，乱序到达也正确处理；只要双 PRP DMA 都失败才不发 CQE
/// （由 `on_dma_complete` 失败路径专门处理）。
struct WriteAccum {
    sq_id: u16,
    cid: u16,
    sq_head: u16,
    cq_id: u16,
    lba: u64,
    num_blocks: u32,
    prp1_data: Option<Vec<u8>>,
    prp2_data: Option<Vec<u8>>,
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
    /// 待 dispatch 的 SQE 队列（按 FIFO 顺序），dispatch 是 sync 逻辑但
    /// 触发 DMA 后异步完成。
    sqe_inbox: Vec<(u16, u16, Sqe)>, // (sq_id, sq_head_after_fetch, sqe)

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
            sqe_inbox: Vec::new(),
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
            asqs, acqs, asq = format_args!("{:#x}", self.asq),
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
        self.sqe_inbox.clear();
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
    fn dispatch_sqe(&mut self, ctx: &mut DeviceCtx<'_>, sq_id: u16, head_after_this: u16, sqe: Sqe) {
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

    /// Admin command dispatch。多数即时完成 → 返回 Some(CQE)；Identify 需
    /// DMA-write 4 KiB 到 PRP1 → 入 pending → 返 None。
    fn dispatch_admin(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        sqe: Sqe,
        cid: u16,
        sq_head: u16,
        cq_id: u16,
    ) -> Option<Cqe> {
        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
        match sqe.opcode() {
            admin_opc::IDENTIFY => {
                // CDW10 bits 7:0 = CNS (Controller or Namespace Structure)
                let cns = (sqe.cdw10 & 0xff) as u8;
                let nsid = sqe.nsid;
                tracing::info!(cns, nsid, "Identify");
                let buf: Vec<u8> = match cns {
                    0x00 => {
                        // Identify Namespace
                        if nsid != 1 {
                            // invalid NSID
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                        }
                        let ns = IdentifyNamespace::build(self.total_lba);
                        ns.as_bytes().to_vec()
                    }
                    0x01 => {
                        // Identify Controller
                        let ctrl = IdentifyController::build(self.vid, self.ssvid);
                        ctrl.as_bytes().to_vec()
                    }
                    0x02 => {
                        // Active NSID list (4 KiB of u32, list active NSIDs)
                        let mut buf = vec![0u8; 4096];
                        buf[..4].copy_from_slice(&1u32.to_le_bytes());
                        buf
                    }
                    0x03 => {
                        // Namespace Identification Descriptor list (NVMe 1.3+)。
                        // 4 KiB；header NIDT=0 表示 list 空 — spec-compliant
                        // 路径，driver 走 EUI64/默认。
                        // 之前返 NGUID 全 0 违反 spec § 5.15.2（"NGUID 0h
                        // indicates the controller does not support NGUID"
                        // → 不应作为 descriptor 返回）。
                        vec![0u8; 4096]
                    }
                    0x06 => {
                        // CNS 0x06 = Identify Controller for the controller list /
                        // I/O Command Set Independent for NS. 返 4 KiB 零即可，
                        // 让 driver 走默认；不发错保证 Windows 后续 init 继续。
                        vec![0u8; 4096]
                    }
                    _ => {
                        tracing::warn!(cns, "Identify: unsupported CNS, returning zeros");
                        // 比 INVALID_FIELD 友好：返 4 KiB 零让 driver 继续。
                        vec![0u8; 4096]
                    }
                };
                // DMA write to PRP1
                self.dma_write_then_complete(ctx, sqe.prp1, buf, cid, 0, sq_head, cq_id);
                None
            }
            admin_opc::CREATE_IO_CQ => {
                // CDW10: bits 15:0 = QID, bits 31:16 = QSIZE-1
                let qid = (sqe.cdw10 & 0xffff) as u16;
                let qsize = ((sqe.cdw10 >> 16) & 0xffff) as u32 + 1;
                // CDW11: bit 0 PC (physically contiguous), bit 1 IEN (interrupts enabled),
                //        bits 31:16 IV (interrupt vector)
                let pc = sqe.cdw11 & 1 != 0;
                let ien = sqe.cdw11 & 2 != 0;
                let iv = ((sqe.cdw11 >> 16) & 0xffff) as u16;
                let prp1 = sqe.prp1;
                if !pc {
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                tracing::info!(qid, qsize, ien, iv, gpa = format_args!("{:#x}", prp1), "Create IO CQ");
                self.cqs.insert(
                    qid,
                    CompletionQueue {
                        base_gpa: prp1,
                        size: qsize,
                        tail: 0,
                        phase: 1,
                        head: 0,
                        interrupt_vector: iv,
                        interrupt_enabled: ien,
                    },
                );
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::CREATE_IO_SQ => {
                let qid = (sqe.cdw10 & 0xffff) as u16;
                let qsize = ((sqe.cdw10 >> 16) & 0xffff) as u32 + 1;
                let pc = sqe.cdw11 & 1 != 0;
                // CDW11 bits 31:16 = CQID
                let cqid = ((sqe.cdw11 >> 16) & 0xffff) as u16;
                let prp1 = sqe.prp1;
                if !pc {
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                if !self.cqs.contains_key(&cqid) {
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                tracing::info!(qid, qsize, cqid, gpa = format_args!("{:#x}", prp1), "Create IO SQ");
                self.sqs.insert(
                    qid,
                    SubmissionQueue {
                        base_gpa: prp1,
                        size: qsize,
                        head: 0,
                        tail: 0,
                        cq_id: cqid,
                    },
                );
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::SET_FEATURES => {
                let fid = (sqe.cdw10 & 0xff) as u8;
                // NVMe spec § 5.21.1.7 (Feature 0x07 = Number of Queues)：driver
                // 写 cdw11 = (NSQR-1) | ((NCQR-1) << 16) 请求 queue 数；controller
                // 在 CQE cdw0 回 (NSQA-1) | ((NCQA-1) << 16) 表示实际授予。
                // 不响应正确 cdw0，nvme.sys 会 bail（无法决定开几个 IO queue）。
                let mut cqe = Cqe::success(cid, 0, sq_head, phase);
                if fid == 0x07 {
                    // v1 仅给 1 IO SQ + 1 IO CQ；0-based。
                    let nsqa = 0u32; // (count-1)
                    let ncqa = 0u32;
                    cqe.cdw0 = nsqa | (ncqa << 16);
                    // 复制到本地变量避免 packed struct 字段取引用 UB。
                    let req_cdw11 = sqe.cdw11;
                    let granted_cdw0 = cqe.cdw0;
                    tracing::info!(
                        requested = format_args!("{:#x}", req_cdw11),
                        granted = format_args!("{:#x}", granted_cdw0),
                        "Set Features Number-of-Queues"
                    );
                } else {
                    tracing::debug!(fid, "Set Features (no-op success)");
                }
                Some(cqe)
            }
            admin_opc::GET_FEATURES => {
                let fid = (sqe.cdw10 & 0xff) as u8;
                tracing::debug!(fid, "Get Features (return cdw0=0)");
                let mut cqe = Cqe::success(cid, 0, sq_head, phase);
                cqe.cdw0 = 0; // 默认值
                Some(cqe)
            }
            admin_opc::KEEP_ALIVE => Some(Cqe::success(cid, 0, sq_head, phase)),
            admin_opc::ASYNC_EVENT_REQUEST => {
                // 不发，driver 会一直等；不返 CQE 实际上是符合 nvme.sys 期望的
                tracing::debug!(cid, "AsyncEventRequest queued (no completion)");
                None
            }
            admin_opc::GET_LOG_PAGE => {
                // 简化：返 4 KiB 零，DMA write 到 PRP1
                let buf = vec![0u8; 4096];
                self.dma_write_then_complete(ctx, sqe.prp1, buf, cid, 0, sq_head, cq_id);
                None
            }
            admin_opc::DELETE_IO_SQ => {
                let qid = (sqe.cdw10 & 0xffff) as u16;
                tracing::info!(qid, "Delete IO SQ");
                self.sqs.remove(&qid);
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::DELETE_IO_CQ => {
                let qid = (sqe.cdw10 & 0xffff) as u16;
                tracing::info!(qid, "Delete IO CQ");
                self.cqs.remove(&qid);
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::ABORT => {
                tracing::debug!(cid, "Abort (no-op success)");
                let mut cqe = Cqe::success(cid, 0, sq_head, phase);
                cqe.cdw0 = 1; // bit 0 = "Could Not Abort" — driver 不报错
                Some(cqe)
            }
            opc => {
                tracing::warn!(opc, "unsupported admin opcode; returning success to keep driver alive");
                // 返 success 而非 INVALID_OPCODE：很多 driver 在
                // boot 期会探测可选 opcode，遇 INVALID_OPCODE 会进入 fallback
                // 路径或直接 fail device。返 success（CQE cdw0=0）通常更安全。
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
        }
    }

    /// IO command dispatch。Read/Write 走 DMA。
    fn dispatch_io(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        sq_id: u16,
        sqe: Sqe,
        cid: u16,
        sq_head: u16,
        cq_id: u16,
    ) -> Option<Cqe> {
        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
        match sqe.opcode() {
            nvm_opc::READ => {
                let cdw10 = sqe.cdw10;
                let cdw11 = sqe.cdw11;
                let cdw12 = sqe.cdw12;
                let prp1 = sqe.prp1;
                let prp2 = sqe.prp2;
                let slba = cdw10 as u64 | ((cdw11 as u64) << 32);
                let nlb = (cdw12 & 0xffff) as u32 + 1;
                let bytes = nlb as u64 * SECTOR_SIZE;
                tracing::debug!(slba, nlb, bytes, prp1 = format_args!("{:#x}", prp1), "NVM READ");
                if bytes > MDTS_MAX_BYTES {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                if slba + nlb as u64 > self.total_lba {
                    return Some(Cqe::error(
                        cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE, 0,
                    ));
                }
                // 从文件读到 buf
                let mut buf = vec![0u8; bytes as usize];
                if let Err(e) = self
                    .file
                    .seek(SeekFrom::Start(slba * SECTOR_SIZE))
                    .and_then(|_| self.file.read_exact(&mut buf))
                {
                    tracing::warn!(error = %e, slba, nlb, "READ: backing file read failed");
                    return Some(Cqe::error(
                        cid, sq_id, sq_head, phase, sc::DATA_TRANSFER_ERROR, 0,
                    ));
                }
                // DMA write to PRP1（v1：bytes ≤ 8 KiB = 2 page = PRP1 + PRP2）
                if bytes <= NVME_PAGE_SIZE {
                    let tok = ctx.dma_write(prp1, buf);
                    self.pending_ios.insert(
                        tok,
                        PendingIo {
                            sq_id, cid, sq_head, cq_id,
                            op: PendingOp::NvmReadDmaWrite,
                        },
                    );
                } else {
                    let half = NVME_PAGE_SIZE as usize;
                    let (b1, b2) = buf.split_at(half);
                    let _tok1 = ctx.dma_write(prp1, b1.to_vec());
                    let tok2 = ctx.dma_write(prp2, b2.to_vec());
                    self.pending_ios.insert(
                        tok2,
                        PendingIo {
                            sq_id, cid, sq_head, cq_id,
                            op: PendingOp::NvmReadDmaWrite,
                        },
                    );
                }
                None
            }
            nvm_opc::WRITE => {
                // 复制 packed 字段到本地变量（packed struct field 取引用 UB，
                // 直接 as u64 在新 rustc 也会触发警告）。
                let cdw10 = sqe.cdw10;
                let cdw11 = sqe.cdw11;
                let cdw12 = sqe.cdw12;
                let prp1 = sqe.prp1;
                let prp2 = sqe.prp2;
                let slba = cdw10 as u64 | ((cdw11 as u64) << 32);
                let nlb = (cdw12 & 0xffff) as u32 + 1;
                let bytes = nlb as u64 * SECTOR_SIZE;
                tracing::debug!(slba, nlb, bytes, prp1 = format_args!("{:#x}", prp1), "NVM WRITE");
                if bytes > MDTS_MAX_BYTES {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                if slba + nlb as u64 > self.total_lba {
                    return Some(Cqe::error(
                        cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE, 0,
                    ));
                }
                // DMA read from PRP1 (+ 可选 PRP2)
                if bytes <= NVME_PAGE_SIZE {
                    let tok = ctx.dma_read(prp1, bytes as u32);
                    self.pending_ios.insert(
                        tok,
                        PendingIo {
                            sq_id, cid, sq_head, cq_id,
                            op: PendingOp::NvmWriteDmaRead { lba: slba, num_blocks: nlb },
                        },
                    );
                } else {
                    // 双 PRP：PRP1 = 第一页 (4 KiB)，PRP2 = 第二页（最多 4 KiB）。
                    // 分配独立 op_id，PRP1/PRP2 完成回调通过 op_id 关联 ——
                    // 解决 PRP2 先到 PRP1 的乱序数据损坏 + leak 问题（C1）。
                    let op_id = self.next_op_id;
                    self.next_op_id = self.next_op_id.wrapping_add(1);
                    self.dual_prp_writes.insert(
                        op_id,
                        WriteAccum {
                            sq_id, cid, sq_head, cq_id,
                            lba: slba, num_blocks: nlb,
                            prp1_data: None, prp2_data: None,
                        },
                    );
                    let prp2_bytes = (bytes - NVME_PAGE_SIZE) as u32;
                    let tok1 = ctx.dma_read(prp1, NVME_PAGE_SIZE as u32);
                    let tok2 = ctx.dma_read(prp2, prp2_bytes);
                    self.pending_ios.insert(
                        tok1,
                        PendingIo {
                            sq_id, cid, sq_head, cq_id,
                            op: PendingOp::NvmWriteDualPrp { op_id, is_prp1: true },
                        },
                    );
                    self.pending_ios.insert(
                        tok2,
                        PendingIo {
                            sq_id, cid, sq_head, cq_id,
                            op: PendingOp::NvmWriteDualPrp { op_id, is_prp1: false },
                        },
                    );
                }
                None
            }
            nvm_opc::FLUSH => {
                let _ = self.file.sync_all();
                Some(Cqe::success(cid, sq_id, sq_head, phase))
            }
            opc => {
                tracing::warn!(opc, "unsupported NVM opcode");
                Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_OPCODE, 0))
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
                op: PendingOp::NvmReadDmaWrite,
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
            0x0c => self.intms |= value as u32, // mask set
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
                        lba, num_blocks,
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
                        Ok(()) => Cqe::success(p.cid, p.sq_id, p.sq_head, phase),
                        Err(e) => {
                            tracing::warn!(error = %e, lba, num_blocks, "NVM Write file write failed");
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR, 0)
                        }
                    };
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmReadDmaWrite => {
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
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
                            Ok(()) => Cqe::success(accum.cid, accum.sq_id, accum.sq_head, phase),
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    lba = accum.lba,
                                    num_blocks = accum.num_blocks,
                                    "NVM Write dual-PRP file write failed"
                                );
                                Cqe::error(
                                    accum.cid, accum.sq_id, accum.sq_head, phase,
                                    sc::DATA_TRANSFER_ERROR, 0,
                                )
                            }
                        };
                        self.post_cqe(ctx, accum.cq_id, cqe);
                    }
                }
            }
            return;
        }
        tracing::debug!(token, "DMA completion for unknown token (likely 2nd PRP)");
    }
}
