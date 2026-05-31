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
use crate::controller::ZnsState;
use crate::regs::*;
use pcie_remote_userspace_sdk::*;
use zerocopy::IntoBytes;

/// **Phase L1c** — 构造 4 KiB ZNS NS Identify (spec ZNS § 3.1.6 / Figure
/// "I/O Command Set Specific Identify Namespace Data Structure")。
///
/// 教学填最关键字段让 Linux/Windows 驱动能正确驱动 ZNS NS：
///
/// | offset | name | val |
/// |--------|------|-----|
/// | 0..2   | ZOC (Zone Operation Characteristics) | 0 = 无特殊 |
/// | 2..4   | OZCS (Optional Zoned CS support) | 0 |
/// | 4..8   | MAR (Max Active Resources) | spec: 0xFFFFFFFF=unlimited，0=1 zone |
/// | 8..12  | MOR (Max Open Resources)   | spec: 0xFFFFFFFF=unlimited，0=1 zone |
/// | 12..16 | RRL (Reset Recommended Limit) | 0 = 无 |
/// | 16..20 | FRL (Finish Recommended Limit) | 0 |
/// | 20..2816 | reserved + ZRWA 字段 | 0 |
/// | 2816..2824 | LBAFE[0].ZSZE | zone_size |
/// | 2824   | LBAFE[0].ZDES | 0 |
///
/// **Reviewer H-A 修复**：MAR/MOR 是 0's-based + 0xFFFFFFFF=unlimited，所以
/// 我们的内部 `max_open=0`=unlimited 必须翻成 0xFFFFFFFF 给 driver；非零
/// `max_open=N` 翻成 `N-1`。Linux nvme-cli `zns id-ns` 会读这些值。
fn build_zns_ns_identify(zns: &ZnsState) -> Vec<u8> {
    let mut buf = vec![0u8; 4096];
    // ZOC = 0
    // OZCS = 0
    let mar_wire = if zns.max_active == 0 {
        0xFFFF_FFFFu32
    } else {
        zns.max_active.saturating_sub(1)
    };
    let mor_wire = if zns.max_open == 0 {
        0xFFFF_FFFFu32
    } else {
        zns.max_open.saturating_sub(1)
    };
    buf[4..8].copy_from_slice(&mar_wire.to_le_bytes());
    buf[8..12].copy_from_slice(&mor_wire.to_le_bytes());
    // RRL/FRL = 0
    // LBAFE[0] @ offset 2816 (spec § 3.1.6 Figure)：
    //   bytes 0..8  ZSZE (Zone Size in LBA)
    //   byte  8     ZDES (Zone Descriptor Extension Size in 64-byte units) = 0
    //   bytes 9..16 reserved
    let zsze_off = 2816;
    buf[zsze_off..zsze_off + 8].copy_from_slice(&zns.zone_size.to_le_bytes());
    // ZDES = 0 已是默认零
    buf
}

/// Test-only wrapper for `build_zns_ns_identify`。
#[cfg(test)]
pub(crate) fn __test_build_zns_ns_identify(zns: &ZnsState) -> Vec<u8> {
    build_zns_ns_identify(zns)
}

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
                        /* Phase A+K1: spec-correct 200+ fields via nvme_spec
                         * with per-NS LBAF/PI parameters */
                        IdentifyNamespace::build_v2_bytes(
                            ns.total_lba,
                            ns.lbads,
                            ns.meta_size,
                            ns.pi_type,
                            ns.pi_first,
                        )
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
                    0x05 => {
                        // **Phase L1c** — Identify Namespace (I/O Command Set
                        // specific)。CDW11 bits 7:0 = CSI；CSI=0x02 (ZNS) 时
                        // 返 4 KiB ZNS NS Identify (spec ZNS § 3.1.6)。其他
                        // CSI → zeros 让 driver 走 fallback。
                        let csi = (sqe.cdw11 & 0xff) as u8;
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
                        if csi == 0x02 {
                            if let Some(zns) = ns.zns.as_ref() {
                                build_zns_ns_identify(zns)
                            } else {
                                // 不是 ZNS NS → spec 说 return zeros
                                vec![0u8; 4096]
                            }
                        } else {
                            vec![0u8; 4096]
                        }
                    }
                    0x06 => {
                        // **Phase L1d** — Identify Namespace I/O Command Set
                        // Independent Identify (spec NVMe 2.0 § 5.17.2.6)。
                        // 4 KiB；驱动用它感知 NS 的 NSFEAT/NMIC/RESCAP 等
                        // command-set-agnostic 属性。最关键的是字节 9 = NSTAT
                        // 中 bit 0 = NRDY (NS Ready)；我们的 NS 总是 Ready。
                        let Some(ns) = self.ns(nsid) else {
                            // 0xFFFFFFFF broadcast / 未知 NSID → 零 buffer
                            return Some(Cqe::error(
                                cid,
                                0,
                                sq_head,
                                phase,
                                sc::INVALID_NAMESPACE,
                                0,
                            ));
                        };
                        let mut buf = vec![0u8; 4096];
                        // NSFEAT @ off 0 — bit 0 = THINP (thin provisioning)，
                        // bit 1 = NSABP (deallocate after format) — 都 0
                        buf[0] = 0x00;
                        // NMIC @ off 1 — bit 0 = shared NS（不 share）
                        buf[1] = 0x00;
                        // RESCAP @ off 2 — Reservation Capabilities；
                        // 与 Identify NS (CNS 0x00) RESCAP 字段保持一致。
                        // 我们支持 Write/Exclusive Access 等 → bit 0..6 = 1
                        // (spec § 5.17.2.1 RESCAP layout)
                        buf[2] = 0x7F;
                        // FPI @ off 3 — Format Progress Indicator；0 = 完成
                        buf[3] = 0x00;
                        // ANAGRPID @ off 4..8 — Asymmetric NS Access GID = 0
                        // NSATTR @ off 9 — bit 0 = WP（write protect）；0
                        // NVMSETID @ off 10..12 = 0
                        // ENDGID @ off 12..14 = 0
                        // NSTAT @ off 14 — bit 0 NRDY (NS Ready) = 1
                        buf[14] = 0x01;
                        // 其余 4082 bytes 留 0（教学；real device 还有 KPIOS
                        // / MAXKT 等 KV-CS 字段）
                        let _ = ns;
                        buf
                    }
                    0x1c => {
                        // **Phase L1d** — CNS 0x1c = I/O Command Set data
                        // structure (spec NVMe 2.0 § 5.17.2.21)。4 KiB，每
                        // 8 byte 一个 entry，512 个 'I/O Command Set Combination'
                        // descriptor (uint64 bitmap)。Entry 0 必须支持，
                        // controller Identify CC.CSS 与之联动。
                        //
                        // 我们：entry 0 = bit0 (NVM)|bit1 (KV=0)|bit2 (ZNS) 启用
                        let mut buf = vec![0u8; 4096];
                        // bit 0 = NVM Command Set，bit 2 = Zoned Namespace CS
                        let combo0: u64 = (1 << 0) | (1 << 2);
                        buf[0..8].copy_from_slice(&combo0.to_le_bytes());
                        buf
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
                        pending_completions: 0,
                        last_fire: None,
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
                    cmd::fid::POWER_MANAGEMENT => {
                        // **Phase K8** — cdw11 bits 4:0 = Power State，
                        // bits 7:5 = Workload Hint。spec § 5.21.1.2。
                        let ps = (cdw11 & 0x1F) as u8;
                        if ps >= 8 {
                            // Identify Controller .npss = 7 (8 states)
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                        }
                        self.current_ps = ps;
                        self.features.insert(fid, cdw11);
                        tracing::info!(ps, "Set Features Power Management");
                    }
                    cmd::fid::INTERRUPT_COALESCING => {
                        // **Phase M1 + reviewer H6/H-4** — spec § 5.21.1.8。
                        // cdw11 bits 7:0 = AGGR_THR (0-based)，
                        // bits 15:8 = AGGR_TIME (100 us 单位)。
                        //
                        // 教学 host 主循环 tick 周期 100 ms = 1000×100 us，
                        // 所以 AGGR_TIME 的实际时间下限是 100 ms 而非 spec
                        // 的 100 us。我们 **接受** driver 的设置 + 发 warn 让
                        // driver 通过日志感知粒度损失；不返 INVALID_FIELD
                        // 是因为 spec 不允许 controller 拒绝合法的 cdw11，
                        // 拒绝会让 driver 死循环（Linux nvme_set_features
                        // 会 panic）。教学诚实 = 接受 + 显式 warn。
                        let aggr_thr = (cdw11 & 0xff) as u8;
                        let aggr_time = ((cdw11 >> 8) & 0xff) as u8;
                        if aggr_time != 0 {
                            tracing::warn!(
                                aggr_time_100us = aggr_time,
                                tick_ms = 100,
                                "INTERRUPT_COALESCING AGGR_TIME finer than tick; \
                                 actual time flush bounded by tick (~100 ms)"
                            );
                        }
                        self.irq_aggr_threshold = aggr_thr;
                        self.irq_aggr_time = aggr_time;
                        self.features.insert(fid, cdw11);
                        tracing::info!(
                            thr = self.irq_aggr_threshold,
                            time_100us = self.irq_aggr_time,
                            "Set Features Interrupt Coalescing"
                        );
                    }
                    cmd::fid::INTERRUPT_VECTOR_CONFIG => {
                        // spec § 5.21.1.9。cdw11 bits 15:0 = IV, bit 16 = CD
                        // (Coalescing Disable for this vector)。存进 features
                        // map，driver 真要 per-vector control 时再实现。
                        self.features.insert(fid, cdw11);
                        tracing::debug!(cdw11, "Set Features Interrupt Vector Config");
                    }
                    cmd::fid::HOST_IDENTIFIER => {
                        // **Phase K9** — spec § 5.21.1.27 Host Identifier。
                        // cdw11 bit 0 = EXHID（0=8byte, 1=16byte）；PRP1
                        // 指向 buffer。DMA-read 完成后存到 host_id_lo/hi。
                        let exhid = cdw11 & 0x1 != 0;
                        let bytes = if exhid { 16 } else { 8 };
                        let tok = ctx.dma_read(sqe.prp1, bytes);
                        self.pending_ios.insert(
                            tok,
                            crate::controller::PendingIo {
                                sq_id: 0,
                                cid,
                                sq_head,
                                cq_id,
                                nsid: 0,
                                op: crate::controller::PendingOp::AdminSetHostIdentifier { exhid },
                            },
                        );
                        return None;
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
                    cmd::fid::POWER_MANAGEMENT => {
                        // Phase K8：返实时 current_ps（不从 features map）
                        self.current_ps as u32
                    }
                    cmd::fid::HOST_IDENTIFIER => {
                        // **K9** — Get Features 0x81 返 HOSTID 通过 PRP1，
                        // CDW0 仅设 EXHID bit。我们 DMA-write 8 或 16 byte。
                        let exhid = sqe.cdw11 & 0x1 != 0;
                        let mut buf = vec![0u8; if exhid { 16 } else { 8 }];
                        buf[0..8].copy_from_slice(&self.host_id_lo.to_le_bytes());
                        if exhid {
                            buf[8..16].copy_from_slice(&self.host_id_hi.to_le_bytes());
                        }
                        self.dma_write_then_complete(ctx, sqe.prp1, buf, cid, 0, sq_head, cq_id);
                        return None;
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
                    0x05 => super::logs::build_cmds_supported_effects(self, bytes),
                    0x06 => super::logs::build_self_test(self, bytes),
                    0x07 => super::logs::build_telemetry_host(self, bytes),
                    0x08 => super::logs::build_telemetry_ctrl(self, bytes),
                    0x09 => super::logs::build_endurance_group(self, bytes),
                    0x0a => super::logs::build_predictable_latency_nvmset(self, bytes),
                    0x0b => super::logs::build_predictable_latency_event(self, bytes),
                    0x0c => super::logs::build_ana_log(self, bytes),
                    0x0d => super::logs::build_persistent_event(self, bytes),
                    0x0e => super::logs::build_lba_status_info(self, bytes),
                    0x0f => super::logs::build_endurance_group_event(self, bytes),
                    0x80 => super::logs::build_reservation_notification(self, bytes),
                    0x81 => super::logs::build_sanitize_status(self, bytes),
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
                if lbafl > 1 || pi > 1 || mset != 0 {
                    // **Phase K1** — 真支持 LBAF[0] (512B no-meta) +
                    // LBAF[1] (4096B+8B meta) + PI Type 0/1。mset=1
                    // (separate metadata buffer) 需 MPTR 二级 DMA，未实现；
                    // driver 用 mset=0 把 meta 与 data inline 存。其余拒绝。
                    tracing::warn!(
                        lbafl,
                        pi,
                        mset,
                        "Format rejected: only LBAF[0/1] + PI Type 0/1 supported"
                    );
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                // **Phase K1** 计算新 LBAF + PI 配置
                let new_lbads: u8 = if lbafl == 0 { 9 } else { 12 };
                let new_meta_size: u8 = if lbafl == 0 { 0 } else { 8 };
                if pi != 0 && new_meta_size == 0 {
                    // PI 需要 metadata 空间承载 8-byte tuple
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                let new_pi_type = pi;
                let new_pi_first = pil == 0; // PIL=0 → first 8, PIL=1 → last 8
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
                    return Some(Cqe::error(
                        cid,
                        0,
                        sq_head,
                        phase,
                        sc::FORMAT_IN_PROGRESS,
                        0,
                    ));
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
                        // **Phase K1** — 应用新 LBAF + PI 配置
                        ns.lbads = new_lbads;
                        ns.meta_size = new_meta_size;
                        ns.pi_type = new_pi_type;
                        ns.pi_first = new_pi_first;
                        // total_lba 按新 block_bytes 重算
                        ns.total_lba = size / ns.block_bytes();
                        tracing::info!(
                            nsid = target_nsid,
                            size,
                            ses,
                            lbads = new_lbads,
                            meta_size = new_meta_size,
                            pi_type = new_pi_type,
                            total_lba = ns.total_lba,
                            "Format NVM: reconfigured"
                        );
                    }
                } else {
                    // SES=0 — 只切换 LBAF/PI 而不擦盘（spec § 5.14 允许）
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
                        let size = ns.file.metadata().map(|m| m.len()).unwrap_or(0);
                        ns.lbads = new_lbads;
                        ns.meta_size = new_meta_size;
                        ns.pi_type = new_pi_type;
                        ns.pi_first = new_pi_first;
                        ns.total_lba = size / ns.block_bytes();
                        tracing::info!(
                            nsid = target_nsid,
                            lbads = new_lbads,
                            meta_size = new_meta_size,
                            pi_type = new_pi_type,
                            total_lba = ns.total_lba,
                            "Format NVM: LBAF/PI switched (SES=0)"
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
                            return Some(Cqe::error(
                                cid,
                                0,
                                sq_head,
                                phase,
                                sc::SELF_TEST_IN_PROGRESS,
                                0,
                            ));
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
                // **Phase K3** — NVMe spec § 5.22 Namespace Management。
                // CDW10 bits 3:0 = SEL：
                //   0 = Create — NSID 必须 = 0xFFFF_FFFF；CQE.cdw0 返新 NSID
                //   1 = Delete — NSID = 要删的 NS（不可 0/0xFFFFFFFF）
                // Create 需 DMA-read PRP1 4 KiB Identify NS 结构：
                //   - NSZE @ 0..8 = size in LBA
                //   - NCAP @ 8..16 = capacity
                //   - FLBAS @ 26 = LBAF index
                //   - DPS @ 29 = PI type
                let sel = (sqe.cdw10 & 0xf) as u8;
                match sel {
                    0 => {
                        // Create — DMA-read 4 KiB Identify NS 结构
                        if sqe.nsid != 0xFFFF_FFFF {
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                        }
                        let tok = ctx.dma_read(sqe.prp1, 4096);
                        self.pending_ios.insert(
                            tok,
                            crate::controller::PendingIo {
                                sq_id: 0,
                                cid,
                                sq_head,
                                cq_id,
                                nsid: 0,
                                op: crate::controller::PendingOp::AdminNsCreate,
                            },
                        );
                        None
                    }
                    1 => {
                        // Delete — NSID 立即删
                        let nsid = sqe.nsid;
                        if nsid == 0 || nsid == 0xFFFF_FFFF {
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                        }
                        // **reviewer H1 修复** — Delete 时若有 in-flight IO
                        // 关联该 NSID，必须拒绝。否则完成回调找不到 NS 误
                        // post DATA_TRANSFER_ERROR 而非 INVALID_NAMESPACE，
                        // NvmReadDmaWrite 路径根本不查 NSID → host 已传
                        // stale data 给 driver。
                        let busy = self.pending_ios.values().any(|p| p.nsid == nsid)
                            || self.dual_prp_writes.values().any(|w| w.nsid == nsid)
                            || self.prp_list_ops.values().any(|o| o.nsid == nsid)
                            || self.compare_ops.values().any(|o| o.nsid == nsid);
                        if busy {
                            tracing::warn!(nsid, "NS Mgmt Delete rejected: IO in flight");
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                        }
                        // **reviewer M2** — 删 backing temp file 不泄漏
                        if let Some(ns) = self.namespaces.remove(&nsid) {
                            let _ = std::fs::remove_file(&ns.path);
                            tracing::info!(
                                nsid,
                                path = %ns.path,
                                "NS Mgmt Delete OK + temp file unlink"
                            );
                        } else {
                            return Some(Cqe::error(
                                cid,
                                0,
                                sq_head,
                                phase,
                                sc::INVALID_NAMESPACE,
                                0,
                            ));
                        }
                        Some(Cqe::success(cid, 0, sq_head, phase))
                    }
                    _ => Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0)),
                }
            }
            admin_opc::NS_ATTACHMENT => {
                // NVMe spec § 5.20 Namespace Attachment。单 controller 单
                // namespace 永远 attached；返 success。
                tracing::debug!(cid, "Namespace Attachment (no-op success)");
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::SECURITY_SEND => {
                // **Phase L5 + O reviewer M5 修复** — spec § 5.27 Security Send。
                // SECP=0 (Info) 在 Spec 中无 Send operation 定义（spec 表
                // 5-19 Info protocol 仅供 Receive 使用），改返 INVALID_FIELD。
                // 其它 SECP 我们都没真实现 → 一律 INVALID_FIELD。
                let secp = ((sqe.cdw10 >> 16) & 0xff) as u8;
                tracing::debug!(secp, "Security Send (INVALID_FIELD; no SP impl)");
                Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0))
            }
            admin_opc::SECURITY_RECEIVE => {
                // **Phase L5 + O reviewer M4 修复** — spec § 5.28 Security
                // Receive。SECP=0 (Info) 返 Security Protocol List。
                // 之前声明 TCG OPAL (0xEF) 但实现没有任何 0xEF Send 路径
                // → 改只声明 SECP=0 Info（list 长度 = 1）。
                let secp = ((sqe.cdw10 >> 16) & 0xff) as u8;
                let alloc = (sqe.cdw11 & 0xffff) as usize;
                let bytes = alloc.max(16);
                if secp == 0 {
                    let mut buf = vec![0u8; bytes];
                    // bytes 0..6 reserved
                    // bytes 6..8 = LIST LENGTH (big endian) = 1 protocol
                    buf[6] = 0;
                    buf[7] = 1;
                    // bytes 8..N = supported protocol IDs (just 0x00 Info)
                    if bytes > 8 {
                        buf[8] = 0x00;
                    }
                    self.dma_write_then_complete(ctx, sqe.prp1, buf, cid, 0, sq_head, cq_id);
                    None
                } else {
                    Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0))
                }
            }
            admin_opc::DIRECTIVE_SEND => {
                // **Phase L4** — Directive Send (spec § 5.10)。CDW10 = numd-1,
                // CDW11 = doper (Directive Operation) + dtype, CDW12 = dspec。
                // Stream Identifier directive (dtype=1) doper:
                //   1 = Enable Directive
                //   2 = Release Identifier
                //   3 = Release Resources
                // 教学：返 success 接受 driver 配置；我们不真分流 Stream。
                let doper = (sqe.cdw11 & 0xff) as u8;
                let dtype = ((sqe.cdw11 >> 8) & 0xff) as u8;
                tracing::debug!(doper, dtype, "Directive Send (no-op success)");
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::DIRECTIVE_RECEIVE => {
                // **Phase L4** — Directive Receive (spec § 5.9)。返 PRP1 4 KiB
                // 全 0 = "no directives currently enabled"。
                let buf = vec![0u8; 4096];
                self.dma_write_then_complete(ctx, sqe.prp1, buf, cid, 0, sq_head, cq_id);
                None
            }
            admin_opc::VIRTUALIZATION_MGMT => {
                // **Phase L5** — Virtualization Management (spec § 5.24)。
                // 我们不实现 SR-IOV / VF resource alloc；返 INVALID_FIELD
                // 让 driver fallback。
                tracing::debug!(cid, "Virtualization Mgmt (INVALID_FIELD)");
                Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0))
            }
            admin_opc::GET_LBA_STATUS => {
                // **Phase L5** — Get LBA Status (spec § 5.15)。CDW10/11 = SLBA,
                // CDW12 bits 31:16 = NDR (max ranges)。返 4 KiB 'no error
                // LBAs' (NLSD=0)。
                let buf = vec![0u8; 4096];
                self.dma_write_then_complete(ctx, sqe.prp1, buf, cid, 0, sq_head, cq_id);
                None
            }
            admin_opc::SANITIZE => {
                // **Phase K5** — NVMe spec § 5.26 Sanitize。CDW10 字段：
                //   bits 2:0   SANACT — 1=Exit Failure / 2=Block Erase /
                //                       3=Overwrite / 4=Crypto Erase
                //   bit 3      AUSE   — Allow Unrestricted Sanitize Exit
                //   bits 7:4   OWPASS — Overwrite Pass Count
                //   bit 8      OIPBP  — Overwrite Invert Pattern Between Passes
                //   bit 9      NDAS   — No Deallocate After Sanitize
                let sanact = (sqe.cdw10 & 0x7) as u8;
                if sanact == 0 || sanact > 4 {
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                if sanact == 1 {
                    // Exit Failure：清失败状态
                    if self.sanitize_last_status == 3 {
                        self.sanitize_last_status = 0;
                    }
                    return Some(Cqe::success(cid, 0, sq_head, phase));
                }
                // 已在进行 → spec § 5.26 'Sanitize In Progress' (SC 0x12)
                if self.sanitize.is_some() {
                    return Some(Cqe::error(
                        cid,
                        0,
                        sq_head,
                        phase,
                        sc::SANITIZE_IN_PROGRESS,
                        0,
                    ));
                }
                self.sanitize = Some(crate::controller::SanitizeState {
                    started_at: std::time::Instant::now(),
                    sanact,
                    total_seconds: 3, // 教学短时长
                    percent_complete: 0,
                });
                self.sanitize_last_status = 2; // in-progress
                tracing::info!(sanact, "Sanitize started");
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::DOORBELL_BUFFER_CONFIG => {
                // **Phase K6** — NVMe spec § 5.7。PRP1 = shadow doorbell GPA，
                // PRP2 = event idx GPA。我们存下来但不真做 polling（vsock
                // 模型 MMIO 已 OK）；driver 信任 controller 偶尔会 poll。
                let prp1 = sqe.prp1;
                let prp2 = sqe.prp2;
                self.doorbell_shadow_gpa = prp1;
                self.doorbell_event_idx_gpa = prp2;
                tracing::info!(
                    shadow = format_args!("{:#x}", prp1),
                    event_idx = format_args!("{:#x}", prp2),
                    "Doorbell Buffer Config (stored, not actively polled)"
                );
                Some(Cqe::success(cid, 0, sq_head, phase))
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
