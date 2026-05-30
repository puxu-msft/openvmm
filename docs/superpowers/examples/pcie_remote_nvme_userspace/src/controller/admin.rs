// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe Admin command dispatch — Identify、Create/Delete IO SQ/CQ、
//! Set/Get Features、Keep Alive、ASYNC Event、Get Log Page、Abort、…
//!
//! 拆出来减小 `controller.rs` 体积（H6 reviewer 建议）。本文件**只**
//! 提供 `impl NvmeController { fn dispatch_admin(...) }`；其他方法
//! （post_cqe / dma_write_then_complete 等）仍在 `controller/mod.rs`，
//! 被这里通过 self.* 调用。
//!
//! 注：所有 packed struct field 访问已 copy 到本地变量，避免 UB。

use crate::cmd::*;
use crate::controller::NvmeController;
use crate::regs::*;
use pcie_remote_userspace_sdk::*;
use zerocopy::IntoBytes;

impl NvmeController {
    /// Admin command dispatch。多数即时完成 → 返回 Some(CQE)；Identify 需
    /// DMA-write 4 KiB 到 PRP1 → 入 pending → 返 None。
    pub(super) fn dispatch_admin(
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
}
