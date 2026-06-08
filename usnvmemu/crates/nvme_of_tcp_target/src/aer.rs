// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V6a** — session 端 AER (Async Event Request) 状态记账。
//!
//! Spec NVMe Base 2.0c § 5.2 行为：
//! - host 发 AER (opc=0x0C) cmd；controller 把 (cid, sq_id, cq_id) 入 `aen_pending`
//!   队列，dispatch_admin 返 `None`（**不**立即 post CQE）
//! - 之后 controller 内任意时刻发生 async event (SMART threshold / Self-Test 完
//!   / Sanitize 完 / 错误计数涨) 调 `fire_aen(type, info, log_id)`，从
//!   `aen_pending` pop 一条并 post_cqe，dw0 编码事件类型 / info / log id
//! - 多事件超 pending AER 数 (AERL+1=4) → controller `fire_aen` 返 false → 事件
//!   按 spec drop
//!
//! V5 session 在 `run_post_dispatch` 看 dispatch 返 None 且无 captured writes →
//! `bail!("did not produce CQE")` ⇒ 整条 TCP 断 ⇒ 致命 regression。
//!
//! V6a fast-path：handle_admin_cmd 入口 peek opc=0x0C → 把 cmd 委派 controller
//! `aen_pending` 累积，session 镜像到 `pending_aers` 用于上限保护 + debug；
//! **跳过** `run_post_dispatch`，直接返 Ok 让 pump_one 继续。
//!
//! V6b 后会有 `pump_one_with_events` 通过 controller `nvme_drain_aer_completions`
//! 把累积的 AEN CQE 真正发到 wire。

/// admin opcode for Async Event Request (spec § 5.2)
///
/// **V6c-polish (review M-1)** — re-export controller `admin_opc` 模块的
/// 同名常量，避免双源 magic number 漂移。
pub use pcie_remote_nvme_userspace::cmd::admin_opc::ASYNC_EVENT_REQUEST as ADMIN_OPC_AER;

/// session 镜像可同时持有的最多 pending AER 数。
/// 与 controller `aen_pending` 容量协同；超过此值 session 直接返
/// SC=0x05 `ASYNC_LIMIT_EXCEEDED`（spec § 5.2 允许 controller 主动拒）。
///
/// 值 = AERL (Asynchronous Event Request Limit, Identify Controller bits 16:24
/// = 3 表示 0-based → 实际 AERL+1=4 个 outstanding)。
pub const MAX_PENDING_AERS: usize = 4;

/// 从 64-byte SQE peek opcode (cdw0 bits 7:0)。
pub fn peek_admin_opc(sqe: &[u8]) -> u8 {
    // SQE byte 0 = cdw0 lo byte = opcode
    sqe.first().copied().unwrap_or(0)
}

/// session 镜像一条 pending AER 的状态。仅用于 cap + debug；source of truth
/// 仍在 controller `aen_pending`。
#[derive(Debug, Clone, Copy)]
pub struct PendingAer {
    /// host 提交时的 CID。
    pub cid: u16,
    /// 始终 0（spec 强制 AER 走 admin SQ）；保留字段方便 V8 multi-queue。
    pub sq_id: u16,
    /// host post 的时刻（debug 用）。
    pub registered_at: std::time::Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peek_opcode_returns_first_byte() {
        let mut sqe = [0u8; 64];
        sqe[0] = ADMIN_OPC_AER;
        assert_eq!(peek_admin_opc(&sqe), 0x0C);
    }

    #[test]
    fn peek_opcode_handles_short_buf() {
        assert_eq!(peek_admin_opc(&[]), 0);
    }

    #[test]
    fn max_pending_aers_matches_spec_aerl() {
        // spec § 5.16.1.1 AERL field is 0-based → 3 means 4 outstanding
        assert_eq!(MAX_PENDING_AERS, 4);
    }

    /// **V6c-polish (review M-4)** — 锁死 controller Identify Controller
    /// 默认 AERL=3（spec § 5.16.1.1 0-based → 4 outstanding）与 session
    /// `MAX_PENDING_AERS=4` 一致。controller 默认 AERL 漂移立刻被该测试捕获。
    #[test]
    fn max_pending_aers_aligns_with_controller_identify_aerl() {
        use pcie_remote_nvme_userspace::cmd::IdentifyController;
        // Identify Controller bytes 259 = AERL field (spec § 5.15.2.1 Figure 312)
        let id = IdentifyController::build_v2_bytes(0x1414, 0, 1);
        let aerl_zero_based = id[259];
        let aerl_outstanding = aerl_zero_based as usize + 1;
        assert_eq!(
            aerl_outstanding, MAX_PENDING_AERS,
            "controller Identify AERL+1 ({aerl_outstanding}) 必须等于 session \
             MAX_PENDING_AERS ({MAX_PENDING_AERS})；漂移会让 host 看到与实际不符的容量"
        );
    }
}
