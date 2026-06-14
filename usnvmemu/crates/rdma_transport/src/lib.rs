// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `rdma_transport` —— NVMe-oF RDMA transport 的 **verbs 抽象层**。
//!
//! V9 RDMA track 的 R1' 阶段产出（详 plan
//! `nvme_of_tcp_target/docs/plans/2026-06-14-phase-v9-rdma-detailed.md`）：
//!
//! - [`RdmaVerbs`] —— 一个 QP 的 ibverbs/rdma 操作面（reg_mr / post_recv / post_send /
//!   post_rdma_read / post_rdma_write / poll_cq）。**同步 poll 模型**，异步包装留
//!   `FabricBackend` 层（R2）。
//! - [`MockRdma`] —— 进程内假实现，**忠实模拟** wire/verbs 契约 §D 语义（completion 保序 /
//!   QP-error→flush→重建 / RNR / ORD 限深），让 RDMA target 逻辑在无内核 RDMA 设备下全测。
//!
//! 未来 `IbverbsRdma`（R5a，sideway/rdma-sys backing，FFI unsafe）impl 同一 `RdmaVerbs`，
//! 是 `MockRdma` 的 drop-in 替换。语义契约权威源见
//! `nvme_of_tcp_target/docs/specs/2026-06-14-nvme-rdma-wire-and-verbs-contract.md`。

mod mock;
mod verbs;

pub use mock::MockRdma;
pub use verbs::{
    Access, Completion, MrHandle, Opcode, QpState, RdmaError, RdmaVerbs, Rkey, WcError, WcStatus,
    WrId,
};
