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
//! - **V2-V8** 计划中（见 docs/superpowers/plans/2026-06-04-phase-v-nvme-of-tcp.md）
//!
//! 完整 wire 参考：`docs/superpowers/specs/2026-06-04-nvme-tcp-wire-reference.md`。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod digest;
pub mod framing;
pub mod pdu;

pub use digest::crc32c;
pub use framing::Pdu;
pub use framing::read_pdu;
pub use framing::write_pdu;
pub use pdu::*;
