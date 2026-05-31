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

use crate::cmd;
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
                        // Identify Namespace — 用 NSID 选具体 NS（Phase H4）
                        let Some(ns) = self.ns(nsid) else {
                            return Some(Cqe::error(
                                cid,
                                0,
                                sq_head,
                                phase,
                                sc::INVALID_NAMESPACE,
                                0,
                            ));
                        };
                        /* Phase A: spec-correct 200+ fields via nvme_spec */
                        IdentifyNamespace::build_v2_bytes(ns.total_lba)
                    }
                    0x01 => {
                        // Identify Controller
                        /* Phase A: spec-correct 200+ fields via nvme_spec */
                        IdentifyController::build_v2_bytes(
                            self.vid,
                            self.ssvid,
                            self.namespaces.len() as u32,
                        )
                    }
                    0x02 => {
                        // Active NSID list — 列所有已注册 NSID（spec § 5.15.1）。
                        // Phase H4：动态枚举 self.namespaces；按 NSID 升序。
                        let mut buf = vec![0u8; 4096];
                        let mut nsids: Vec<u32> = self.namespaces.keys().copied().collect();
                        nsids.sort();
                        for (i, n) in nsids.iter().enumerate() {
                            let off = i * 4;
                            if off + 4 > buf.len() {
                                break;
                            }
                            buf[off..off + 4].copy_from_slice(&n.to_le_bytes());
                        }
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
                tracing::info!(
                    qid,
                    qsize,
                    ien,
                    iv,
                    gpa = format_args!("{:#x}", prp1),
                    "Create IO CQ"
                );
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
                tracing::info!(
                    qid,
                    qsize,
                    cqid,
                    gpa = format_args!("{:#x}", prp1),
                    "Create IO SQ"
                );
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
                // **Phase H1+H2** — NVMe spec § 5.21 Set Features。
                // CDW10 bits 7:0 = FID；bits 31 = SV (Save，我们不实现持
                // 久化，保留 spec 默认 = 易失，reset 清空)。CDW11 = 值。
                // 大部分 fid 行为 = 存进 features map，Get 回填；少数
                // (0x07 NumberOfQueues、0x06 VWC) 有 controller 强约束。
                let fid = (sqe.cdw10 & 0xff) as u8;
                let cdw11 = sqe.cdw11;
                let mut cqe = Cqe::success(cid, 0, sq_head, phase);
                match fid {
                    cmd::fid::NUMBER_OF_QUEUES => {
                        // 真实硬件常授 ≤ requested；我们 cap=IO_QUEUE_CAP。
                        // driver 写 cdw11 = (NSQR-1) | ((NCQR-1) << 16) 请
                        // 求 queue 数；controller 在 CQE cdw0 回 (NSQA-1)
                        // | ((NCQA-1) << 16) 表示实际授予（0-based）。
                        let req_nsq = (cdw11 & 0xffff) as u16 + 1;
                        let req_ncq = ((cdw11 >> 16) & 0xffff) as u16 + 1;
                        let granted = req_nsq.min(req_ncq).min(crate::controller::IO_QUEUE_CAP);
                        self.granted_io_queues = granted;
                        let nsqa_minus_1 = (granted - 1) as u32;
                        let ncqa_minus_1 = (granted - 1) as u32;
                        cqe.cdw0 = nsqa_minus_1 | (ncqa_minus_1 << 16);
                        let req_dump = cdw11;
                        let granted_dump = cqe.cdw0;
                        tracing::info!(
                            req_nsq,
                            req_ncq,
                            granted,
                            requested = format_args!("{:#x}", req_dump),
                            granted_cdw0 = format_args!("{:#x}", granted_dump),
                            "Set Features Number-of-Queues"
                        );
                        // 不写入 features map：Get 时直接根据 granted_io_queues 重算
                    }
                    cmd::fid::VOLATILE_WRITE_CACHE => {
                        // VWC bit 0 = WCE (Write Cache Enable)。我们 backing
                        // file 始终有 host page cache → WCE 实际不可关；
                        // 接受 driver 写但行为不变；Get 回 WCE=1。
                        self.features.insert(fid, cdw11 | 0x1);
                        tracing::info!(
                            wce = (cdw11 & 0x1),
                            "Set Features VWC (强制 WCE=1 反映 backing cache)"
                        );
                    }
                    cmd::fid::TIMESTAMP => {
                        // spec § 5.21.1.14：cdw11 在 Set 时 reserved；
                        // 真值通过 PRP1 指向 8 字节 timestamp。我们当前
                        // 只走 cdw11 路径不做 PRP fetch（spec 允许返
                        // success 但实际 ignore，driver fallback host clock）。
                        // 把 0 存进让 Get 至少能回。
                        self.features.insert(fid, 0);
                        tracing::debug!("Set Features Timestamp (no-PRP, stored 0)");
                    }
                    _ => {
                        // 其它 fid：原样存 cdw11，Get 回填
                        self.features.insert(fid, cdw11);
                        tracing::debug!(
                            fid,
                            cdw11 = format_args!("{:#x}", cdw11),
                            "Set Features (stored)"
                        );
                    }
                }
                Some(cqe)
            }
            admin_opc::GET_FEATURES => {
                // **Phase H1** — spec § 5.21.2 Get Features。CDW10 bits 7:0
                // = FID，bits 10:8 = SEL (0=current, 1=default, 2=saved,
                // 3=supported)。我们都按 current 返回。
                let fid = (sqe.cdw10 & 0xff) as u8;
                let sel = ((sqe.cdw10 >> 8) & 0x7) as u8;
                let mut cqe = Cqe::success(cid, 0, sq_head, phase);
                cqe.cdw0 = match fid {
                    cmd::fid::NUMBER_OF_QUEUES => {
                        // 实时返实际授予数（不查 features map）
                        let g = (self.granted_io_queues - 1) as u32;
                        g | (g << 16)
                    }
                    cmd::fid::VOLATILE_WRITE_CACHE => {
                        // 始终回 WCE=1（参 Set 路径）
                        *self.features.get(&fid).unwrap_or(&0x1)
                    }
                    _ => {
                        // 其它：未 Set 过返 0 = spec 默认（多数 fid 默认 0
                        // 即可，少数 fid 如 ASYNC_EVENT_CONFIG 默认 0 也合理）
                        *self.features.get(&fid).unwrap_or(&0)
                    }
                };
                tracing::debug!(
                    fid,
                    sel,
                    cdw0 = format_args!("{:#x}", { cqe.cdw0 }),
                    "Get Features"
                );
                Some(cqe)
            }
            admin_opc::KEEP_ALIVE => Some(Cqe::success(cid, 0, sq_head, phase)),
            admin_opc::ASYNC_EVENT_REQUEST => {
                // **Phase F** — NVMe spec § 5.2 Async Event Request。Driver
                // 提交此命令后 controller 必须挂起（不立即返 CQE）；当任意
                // 异步事件触发时（health critical、namespace change、log
                // page available 等），controller 从挂起队列弹一条 AER 并
                // 用 CQE.cdw0 编码事件类型/info/log page id 完成它。
                //
                // 设计选择：用 VecDeque 队列；驱动通常会预投 4 个 AER
                // 让 controller 缓冲事件突发（Identify Controller AERL+1 个）。
                // 暂未触发实际事件 → AER 永挂；在 disable() 清空。未来
                // 加 fire_namespace_changed / fire_log_available 等时会调
                // self.fire_aen()。
                self.aen_pending.push_back((cid, 0, cq_id));
                tracing::debug!(
                    cid,
                    queued = self.aen_pending.len(),
                    "AsyncEventRequest queued (Phase F: real queue, fire on event)"
                );
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
                let bytes_req = numd * 4; // bytes
                tracing::debug!(lid, bytes = bytes_req, "Get Log Page");
                // **H1 修复**：我们目前没实现 PRP list（Phase E TODO），
                // 单次最多用 PRP1+PRP2 = 8 KiB；超过返 INVALID_FIELD 让
                // driver 明确知道（不再 silent truncate）。
                if bytes_req > 8192 {
                    tracing::warn!(
                        lid,
                        bytes = bytes_req,
                        "Get Log Page: request > 8 KiB unsupported (no PRP list yet)"
                    );
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                let bytes = bytes_req as usize;
                let buf: Vec<u8> = match lid {
                    // NVMe 2.0c spec 表 5-105 标准 LID：
                    // 0x01 Error Information
                    // 0x02 SMART / Health Information
                    // 0x03 Firmware Slot Information
                    // 0x04 Changed Namespace List
                    // 0x05 Commands Supported and Effects
                    // 0x06 Device Self-test
                    // 0x07 Telemetry Host-Initiated
                    // 0x08 Telemetry Controller-Initiated
                    // 0x09 Endurance Group
                    // 0x0a Predictable Latency Per NVM Set
                    // 0x0b Predictable Latency Event Aggregate
                    // 0x0c Asymmetric Namespace Access
                    // 0x0d Persistent Event Log
                    // 0x0e LBA Status Information
                    // 0x0f Endurance Group Event Aggregate
                    // 0x80 Reservation Notification
                    // 0x81 Sanitize Status
                    0x01 => super::logs::build_error_info(self, bytes),
                    0x02 => super::logs::build_smart_health(self, bytes),
                    0x03 => super::logs::build_fw_slot_info(self, bytes),
                    0x06 => super::logs::build_self_test(self, bytes),
                    0x80 => super::logs::build_reservation_notification(self, bytes),
                    _ => {
                        tracing::debug!(
                            lid = format_args!("{:#x}", lid),
                            "Get Log Page: unknown LID, returning zeros"
                        );
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
                // NVMe spec § 5.14 Format NVM Command。CDW10 字段位段
                // （spec 表 5-72）：
                //   bits 3:0   LBAFL — LBA Format Index (low 4 bits)
                //   bit 4      MSET — Metadata Settings (0=as-is, 1=inline)
                //   bits 7:5   PI   — Protection Information (0..7)
                //   bit 8      PIL  — Protection Info Location
                //   bits 11:9  SES  — Secure Erase Settings (0=no, 1=user
                //                     data erase, 2=cryptographic erase)
                //   bits 13:12 ZF / LBAFU (NVMe 2.0 LBAF index 高 2 位)
                let lbafl = (sqe.cdw10 & 0xf) as u8;
                let mset = ((sqe.cdw10 >> 4) & 0x1) as u8;
                let pi = ((sqe.cdw10 >> 5) & 0x7) as u8;
                let pil = ((sqe.cdw10 >> 8) & 0x1) as u8;
                let ses = ((sqe.cdw10 >> 9) & 0x7) as u8;
                tracing::info!(lbafl, mset, pi, pil, ses, "Format NVM");
                if lbafl != 0 || pi != 0 || mset != 0 {
                    // **Phase H7 reviewer C3 修复** — 完整 PI 路径未实现：
                    // SECTOR_SIZE 在 mod.rs 是 const 512，IO pipeline 用
                    // 512 算 offset。若接受 LBAF[1] (4 KiB) 但 IO 仍按
                    // 512B sector → 8x address 错位 → 87.5% 数据静默损坏。
                    //
                    // 在 SECTOR_SIZE 改为 per-NS + PI CRC 引擎落地前，
                    // Format 必须拒绝 lbafl != 0 / pi != 0。Identify NS
                    // 仍声明 DPC 能力（spec § 8.3 允许 capability 暴露
                    // 但 disabled）；driver 看到 LBAF[1] 也行，但 Format
                    // 选它会被这里拒绝。
                    tracing::warn!(
                        lbafl,
                        pi,
                        mset,
                        "Format rejected: only LBAF[0]+PI=0 supported (SECTOR_SIZE/PI未真实现)"
                    );
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                // **C1 修复**：FORMAT 不能在有 in-flight IO 时执行。否则
                // outstanding dma_read 完成后 write_all 到已被 truncate 后
                // 重新分配的 sparse hole，导致 driver 视角"擦除前的写已
                // 完成"但盘上随机残留 in-flight 写。返 sc=0x84 (Format
                // In Progress) 让 driver 重试。
                if !self.pending_ios.is_empty() || !self.dual_prp_writes.is_empty() {
                    tracing::warn!(
                        pending_ios = self.pending_ios.len(),
                        pending_dual = self.dual_prp_writes.len(),
                        "Format NVM rejected: IO in flight"
                    );
                    // SC 0x84 Format In Progress (NVMe 1.4 § 4.6.1.2.1)。
                    return Some(Cqe::error(cid, 0, sq_head, phase, 0x84, 0));
                }
                if ses == 1 || ses == 2 {
                    // SES=1 User Data Erase / SES=2 Cryptographic Erase。
                    // **教学说明**：这里用 `set_len(0) + set_len(size)` 创
                    // 建 sparse hole — host filesystem 看 hole 区域返零，
                    // 但底层物理扇区**未真写零**（不是 SCSI BLKZEROOUT /
                    // ATA TRIM 那种擦除）。真安全擦除需 write 全零 + fsync
                    // 或调 fallocate(FALLOC_FL_ZERO_RANGE)。本 example 教学
                    // 用，sparse hole 行为对 guest 而言等价 "全零盘"。SES=2
                    // 没有加密 key 销毁概念，因为我们没加密；行为等价 SES=1。
                    //
                    // **Phase H4**：sqe.nsid 0xFFFF_FFFF = broadcast，format
                    // 所有 NS；具体 NSID 仅 format 该 NS。
                    let nsid = sqe.nsid;
                    let targets: Vec<u32> = if nsid == 0xFFFF_FFFF {
                        self.namespaces.keys().copied().collect()
                    } else if self.namespaces.contains_key(&nsid) {
                        vec![nsid]
                    } else {
                        return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_NAMESPACE, 0));
                    };
                    for target_nsid in targets {
                        let ns = self.namespaces.get_mut(&target_nsid).unwrap();
                        let size = match ns.file.metadata() {
                            Ok(m) => m.len(),
                            Err(e) => {
                                tracing::warn!(error = %e, nsid = target_nsid, "Format NVM: stat failed");
                                return Some(Cqe::error(
                                    cid,
                                    0,
                                    sq_head,
                                    phase,
                                    sc::INTERNAL_ERROR,
                                    0,
                                ));
                            }
                        };
                        if let Err(e) = ns.file.set_len(0).and_then(|_| ns.file.set_len(size)) {
                            tracing::warn!(error = %e, nsid = target_nsid, "Format NVM: truncate failed");
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INTERNAL_ERROR, 0));
                        }
                        use std::io::Write as _;
                        if let Err(e) = ns.file.flush() {
                            tracing::warn!(error = %e, nsid = target_nsid, "Format NVM: flush failed");
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INTERNAL_ERROR, 0));
                        }
                        tracing::info!(
                            nsid = target_nsid,
                            size,
                            ses,
                            "Format NVM: sparse-hole erase done"
                        );
                    }
                }
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::FW_COMMIT => {
                // **Phase H5** — NVMe spec § 5.16 Firmware Commit。
                // CDW10 字段：
                //   bits 2:0   FS  — Firmware Slot (1..7)
                //   bits 5:3   CA  — Commit Action
                //     0 = downloaded image replaces FS (no activation)
                //     1 = downloaded image replaces FS + activates on next reset
                //     2 = activate existing FS on next reset
                //     3 = activate downloaded image immediately
                //   bit 31     BPID — Boot Partition ID（无 BP 不用）
                // CQE cdw0：activation 状态码：
                //   0x00 = success (no reset needed)
                //   0x01 = success, NVM subsystem reset required
                //   0x02 = success, controller-level reset required
                //   0x10 + reason = error
                let fs = (sqe.cdw10 & 0x7) as u8;
                let ca = ((sqe.cdw10 >> 3) & 0x7) as u8;
                tracing::info!(fs, ca, "Firmware Commit");
                if !(1..=7).contains(&fs) {
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                let buf_len = self.fw_download_buf.len();
                match ca {
                    0 | 1 => {
                        // 把 download buffer 内容 'flash' 到 slot FS。我们用
                        // download size 头 8 字节作 ASCII revision string；
                        // 真硬件这是 vendor 编码 image。
                        if buf_len < 8 {
                            tracing::warn!(buf_len, "FW Commit: insufficient downloaded image");
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                        }
                        let rev_bytes: [u8; 8] = self.fw_download_buf[..8].try_into().unwrap();
                        let rev = String::from_utf8_lossy(&rev_bytes).to_string();
                        self.fw_slot_revisions[fs as usize] = rev.clone();
                        tracing::info!(fs, rev = %rev, "FW Commit: slot replaced");
                        if ca == 1 {
                            self.fw_next_active_slot = fs;
                        }
                    }
                    2 => {
                        // 仅 mark next-boot active
                        if self.fw_slot_revisions[fs as usize].is_empty() {
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                        }
                        self.fw_next_active_slot = fs;
                    }
                    3 => {
                        // 立即激活
                        if self.fw_slot_revisions[fs as usize].is_empty() {
                            // 没 image：用 download buffer 先 flash 再激活
                            if buf_len < 8 {
                                return Some(Cqe::error(
                                    cid,
                                    0,
                                    sq_head,
                                    phase,
                                    sc::INVALID_FIELD,
                                    0,
                                ));
                            }
                            let rev_bytes: [u8; 8] = self.fw_download_buf[..8].try_into().unwrap();
                            self.fw_slot_revisions[fs as usize] =
                                String::from_utf8_lossy(&rev_bytes).to_string();
                        }
                        self.fw_active_slot = fs;
                        tracing::info!(fs, "FW Commit: activated immediately");
                    }
                    _ => return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0)),
                }
                // 清 download buffer（spec § 5.16：commit consumes download)
                self.fw_download_buf.clear();
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::FW_IMAGE_DOWNLOAD => {
                // **Phase H5** — NVMe spec § 5.17 Firmware Image Download。
                // CDW10 = NUMD (dwords - 1)；CDW11 = OFFSET (dwords)。Data
                // 通过 PRP1 提供。我们 DMA-read PRP1 到 fw_download_buf
                // 对应 offset，完成后构造 success CQE。
                //
                // **reviewer H5 修复**：用 checked_add 避 u32 wrap：cdw11
                // = 0xffff_ffff 时 +1 wrap 到 0；offset+bytes 累加可能 wrap。
                // 全部用 u64 计算 + checked 边界，超 FW_MAX (8 MiB) → reject。
                let numd = match (sqe.cdw10 as u64).checked_add(1) {
                    Some(n) => n, // dwords (4 byte units)
                    None => {
                        return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                    }
                };
                let offset_dwords = sqe.cdw11 as u64;
                let bytes_count_u64 = numd.saturating_mul(4);
                let offset_bytes_u64 = offset_dwords.saturating_mul(4);
                let prp1 = sqe.prp1;
                tracing::info!(
                    bytes = bytes_count_u64,
                    offset = offset_bytes_u64,
                    "FW Image Download chunk"
                );
                const FW_MAX: u64 = 8 * 1024 * 1024;
                let need_total = match offset_bytes_u64.checked_add(bytes_count_u64) {
                    Some(t) => t,
                    None => {
                        tracing::warn!("FW Download offset+bytes overflow");
                        return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                    }
                };
                if need_total > FW_MAX || bytes_count_u64 == 0 || bytes_count_u64 > u32::MAX as u64
                {
                    tracing::warn!(need_total, "FW Download invalid size");
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                if self.fw_download_buf.len() < need_total as usize {
                    self.fw_download_buf.resize(need_total as usize, 0);
                }
                // DMA-read PRP1 → 完成回调 AdminFwDownloadChunk
                let tok = ctx.dma_read(prp1, bytes_count_u64 as u32);
                self.pending_ios.insert(
                    tok,
                    crate::controller::PendingIo {
                        sq_id: 0,
                        cid,
                        sq_head,
                        cq_id,
                        nsid: 0,
                        op: crate::controller::PendingOp::AdminFwDownloadChunk {
                            offset_bytes: offset_bytes_u64 as u32,
                        },
                    },
                );
                None
            }
            admin_opc::DEVICE_SELF_TEST => {
                // **Phase G (rev. CRITICAL fix)** — NVMe spec § 5.11
                // Device Self-test。CDW10 bits 3:0 = STC：
                //   0x0 = abort current self-test
                //   0x1 = short self-test
                //   0x2 = extended self-test
                //   0xf = vendor specific
                // 教学：把时间常数压成秒级（5 s / 20 s），让 tick 推进
                // percent_complete + Get Log Page 0x06 反映；完成时一次性
                // (transition 边沿) fire AEN Notice 通知 driver。
                let stc = (sqe.cdw10 & 0xf) as u8;
                match stc {
                    0x0 => {
                        // Abort：清掉 in_progress 并把结果 0x09=aborted 记
                        // 进 self_test_last（spec § 5.16.1.6 Self-Test
                        // Result Codes）。take() 保证 tick 不会再当作
                        // in-progress 推进。
                        if let Some(in_prog) = self.self_test_in_progress.take() {
                            let poh = self.power_on_instant.elapsed().as_secs() / 3600;
                            self.self_test_last = Some(crate::controller::SelfTestCompleted {
                                stc: in_prog.stc,
                                result: 0x09,
                                completed_at_poh: poh,
                            });
                            tracing::info!(stc = in_prog.stc, "Self-Test aborted by host");
                        }
                        Some(Cqe::success(cid, 0, sq_head, phase))
                    }
                    0x1 | 0x2 => {
                        if self.self_test_in_progress.is_some() {
                            // Spec：已在进行 → 0x1d Self-Test In Progress
                            tracing::warn!(stc, "Self-Test rejected: already in progress");
                            return Some(Cqe::error(cid, 0, sq_head, phase, 0x1d, 0));
                        }
                        let total = if stc == 0x1 { 5 } else { 20 };
                        self.self_test_in_progress = Some(crate::controller::SelfTestInProgress {
                            started_at: std::time::Instant::now(),
                            stc,
                            total_seconds: total,
                            percent_complete: 0,
                        });
                        tracing::info!(stc, total, "Self-Test started");
                        Some(Cqe::success(cid, 0, sq_head, phase))
                    }
                    _ => {
                        tracing::warn!(stc, "Self-Test: unsupported STC");
                        Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0))
                    }
                }
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
                tracing::warn!(
                    opc,
                    "unsupported admin opcode; returning success to keep driver alive"
                );
                // 返 success 而非 INVALID_OPCODE：很多 driver 在
                // boot 期会探测可选 opcode，遇 INVALID_OPCODE 会进入 fallback
                // 路径或直接 fail device。返 success（CQE cdw0=0）通常更安全。
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
        }
    }
}
