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
    /// **教学边界（deliberate omission，reviewer M-2/M-3）**：
    /// - 不 quiesce：完成后不拦截新命令的 doorbell（`on_sq_tail_doorbell` 不 gate
    ///   SHST/RDY）。教学 driver 协作、shutdown 后不再敲门，故不建模。
    /// - flush 失败仍报 complete：spec 无 "shutdown failed" 状态；production controller
    ///   会在此置 `csts::CFS`（fatal）让 driver 知数据可能丢，教学版仅 warn。
    fn process_shutdown(&mut self, shn_field: u32) {
        tracing::info!(
            shn = shn_field,
            "NVMe: CC.SHN shutdown → flush all NS + CSTS.SHST=complete"
        );
        let nsids: Vec<u32> = self.namespaces.keys().collect();
        for nsid in nsids {
            if let Some(ns) = self.namespaces.get(&nsid)
                && let Err(e) = ns.flush()
            {
                tracing::warn!(nsid, error = %e, "shutdown flush failed");
            }
        }
        self.csts = (self.csts & !csts::SHST_MASK) | csts::SHST_COMPLETE;
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
        self.compare_ops.clear();
        self.pi_writes.clear();
        self.pi_reads.clear();
        self.pending_fused.clear();
        self.sqe_inbox.clear();
        debug_assert!(self.pending_ios.is_empty());
        debug_assert!(self.dual_prp_writes.is_empty());
        debug_assert!(self.prp_list_ops.is_empty());
        debug_assert!(self.compare_ops.is_empty());
        debug_assert!(self.pi_writes.is_empty());
        debug_assert!(self.pi_reads.is_empty());
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
        self.granted_io_queues = self.io_queue_pairs;
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
}
