// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase U** — vfio-user protocol backend for [`pcie_remote_userspace_sdk`].
//!
//! 让任意实现了 [`pcie_remote_userspace_sdk::PcieDevice`] 的设备（如本仓库
//! 的 NVMe controller）通过 [vfio-user 协议][spec] 暴露成 UNIX-socket
//! server，被 QEMU `-device vfio-user-pci,socket=...` 直接接管。
//!
//! 与 OpenHCL pcie_remote backend 互为兄弟 transport：同一份 NvmeController
//! 跨 hypervisor 复用，验证 Phase T 抽象。
//!
//! 当前进度：
//! - **U1**：proto 数据结构 + 编解码 + 单测 ✅
//! - **U2**：UNIX socket framing + SCM_RIGHTS fd 传递 + 单测 ✅
//! - **U3**：VfioUserSession + REGION/INFO/RESET 命令派发到 PcieDevice ✅
//! - **U4-U5**：DMA / IRQ / 端到端 QEMU demo（pending）
//!
//! 完整 wire 参考：`docs/superpowers/specs/2026-06-04-vfio-user-wire-reference.md`。
//!
//! # unsafe 范围
//!
//! 本 crate 整体走 `#![deny(unsafe_code)]`（不是 forbid），仅
//! [`framing::into_owned_fd`] 一处 `#[allow(unsafe_code)]` 调
//! `OwnedFd::from_raw_fd` 将 `SCM_RIGHTS` 收到的 RawFd 转 owned 句柄。
//! 该 unsafe 由严密 SAFETY 注释保护并被 [`framing::tests::roundtrip_with_one_fd`]
//! 覆盖。
//!
//! [spec]: https://github.com/nutanix/libvfio-user/blob/master/docs/vfio-user.rst

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod framing;
pub mod handshake;
pub mod proto;
pub mod server;
pub mod session;
pub mod transport;

pub use framing::MAX_MSG_FDS;
pub use framing::Message;
pub use framing::read_message;
pub use framing::write_message;
pub use handshake::Negotiated;
pub use handshake::SERVER_CAPS_JSON;
pub use handshake::server_handshake;
pub use proto::Command;
pub use proto::HEADER_LEN;
pub use proto::Header;
pub use proto::HeaderFlags;
pub use proto::ProtoError;
pub use server::serve_unix;
pub use session::Regions;
pub use session::VfioUserSession;
pub use transport::NoopTransport;
