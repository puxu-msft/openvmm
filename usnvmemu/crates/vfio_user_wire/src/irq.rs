// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `DEVICE_SET_IRQS` 的 sans-IO 决策。
//!
//! 抽自 `vfio_user_transport::irq::handle_set_irqs`（W0.5，2026-06-11）。本模块
//! 只做"给定 idx/flags/start/count/fd_count → 该执行哪个动作"的纯决策；真正的
//! vector mutation + eventfd 收发 + reply/err 由 transport 端按返回的
//! [`SetIrqsAction`] 分发。
//!
//! flag 语义见 [`crate::proto::irq_set`] / [`crate::proto::pci_irq`]。

use crate::proto::irq_set;
use crate::proto::pci_irq;

/// [`decide_set_irqs_action`] 的决策结果。transport 端按此 match 执行 IO。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetIrqsAction {
    /// idx != MSIX：当前只 wire MSI-X，其它 idx no-op + best-effort OK。
    BestEffortNonMsix,
    /// DATA_NONE + TRIGGER + count==0：清整个 vector 数组。
    ClearAll,
    /// DATA_EVENTFD + TRIGGER + count>0 + fd_count==count：把 fd 赋给
    /// `[start, start+count)` 槽位。
    Assign { start: u32, count: u32 },
    /// DATA_EVENTFD + TRIGGER + fd_count==0：`[start, start+count)` 全部 de-assign
    /// （清 None / 掩码；含 count==0 的空范围）。
    Deassign { start: u32, count: u32 },
    /// DATA_EVENTFD + TRIGGER + 0<fd_count<count：部分 fd（QEMU 不会这么发），非法。
    InvalidPartialFds { need: u32, got: u32 },
    /// 其它 flag 组合（MASK/UNMASK 等）：spec 允许 server 不实现，best-effort OK。
    BestEffortUnsupported,
}

/// 纯决策：给定已解码的 SET_IRQS 参数 + 收到的 fd 数，返回该执行的动作。
///
/// **优先级**（与原 `handle_set_irqs` if/else 链一致，必须保留）：
/// 1. 非 MSIX idx 早返
/// 2. DATA_NONE + count==0 + TRIGGER（优先于 DATA_EVENTFD）
/// 3. DATA_EVENTFD + TRIGGER：按 fd_count 分 Assign / Deassign / InvalidPartialFds
/// 4. 其它 → BestEffortUnsupported
///
/// **review 灰区（真 QEMU 11 oracle）**：DATA_EVENTFD 的 fd 数未必 == count——
/// vfio-user 把 `-1`（de-assign / 跳过）编码为不经 SCM_RIGHTS 传该槽位。合法只两种：
/// `fd_count==count`（assign）或 `fd_count==0`（全 de-assign）。`0<fd_count<count`
/// 才非法。
pub fn decide_set_irqs_action(
    idx: u32,
    flags: u32,
    start: u32,
    count: u32,
    fd_count: usize,
) -> SetIrqsAction {
    if idx != pci_irq::MSIX {
        return SetIrqsAction::BestEffortNonMsix;
    }
    let is_data_none = flags & irq_set::DATA_NONE != 0;
    let is_data_eventfd = flags & irq_set::DATA_EVENTFD != 0;
    let is_trigger = flags & irq_set::ACTION_TRIGGER != 0;

    if is_data_none && count == 0 && is_trigger {
        return SetIrqsAction::ClearAll;
    }
    if is_data_eventfd && is_trigger {
        let need = count as usize;
        if need > 0 && fd_count == need {
            return SetIrqsAction::Assign { start, count };
        }
        if fd_count == 0 {
            return SetIrqsAction::Deassign { start, count };
        }
        return SetIrqsAction::InvalidPartialFds {
            need: count,
            got: fd_count as u32,
        };
    }
    SetIrqsAction::BestEffortUnsupported
}

#[cfg(test)]
mod tests {
    use super::*;

    const FL_EVENTFD_TRIGGER: u32 = irq_set::DATA_EVENTFD | irq_set::ACTION_TRIGGER;
    const FL_NONE_TRIGGER: u32 = irq_set::DATA_NONE | irq_set::ACTION_TRIGGER;

    #[test]
    fn non_msix_returns_best_effort() {
        assert_eq!(
            decide_set_irqs_action(pci_irq::INTX, FL_EVENTFD_TRIGGER, 0, 1, 1),
            SetIrqsAction::BestEffortNonMsix
        );
    }

    #[test]
    fn data_none_count_zero_returns_clear_all() {
        assert_eq!(
            decide_set_irqs_action(pci_irq::MSIX, FL_NONE_TRIGGER, 0, 0, 0),
            SetIrqsAction::ClearAll
        );
    }

    #[test]
    fn eventfd_full_fds_returns_assign() {
        assert_eq!(
            decide_set_irqs_action(pci_irq::MSIX, FL_EVENTFD_TRIGGER, 2, 3, 3),
            SetIrqsAction::Assign { start: 2, count: 3 }
        );
    }

    #[test]
    fn eventfd_zero_fds_returns_deassign() {
        assert_eq!(
            decide_set_irqs_action(pci_irq::MSIX, FL_EVENTFD_TRIGGER, 0, 1, 0),
            SetIrqsAction::Deassign { start: 0, count: 1 }
        );
    }

    /// count==0 + EVENTFD（非 DATA_NONE）走 Deassign 空范围分支（不进 Assign，
    /// 因 need>0 不满足）——锁死原 handle_set_irqs 微妙等价。
    #[test]
    fn eventfd_count_zero_returns_deassign() {
        assert_eq!(
            decide_set_irqs_action(pci_irq::MSIX, FL_EVENTFD_TRIGGER, 0, 0, 0),
            SetIrqsAction::Deassign { start: 0, count: 0 }
        );
    }

    #[test]
    fn eventfd_partial_fds_returns_invalid() {
        assert_eq!(
            decide_set_irqs_action(pci_irq::MSIX, FL_EVENTFD_TRIGGER, 0, 3, 1),
            SetIrqsAction::InvalidPartialFds { need: 3, got: 1 }
        );
    }

    #[test]
    fn unsupported_flag_combo_returns_best_effort() {
        // 只 set ACTION_TRIGGER（既非 DATA_NONE+count0 也非 DATA_EVENTFD+TRIGGER）
        assert_eq!(
            decide_set_irqs_action(pci_irq::MSIX, irq_set::ACTION_TRIGGER, 0, 1, 0),
            SetIrqsAction::BestEffortUnsupported
        );
    }

    /// DATA_NONE 但 count>0（非 0）→ ClearAll 不命中（要求 count==0）→ 非 eventfd
    /// → BestEffortUnsupported。锁死 DATA_NONE 灰区（architect C-5）。
    #[test]
    fn data_none_count_nonzero_returns_best_effort() {
        assert_eq!(
            decide_set_irqs_action(pci_irq::MSIX, FL_NONE_TRIGGER, 0, 3, 0),
            SetIrqsAction::BestEffortUnsupported
        );
    }
}
