// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `trait RdmaVerbs` + 支撑类型 —— NVMe-oF RDMA target 用到的 ibverbs/rdma 操作的
//! 薄抽象层。**语义契约**见
//! `nvme_of_tcp_target/docs/specs/2026-06-14-nvme-rdma-wire-and-verbs-contract.md` §D
//! （RC QP completion 保序 / QP-error-state+flush+重建 / RNR / IRD-ORD / per-QP CQ）。
//!
//! 设计原则（R0 §G）：
//! - **同步 poll 模型**（`poll_cq` 返回本 QP 完成项）；异步包装留 `FabricBackend` 层
//!   （R2），不在 verbs 层假设 runtime。
//! - **一个 `RdmaVerbs` 实例 = 一个 QP 的操作面**（per-QP，对齐 R0 §F session 模型）。
//! - 两个 impl：`MockRdma`（本 crate，进程内假实现，模拟真语义）+ 未来 `IbverbsRdma`
//!   （R5a，sideway/rdma-sys backing，FFI unsafe）。

use thiserror::Error;

/// work request id —— caller 给每个 post 的标识，completion 原样带回（关联回命令/fragment
/// token，对齐桥的 token-FIFO 机制）。
pub type WrId = u64;

/// 远端内存保护键（rkey）。host 在 keyed-SGL 里给（wire reference §B.2，le32）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rkey(pub u32);

/// 本地已注册内存区句柄（`reg_mr` 返回）。不透明，仅作引用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MrHandle(pub(crate) u64);

/// MR 访问权限（对应 ibverbs `IBV_ACCESS_*` 标志）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    /// 本地写（recv / RDMA READ 落地缓冲需要）。
    pub local_write: bool,
    /// 允许远端 RDMA READ 本 MR。
    pub remote_read: bool,
    /// 允许远端 RDMA WRITE 本 MR。
    pub remote_write: bool,
}

impl Access {
    /// 仅本地写（recv 缓冲 / RDMA READ 目标缓冲；target 侧本地 MR 常用）。
    pub const fn local_write() -> Self {
        Self {
            local_write: true,
            remote_read: false,
            remote_write: false,
        }
    }
    /// 只读本地（SEND 源缓冲；无需 local_write）。
    pub const fn local_read() -> Self {
        Self {
            local_write: false,
            remote_read: false,
            remote_write: false,
        }
    }
}

/// QP 状态机（RC QP，简化为契约关心的两态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QpState {
    /// 可正常 post（INIT→RTR→RTS 后的就绪态）。
    Ready,
    /// 进入 `IB_QPS_ERR`：任何 in-flight WR 以 `WrFlushError` 完成，新 post 被拒，
    /// **必须重建 QP**（wire reference §D.2）。
    Error,
}

/// completion 的 work 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    /// 本端发起的 RDMA READ（拉 host 写命令数据）完成。
    RdmaRead,
    /// 本端发起的 RDMA WRITE（推读命令数据到 host）完成。
    RdmaWrite,
    /// 本端 SEND（响应胶囊）完成。
    Send,
    /// 收到对端 SEND（命令胶囊）—— 消耗一个 posted recv buffer。
    Recv,
}

/// `IBV_WC_*` 错误码（契约 §D.2 关心的子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WcError {
    /// RNR 重试耗尽（对端无 recv buffer 接 SEND）。
    RnrRetryExceeded,
    /// 远端访问错（rkey 无效 / 越界 / 权限不符）。
    RemoteAccessError,
    /// QP 进 error 后，先前 post 的 in-flight WR 被 flush。
    WrFlushError,
    /// 本地保护错（本地 MR 越界 / 权限）。
    LocalProtectionError,
}

/// completion 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WcStatus {
    /// 成功。
    Success,
    /// 失败 + 错误码。
    Error(WcError),
}

/// 一条 work completion（`poll_cq` 返回）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
    /// 对应 post 时的 `wr_id`。
    pub wr_id: WrId,
    /// work 类型。
    pub opcode: Opcode,
    /// 成功 / 失败。
    pub status: WcStatus,
    /// 字节数：`Recv` = 收到的胶囊字节数；其它 opcode = 0。
    pub byte_len: u32,
    /// `Recv` 且对端用 SEND_WITH_INV 时携带的失效 rkey（wire reference §B.4）；否则 None。
    pub invalidated_rkey: Option<Rkey>,
}

/// verbs 操作错误（同步返回，区别于异步 completion 里的 `WcError`）。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RdmaError {
    /// QP 在 error 态，post 被拒（须先 `recreate_qp`）。
    #[error("QP in error state; must recreate before posting")]
    QpInError,
    /// MR 句柄无效。
    #[error("invalid MR handle {0:?}")]
    BadMr(MrHandle),
    /// 超出 ORD（outgoing RDMA READ depth）—— in-flight READ 太多（§D.4）。
    #[error("outstanding RDMA READ depth exceeded (ord_max={max})")]
    OrdExceeded {
        /// 协商的 ORD 上限。
        max: u32,
    },
    /// MR 偏移/长度越界。
    #[error("MR access out of bounds (off={off}, len={len}, mr_len={mr_len})")]
    OutOfBounds {
        /// 请求偏移。
        off: u32,
        /// 请求长度。
        len: u32,
        /// MR 实际长度。
        mr_len: u32,
    },
}

/// 一个 QP 的 verbs 操作面（per-QP，R0 §F）。所有 post 异步，完成经 `poll_cq` 取回。
///
/// 实现须遵守 wire reference §D 语义契约：completion 保序、QP-error→flush→重建、
/// ORD 限深。`MockRdma` 模拟这些（**非乐观 FIFO-永不失败**，否则 R5 真 verbs 一接全改，
/// 项目 review-not-optional-self-consistent-trap 教训）。
pub trait RdmaVerbs {
    /// 注册本地内存区。`bytes` 的所有权交给 verbs 层（mock 直接持有；真 ibverbs 会 pin +
    /// 建 NIC 翻译表）。返回句柄。
    fn reg_mr(&mut self, bytes: Vec<u8>, access: Access) -> Result<MrHandle, RdmaError>;

    /// 借读已注册 MR 的字节（零拷贝：RDMA READ 落地后桥直读，对齐 R0 §3.2「借 MR」）。
    fn mr_bytes(&self, mr: MrHandle) -> Option<&[u8]>;

    /// 借写已注册 MR（填 SEND 源 / 校验 RDMA READ 结果）。
    fn mr_bytes_mut(&mut self, mr: MrHandle) -> Option<&mut [u8]>;

    /// post 一个 recv buffer（接对端 SEND 命令胶囊）。`wr_id` 在 `Recv` completion 带回。
    fn post_recv(&mut self, mr: MrHandle, wr_id: WrId) -> Result<(), RdmaError>;

    /// post SEND（响应胶囊，源 = `mr` 前 `len` 字节）。`invalidate=Some(rkey)` 走
    /// SEND_WITH_INV 帮 host 远程失效（§B.4）。
    fn post_send(
        &mut self,
        mr: MrHandle,
        len: u32,
        wr_id: WrId,
        invalidate: Option<Rkey>,
    ) -> Result<(), RdmaError>;

    /// post RDMA READ：从远端 `(remote_addr, rkey)` 读 `len` 字节落到本地 `local[local_off..]`
    /// （host 写命令取数据）。受 ORD 限深。
    fn post_rdma_read(
        &mut self,
        local: MrHandle,
        local_off: u32,
        remote_addr: u64,
        rkey: Rkey,
        len: u32,
        wr_id: WrId,
    ) -> Result<(), RdmaError>;

    /// post RDMA WRITE：把本地 `local[local_off..local_off+len]` 写到远端 `(remote_addr, rkey)`
    /// （host 读命令送数据）。
    fn post_rdma_write(
        &mut self,
        local: MrHandle,
        local_off: u32,
        remote_addr: u64,
        rkey: Rkey,
        len: u32,
        wr_id: WrId,
    ) -> Result<(), RdmaError>;

    /// poll 本 QP 的 CQ。返回**按提交顺序**的 completion（§D.1 RC QP 保序）。无完成则空 Vec。
    fn poll_cq(&mut self) -> Vec<Completion>;

    /// 当前 QP 状态。
    fn qp_state(&self) -> QpState;
}
