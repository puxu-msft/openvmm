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
                        /* Phase A: spec-correct 200+ fields via nvme_spec */
                        IdentifyNamespace::build_v2_bytes(self.total_lba)
                    }
                    0x01 => {
                        // Identify Controller
                        /* Phase A: spec-correct 200+ fields via nvme_spec */
                        IdentifyController::build_v2_bytes(self.vid, self.ssvid)
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
                // NVMe spec § 5.16 Get Log Page。CDW10 bits 7:0 = LID
                // (log page identifier)，bits 31:16 = NUMDL (number of
                // dwords lower, zero-based)。CDW11 高 16 = NUMDU。
                let lid = (sqe.cdw10 & 0xff) as u8;
                let numd_lo = ((sqe.cdw10 >> 16) & 0xffff) as u32;
                let numd_hi = (sqe.cdw11 & 0xffff) as u32;
                let numd = ((numd_hi << 16) | numd_lo) as u64 + 1; // zero-based dwords
                let bytes = (numd * 4).min(4096) as usize; // 我们最多返一 page
                tracing::debug!(lid, bytes, "Get Log Page");
                let buf: Vec<u8> = match lid {
                    0x01 => self.build_error_info_log(bytes),
                    0x02 => self.build_smart_health_log(bytes),
                    0x03 => self.build_fw_slot_info_log(bytes),
                    0x06 => {
                        // Reservation Notification — 我们不支持 reservations，
                        // 返全零（spec 允许 controller 无 reservation 事件时
                        // 返 zero notification record）。
                        vec![0u8; bytes]
                    }
                    _ => {
                        tracing::debug!(lid, "Get Log Page: unknown LID, returning zeros");
                        vec![0u8; bytes]
                    }
                };
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
            admin_opc::FORMAT_NVM => {
                // NVMe spec § 5.14 Format NVM Command。CDW10 含 LBA Format
                // Index (LBAFL) / Secure Erase Settings (SES) / Protection
                // Information (PI/PIL) / Metadata Settings (MSET)。我们只
                // 实现最基本：对单一 LBAF=0 (512B) 的 namespace 做"逻辑
                // 格式化" — 不真去把 backing file 清零（那是 SES≠0 的事），
                // 仅 return success 让 driver 信任格式已完成。真要清零数据
                // 可在 file.set_len(0) + 再扩容；可选实现。
                let lbafl = (sqe.cdw10 & 0xf) as u8;
                let ses = ((sqe.cdw10 >> 9) & 0x7) as u8;
                let pil = ((sqe.cdw10 >> 8) & 0x1) as u8;
                let pi = ((sqe.cdw10 >> 5) & 0x7) as u8;
                let mset = ((sqe.cdw10 >> 4) & 0x1) as u8;
                let ms = ((sqe.cdw10 >> 4) & 0x1) as u8;
                tracing::info!(lbafl, ses, pil, pi, mset, ms, "Format NVM");
                if lbafl != 0 || pi != 0 || mset != 0 || ms != 0 {
                    // 我们只支持 LBAF[0] (512B, no metadata, no PI)。
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                if ses == 1 {
                    // User Data Erase：把 backing file 全清零。
                    if let Ok(size) = self.file.metadata().map(|m| m.len()) {
                        use std::io::Write as _;
                        let _ = self.file.set_len(0).and_then(|_| self.file.set_len(size));
                        let _ = self.file.flush();
                        tracing::info!(size, "Format NVM: user data erased");
                    }
                } else if ses == 2 {
                    // Cryptographic Erase — 我们的 backing 没加密，等价 SES=1。
                    if let Ok(size) = self.file.metadata().map(|m| m.len()) {
                        let _ = self.file.set_len(0).and_then(|_| self.file.set_len(size));
                        tracing::info!(size, "Format NVM: cryptographic erase (= user data erase here)");
                    }
                }
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::FW_COMMIT => {
                // NVMe spec § 5.16 Firmware Commit。我们没真正 firmware；
                // 返 success + cdw0=0 (Activation 完成，无需 reset)。
                tracing::debug!(cid, "Firmware Commit (no-op success)");
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::FW_IMAGE_DOWNLOAD => {
                // NVMe spec § 5.17 Firmware Image Download。我们直接 success
                // 不存（FW_COMMIT 也不真激活，行为一致）。
                tracing::debug!(cid, "Firmware Image Download (discarded, no-op success)");
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::DEVICE_SELF_TEST => {
                // NVMe spec § 5.11 Device Self-test。CDW10 bits 3:0 = STC
                // (0=abort, 1=short, 2=extended, 15=vendor). 我们 always
                // success 不真自检（教学：真实现要起 background task 周期
                // 性更新 self-test log 页）。
                tracing::debug!(cid, "Device Self-Test (no-op success)");
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::NS_MANAGEMENT => {
                // NVMe spec § 5.22 Namespace Management。CDW10 bits 3:0 = SEL
                // (0=Create, 1=Delete)。我们 single-namespace 不真支持创建/
                // 删除，返 INVALID_FIELD（spec 允许，driver 会 fall back 到
                // pre-existing namespace）。
                tracing::debug!(cid, "Namespace Management (not supported; INVALID_FIELD)");
                Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0))
            }
            admin_opc::NS_ATTACHMENT => {
                // NVMe spec § 5.20 Namespace Attachment。单 controller 单
                // namespace 永远 attached；返 success。
                tracing::debug!(cid, "Namespace Attachment (no-op success)");
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::SECURITY_SEND | admin_opc::SECURITY_RECEIVE => {
                // NVMe spec § 5.27/5.28 Security Send/Receive。我们没 TCG
                // OPAL / Sanitize 安全协议；返 INVALID_OPCODE 让 driver
                // 直接放弃（比 success 更安全：避免 driver 误以为命令完成）。
                tracing::debug!(cid, opc = sqe.opcode(), "Security cmd (INVALID_OPCODE)");
                Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_OPCODE, 0))
            }
            admin_opc::SANITIZE => {
                // NVMe spec § 5.26 Sanitize。需要 sanicap > 0 才支持，我们
                // build_v2_bytes 未设 sanicap → 不应被发；驱动若发，
                // 返 INVALID_FIELD。
                tracing::warn!(cid, "Sanitize (sanicap=0; INVALID_FIELD)");
                Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0))
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
