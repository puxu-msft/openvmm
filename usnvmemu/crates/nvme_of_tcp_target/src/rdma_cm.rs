// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **V9 RDMA R3b** — NVMe-oF RDMA transport 的 **RDMA-CM Private Data** wire 结构。
//!
//! 连接建立时（host `rdma_connect` / target `rdma_accept`），协商参数走 CM Private Data
//! （≤56B，非 NVMe capsule）。这是 RDMA transport 的「ICReq/ICResp 等价物」——TCP 用 PDU 握手，
//! RDMA 用这套 CM 结构。字段级布局**权威源** = R0 契约
//! [`docs/specs/2026-06-14-nvme-rdma-wire-and-verbs-contract.md`](../docs/specs/2026-06-14-nvme-rdma-wire-and-verbs-contract.md)
//! §A（已对 Linux `include/linux/nvme-rdma.h` 核实）。
//!
//! 与 [`crate::fabric::ConnectData`] 分工：CM Private Data = **transport 层**连接（带 qid/cntlid
//! 把 QP 绑到 association）；Fabric Connect capsule（`ConnectData`，admin 命令 over QP）= **NVMe 层**
//! 认证（HOSTNQN/SUBNQN），RDMA 复用同一份解码。
//!
//! **字节序铁律**：全 little-endian；`#[repr(C, packed)]` + plain `u16`（同 `ConnectData` 约定，LE host）；
//! `offset_of!` 锚定，禁手算 offset（LESSONS §17）。

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// CM Private Data record format —— 目前唯一合法值（R0 §A.1）。
pub const NVME_RDMA_CM_FMT_1_0: u16 = 0x0000;

/// NVMe-oF RDMA 默认端口（R0 §A.3）。
pub const NVME_RDMA_IP_PORT: u16 = 4420;
/// 队列深度上限（R0 §A.3）。
pub const NVME_RDMA_MAX_QUEUE_SIZE: u16 = 256;
/// 默认队列深度。
pub const NVME_RDMA_DEFAULT_QUEUE_SIZE: u16 = 128;
/// admin queue 深度（host admin connect 填 `hrqsize=NVME_AQ_DEPTH`，`hsqsize=NVME_AQ_DEPTH-1`）。
pub const NVME_AQ_DEPTH: u16 = 32;

/// `enum nvme_rdma_cm_status`（**spec 强制全集** 0x01..=0x09，R0 §A.3）。
/// emulator 作 host 解析端须认全集；作 target 发送端可选严格度（Linux target 的 CM connect 校验
/// 实发 `INVALID_LEN`/`INVALID_RECFMT`/`INVALID_HSQSIZE`，`NO_RSC` 在别处 QP 分配失败时才发）。
pub mod cm_status {
    /// private data 长度错。
    pub const INVALID_LEN: u16 = 0x01;
    /// recfmt 不识别。
    pub const INVALID_RECFMT: u16 = 0x02;
    /// qid 无效。
    pub const INVALID_QID: u16 = 0x03;
    /// hsqsize 越界。
    pub const INVALID_HSQSIZE: u16 = 0x04;
    /// hrqsize 越界。
    pub const INVALID_HRQSIZE: u16 = 0x05;
    /// 资源不足。
    pub const NO_RSC: u16 = 0x06;
    /// IRD 协商失败。
    pub const INVALID_IRD: u16 = 0x07;
    /// ORD 协商失败。
    pub const INVALID_ORD: u16 = 0x08;
    /// cntlid 无效。
    pub const INVALID_CNTLID: u16 = 0x09;
}

/// `struct nvme_rdma_cm_req`（host→target，CONNECT_REQUEST private_data，**32B**，R0 §A.1）。
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
pub struct NvmeRdmaCmReq {
    /// record format，必须 `NVME_RDMA_CM_FMT_1_0`。
    pub recfmt: u16,
    /// queue id；0=admin，≥1=IO。
    pub qid: u16,
    /// host RQ size（**1-based** = 条目数）。
    pub hrqsize: u16,
    /// host SQ size（**0-based** = 条目数−1，即 sqsize）。
    pub hsqsize: u16,
    /// controller id；admin 填 `0xFFFF`/由 Connect 决定，**IO queue 填已分配 cntlid**。
    pub cntlid: u16,
    /// 保留，置 0。
    pub rsvd: [u8; 22],
}

/// `struct nvme_rdma_cm_rep`（target→host，accept private_data，**32B**，R0 §A.2）。
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
pub struct NvmeRdmaCmRep {
    /// record format，`NVME_RDMA_CM_FMT_1_0`。
    pub recfmt: u16,
    /// controller RQ size（target 接受深度，**1-based**）。
    pub crqsize: u16,
    /// 保留，置 0。
    pub rsvd: [u8; 28],
}

/// `struct nvme_rdma_cm_rej`（target→host，reject private_data，**4B**，R0 §A.3）。
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
pub struct NvmeRdmaCmRej {
    /// record format，`NVME_RDMA_CM_FMT_1_0`。
    pub recfmt: u16,
    /// reject status（[`cm_status`]）。
    pub sts: u16,
}

/// CM Connect 解析错误（target 据此回 `NvmeRdmaCmRej`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CmReqError {
    /// private data 长度不足 32B。
    #[error("CM private data too short ({0}B < 32)")]
    TooShort(usize),
    /// recfmt 非 1.0。
    #[error("CM recfmt {0:#x} unsupported (only NVME_RDMA_CM_FMT_1_0)")]
    InvalidRecfmt(u16),
    /// admin queue 的 recv_queue_size 超 NVME_AQ_DEPTH。
    #[error("admin queue recv_queue_size {0} exceeds NVME_AQ_DEPTH ({1})")]
    AdminQueueTooDeep(u16, u16),
}

impl CmReqError {
    /// 映射到 [`cm_status`] reject 码（target 发 [`NvmeRdmaCmRej`] 用）。
    pub fn cm_status(self) -> u16 {
        match self {
            CmReqError::TooShort(_) => cm_status::INVALID_LEN,
            CmReqError::InvalidRecfmt(_) => cm_status::INVALID_RECFMT,
            CmReqError::AdminQueueTooDeep(..) => cm_status::INVALID_HSQSIZE,
        }
    }
}

impl NvmeRdmaCmReq {
    /// 解析 + 校验入站 CM Private Data（R0 §A.4 target 校验规则）。
    /// 校验 recfmt + admin 深度；返回解析后的请求。**off-by-one**：调用方用 [`Self::recv_queue_size`]
    /// （= hsqsize+1）/ [`Self::send_queue_size`]（= hrqsize），别直接拿 hsqsize 当深度。
    pub fn parse(private_data: &[u8]) -> Result<Self, CmReqError> {
        if private_data.len() < core::mem::size_of::<Self>() {
            return Err(CmReqError::TooShort(private_data.len()));
        }
        let req = Self::read_from_prefix(private_data)
            .expect("len checked above")
            .0;
        if req.recfmt != NVME_RDMA_CM_FMT_1_0 {
            return Err(CmReqError::InvalidRecfmt(req.recfmt));
        }
        // admin queue（qid==0）：recv_queue_size 不得超 NVME_AQ_DEPTH。
        if req.qid == 0 && req.recv_queue_size() > NVME_AQ_DEPTH {
            return Err(CmReqError::AdminQueueTooDeep(
                req.recv_queue_size(),
                NVME_AQ_DEPTH,
            ));
        }
        Ok(req)
    }

    /// target 视角的接收队列深度（**1-based** = `hsqsize + 1`，R0 §A.4 off-by-one）。
    pub fn recv_queue_size(&self) -> u16 {
        self.hsqsize.saturating_add(1)
    }

    /// target 视角的发送队列深度（已 1-based = `hrqsize`）。
    pub fn send_queue_size(&self) -> u16 {
        self.hrqsize
    }

    /// 构造 accept 回复：`crqsize = recv_queue_size`（1-based，R0 §A.4）。
    pub fn accept_reply(&self) -> NvmeRdmaCmRep {
        NvmeRdmaCmRep {
            recfmt: NVME_RDMA_CM_FMT_1_0,
            crqsize: self.recv_queue_size(),
            rsvd: [0u8; 28],
        }
    }
}

impl NvmeRdmaCmRej {
    /// 构造 reject 回复。
    pub fn new(sts: u16) -> Self {
        Self {
            recfmt: NVME_RDMA_CM_FMT_1_0,
            sts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    /// 结构大小 + 字段偏移锚定（R0 §A，对 Linux nvme-rdma.h；禁手算）。
    #[test]
    fn cm_struct_layout_matches_spec() {
        // req: 5×u16 + rsvd[22] = 32B。
        assert_eq!(core::mem::size_of::<NvmeRdmaCmReq>(), 32);
        assert_eq!(offset_of!(NvmeRdmaCmReq, recfmt), 0);
        assert_eq!(offset_of!(NvmeRdmaCmReq, qid), 2);
        assert_eq!(offset_of!(NvmeRdmaCmReq, hrqsize), 4);
        assert_eq!(offset_of!(NvmeRdmaCmReq, hsqsize), 6);
        assert_eq!(offset_of!(NvmeRdmaCmReq, cntlid), 8);
        assert_eq!(offset_of!(NvmeRdmaCmReq, rsvd), 10);
        // rep: 2×u16 + rsvd[28] = 32B。
        assert_eq!(core::mem::size_of::<NvmeRdmaCmRep>(), 32);
        assert_eq!(offset_of!(NvmeRdmaCmRep, crqsize), 2);
        assert_eq!(offset_of!(NvmeRdmaCmRep, rsvd), 4);
        // rej: 2×u16 = 4B。
        assert_eq!(core::mem::size_of::<NvmeRdmaCmRej>(), 4);
        assert_eq!(offset_of!(NvmeRdmaCmRej, sts), 2);
    }

    /// 解码 + 校验 happy path（admin connect：hrqsize=32, hsqsize=31）。
    #[test]
    fn parse_admin_connect_ok() {
        let req = NvmeRdmaCmReq {
            recfmt: NVME_RDMA_CM_FMT_1_0,
            qid: 0,
            hrqsize: NVME_AQ_DEPTH,     // 32
            hsqsize: NVME_AQ_DEPTH - 1, // 31 (0-based)
            cntlid: 0xFFFF,
            rsvd: [0u8; 22],
        };
        let bytes = req.as_bytes();
        let parsed = NvmeRdmaCmReq::parse(bytes).unwrap();
        // **off-by-one 决定性断言**：hsqsize=31 (0-based) → recv_queue_size=32 (1-based)。
        assert_eq!(parsed.recv_queue_size(), 32);
        assert_eq!(parsed.send_queue_size(), 32);
        // accept 回 crqsize=recv_queue_size=32（1-based）。
        let rep = parsed.accept_reply();
        let crqsize = rep.crqsize;
        assert_eq!(crqsize, 32);
        let recfmt = rep.recfmt;
        assert_eq!(recfmt, NVME_RDMA_CM_FMT_1_0);
    }

    /// IO connect 带 cntlid（R0 §A.4：IO queue 填已分配 cntlid）。
    #[test]
    fn parse_io_connect_carries_cntlid() {
        let req = NvmeRdmaCmReq {
            recfmt: NVME_RDMA_CM_FMT_1_0,
            qid: 1,
            hrqsize: 128,
            hsqsize: 127,
            cntlid: 1,
            rsvd: [0u8; 22],
        };
        let parsed = NvmeRdmaCmReq::parse(req.as_bytes()).unwrap();
        let cntlid = parsed.cntlid;
        assert_eq!(cntlid, 1);
        assert_eq!(parsed.recv_queue_size(), 128); // IO queue 不校验 AQ_DEPTH
    }

    /// reject 路径：recfmt 错 / 太短 / admin 过深。
    #[test]
    fn parse_rejects() {
        // 太短。
        assert!(matches!(
            NvmeRdmaCmReq::parse(&[0u8; 16]),
            Err(CmReqError::TooShort(16))
        ));
        // recfmt 非 1.0。
        let mut bad = NvmeRdmaCmReq::default();
        bad.recfmt = 0x99;
        assert!(matches!(
            NvmeRdmaCmReq::parse(bad.as_bytes()),
            Err(CmReqError::InvalidRecfmt(0x99))
        ));
        assert_eq!(
            CmReqError::InvalidRecfmt(0x99).cm_status(),
            cm_status::INVALID_RECFMT
        );
        // admin queue 过深：hsqsize=32 → recv_queue_size=33 > 32。
        let deep = NvmeRdmaCmReq {
            recfmt: NVME_RDMA_CM_FMT_1_0,
            qid: 0,
            hrqsize: 33,
            hsqsize: 32,
            cntlid: 0xFFFF,
            rsvd: [0u8; 22],
        };
        assert!(matches!(
            NvmeRdmaCmReq::parse(deep.as_bytes()),
            Err(CmReqError::AdminQueueTooDeep(33, 32))
        ));
        assert_eq!(
            CmReqError::AdminQueueTooDeep(33, 32).cm_status(),
            cm_status::INVALID_HSQSIZE
        );
    }

    /// rej 构造。
    #[test]
    fn rej_construct() {
        let rej = NvmeRdmaCmRej::new(cm_status::INVALID_CNTLID);
        let (recfmt, sts) = (rej.recfmt, rej.sts);
        assert_eq!(recfmt, NVME_RDMA_CM_FMT_1_0);
        assert_eq!(sts, 0x09);
    }
}
