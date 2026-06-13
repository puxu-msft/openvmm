// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Controller enable / disable / CC.EN state machine。
//!
//! 拆出来减小 `controller/mod.rs` 体积（reviewer M-5 建议）。逻辑全部在
//! `impl NvmeController` 的方法上，外部行为无变化。
//!
//! - `write_cc`: CC 寄存器写入 dispatcher（启用 / 禁用）
//! - `enable`: CC.EN 0→1 流程（验证 AQA/ASQ/ACQ + 初始化 admin SQ/CQ +
//!   CSTS.RDY=1）
//! - `disable`: CC.EN 1→0 流程（清队列 / pending IO / 累积器 + CSTS.RDY=0）
//!
//! 注：reset (PCIe FLR) 走 disable 路径；reset 入口仍在 mod.rs 的
//! `impl PcieDevice::reset`。

use super::*;

impl NvmeController {
    pub(super) fn write_cc(&mut self, new_cc: u32) {
        let old_en = self.cc & cc::EN != 0;
        let new_en = new_cc & cc::EN != 0;
        let old_shn = (self.cc & cc::SHN_MASK) >> cc::SHN_SHIFT;
        let new_shn = (new_cc & cc::SHN_MASK) >> cc::SHN_SHIFT;
        self.cc = new_cc;
        if !old_en && new_en {
            self.enable();
        } else if old_en && !new_en {
            self.disable();
        }
        // **Shutdown sequence (spec § 3.1.4.5)**：CC.SHN 由 00 → 01(normal)/10(abrupt)
        // → controller flush volatile 数据到 NVM，再置 CSTS.SHST=10(complete) 让 driver
        // 轮询确认可安全断电。此前忽略 SHN（直接走 CSTS.RDY=0）。
        if old_shn == cc::SHN_NORMAL
            && (new_shn == cc::SHN_NORMAL_SHUTDOWN || new_shn == cc::SHN_ABRUPT_SHUTDOWN)
        {
            self.process_shutdown(new_shn);
        } else if old_shn != cc::SHN_NORMAL && new_shn == cc::SHN_NORMAL {
            // **reviewer M-1**：CC.SHN 由 shutdown 清回 00 → CSTS.SHST 回 normal
            // （spec：SHST 反映**当前** shutdown-processing 状态，不是粘滞历史）。
            self.csts &= !csts::SHST_MASK;
        }
    }

    /// **Shutdown 序列（spec § 3.1.4.5）**：flush 所有 NS 的 volatile 数据 →
    /// CSTS.SHST = complete。normal(01) 与 abrupt(10) 都 flush（教学上一律持久化更安全；
    /// spec 允许 abrupt 跳过以求快）。flush 失败仅 warn —— shutdown 不应卡死 driver。
    ///
    /// **完成后 quiesce（reviewer M-2，2026-06-10 实现）**：`on_sq_tail_doorbell` gate
    /// 在 CSTS.SHST=complete → 关机后下发的命令被忽略（SQ 仍在但不处理）；re-enable
    /// 清 SHST 后恢复。
    ///
    /// **flush 失败 → CSTS.CFS（D，spec § 3.1.4.5）**：任一 NS flush 失败即置
    /// `csts::CFS`（Controller Fatal Status）+ 仍报 SHST=complete，让 driver 知
    /// volatile 数据可能丢失（走 reset 恢复）。此前是教学边界（仅 warn 不置 CFS），
    /// 现已按 spec 报 fatal。
    ///
    /// **仍保留的教学边界（deliberate omission，D 重审保留）**：
    /// - 只 quiesce **新命令**（`on_sq_tail_doorbell` gate SHST=complete），不停 async
    ///   AEN 源（reviewer M-1）：shutdown-complete 到下次 CC.EN=0 之间，in-flight 的
    ///   self-test / sanitize / error AEN + IRQ-coalesce flush 仍可能 fire。cooperative
    ///   driver 先停自己提交侧故可接受；CC.EN=0(disable) 才真正拆掉这些。
    /// - 不 drain in-flight DMA（reviewer LOW-2 / D-③）：CC.SHN 时正在飞的 write 不强 flush。
    ///   **与 spec 一致**——NVMe reset/shutdown 终止 in-flight 命令、不承诺其数据持久化
    ///   （spec § 5.2 "Implicit Aborts on Reset"），故此为**正当边界**而非缺陷。
    fn process_shutdown(&mut self, shn_field: u32) {
        tracing::info!(
            shn = shn_field,
            "NVMe: CC.SHN shutdown → flush all NS + CSTS.SHST=complete"
        );
        let nsids: Vec<u32> = self.namespaces.keys().collect();
        let mut flush_failed = false;
        for nsid in nsids {
            if let Some(ns) = self.namespaces.get(&nsid)
                && let Err(e) = ns.flush()
            {
                tracing::warn!(nsid, error = %e, "shutdown flush failed");
                flush_failed = true;
            }
        }
        self.csts = (self.csts & !csts::SHST_MASK) | csts::SHST_COMPLETE;
        // **D（spec § 3.1.4.5）** — flush 失败 → 置 CSTS.CFS（Controller Fatal Status）
        // 让 driver 知 volatile 数据可能未持久化（应走 controller reset 恢复）。此前
        // 仅 warn（教学边界），现按 spec 报 fatal。与 DBBUF runaway 用同一 CFS 语义。
        if flush_failed {
            self.csts |= csts::CFS;
            tracing::error!("shutdown flush failed → CSTS.CFS set（volatile 数据可能丢失）");
        }
    }

    pub(super) fn enable(&mut self) {
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
        // (重新) enable = normal operation：清 CSTS.SHST（上次 shutdown 的 complete 状态）。
        self.csts &= !csts::SHST_MASK;
        // **D（persistent features，spec § 5.21.1）** — Controller Reset 后 saved
        // features 成为 current（disable 清了 current 但留了 saved）。此前 features
        // 跨 reset 全丢、SV=1 形同虚设；现回灌 saved → current。
        //
        // **reviewer H-1**：部分 FID 的 current 真相不在 features map 而在专用 live
        // 字段（POWER_MANAGEMENT→current_ps、INTERRUPT_COALESCING→irq_aggr_*），
        // disable 把这些 live 字段重置了。光回灌 map 会让 Get SEL=0 / 真实行为与
        // saved 不符——故 mirror-backed FID 必须**同步回灌其 live 字段**（与 Set
        // 路径同一 cdw11→字段 推导）。
        let restored: Vec<(u8, u32)> = self.saved_features.iter().map(|(&f, &v)| (f, v)).collect();
        for (f, v) in restored {
            self.features.insert(f, v);
            match f {
                crate::cmd::fid::POWER_MANAGEMENT => self.current_ps = (v & 0x1F) as u8,
                crate::cmd::fid::INTERRUPT_COALESCING => {
                    self.irq_aggr_threshold = (v & 0xff) as u8;
                    self.irq_aggr_time = ((v >> 8) & 0xff) as u8;
                }
                _ => {}
            }
        }
        tracing::info!(
            asqs,
            acqs,
            asq = format_args!("{:#x}", self.asq),
            acq = format_args!("{:#x}", self.acq),
            "NVMe: ready"
        );
    }

    pub(super) fn disable(&mut self) {
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
        self.sgl_ops.clear();
        self.compare_ops.clear();
        self.pi_writes.clear();
        self.pi_reads.clear();
        self.sep_meta_writes.clear();
        self.sep_meta_reads.clear();
        self.inline_meta_writes.clear();
        self.inline_meta_reads.clear();
        self.pending_fused.clear();
        self.sqe_inbox.clear();
        debug_assert!(self.pending_ios.is_empty());
        debug_assert!(self.dual_prp_writes.is_empty());
        debug_assert!(self.prp_list_ops.is_empty());
        debug_assert!(self.sgl_ops.is_empty());
        debug_assert!(self.compare_ops.is_empty());
        debug_assert!(self.pi_writes.is_empty());
        debug_assert!(self.pi_reads.is_empty());
        debug_assert!(self.sep_meta_writes.is_empty());
        debug_assert!(self.sep_meta_reads.is_empty());
        debug_assert!(self.inline_meta_writes.is_empty());
        debug_assert!(self.inline_meta_reads.is_empty());
        debug_assert!(self.pending_fused.is_empty());
        // AEN queue 跨 reset 不保留 (NVMe spec § 5.2 "Implicit Aborts on Reset")
        self.aen_pending.clear();
        self.aen_last_err_count = self.stat_num_err_log_entries;
        // Phase G：self-test 跨 reset 撤回（spec § 5.11 "Reset terminates
        // any in-progress Device Self-test"）；error log + self_test_last
        // 跨 reset 保留（spec § 5.16.1.1 / § 5.16.1.6 持久化，仅 power-
        // cycle 清空）。
        self.self_test_in_progress = None;
        // **D（persistent features）** — current features 跨 reset 不保留，但
        // **saved_features（Set SV=1 持久化的）保留**，enable 时回灌为 current
        // （spec § 5.21.1 'Save' bit）。此前 SV 被忽略、features 全丢。
        self.features.clear();
        self.granted_io_queues = self.io_queue_pairs;
        // K5: sanitize 跨 reset 撤回（spec § 5.26 'Sanitize Operation
        // Aborts on Reset'），last_status 保留作 history
        self.sanitize = None;
        // K6 / DBBUF: doorbell buffer 跨 reset 清（driver 重新配置）。连带清所有
        // shadow-poll in-flight 状态：in-flight DMA 完成时其 token 已不在表中 →
        // 静默走 unknown-token 路径（无害）；下次 enable + Doorbell Buffer Config
        // 重新登记。
        self.doorbell_shadow_gpa = 0;
        self.doorbell_event_idx_gpa = 0;
        self.pending_shadow_polls.clear();
        self.pending_eventidx_writes.clear();
        self.shadow_poll_inflight.clear();
        self.shadow_ring_pending.clear();
        self.last_mmio_sq_doorbell.clear();
        self.last_eventidx_sq.clear();
        // **reviewer MED-2** — Boot Partition Read 在飞 token + BRS 状态在 CC.EN 1→0 复位
        // 时清（与上方所有 in-flight token 集同侪）。BP Read 通常 pre-enable，但 reset 后
        // 残留 token 会落 unknown-token、BRS 也会冻结上次值——清掉保持一致。
        self.pending_boot_reads.clear();
        self.boot_read_status = 0;
        // K8: power state 重置到 PS0
        self.current_ps = 0;
        // M1: interrupt coalescing 重置默认（无 coalesce）
        self.irq_aggr_time = 0;
        self.irq_aggr_threshold = 0;
        // **CMB lifecycle 纠正（真 Linux nvme + QEMU interop oracle 推翻原 architect 复核 #4）** —
        // Controller Reset（CC.EN 1→0）**保留** CMBMSC 的 CRE/CMSE/CBA/CBAI，不清。
        // 真 Linux `nvme_map_cmb` 仅编程 CMBMSC 一次（`if (dev->cmb_size) return` 守卫），
        // init 期间的 CC.EN 周期后**不**重编程，依赖其跨 Controller Reset 持久；QEMU
        // `nvme_ctrl_reset(NVME_RESET_CONTROLLER)` 同样不动 cmbmsc（interop 参考实现）。
        // 原"reset 清 CMBMSC"是 **self-consistent trap**：in-process 测试 + 误读 spec 两端
        // 自洽通过，但真驱动据 cmse=false 把 SQ-in-CMB 的 SQE-fetch 当非-CMB 走 DMA → fail。
        // CMB enable 态**只**由 driver 的 CMBMSC 写改，或 Controller Level Reset（FLR/PCIe，
        // 见 `reset`）清。CMB backing 内容亦不动（spec 规定 reset 后"未定义"，可不清）。
        self.state = CtrlState::Disabled;
        // spec § 3.1.4.2：Controller Reset（CC.EN→0）清 CSTS.RDY **与 CSTS.CFS**。
        // 清 CFS 是 I2 门控（completion.rs `on_dma_complete_impl` 入口）的**成立前提**：
        // 否则 CFS 跨 reset 粘滞 → 重新 enable 的 controller 虽 RDY=1，但门控对所有新
        // completion `return` → IO 永不完成 = 砖化。本行同时修一处既有 spec 违规
        // （此前 CFS 一旦置位永不清）。
        self.csts &= !(csts::RDY | csts::CFS);
    }
}
