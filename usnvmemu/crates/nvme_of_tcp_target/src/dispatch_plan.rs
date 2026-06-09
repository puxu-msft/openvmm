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
use nvme_firmware::cmd::Sqe;

/// NVMe PRP 页大小（教学固定 4 KiB；MPSMIN=0）。纯-4K 扇区感知按此算页边界。
pub const NVME_PRP_PAGE_BYTES: u32 = 4096;

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
/// 历史原因：V5 教学版 session 合成 PRP 写死 `LBADS=9`（单 PRP1 ≤ 4 KiB /
/// dual-PRP ≤ 8 KiB 的页边界按 512B 算）。一旦 host `nvme format` 改 lbads，
/// session 的 prp2 sentinel 阈值与 chunk 大小就与真实扇区不符 → 撕裂
/// dual-PRP 边界 → 数据 corruption，故默认 block 0x80/0x0D。
///
/// **2026-06-09 纯 4K** — session 现已**扇区感知**（[`dual_prp_max_lbas`] /
/// [`prp2_sentinel_for_nlb`] 按 `ns_lbads` 算），Format 改 lbads 后下条 IO
/// 即读到新扇区。故 **0x80 FORMAT_NVM 可经 `--allow-format` 显式 opt-in 放行**
/// （firmware Format handler 自身 gate in-flight IO + lbafl≤2）。
///
/// 0x0D NAMESPACE_MANAGEMENT 仍恒 block：创建 NS 会引入 session 未追踪的
/// 新 NS，且教学 target NS 集合固定，无放行需求。
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

/// 决策：admin opc 是否触犯 NS 形状黑名单。`allow_format=true` 时显式放行
/// 0x80 FORMAT_NVM（用户经 CLI `--allow-format` opt-in）；0x0D 恒 block。
pub fn decide_admin_blocked_opc(sqe: &[u8], allow_format: bool) -> AdminBlockedOpcDecision {
    let opc = crate::aer::peek_admin_opc(sqe);
    match opc {
        0x80 if allow_format => AdminBlockedOpcDecision::Allowed,
        0x80 | 0x0D => AdminBlockedOpcDecision::Blocked,
        _ => AdminBlockedOpcDecision::Allowed,
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

/// 决策：IO SQE 的 NLB 是否在单次 dispatch（dual-PRP）能覆盖的上限内。
/// **2026-06-09 纯 4K** — 上限按 per-NS 扇区算 = [`dual_prp_max_lbas`]
/// （512B→16、4K→2），而非旧的固定 `V5_NLB_MAX`。
pub fn decide_io_nlb_check(sqe: &Sqe, lbads: u8) -> IoNlbDecision {
    let opc = (sqe.cdw0 & 0xff) as u8;
    if !matches!(opc, 0x01 | 0x02) {
        return IoNlbDecision::NotReadWrite;
    }
    let nlb_real = (sqe.cdw12 & 0xffff) + 1;
    if nlb_real > dual_prp_max_lbas(lbads) {
        IoNlbDecision::OverMax { nlb_real }
    } else {
        IoNlbDecision::Ok { nlb_real }
    }
}

/// **2026-06-09 纯 4K** — 一个 4 KiB PRP 页能装多少 LBA（512B→8、4K→1）。
/// 仅支持 lbads∈{9,12}；`.max(1)` 防御性兜底（lbads≥12 时至少 1）。
pub fn page_lbas(lbads: u8) -> u32 {
    (NVME_PRP_PAGE_BYTES >> lbads).max(1)
}

/// 单次 dispatch（controller dual-PRP path，≤ 2 页 = 8 KiB）能覆盖的最大
/// LBA 数：512B→16、4K→2。同时是 session chunking 的 chunk 大小与触发阈值。
pub fn dual_prp_max_lbas(lbads: u8) -> u32 {
    (2 * NVME_PRP_PAGE_BYTES) >> lbads
}

/// host 单条 IO 的 MDTS LBA 上限（按字节预算 = `V_HOST_IO_NLB_MAX`×512B =
/// 128 KiB，再 ÷ 扇区）：512B→256、4K→32。超出 → 拒 SC=0x18。
pub fn host_io_max_lbas(lbads: u8) -> u32 {
    (crate::V_HOST_IO_NLB_MAX * 512) >> lbads
}

/// **V5e-2** — IO Read/Write 的 prp2 sentinel 决策。
///
/// **2026-06-09 纯 4K** — 阈值按 per-NS 页边界（[`page_lbas`]）：
/// - nlb ≤ page_lbas（≤ 1 页）：prp2 = 0（controller 走单 PRP1 path）
/// - page_lbas < nlb ≤ 2×page_lbas（≤ 2 页）：prp2 = `PRP2_SENTINEL`，
///   controller 走 dual-PRP path 产 2 个 dma_read/dma_write
///
/// 512B：page_lbas=8 → nlb>8 才 dual；4K：page_lbas=1 → nlb≥2 即 dual。
pub fn prp2_sentinel_for_nlb(nlb_real: u32, lbads: u8) -> u64 {
    if nlb_real > page_lbas(lbads) {
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
        // 默认（allow_format=false）→ block
        assert_eq!(
            decide_admin_blocked_opc(&sqe, false),
            AdminBlockedOpcDecision::Blocked
        );
        // **2026-06-09 纯 4K** — opt-in 后放行 Format
        assert_eq!(
            decide_admin_blocked_opc(&sqe, true),
            AdminBlockedOpcDecision::Allowed
        );
    }

    #[test]
    fn v8e7_1_decide_admin_blocked_opc_ns_management() {
        let sqe = admin_sqe_with_opc(0x0D);
        // NS Management 恒 block，即便 allow_format=true
        assert_eq!(
            decide_admin_blocked_opc(&sqe, false),
            AdminBlockedOpcDecision::Blocked
        );
        assert_eq!(
            decide_admin_blocked_opc(&sqe, true),
            AdminBlockedOpcDecision::Blocked
        );
    }

    #[test]
    fn v8e7_1_decide_admin_blocked_opc_others_allowed() {
        for opc in [0x06u8, 0x02, 0x09, 0x18, 0x0C, 0x05, 0x01] {
            let sqe = admin_sqe_with_opc(opc);
            assert_eq!(
                decide_admin_blocked_opc(&sqe, false),
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
        assert_eq!(decide_io_nlb_check(&sqe, 9), IoNlbDecision::NotReadWrite);
    }

    #[test]
    fn v8e7_1_decide_io_nlb_ok_at_max() {
        let sqe = io_sqe(0x02, crate::V5_NLB_MAX - 1); // nlb_real = MAX (512B)
        assert_eq!(
            decide_io_nlb_check(&sqe, 9),
            IoNlbDecision::Ok {
                nlb_real: crate::V5_NLB_MAX
            }
        );
    }

    #[test]
    fn v8e7_1_decide_io_nlb_over_max() {
        let sqe = io_sqe(0x01, crate::V5_NLB_MAX); // nlb_real = MAX+1 (512B)
        assert_eq!(
            decide_io_nlb_check(&sqe, 9),
            IoNlbDecision::OverMax {
                nlb_real: crate::V5_NLB_MAX + 1
            }
        );
    }

    /// **2026-06-09 纯 4K** — 4K NS（lbads=12）单次 dispatch 上限 = 2 LBA。
    #[test]
    fn pure_4k_decide_io_nlb_cap_is_2() {
        assert_eq!(dual_prp_max_lbas(12), 2);
        assert_eq!(dual_prp_max_lbas(9), 16);
        let ok = io_sqe(0x02, 1); // nlb_real = 2
        assert_eq!(
            decide_io_nlb_check(&ok, 12),
            IoNlbDecision::Ok { nlb_real: 2 }
        );
        let over = io_sqe(0x01, 2); // nlb_real = 3 > 2
        assert_eq!(
            decide_io_nlb_check(&over, 12),
            IoNlbDecision::OverMax { nlb_real: 3 }
        );
    }

    #[test]
    fn v8e7_1_prp2_sentinel_single_prp_when_nlb_le_8() {
        // 512B：≤ 8 LBA = ≤ 1 页 → 单 PRP
        assert_eq!(prp2_sentinel_for_nlb(1, 9), 0);
        assert_eq!(prp2_sentinel_for_nlb(8, 9), 0);
    }

    #[test]
    fn v8e7_1_prp2_sentinel_dual_prp_when_nlb_over_8() {
        // 512B：> 8 LBA = > 1 页 → dual PRP
        assert_eq!(prp2_sentinel_for_nlb(9, 9), crate::session::PRP2_SENTINEL);
        assert_eq!(prp2_sentinel_for_nlb(16, 9), crate::session::PRP2_SENTINEL);
    }

    /// **2026-06-09 纯 4K** — 4K NS：1 LBA = 1 页 → 单 PRP；2 LBA = 2 页 → dual。
    #[test]
    fn pure_4k_prp2_sentinel_threshold_is_1_lba() {
        assert_eq!(page_lbas(12), 1);
        assert_eq!(page_lbas(9), 8);
        assert_eq!(prp2_sentinel_for_nlb(1, 12), 0); // 1 页 → 单 PRP
        assert_eq!(
            prp2_sentinel_for_nlb(2, 12),
            crate::session::PRP2_SENTINEL // 2 页 → dual
        );
        // MDTS host 上限：4K → 32 LBA (128 KiB)，512B → 256。
        assert_eq!(host_io_max_lbas(12), 32);
        assert_eq!(host_io_max_lbas(9), 256);
    }
}
