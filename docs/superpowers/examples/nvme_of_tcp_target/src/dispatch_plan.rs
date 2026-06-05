// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V8e-7-1** — Capsule CMD 分发 sans-IO 决策表。
//!
//! 提取 sync `V2Session::dispatch_capsule_cmd / handle_admin_cmd / handle_io_cmd`
//! 里的**纯决策**部分到独立 module，sync/async dispatcher 共用 → 防止 V8e-7-2/3
//! 后两套 dispatch 行为漂移（plan §3 Q3 / §2 R-1）。
//!
//! 设计原则：
//! - 纯函数：不接 `&mut TcpStream` / `&mut SharedController` / `&mut self`
//! - 输入：SQE bytes + `ConnStateSnapshot`（caller 在锁外构造）
//! - 输出：`AdminAerDecision` / `AdminBlockedOpcDecision` / `DiscoveryWhitelist
//!   Decision` / `IoNlbDecision` / `IoPrp2SentinelDecision`
//! - 不直接吐 wire bytes；caller 根据 decision 自己 emit
//!
//! V8e-7-2 起 async dispatcher 调本 module；sync hot path 暂不动（迁移成本与
//! 兼容性权衡，留 V8e-followup）；regression gate 由 v8e1 byte-identical
//! 防漂移。

#![allow(missing_docs)]

use crate::aer::{ADMIN_OPC_AER, MAX_PENDING_AERS};
use pcie_remote_nvme_userspace::cmd::Sqe;

/// 一条 capsule cmd PDU 的粗分类（fabric / admin / IO / 非法长度）。
///
/// 取代 sync `dispatch_capsule_cmd` 入口的 `if opc == NVME_OPC_FABRIC { ... }
/// else if current_qid == 0 { admin } else { io }` 决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapsuleKind {
    /// PSH < 64 字节：不是合法 SQE，caller 应发 C2HTerm 后 bail。
    PshTooShort {
        /// 实际 PSH 长度（< 64）。
        psh_len: usize,
    },
    /// fabric command (opc = 0x7F)；caller 解 fctype 后进一步分发。
    Fabric {
        /// command id（SQE bytes 2..4）。
        cid: u16,
    },
    /// admin command (current_qid = 0)。
    Admin {
        /// command id。
        cid: u16,
    },
    /// IO command (current_qid ≥ 1)。
    Io {
        /// command id。
        cid: u16,
    },
}

/// 决策所需的 immutable 连接状态快照。
///
/// caller 在 `with_controller` 锁外构造（V8e-7-1 仅依赖 session 端 state，不
/// 需 controller lock）。
#[derive(Debug, Clone, Copy)]
pub struct ConnStateSnapshot {
    /// 当前 SQ id（V5a 单 conn 单 IO SQ，0 = admin）。
    pub current_qid: u16,
    /// V7 discovery mode（handshake 时由 controller 决定）。
    pub discovery_mode: bool,
    /// session 镜像的 pending AER 数（V6a 用作 cap）。
    pub pending_aers_len: usize,
}

/// 从 PDU PSH 字节切片决定 `CapsuleKind`。
///
/// PSH 必须 ≥ 64 字节（NVMe SQE 标准长度）；否则返 `PshTooShort`。
pub fn decide_capsule_kind(psh: &[u8], state: ConnStateSnapshot) -> CapsuleKind {
    if psh.len() < 64 {
        return CapsuleKind::PshTooShort { psh_len: psh.len() };
    }
    let sqe = &psh[..64];
    let opc = sqe[0];
    let cid = u16::from_le_bytes([sqe[2], sqe[3]]);
    if opc == crate::fabric::NVME_OPC_FABRIC {
        CapsuleKind::Fabric { cid }
    } else if state.current_qid == 0 {
        CapsuleKind::Admin { cid }
    } else {
        CapsuleKind::Io { cid }
    }
}

/// **V6a** AER fast-path 决策。
///
/// AER (opc=0x0C) 在 controller 内只 push `aen_pending` 返 None（不 post CQE），
/// V5 `run_post_dispatch` Phase 4 会 bail "did not produce CQE"。所以 session
/// 端 peek opc 走 fast-path：超 `MAX_PENDING_AERS` → SC=0x05 reject；否则
/// stash + 跳过 run_post_dispatch。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminAerDecision {
    /// 不是 AER opc；caller 应继续正常 admin dispatch path。
    NotAer,
    /// AER cmd 但已超 session-level cap `MAX_PENDING_AERS=4`；reject SC=0x05
    /// ASYNC_LIMIT_EXCEEDED（spec § 5.2 允许）。
    OverCap,
    /// AER cmd 且 cap 内；caller 应走 fast-path（dispatch + stash + skip
    /// run_post_dispatch）。
    FastPath,
}

/// 决策：admin SQE 是否走 AER fast-path。
pub fn decide_admin_aer_path(sqe: &[u8], state: ConnStateSnapshot) -> AdminAerDecision {
    let opc = crate::aer::peek_admin_opc(sqe);
    if opc != ADMIN_OPC_AER {
        return AdminAerDecision::NotAer;
    }
    if state.pending_aers_len >= MAX_PENDING_AERS {
        AdminAerDecision::OverCap
    } else {
        AdminAerDecision::FastPath
    }
}

/// **V7** discovery mode admin opc 白名单决策。
///
/// spec § 5.1.4 Discovery Controller 仅响应有限 admin cmd：
/// - 0x02 GET_LOG_PAGE (含 LID 0x70 Discovery Log)
/// - 0x06 IDENTIFY     (CNS=0x01 Identify Controller → V7 CNTRLTYPE patch)
/// - 0x09 SET_FEATURES (Async Event Config FID=0x0B)
/// - 0x0A GET_FEATURES
/// - 0x0C ASYNC_EVENT_REQUEST
/// - 0x18 KEEP_ALIVE
///
/// 其余 reject SC=0x01 INVALID_OPCODE 防 host 误用 Discovery Ctrl 当 IO Ctrl。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryWhitelistDecision {
    /// 不在 discovery mode；caller 跳过白名单检查。
    NotDiscoveryMode,
    /// 在白名单内；caller 继续正常 dispatch。
    Allowed,
    /// 不在白名单；caller 应 reject SC=0x01。
    Rejected,
}

/// 决策：discovery mode 下 admin opc 是否在白名单。
pub fn decide_admin_discovery_whitelist(
    sqe: &[u8],
    state: ConnStateSnapshot,
) -> DiscoveryWhitelistDecision {
    if !state.discovery_mode {
        return DiscoveryWhitelistDecision::NotDiscoveryMode;
    }
    const DISCOVERY_ALLOWED: &[u8] = &[0x02, 0x06, 0x09, 0x0A, 0x0C, 0x18];
    let opc = crate::aer::peek_admin_opc(sqe);
    if DISCOVERY_ALLOWED.contains(&opc) {
        DiscoveryWhitelistDecision::Allowed
    } else {
        DiscoveryWhitelistDecision::Rejected
    }
}

/// **V5e-1-fix (review H-1)** — 改 NS 形状的 admin opc 黑名单。
///
/// V5 教学版 `V5_NLB_MAX` 单 PRP1 假设直接挂钩 `LBADS=9 + pi_type=0`：一旦
/// host `nvme format --lbaf=1 --pi=1`，controller dual-PRP path 走 prp2=0
/// sentinel → dma_read 静默到非法 gpa → wire 破 / 数据 corruption。V8+ 真支
/// 持多 PRP 后解封。
///
/// 黑名单：
/// - 0x80 FORMAT_NVM            — 改 lbads / pi_type
/// - 0x0D NAMESPACE_MANAGEMENT — 创建/删除 NS
///
/// (0x15 NS_ATTACHMENT 在 sync 注释里提过保守 block，但 sync 实现实际只 block
/// 0x80/0x0D；本决策表与 sync 行为一致以避免漂移。)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminBlockedOpcDecision {
    /// 不在黑名单；caller 继续正常 dispatch。
    Allowed,
    /// 在黑名单；caller 应 reject SC=0x01 INVALID_OPCODE。
    Blocked,
}

/// 决策：admin opc 是否触犯 NS 形状黑名单。
pub fn decide_admin_blocked_opc(sqe: &[u8]) -> AdminBlockedOpcDecision {
    let opc = crate::aer::peek_admin_opc(sqe);
    if matches!(opc, 0x80 | 0x0D) {
        AdminBlockedOpcDecision::Blocked
    } else {
        AdminBlockedOpcDecision::Allowed
    }
}

/// **V5b/V5c/V5e-1/V5e-2** — IO Read/Write NLB 上限决策（`V5_NLB_MAX=16`）。
///
/// LBADS=9：
/// - 单 PRP1 (≤4 KiB) → nlb ≤ 8
/// - 双 PRP (≤8 KiB) → nlb ≤ 16
///
/// 超出 → 拒 SC=0x18 SGL_DATA_LENGTH_INVALID 让 driver 拆分。仅 Read/Write
/// opc (0x01/0x02) 需检；其它 opc caller 跳过。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoNlbDecision {
    /// 不是 IO Read/Write opc；caller 跳过 NLB 检查 + prp2 sentinel。
    NotReadWrite,
    /// NLB 在 [1, V5_NLB_MAX] 范围；caller 继续 dispatch。`nlb_real` 是真实
    /// LBA 块数（cdw12 字段 + 1，0-based → 1-based）。
    Ok {
        /// 真实 LBA 块数（1-based）。
        nlb_real: u32,
    },
    /// NLB 超 V5_NLB_MAX；caller 应 reject SC=0x18 SGL_DATA_LENGTH_INVALID。
    OverMax {
        /// 真实 NLB 值（用于 log）。
        nlb_real: u32,
    },
}

/// 决策：IO SQE 的 NLB 是否在 `V5_NLB_MAX` 上限内。
pub fn decide_io_nlb_check(sqe: &Sqe) -> IoNlbDecision {
    let opc = (sqe.cdw0 & 0xff) as u8;
    if !matches!(opc, 0x01 | 0x02) {
        return IoNlbDecision::NotReadWrite;
    }
    let nlb_real = (sqe.cdw12 & 0xffff) + 1;
    if nlb_real > crate::V5_NLB_MAX {
        IoNlbDecision::OverMax { nlb_real }
    } else {
        IoNlbDecision::Ok { nlb_real }
    }
}

/// **V5e-2** — IO Read/Write 的 prp2 sentinel 决策。
///
/// - nlb ≤ 8 (单 PRP1 ≤ 4 KiB)：prp2 = 0（controller 走单 PRP1 path）
/// - 8 < nlb ≤ 16 (双 PRP ≤ 8 KiB)：prp2 = `PRP2_SENTINEL`，controller 走
///   dual-PRP path 产 2 个 dma_read/dma_write，session 累计处理
pub fn prp2_sentinel_for_nlb(nlb_real: u32) -> u64 {
    if nlb_real > 8 {
        crate::session::PRP2_SENTINEL
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(qid: u16, discovery: bool, pending: usize) -> ConnStateSnapshot {
        ConnStateSnapshot {
            current_qid: qid,
            discovery_mode: discovery,
            pending_aers_len: pending,
        }
    }

    fn admin_sqe_with_opc(opc: u8) -> [u8; 64] {
        let mut sqe = [0u8; 64];
        sqe[0] = opc;
        sqe[2..4].copy_from_slice(&0x1234u16.to_le_bytes());
        sqe
    }

    #[test]
    fn v8e7_1_decide_capsule_kind_too_short_psh() {
        let r = decide_capsule_kind(&[0u8; 32], snap(0, false, 0));
        assert!(matches!(r, CapsuleKind::PshTooShort { psh_len: 32 }));
    }

    #[test]
    fn v8e7_1_decide_capsule_kind_fabric() {
        let sqe = admin_sqe_with_opc(crate::fabric::NVME_OPC_FABRIC);
        let r = decide_capsule_kind(&sqe, snap(0, false, 0));
        assert!(matches!(r, CapsuleKind::Fabric { cid: 0x1234 }));
    }

    #[test]
    fn v8e7_1_decide_capsule_kind_admin_when_qid0() {
        let sqe = admin_sqe_with_opc(0x06); // Identify
        let r = decide_capsule_kind(&sqe, snap(0, false, 0));
        assert!(matches!(r, CapsuleKind::Admin { cid: 0x1234 }));
    }

    #[test]
    fn v8e7_1_decide_capsule_kind_io_when_qid_nonzero() {
        let sqe = admin_sqe_with_opc(0x02); // IO Read
        let r = decide_capsule_kind(&sqe, snap(1, false, 0));
        assert!(matches!(r, CapsuleKind::Io { cid: 0x1234 }));
    }

    #[test]
    fn v8e7_1_decide_admin_aer_path_not_aer() {
        let sqe = admin_sqe_with_opc(0x06);
        assert_eq!(
            decide_admin_aer_path(&sqe, snap(0, false, 0)),
            AdminAerDecision::NotAer
        );
    }

    #[test]
    fn v8e7_1_decide_admin_aer_path_under_cap_fast_path() {
        let sqe = admin_sqe_with_opc(ADMIN_OPC_AER);
        assert_eq!(
            decide_admin_aer_path(&sqe, snap(0, false, 3)), // 3 < 4
            AdminAerDecision::FastPath
        );
    }

    #[test]
    fn v8e7_1_decide_admin_aer_path_at_cap_over() {
        let sqe = admin_sqe_with_opc(ADMIN_OPC_AER);
        assert_eq!(
            decide_admin_aer_path(&sqe, snap(0, false, MAX_PENDING_AERS)),
            AdminAerDecision::OverCap
        );
    }

    #[test]
    fn v8e7_1_decide_admin_discovery_whitelist_not_mode() {
        let sqe = admin_sqe_with_opc(0x80); // FORMAT_NVM
        assert_eq!(
            decide_admin_discovery_whitelist(&sqe, snap(0, false, 0)),
            DiscoveryWhitelistDecision::NotDiscoveryMode
        );
    }

    #[test]
    fn v8e7_1_decide_admin_discovery_whitelist_allowed() {
        for opc in [0x02u8, 0x06, 0x09, 0x0A, 0x0C, 0x18] {
            let sqe = admin_sqe_with_opc(opc);
            assert_eq!(
                decide_admin_discovery_whitelist(&sqe, snap(0, true, 0)),
                DiscoveryWhitelistDecision::Allowed,
                "opc={opc:#x} 应在 discovery 白名单",
            );
        }
    }

    #[test]
    fn v8e7_1_decide_admin_discovery_whitelist_rejected() {
        for opc in [0x01u8, 0x80, 0x0D, 0x05] {
            let sqe = admin_sqe_with_opc(opc);
            assert_eq!(
                decide_admin_discovery_whitelist(&sqe, snap(0, true, 0)),
                DiscoveryWhitelistDecision::Rejected,
                "opc={opc:#x} 应被 discovery 拒",
            );
        }
    }

    #[test]
    fn v8e7_1_decide_admin_blocked_opc_format_nvm() {
        let sqe = admin_sqe_with_opc(0x80);
        assert_eq!(
            decide_admin_blocked_opc(&sqe),
            AdminBlockedOpcDecision::Blocked
        );
    }

    #[test]
    fn v8e7_1_decide_admin_blocked_opc_ns_management() {
        let sqe = admin_sqe_with_opc(0x0D);
        assert_eq!(
            decide_admin_blocked_opc(&sqe),
            AdminBlockedOpcDecision::Blocked
        );
    }

    #[test]
    fn v8e7_1_decide_admin_blocked_opc_others_allowed() {
        for opc in [0x06u8, 0x02, 0x09, 0x18, 0x0C, 0x05, 0x01] {
            let sqe = admin_sqe_with_opc(opc);
            assert_eq!(
                decide_admin_blocked_opc(&sqe),
                AdminBlockedOpcDecision::Allowed,
                "opc={opc:#x} 不应在黑名单",
            );
        }
    }

    fn io_sqe(opc: u8, nlb_zero_based: u32) -> Sqe {
        use zerocopy::FromZeros as _;
        let mut sqe = Sqe::new_zeroed();
        sqe.cdw0 = opc as u32;
        sqe.cdw12 = nlb_zero_based;
        sqe
    }

    #[test]
    fn v8e7_1_decide_io_nlb_not_read_write() {
        let sqe = io_sqe(0x05, 100); // Create IO CQ
        assert_eq!(decide_io_nlb_check(&sqe), IoNlbDecision::NotReadWrite);
    }

    #[test]
    fn v8e7_1_decide_io_nlb_ok_at_max() {
        let sqe = io_sqe(0x02, crate::V5_NLB_MAX - 1); // nlb_real = MAX
        assert_eq!(
            decide_io_nlb_check(&sqe),
            IoNlbDecision::Ok {
                nlb_real: crate::V5_NLB_MAX
            }
        );
    }

    #[test]
    fn v8e7_1_decide_io_nlb_over_max() {
        let sqe = io_sqe(0x01, crate::V5_NLB_MAX); // nlb_real = MAX+1
        assert_eq!(
            decide_io_nlb_check(&sqe),
            IoNlbDecision::OverMax {
                nlb_real: crate::V5_NLB_MAX + 1
            }
        );
    }

    #[test]
    fn v8e7_1_prp2_sentinel_single_prp_when_nlb_le_8() {
        assert_eq!(prp2_sentinel_for_nlb(1), 0);
        assert_eq!(prp2_sentinel_for_nlb(8), 0);
    }

    #[test]
    fn v8e7_1_prp2_sentinel_dual_prp_when_nlb_over_8() {
        assert_eq!(prp2_sentinel_for_nlb(9), crate::session::PRP2_SENTINEL);
        assert_eq!(prp2_sentinel_for_nlb(16), crate::session::PRP2_SENTINEL);
    }
}
