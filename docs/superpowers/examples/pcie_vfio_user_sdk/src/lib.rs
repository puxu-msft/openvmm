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
//! - **U2-U5**：handshake / region IO / DMA / IRQ / 端到端 QEMU demo（pending）
//!
//! 完整 wire 参考：`docs/superpowers/specs/2026-06-04-vfio-user-wire-reference.md`。
//!
//! [spec]: https://github.com/nutanix/libvfio-user/blob/master/docs/vfio-user.rst

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod proto;

pub use proto::Command;
pub use proto::HEADER_LEN;
pub use proto::Header;
pub use proto::HeaderFlags;
pub use proto::ProtoError;
