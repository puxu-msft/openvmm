// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V** — NVMe-over-Fabrics TCP target。
//!
//! 把 [`pcie_remote_nvme_userspace::NvmeController`] 暴露在 TCP port 4420，
//! 任何安装了 nvme-tcp host 驱动（Linux ≥ 5.0 / Windows Server 2025）的机器
//! `nvme connect -t tcp -a <host> -n <NQN>` 即可挂载。
//!
//! 当前进度：
//! - **V0** ✅ controller crate 拆 lib+bin（外部 crate 可 import）
//! - **V1** ✅ PDU 数据结构 + 编解码 + CRC32C digest + 同步 TCP framing
//! - **V2** ✅ ICReq/ICResp 握手 + Fabric Connect / Property Get/Set
//!   + minimal session state machine
//! - **V3** ✅ admin cmd 派发到 NvmeController：Identify/Get Log Page
//!   等异步路径 captured dma_write 转 C2HData + CapsuleResp
//! - **V4a** ✅ wire layer：R2T encode + H2CData reassembler + TTAG 分配器
//! - **V4b** ✅ controller dma_read → R2T → H2CData 闭环（单段 ≤ 64 KiB）
//! - **V4c** ✅ MAXH2CDATA 分片 + 多 R2T 串行（dma_read > 64 KiB 自动切片）
//! - **V5a** ✅ IO queue 安装 + Fabric Connect qid≥1 + dispatch 二分
//! - **V5b** ✅ IO Read nlb=1 走 C2HData 闭环
//! - **V5c** ✅ IO Write nlb=1 走 R2T/H2CData 闭环（含数据持久化验证）
//! - **V5d** ✅ `main.rs` TcpListener:4420 + README + bin smoke test
//! - **V5d-fix / V5d-fix-2** ✅ security hardening: max-conn cap / loopback
//!   default / ctrlc graceful / handshake timeout / SIGPIPE invariant
//! - **V5e-1** ✅ nlb 上限放宽到 8（单 PRP1 4 KiB；Linux dd bs=4k 单 cmd 完成）
//! - **V5e-2–V8** 计划中

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod digest;
pub mod fabric;
pub mod framing;
pub mod h2c_reassembler;
pub mod io_queue;
pub mod pdu;
pub mod r2t;
pub mod session;
pub mod tcp_transport;
pub mod ttag;

pub use digest::crc32c;
pub use fabric::ConnectData;
pub use fabric::ConnectFabricFields;
pub use fabric::FabricError;
pub use fabric::PropertyFabricFields;
pub use framing::Pdu;
pub use framing::read_pdu;
pub use framing::write_pdu;
pub use h2c_reassembler::{AcceptOutcome, H2cReassembler};
pub use pdu::*;
pub use r2t::encode_r2t;
pub use session::CQ_BASE_GPA;
pub use session::MAXH2CDATA_BYTES;
pub use session::NegotiatedIc;
pub use session::PRP1_SENTINEL;
pub use session::V2Session;
pub use session::V5_NLB_MAX;
pub use session::ic_handshake;
pub use tcp_transport::TcpAdminTransport;
pub use ttag::TtagAllocator;
