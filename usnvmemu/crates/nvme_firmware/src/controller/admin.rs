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
use pcie_device_core::*;
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

/// **Phase L1d (修正后)** — 构造 CNS 0x08 (I/O Command Set Independent
/// Identify Namespace) 4 KiB 数据。**之前 commit 误用 CNS 0x06 (Reviewer
/// C-1 修正)** — CNS 0x06 实际是 "specific Controller for I/O CS"，与 NS
/// 无关；NS-level command-set-agnostic descriptor 在 NVMe 2.0 spec 是
/// CNS 0x08。
///
/// 抽成纯函数便于 unit test 字段 offset 和 RESCAP 与 CNS 0x00 的 spec
/// 一致性约束。spec 见 NVMe 2.0 § 5.17.2 (Identify Namespace - CSI
/// Independent)。
pub(crate) fn build_cs_indep_ns_identify() -> Vec<u8> {
    let mut buf = vec![0u8; 4096];
    buf[0] = 0x00; // NSFEAT
    buf[1] = 0x00; // NMIC
    buf[2] = 0x1E; // RESCAP **必须 == CNS 0x00 RESCAP** (spec § 5.17.2.6)
    buf[3] = 0x00; // FPI = 0 (format complete)
    // ANAGRPID @ 4..8 = 0
    // NSATTR @ 8 = 0
    // NVMSETID @ 9..11 = 0
    // ENDGID @ 11..13 = 0
    buf[13] = 0x00; // NSTAT bit 0 NRDY = 0 (ready)
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
        let opc = sqe.opcode();
        // **Phase Q7** — Lockdown 在 dispatch 前优先校验。LOCKDOWN 本身
        // 永远不被 lock（否则 driver 无法 unlock 任何 opcode）。
        if opc != admin_opc::LOCKDOWN && self.locked_admin_opcodes.contains(&opc) {
            tracing::warn!(
                opc = format_args!("{:#x}", opc),
                "admin opcode locked by Lockdown → COMMAND_PROHIBITED_BY_LOCKDOWN"
            );
            return Some(Cqe::error(
                cid,
                0,
                sq_head,
                phase,
                sc::COMMAND_PROHIBITED_BY_LOCKDOWN,
            ));
        }
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
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_NAMESPACE));
                        };
                        /* Phase A+K1: spec-correct 200+ fields via nvme_spec
                         * with per-NS LBAF/PI parameters */
                        IdentifyNamespace::build_v2_bytes(
                            ns.total_lba,
                            ns.lbads,
                            ns.meta_size,
                            ns.pi_type,
                            ns.pi_first,
                            ns.meta_inline,
                        )
                    }
                    0x01 => {
                        // Identify Controller
                        /* Phase A: spec-correct 200+ fields via nvme_spec */
                        // **V8a (review H-1 cleanup)** — 用 builder pattern
                        // 替代 V7c-fix 的 post-hoc byte 111 patch。Discovery
                        // mode 直接构造 CNTRLTYPE=0x02 + NN=0；spec § 5.1.4 +
                        // 5.17.2.1 Figure 312 byte 111。
                        let discovery = self.nvme_is_discovery_mode();
                        let cntrltype = if discovery { 0x02 } else { 0x01 };
                        let nn = if discovery {
                            0
                        } else {
                            self.namespaces.len() as u32
                        };
                        // **2026-06-09** — MAXCMD 跟随队列深度：= MQES+1（entry
                        // 数），clamp 到 u16 上限，避免 host 把深度 clamp 到旧硬编 64。
                        let mqes_plus_1 = ((self.cap & 0xffff) + 1).min(u16::MAX as u64) as u16;
                        // **CMB-P5** — SGLS SAOS（bit20，CMB-relative SGL 支持）条件
                        // advertise：CMB **一经 advertise**（`enable_cmb*` 调过，`self.cmb`
                        // Some）即置——不门控于 CMSE。理由：driver 先读 Identify 看 SAOS +
                        // CMBLOC/CMBSZ，**之后**才编程 CMBMSC.CMSE；若 SAOS 门控 CMSE 则
                        // driver 在置 CMSE 前永远看不到 SAOS（鸡生蛋）。与 `is_cmb_bar`
                        // "BAR 暴露不要求 CMSE" 同纪律。CMB off（`self.cmb` None）→ 不 advertise。
                        let cmb_offset_sgl = self.cmb.is_some();
                        IdentifyController::build_v2_bytes_with_cmb(
                            self.vid,
                            self.ssvid,
                            nn,
                            cntrltype,
                            mqes_plus_1,
                            cmb_offset_sgl,
                        )
                    }
                    0x02 => {
                        // Active NSID list — 列所有已注册 NSID（spec § 5.15.1）。
                        // Phase H4：动态枚举 self.namespaces；按 NSID 升序。
                        let mut buf = vec![0u8; 4096];
                        let mut nsids: Vec<u32> = self.namespaces.keys().collect();
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
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_NAMESPACE));
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
                        // **Reviewer C-1 修正** — CNS 0x06 是 "Identify
                        // Controller for the specific I/O Command Set"
                        // (spec NVMe 2.0 § 5.17.2.7 + in-tree nvme_spec::Cns
                        // SPECIFIC_CONTROLLER_IO_COMMAND_SET = 0x6)，**不是**
                        // NS Identify。CDW11 bits 31:24 = CSI。
                        //
                        // 我们的 ZNS controller 与 NVM controller 行为一样
                        // （同一 SQ/CQ stack），CSI-specific controller 字段
                        // (ZASL 等) 留 0 让 driver 走默认 MDTS。
                        // 非 ZNS CSI 也返零 buffer。
                        vec![0u8; 4096]
                    }
                    0x04 => {
                        // **Phase P1** — CNS 0x04 = NVM Set List (spec NVMe 2.0
                        // § 5.17.2.5)。4 KiB；header byte 0 = num NVM Sets，
                        // 之后是 128-byte 描述符 array。教学 controller 不分
                        // NVM Set（所有 NS 在默认 Set 0），返 num=1 + 一个
                        // default-set descriptor。
                        let mut buf = vec![0u8; 4096];
                        buf[0] = 1; // 1 NVM Set
                        // Descriptor @ off 4 (header 0..4)：
                        //   bytes 0..2 NVM Set ID = 1
                        //   bytes 2..4 ENDGID = 1（属于 Endurance Group 1）
                        //   bytes 4..8 reserved
                        //   bytes 8..16 Random 4 KiB Read Typical (ns)
                        //   bytes 16..32 Optimal Write Size (LBA) 等
                        let desc_off = 4;
                        buf[desc_off..desc_off + 2].copy_from_slice(&1u16.to_le_bytes());
                        buf[desc_off + 2..desc_off + 4].copy_from_slice(&1u16.to_le_bytes());
                        buf
                    }
                    0x08 => {
                        // **Phase L1d 修正后** — CNS 0x08 = I/O Command Set
                        // Independent Identify Namespace (spec NVMe 2.0
                        // § 5.17.2.8)。这才是 NS-level command-set-agnostic
                        // descriptor，含 NSFEAT/NMIC/RESCAP/FPI/NSTAT。
                        // （之前 commit 误用 CNS 0x06。）
                        let Some(ns) = self.ns(nsid) else {
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_NAMESPACE));
                        };
                        // **NS-not-ready spec-completeness** — NSTAT.NRDY（byte 13 bit 0）
                        // 跟随 NS 真状态：not_ready=true → NRDY=1（与 io.rs IO 门返
                        // NAMESPACE_NOT_READY 一致，广告⟺门不矛盾）。builder 默认 NRDY=0
                        // （ready），此处按 live 状态覆盖。
                        let not_ready = ns.not_ready;
                        let mut buf = build_cs_indep_ns_identify();
                        buf[13] = not_ready as u8;
                        buf
                    }
                    0x19 => {
                        // **Phase P1** — CNS 0x19 = Endurance Group List (spec
                        // § 5.17.2.20)。4 KiB；byte 0 = num groups + 后续
                        // 描述符。教学：1 个 group covering 所有 NS。
                        let mut buf = vec![0u8; 4096];
                        buf[0] = 1;
                        let desc_off = 4;
                        buf[desc_off..desc_off + 2].copy_from_slice(&1u16.to_le_bytes());
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
                    0x12 => {
                        // **Phase S5** — CNS 0x12 = Controller List attached
                        // to the NSID (spec § 5.17.2.13)。CDW10 bits 31:16 =
                        // start CNTLID (返 ≥ 该值的 cntlid 列表，递增)。
                        // 返 4 KiB buffer：bytes 0..2 = NumIDs (LE u16)，
                        //                  bytes 2.. = u16[] cntlids 升序。
                        // 教学：单 controller cntlid=1；若 NSID 不存在 →
                        // INVALID_NAMESPACE；若 NS detached → NumIDs=0。
                        // **review M3** — NSID=0 / broadcast 0xFFFF_FFFF 在
                        // CNS 0x12 不合法 (spec 表 273) → INVALID_FIELD。
                        if nsid == 0 || nsid == 0xFFFF_FFFF {
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                        }
                        let start_cntlid = ((sqe.cdw10 >> 16) & 0xffff) as u16;
                        let Some(ns) = self.namespaces.get(&nsid) else {
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_NAMESPACE));
                        };
                        let mut buf = vec![0u8; 4096];
                        if ns.attached && start_cntlid <= 1 {
                            buf[0..2].copy_from_slice(&1u16.to_le_bytes());
                            buf[2..4].copy_from_slice(&1u16.to_le_bytes());
                        }
                        buf
                    }
                    0x13 => {
                        // **Phase S5** — CNS 0x13 = Controller List of all
                        // controllers in the NVM subsystem (spec § 5.17.2.14)。
                        // CDW10 bits 31:16 = start CNTLID。返同 0x12 格式。
                        // 教学：subsystem 仅 1 controller (cntlid=1)。
                        let start_cntlid = ((sqe.cdw10 >> 16) & 0xffff) as u16;
                        let mut buf = vec![0u8; 4096];
                        if start_cntlid <= 1 {
                            buf[0..2].copy_from_slice(&1u16.to_le_bytes());
                            buf[2..4].copy_from_slice(&1u16.to_le_bytes());
                        }
                        buf
                    }
                    _ => {
                        tracing::warn!(cns, "Identify: unsupported CNS, returning zeros");
                        // 比 INVALID_FIELD 友好：返 4 KiB 零让 driver 继续。
                        vec![0u8; 4096]
                    }
                };
                // DMA write to PRP1
                self.dma_write_then_complete(ctx, sqe.prp1, sqe.prp2, buf, cid, 0, sq_head, cq_id);
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
                // **2026-06-09** — qid 范围校验：admin(0) 不可重建；> IO_QUEUE_SLOT_CAPACITY
                // 超授予上限。DenseMap 越界 insert 静默丢弃，必须在此显式拒，否则会
                // 对未创建的队列误返 success。
                //
                // 注：此 qid-范围错误按 spec 语义更贴 INVALID_QUEUE_IDENTIFIER，但
                // controller/tests.rs::create_io_queue_rejects_out_of_range_qid /
                // create_io_queue_gate_uses_runtime_io_queue_pairs 现锚定 INVALID_FIELD，
                // 为不破坏并行会话的测试，**保留** INVALID_FIELD（残留不一致：范围错用
                // Generic、重复/无效 CQID 用 Command-Specific）。规范统一待 M-matrix owner。
                if qid == 0 || qid > self.io_queue_pairs {
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                }
                if !pc {
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                }
                // **IO 队列管理 spec gap 修复（2026-06-11，spec § 5.4）** — qid 已存在则
                // 拒，**不**静默覆盖（旧版 `cqs.insert` 直接覆写既有 CQ → driver 误以为
                // 重建成功，实则丢了原队列状态）。返 Command-Specific Invalid Queue Identifier。
                if self.cqs.contains_key(&qid) {
                    return Some(Cqe::error(
                        cid,
                        0,
                        sq_head,
                        phase,
                        sc::INVALID_QUEUE_IDENTIFIER,
                    ));
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
                // **2026-06-09** — qid 范围校验（同 Create IO CQ；防 DenseMap 越界误 success）。
                // 同上：保留 INVALID_FIELD 以不破坏 tests.rs 的范围锚定。
                if qid == 0 || qid > self.io_queue_pairs {
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                }
                if !pc {
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                }
                // **IO 队列管理 spec gap 修复（2026-06-11，spec § 5.5）** — qid 已存在则拒，
                // 不静默覆盖（旧版 `sqs.insert` 覆写既有 SQ）。Command-Specific Invalid
                // Queue Identifier。
                if self.sqs.contains_key(&qid) {
                    return Some(Cqe::error(
                        cid,
                        0,
                        sq_head,
                        phase,
                        sc::INVALID_QUEUE_IDENTIFIER,
                    ));
                }
                // **IO 队列管理 spec gap 修复（2026-06-11，spec § 5.5）** — SQ 绑定的 CQID
                // 必须是一个**已创建**的 CQ。旧版用通用 INVALID_FIELD；spec 规定此条件应返
                // Command-Specific **Completion Queue Invalid**（0x100）——driver 据此区分
                // "CQID 无效" 与其它字段错误。CQID 解析自 cdw11 bits 31:16（见上）。
                if !self.cqs.contains_key(&cqid) {
                    return Some(Cqe::error(
                        cid,
                        0,
                        sq_head,
                        phase,
                        sc::COMPLETION_QUEUE_INVALID,
                    ));
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
                        // 真实硬件常授 ≤ requested；我们 cap=IO_QUEUE_SLOT_CAPACITY。
                        // driver 写 cdw11 = (NSQR-1) | ((NCQR-1) << 16) 请
                        // 求 queue 数；controller 在 CQE cdw0 回 (NSQA-1)
                        // | ((NCQA-1) << 16) 表示实际授予（0-based）。
                        let req_nsq = (cdw11 & 0xffff) as u16 + 1;
                        let req_ncq = ((cdw11 >> 16) & 0xffff) as u16 + 1;
                        let granted = req_nsq.min(req_ncq).min(self.io_queue_pairs);
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
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
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
                        let tok = self.guest_read(ctx, sqe.prp1, bytes);
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
                    cmd::fid::NS_WRITE_PROTECTION => {
                        // **Phase S1** — per-NS Write Protection State (spec § 8.19)。
                        // cdw11 bits 2:0 = WPS：0 NoWP / 1 WP / 2 WP until power
                        // cycle / 3 Permanent。NSID 必须 ≠ 0 / 非 broadcast。
                        // Permanent WP (3) 不可降级，再写也保持 3。
                        let wps = (cdw11 & 0x7) as u8;
                        let nsid = sqe.nsid;
                        if nsid == 0 || nsid == 0xFFFF_FFFF {
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                        }
                        let Some(ns) = self.namespaces.get_mut(&nsid) else {
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_NAMESPACE));
                        };
                        if ns.nswp == 3 {
                            tracing::warn!(
                                nsid,
                                "Set Features 0x84 on permanently-write-protected NS rejected"
                            );
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                        }
                        ns.nswp = wps;
                        // **Phase S1 H2** — 不写 self.features：per-NS 值的
                        // 真实源头是 ns.nswp，写 features 缓存会让 Get 路径
                        // 返 controller-global 误导值。Get 路径自己按 nsid
                        // 读 ns.nswp。
                        tracing::info!(nsid, wps, "Set Features NS Write Protection");
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
                // **D（persistent features，spec § 5.21.1）** — SV=1（cdw10 bit 31）
                // → 把本 FID 的 current 值镜像进 saved_features（跨 reset 保留，
                // enable 时回灌）。只镜像真存进 features map 的 FID（NUMBER_OF_QUEUES
                // 等实时 FID 不入 map，不走持久化路径）。
                let sv = (sqe.cdw10 >> 31) & 1 != 0;
                if sv && let Some(&v) = self.features.get(&fid) {
                    self.saved_features.insert(fid, v);
                    tracing::debug!(fid, "Set Features SV=1 → saved (persistent across reset)");
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
                // **D（persistent features）** — SEL=2(saved) 返 saved_features 值
                // （Set SV=1 持久化的）。未保存过的 FID 返 0（教学：spec 允许返 default，
                // 0 即多数 FID 的 default）。SEL 0/1/3 仍走下方 current 计算。
                // **reviewer M-2**：HOST_IDENTIFIER(0x81，DMA 输出)/NS_WRITE_PROTECTION
                // (0x84，per-NS) 不走 scalar cdw0 路径，short-circuit 会返错形状结果 →
                // 排除它们，让其落到下方正常处理。
                if sel == 2
                    && !matches!(
                        fid,
                        cmd::fid::HOST_IDENTIFIER | cmd::fid::NS_WRITE_PROTECTION
                    )
                {
                    cqe.cdw0 = self.saved_features.get(&fid).copied().unwrap_or(0);
                    return Some(cqe);
                }
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
                        self.dma_write_then_complete(
                            ctx, sqe.prp1, sqe.prp2, buf, cid, 0, sq_head, cq_id,
                        );
                        return None;
                    }
                    cmd::fid::NS_WRITE_PROTECTION => {
                        // **Phase S1 H2** — Feature 0x84 是 per-NS，必须按 sqe.nsid
                        // 读取目标 NS 的 nswp，而不是回 controller-global 缓存。
                        let nsid = sqe.nsid;
                        if nsid == 0 || nsid == 0xFFFF_FFFF {
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                        }
                        let Some(ns) = self.namespaces.get(&nsid) else {
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_NAMESPACE));
                        };
                        ns.nswp as u32
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
                // **V8c** — push 带 conn_id（由 nvme_admin_dispatch_with_conn
                // 设置；legacy nvme_admin_dispatch 调用时 = 0）。让 fire_aen
                // 路径只投回原 conn 防 cross-conn AER 窃取（security H-1）。
                // **V8d (V8c reviewer H-3 + V8d reviewer H-4)** — 双层 cap：
                //   - per-conn 8（= session MAX_PENDING_AERS=4 的 2× 兜底，
                //     防 cross-tenant：恶意 conn 灌满不影响别 conn 的 4 slot）
                //   - 全 controller 256（防 controller 视角 fire_aen O(n) DoS）
                // session 端 per-conn cap=4 已是 first line；本 cap 是 controller
                // 视角的 defense-in-depth。conn_id=0（legacy 路径）不参与 per-conn
                // 限制（避免老测试 fixture 误触发）；仍受全局 256 cap 约束。
                const CTRL_AER_HARD_CAP: usize = 256;
                const PER_CONN_AER_CAP: usize = 8;
                if self.aen_pending.len() >= CTRL_AER_HARD_CAP {
                    tracing::warn!(
                        cid,
                        pending = self.aen_pending.len(),
                        cap = CTRL_AER_HARD_CAP,
                        "AsyncEventRequest rejected: controller-side global hard cap exceeded"
                    );
                    return Some(Cqe::error(
                        cid,
                        0,
                        sq_head,
                        phase,
                        sc::ASYNC_EVENT_REQUEST_LIMIT_EXCEEDED,
                    ));
                }
                let conn_id = self.current_dispatch_conn_id;
                if conn_id != 0 {
                    let per_conn_count = self.aen_pending.iter().filter(|t| t.3 == conn_id).count();
                    if per_conn_count >= PER_CONN_AER_CAP {
                        tracing::warn!(
                            cid,
                            conn_id,
                            per_conn_count,
                            cap = PER_CONN_AER_CAP,
                            "AsyncEventRequest rejected: per-conn hard cap exceeded \
                             (V8d反 cross-tenant AER squat)"
                        );
                        return Some(Cqe::error(
                            cid,
                            0,
                            sq_head,
                            phase,
                            sc::ASYNC_EVENT_REQUEST_LIMIT_EXCEEDED,
                        ));
                    }
                }
                self.aen_pending
                    .push_back((cid, 0, cq_id, self.current_dispatch_conn_id));
                tracing::debug!(
                    cid,
                    conn_id = self.current_dispatch_conn_id,
                    queued = self.aen_pending.len(),
                    "AsyncEventRequest queued (Phase F: real queue, fire on event; V8c per-conn)"
                );
                None
            }
            admin_opc::GET_LOG_PAGE => {
                // NVMe spec § 5.16 Get Log Page。CDW10 bits 7:0 = LID
                // (log page identifier)，bits 31:16 = NUMDL (number of
                // dwords lower, zero-based)。CDW11 高 16 = NUMDU。
                // **V-followup-interop-6** — CDW12 (LPO low 32b) + CDW13 (LPO
                // hi 32b) 是 Log Page Offset。libnvme Discovery 流程：
                //   1. probe 20 byte (LPO=0) 拿 header NUMREC
                //   2. 第 2 次 LPO=sizeof(header)=1024, len=numrec*1024，**只拿 entries**
                // 我们之前忽略 LPO 直接调 build_discovery_log(bytes)，第 2 次
                // 拿到的还是 [0..bytes] header 截断，entries 全没发出去。
                let lid = (sqe.cdw10 & 0xff) as u8;
                let numd_lo = ((sqe.cdw10 >> 16) & 0xffff) as u32;
                let numd_hi = (sqe.cdw11 & 0xffff) as u32;
                let numd = ((numd_hi << 16) | numd_lo) as u64 + 1; // zero-based dwords
                let bytes_req = numd * 4; // bytes
                let lpo: u64 = (sqe.cdw12 as u64) | ((sqe.cdw13 as u64) << 32);
                tracing::debug!(lid, bytes = bytes_req, lpo, "Get Log Page");
                // **P2（2026-06-10）+ C1② chaining 真激活（2026-06-11）** — admin 数据
                // DMA 经 `dma_write_then_complete` 复用 IO PRP-list 机件，含 chain pointer
                // 跟随。原 2 MiB cap（单 PRP list 页 = 512 entry ≈ 2 MiB）已抬到
                // MAX_PRP_LIST_PAGES × 511 entry × 4 KiB ≈ 32 MiB（chain 多页激活）。
                // 真正的防御在 chain walk arm（NvmReadPrpListFetch）的深度上限。
                // 此处仅做粗粒度合理性校验避免巨量 vec 提前分配 DoS。
                if bytes_req > 32 * 1024 * 1024 {
                    tracing::warn!(
                        lid,
                        bytes = bytes_req,
                        "Get Log Page: request > 32 MiB unsupported (chain depth cap)"
                    );
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
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
                    // **Phase V7** — Discovery Log Page (spec § 5.16.1.20 / § 5.1.4)。
                    // session 启动 `--discovery-mode` 时通过 nvme_set_discovery_target
                    // 注入 portals；非 discovery mode 时 portals 空 → 返 header-only
                    // empty log (NUMREC=0)。host 也可正常解析。
                    0x70 => {
                        // **V-followup-interop-6** — 真正应用 LPO：build 完整
                        // log (header + N entries)，然后按 [lpo .. lpo+bytes]
                        // 切片返。libnvme `nvme_discovery_log()` 用 LPO=1024
                        // (sizeof header) 拉 entries 部分。
                        let full = super::discovery_log::build_discovery_log(
                            self.discovery_gen_ctr,
                            &self.discovery_portals,
                            // 强制让 build 产足够大 buffer (header + entries 实际 size)
                            1024 + self.discovery_portals.len() * 1024,
                        );
                        let start = (lpo as usize).min(full.len());
                        let end = (start + bytes).min(full.len());
                        let mut out = full[start..end].to_vec();
                        // host 期待恰好 bytes 个字节；zero-pad 若 source 不够长
                        if out.len() < bytes {
                            out.resize(bytes, 0);
                        }
                        out
                    }
                    _ => {
                        tracing::debug!(
                            lid = format_args!("{:#x}", lid),
                            "Get Log Page: unknown LID, returning zeros"
                        );
                        // 未知 LID 无自然 log；返 ≤1 页零让 driver 继续，**不按 attacker NUMD
                        // pad 到 32 MiB**（与 logs.rs 各 builder 的自然尺寸截断同纪律）。
                        vec![0u8; bytes.min(NVME_PAGE_SIZE as usize)]
                    }
                };
                self.dma_write_then_complete(ctx, sqe.prp1, sqe.prp2, buf, cid, 0, sq_head, cq_id);
                None
            }
            admin_opc::DELETE_IO_SQ => {
                let qid = (sqe.cdw10 & 0xffff) as u16;
                tracing::info!(qid, "Delete IO SQ");
                // **IO 队列管理 spec gap 修复（2026-06-11，spec § 5.7）** — 删一个不存在的
                // SQ 必须返 Command-Specific Invalid Queue Identifier，**不**无条件返
                // success（旧版恒 success，会让 driver 误以为删了一个从未建过的队列）。
                // `remove` 返 None 即原本不存在。
                if self.sqs.remove(&qid).is_none() {
                    return Some(Cqe::error(
                        cid,
                        0,
                        sq_head,
                        phase,
                        sc::INVALID_QUEUE_IDENTIFIER,
                    ));
                }
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::DELETE_IO_CQ => {
                let qid = (sqe.cdw10 & 0xffff) as u16;
                tracing::info!(qid, "Delete IO CQ");
                // **IO 队列管理 spec gap 修复（2026-06-11，spec § 5.6）** — 两个 spec 错误
                // 条件（顺序：先验存在，再验无关联 SQ，最后才删）：
                //   ① CQ 不存在 → Invalid Queue Identifier；
                //   ② 仍有 SQ 绑定到本 CQID → Invalid Queue Deletion（spec 要求删 CQ 前
                //      必须先删其所有关联 SQ；否则那些 SQ 的完成将无处投递）。
                // SQ→CQ 绑定存于 `SubmissionQueue.cq_id`（Create IO SQ 时由 cdw11 高 16 位
                // 设定）；扫 `self.sqs` 找任一 `cq_id == qid` 的 SQ（admin qid 0 的 SQ 绑
                // CQ0，永不命中 IO CQID，无需特判）。
                if !self.cqs.contains_key(&qid) {
                    return Some(Cqe::error(
                        cid,
                        0,
                        sq_head,
                        phase,
                        sc::INVALID_QUEUE_IDENTIFIER,
                    ));
                }
                if self.sqs.iter().any(|(_, sq)| sq.cq_id == qid) {
                    return Some(Cqe::error(
                        cid,
                        0,
                        sq_head,
                        phase,
                        sc::INVALID_QUEUE_DELETION,
                    ));
                }
                self.cqs.remove(&qid);
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::ABORT => {
                // **A1（spec § 5.1 Abort）** — cdw10 bits15:0=SQID，bits31:16=CID
                // 命名要中止的命令。真定位 in-flight 异步命令并中止；dw0 bit0 报
                // 结果（0=已中止 / 1=Could Not Abort）。此前是 no-op（恒返 dw0=1）。
                // ACL=3（最多 4 条并发 Abort）已在 Identify Controller 广告。
                let target_sqid = (sqe.cdw10 & 0xffff) as u16;
                let target_cid = ((sqe.cdw10 >> 16) & 0xffff) as u16;
                let aborted = self.try_abort_inflight(ctx, target_sqid, target_cid);
                tracing::debug!(target_sqid, target_cid, aborted, "Abort");
                let mut cqe = Cqe::success(cid, 0, sq_head, phase);
                cqe.cdw0 = u32::from(!aborted); // bit0: 0=Aborted, 1=Could Not Abort
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
                if lbafl > 2 || pi > 1 {
                    // **Phase K1 + 2026-06-09** — 真支持 LBAF[0]=512B/no-meta、
                    // LBAF[1]=4K+8B-meta(PI)、LBAF[2]=纯 4K/no-meta + PI Type 0/1。
                    tracing::warn!(
                        lbafl,
                        pi,
                        "Format rejected: only LBAF[0/1/2] + PI Type 0/1 supported"
                    );
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                }
                // **B6a 极性 + B6b separate buffer（spec § 5.14 Format MSET /
                // FLBAS inband_metadata）**：mset=1 = metadata 作 extended LBA 内联；
                // mset=0 = metadata 走独立 MPTR buffer（B6b 已实现 PRACT=0 host-PI 路径）。
                // 仅当所选 LBAF 含 metadata（LBAF[1]，MS=8）时 mset 有意义；无 meta
                // （LBAF[0/2]，MS=0）时 mset 被忽略（meta_inline 取默认 true 无影响）。
                let lbaf_has_meta = lbafl == 1;
                // meta_inline：有 meta 时由 mset 决定（1=内联/extended，0=separate）；
                // 无 meta 时取 true（无意义，FLBAS bit4 仍为 0 因 meta_size=0）。
                let new_meta_inline = !lbaf_has_meta || mset == 1;
                // **Phase K1 + 2026-06-09** 计算新 LBAF + PI 配置。
                // LBAF: 0→(512B,no-meta) / 1→(4K,8B-meta) / 2→(4K,no-meta)。
                let (new_lbads, new_meta_size): (u8, u8) = match lbafl {
                    0 => (9, 0),
                    1 => (12, 8),
                    _ => (12, 0), // lbafl == 2（已被上面 gate 限制 ≤2）
                };
                if pi != 0 && new_meta_size == 0 {
                    // PI 需要 metadata 空间承载 8-byte tuple（纯 4K LBAF[2] 不能开 PI）。
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                }
                let new_pi_type = pi;
                let new_pi_first = pil == 0; // PIL=0 → first 8, PIL=1 → last 8
                // **C1 修复 + Reviewer H-1 (7轮)**：FORMAT 不能在任何 in-flight
                // IO 时执行。除 pending_ios / dual_prp_writes 外，K2/K4c/O2
                // 引入的 prp_list_ops / compare_ops / pi_writes / pending_fused
                // 累积器也都得空 — 否则它们后续完成时会写已被 truncate 的
                // 文件。
                let inflight = !self.pending_ios.is_empty()
                    || !self.dual_prp_writes.is_empty()
                    || !self.prp_list_ops.is_empty()
                    || !self.compare_ops.is_empty()
                    || !self.pi_writes.is_empty()
                    || !self.pi_reads.is_empty()
                    || !self.sep_meta_writes.is_empty()
                    || !self.sep_meta_reads.is_empty()
                    || !self.inline_meta_writes.is_empty()
                    || !self.inline_meta_reads.is_empty()
                    || !self.pending_fused.is_empty();
                if inflight {
                    tracing::warn!(
                        pending_ios = self.pending_ios.len(),
                        pending_dual = self.dual_prp_writes.len(),
                        pending_prp_list = self.prp_list_ops.len(),
                        pending_compare = self.compare_ops.len(),
                        pending_pi_writes = self.pi_writes.len(),
                        pending_pi_reads = self.pi_reads.len(),
                        pending_fused = self.pending_fused.len(),
                        "Format NVM rejected: IO in flight"
                    );
                    // SC 0x84 Format In Progress (NVMe 1.4 § 4.6.1.2.1)。
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::FORMAT_IN_PROGRESS));
                }
                if ses == 1 || ses == 2 {
                    // SES=1 User Data Erase / SES=2 Cryptographic Erase。
                    // **教学说明**：用 `set_len(0) + set_len(size)` 创建 sparse
                    // hole — host filesystem 看 hole 区域返零，但底层物理扇区
                    // **未真写零**（不是 SCSI BLKZEROOUT / ATA TRIM 那种擦除）。
                    // 真安全擦除需 write 全零 + fsync 或调 fallocate(
                    // FALLOC_FL_ZERO_RANGE)。本 example 教学用，sparse hole
                    // 行为对 guest 而言等价 "全零盘"。
                    //
                    // **Phase Q8** — SES=2 Cryptographic Erase：真硬件
                    // 销毁 NS 加密 key + KEK，盘上 data 立即变 garbage。
                    // 我们没真加密，但**bump `crypto_gen` counter**
                    // 模拟 key 生成代际；driver 拿 Identify NS DPS / SMART
                    // 看 key generation 变化能感知 erase 发生。
                    if ses == 2 {
                        self.crypto_gen = self.crypto_gen.wrapping_add(1);
                        tracing::info!(
                            crypto_gen = self.crypto_gen,
                            "Format SES=2 Cryptographic Erase: key generation bumped"
                        );
                    }
                    //
                    // **Phase H4**：sqe.nsid 0xFFFF_FFFF = broadcast，format
                    // 所有 NS；具体 NSID 仅 format 该 NS。
                    let nsid = sqe.nsid;
                    let targets: Vec<u32> = if nsid == 0xFFFF_FFFF {
                        self.namespaces.keys().collect()
                    } else if self.namespaces.contains_key(&nsid) {
                        vec![nsid]
                    } else {
                        return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_NAMESPACE));
                    };
                    // **Phase S1 H1** — Format 会清盘，对任何 write-protected NS
                    // 必须拒绝 (NVMe 2.0 § 8.19 + cmd::fid::NS_WRITE_PROTECTION)。
                    // broadcast 时只要 *任意* target NS protected 即整批失败。
                    for target_nsid in &targets {
                        let Some(ns) = self.namespaces.get(target_nsid) else {
                            continue;
                        };
                        if ns.nswp != 0 {
                            tracing::debug!(
                                target_nsid,
                                wps = ns.nswp,
                                "Format rejected: NS write-protected"
                            );
                            return Some(Cqe::error(
                                cid,
                                0,
                                sq_head,
                                phase,
                                sc::NAMESPACE_IS_WRITE_PROTECTED,
                            ));
                        }
                    }
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
                                ));
                            }
                        };
                        // **Reviewer C-2 修正** — set_len(0) 会让 live mmap
                        // 的 page mapping 越界 → 后续 mmap 访问 SIGBUS。
                        // 先 drop mmap，truncate 完成后再 rebuild。
                        ns.mmap = None;
                        if let Err(e) = ns.file.set_len(0).and_then(|_| ns.file.set_len(size)) {
                            tracing::warn!(error = %e, nsid = target_nsid, "Format NVM: truncate failed");
                            // 失败也尝试 rebuild mmap 以维持一致性
                            ns.mmap = crate::controller::try_mmap_file(&ns.file);
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INTERNAL_ERROR));
                        }
                        use std::io::Write as _;
                        if let Err(e) = ns.file.flush() {
                            tracing::warn!(error = %e, nsid = target_nsid, "Format NVM: flush failed");
                            ns.mmap = crate::controller::try_mmap_file(&ns.file);
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INTERNAL_ERROR));
                        }
                        // **Reviewer C-2** — rebuild mmap，让后续 read_at/write_at
                        // 重走零拷贝 fast path。
                        ns.mmap = crate::controller::try_mmap_file(&ns.file);
                        // **Phase K1** — 应用新 LBAF + PI 配置
                        ns.lbads = new_lbads;
                        // **NS-not-ready spec-completeness** — Format NVM 初始化 media →
                        // NS 转 ready（清 not_ready）；后续 IO 不再返 NAMESPACE_NOT_READY，
                        // Identify NS NSTAT.NRDY 也随之变 0。
                        ns.not_ready = false;
                        ns.meta_size = new_meta_size;
                        ns.pi_type = new_pi_type;
                        ns.pi_first = new_pi_first;
                        ns.meta_inline = new_meta_inline;
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
                        self.namespaces.keys().collect()
                    } else if self.namespaces.contains_key(&nsid) {
                        vec![nsid]
                    } else {
                        return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_NAMESPACE));
                    };
                    // **Phase S1 H1** — Format 会清盘，对任何 write-protected NS
                    // 必须拒绝 (NVMe 2.0 § 8.19 + cmd::fid::NS_WRITE_PROTECTION)。
                    // broadcast 时只要 *任意* target NS protected 即整批失败。
                    for target_nsid in &targets {
                        let Some(ns) = self.namespaces.get(target_nsid) else {
                            continue;
                        };
                        if ns.nswp != 0 {
                            tracing::debug!(
                                target_nsid,
                                wps = ns.nswp,
                                "Format rejected: NS write-protected"
                            );
                            return Some(Cqe::error(
                                cid,
                                0,
                                sq_head,
                                phase,
                                sc::NAMESPACE_IS_WRITE_PROTECTED,
                            ));
                        }
                    }
                    for target_nsid in targets {
                        let ns = self.namespaces.get_mut(&target_nsid).unwrap();
                        let size = ns.file.metadata().map(|m| m.len()).unwrap_or(0);
                        ns.lbads = new_lbads;
                        // **NS-not-ready spec-completeness** — 同 SES≠0 路径：Format
                        // 初始化 NS → 转 ready（清 not_ready）。
                        ns.not_ready = false;
                        ns.meta_size = new_meta_size;
                        ns.pi_type = new_pi_type;
                        ns.pi_first = new_pi_first;
                        ns.meta_inline = new_meta_inline;
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
                let bpid = (sqe.cdw10 >> 31) & 0x1 != 0;
                tracing::info!(fs, ca, bpid, "Firmware Commit");
                // **Boot Partition write-protect（spec § 8.13 + § 5.16 Firmware Commit BPID）**
                // — BPID=1 表示把下载的 image 提交到 boot partition。本教学 controller 的 boot
                // partition 是**只读出厂镜像**（write-protected）→ BOOT_PARTITION_WRITE_PROHIBITED；
                // 未广告 BP（BPSZ=0，boot_partition 空）却带 BPID → INVALID_FIELD（无此 BP）。
                if bpid {
                    return Some(if self.boot_partition.is_empty() {
                        Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD)
                    } else {
                        tracing::warn!(
                            "FW Commit BPID=1 到 write-protected boot partition → \
                             BOOT_PARTITION_WRITE_PROHIBITED"
                        );
                        Cqe::error(cid, 0, sq_head, phase, sc::BOOT_PARTITION_WRITE_PROHIBITED)
                    });
                }
                if !(1..=7).contains(&fs) {
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                }
                let buf_len = self.fw_download_buf.len();
                match ca {
                    0 | 1 => {
                        // 把 download buffer 内容 'flash' 到 slot FS。我们用
                        // download size 头 8 字节作 ASCII revision string；
                        // 真硬件这是 vendor 编码 image。
                        if buf_len < 8 {
                            tracing::warn!(buf_len, "FW Commit: insufficient downloaded image");
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
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
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                        }
                        self.fw_next_active_slot = fs;
                    }
                    3 => {
                        // 立即激活
                        if self.fw_slot_revisions[fs as usize].is_empty() {
                            // 没 image：用 download buffer 先 flash 再激活
                            if buf_len < 8 {
                                return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                            }
                            let rev_bytes: [u8; 8] = self.fw_download_buf[..8].try_into().unwrap();
                            self.fw_slot_revisions[fs as usize] =
                                String::from_utf8_lossy(&rev_bytes).to_string();
                        }
                        self.fw_active_slot = fs;
                        tracing::info!(fs, "FW Commit: activated immediately");
                    }
                    _ => return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD)),
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
                        return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
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
                        return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                    }
                };
                if need_total > FW_MAX || bytes_count_u64 == 0 || bytes_count_u64 > u32::MAX as u64
                {
                    tracing::warn!(need_total, "FW Download invalid size");
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                }
                if self.fw_download_buf.len() < need_total as usize {
                    self.fw_download_buf.resize(need_total as usize, 0);
                }
                // DMA-read PRP1 → 完成回调 AdminFwDownloadChunk
                let tok = self.guest_read(ctx, prp1, bytes_count_u64 as u32);
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
                            // (Command-Specific，SCT=1；与 Generic 0x1d 的
                            // Sanitize In Progress 靠 SCT 区分)。
                            tracing::warn!(stc, "Self-Test rejected: already in progress");
                            return Some(Cqe::error(
                                cid,
                                0,
                                sq_head,
                                phase,
                                sc::SELF_TEST_IN_PROGRESS,
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
                        Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD))
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
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                        }
                        let tok = self.guest_read(ctx, sqe.prp1, 4096);
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
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
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
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
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
                            return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_NAMESPACE));
                        }
                        Some(Cqe::success(cid, 0, sq_head, phase))
                    }
                    _ => Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD)),
                }
            }
            admin_opc::NS_ATTACHMENT => {
                // **Phase S4** — NVMe spec § 5.20 Namespace Attachment。
                // CDW10 bits 3:0 = SEL：0 = Attach, 1 = Detach
                // sqe.nsid = 目标 NSID（必须 ≠ 0 / 非 broadcast）
                // PRP1 → 4 KiB Controller List：bytes 0..2 = NumIDs (LE u16)，
                //                              bytes 2..  = u16[NumIDs] CNTLIDs
                // 教学单 controller cntlid=1：list 必须含 1 才能 act on 本 ctrl。
                let sel = (sqe.cdw10 & 0xf) as u8;
                let nsid = sqe.nsid;
                if nsid == 0 || nsid == 0xFFFF_FFFF {
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                }
                if !self.namespaces.contains_key(&nsid) {
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_NAMESPACE));
                }
                if sel > 1 {
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
                }
                // PRP1 → controller list 4 KiB DMA read，完成在 on_dma_complete。
                let tok = self.guest_read(ctx, sqe.prp1, 4096);
                self.pending_ios.insert(
                    tok,
                    crate::controller::PendingIo {
                        sq_id: 0,
                        cid,
                        sq_head,
                        cq_id,
                        nsid,
                        op: crate::controller::PendingOp::AdminNsAttachmentList { sel },
                    },
                );
                None
            }
            admin_opc::SECURITY_SEND => {
                // **Phase L5 + O reviewer M5 修复** — spec § 5.27 Security Send。
                // SECP=0 (Info) 在 Spec 中无 Send operation 定义（spec 表
                // 5-19 Info protocol 仅供 Receive 使用），改返 INVALID_FIELD。
                // 其它 SECP 我们都没真实现 → 一律 INVALID_FIELD。
                let secp = ((sqe.cdw10 >> 16) & 0xff) as u8;
                tracing::debug!(secp, "Security Send (INVALID_FIELD; no SP impl)");
                Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD))
            }
            admin_opc::SECURITY_RECEIVE => {
                // **Phase L5 + O reviewer M4 修复** — spec § 5.28 Security
                // Receive。SECP=0 (Info) 返 Security Protocol List。
                // 之前声明 TCG OPAL (0xEF) 但实现没有任何 0xEF Send 路径
                // → 改只声明 SECP=0 Info（list 长度 = 1）。
                let secp = ((sqe.cdw10 >> 16) & 0xff) as u8;
                let alloc = (sqe.cdw11 & 0xffff) as usize;
                if secp == 0 {
                    // 自然尺寸 = Security Protocol List header(8) + 1 protocol ID = 9
                    // （SECP=0 只声明 Info）。**alloc 截断纪律（同 Get Log Page NUMD）**：alloc
                    // (cdw11 低 16，≤64 KiB host 控) 历史直接分配+pad → 小命令撑出 64 KiB 全零
                    // （放大）。改：返自然 9B、超请求截到 9、不按 alloc pad。
                    let mut buf = vec![0u8; 9];
                    // bytes 0..6 reserved
                    // bytes 6..8 = LIST LENGTH (big endian) = 1 protocol
                    buf[6] = 0;
                    buf[7] = 1;
                    // byte 8 = supported protocol ID (0x00 Info)
                    buf[8] = 0x00;
                    buf.truncate(alloc); // 返 min(alloc, 9)：alloc≥9 noop、alloc<9 返前缀
                    self.dma_write_then_complete(
                        ctx, sqe.prp1, sqe.prp2, buf, cid, 0, sq_head, cq_id,
                    );
                    None
                } else {
                    Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD))
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
                self.dma_write_then_complete(ctx, sqe.prp1, sqe.prp2, buf, cid, 0, sq_head, cq_id);
                None
            }
            admin_opc::VIRTUALIZATION_MGMT => {
                // **Phase L5** — Virtualization Management (spec § 5.24)。
                // 我们不实现 SR-IOV / VF resource alloc；返 INVALID_FIELD
                // 让 driver fallback。
                tracing::debug!(cid, "Virtualization Mgmt (INVALID_FIELD)");
                Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD))
            }
            admin_opc::GET_LBA_STATUS => {
                // **Phase L5 + reviewer H-4** — Get LBA Status (spec § 5.15)。
                // CDW10/11 = SLBA, CDW12 bits 31:16 = MNDW (Max Number of
                // Dwords)。返 4 KiB buffer with valid LBA Status Header：
                //   bytes 0..4  NLSD (Number of LBA Status Descriptors) = 0
                //   bytes 4..7  CMPC (Completion Condition) = 0
                //   bytes 7..8  reserved
                //   bytes 8..   per-LBA descriptors (empty since NLSD=0)
                // 教学：我们的 backing 没 'suspected error LBA' 概念，所以
                // NLSD 永远 0；header 字段必须真填零（vec![0u8; 4096] 已满足）。
                let buf = vec![0u8; 4096];
                self.dma_write_then_complete(ctx, sqe.prp1, sqe.prp2, buf, cid, 0, sq_head, cq_id);
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
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_FIELD));
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
                    return Some(Cqe::error(cid, 0, sq_head, phase, sc::SANITIZE_IN_PROGRESS));
                }
                // **Phase S1 H1** — Sanitize 抹整盘，命中任何 protected NS 都
                // 拒（NVMe 2.0 § 8.19）。Sanitize 是 controller 范围，遍历所有 NS。
                for (target_nsid, ns) in self.namespaces.iter() {
                    if ns.nswp != 0 {
                        tracing::debug!(
                            target_nsid,
                            wps = ns.nswp,
                            "Sanitize rejected: NS write-protected"
                        );
                        return Some(Cqe::error(
                            cid,
                            0,
                            sq_head,
                            phase,
                            sc::NAMESPACE_IS_WRITE_PROTECTED,
                        ));
                    }
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
                // **DBBUF（spec § 5.7 Doorbell Buffer Config + § 7.13 shadow doorbells）**
                // PRP1 = shadow doorbell buffer GPA（dbbuf_dbs）；PRP2 = event idx
                // buffer GPA（dbbuf_eis）。存下二者即 **激活** DBBUF：此后 controller 不
                // 再信任可能 stale 的 MMIO doorbell value，改 DMA-poll shadow 拿真
                // tail/head 并写回 event_idx（race-safe 循环见 mod.rs
                // `handle_shadow_sq` / `handle_shadow_cq`）。Linux nvme 据 event_idx 的
                // `nvme_dbbuf_need_event` 决定是否 ring 真 doorbell（省 VM-exit）。
                let prp1 = sqe.prp1;
                let prp2 = sqe.prp2;
                self.doorbell_shadow_gpa = prp1;
                self.doorbell_event_idx_gpa = prp2;
                // (重)配置 → 清上次写回的 event_idx 缓存，保证新 buffer 被重新写入
                // （否则 last_eventidx_sq 残留会让 "已追平" 分支误判无需写）。
                self.last_eventidx_sq.clear();
                tracing::info!(
                    shadow = format_args!("{:#x}", prp1),
                    event_idx = format_args!("{:#x}", prp2),
                    "Doorbell Buffer Config: DBBUF activated (shadow doorbells polled)"
                );
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            admin_opc::LOCKDOWN => {
                // **Phase Q7** — Lockdown (NVMe 2.0 § 5.18)。Driver 控制
                // controller 禁用/启用 specific admin commands by opcode。
                //
                // CDW10:
                //   bits 7:0   OFI — Opcode/Feature Identifier
                //   bit  8     IFC — 0=Admin commands, 1=Set Features
                //   bits 18:16 SCP — Scope (0=current ctrl, 1=NVM Subsystem)
                //   bit 30     OPC — Opcode (0=ignore IFC, 1=use IFC)
                //   bit 31     LCKDWN — 0=unlock / 1=lock
                //
                // 教学 controller 实现 in-memory lockdown set：lock opcode →
                // 加入 HashSet，下次 dispatch_admin 进入前查 set 拒
                // COMMAND_PROHIBITED_BY_COMMAND_AND_FEATURE_LOCKDOWN (SC 0x23)。
                // 单 controller scope only。
                let ofi = (sqe.cdw10 & 0xff) as u8;
                let lock = (sqe.cdw10 >> 31) & 0x1 != 0;
                if lock {
                    self.locked_admin_opcodes.insert(ofi);
                    tracing::info!(
                        opc = format_args!("{:#x}", ofi),
                        "Lockdown: admin opcode locked"
                    );
                } else {
                    self.locked_admin_opcodes.remove(&ofi);
                    tracing::info!(
                        opc = format_args!("{:#x}", ofi),
                        "Lockdown: admin opcode unlocked"
                    );
                }
                Some(Cqe::success(cid, 0, sq_head, phase))
            }
            opc => {
                // **Reviewer C-2 修正** — spec § 3.3.3.2.1：未识别 opcode
                // 必须返 INVALID_OPCODE (0x01)，否则 driver 在探测 opcode
                // 时会被 'success+cdw0=0' 误导，可能基于幻觉的副作用继续
                // 操作。INVALID_OPCODE 是 NVMe driver 的正常路径，不会
                // fail device — Linux nvme_set_features / Windows
                // nvme_query_directive 都把它视为 'feature 不支持'。
                tracing::warn!(opc, "unsupported admin opcode → INVALID_OPCODE");
                Some(Cqe::error(cid, 0, sq_head, phase, sc::INVALID_OPCODE))
            }
        }
    }
}
