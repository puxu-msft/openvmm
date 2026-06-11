// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase U** — vfio-user protocol backend for [`pcie_device_core`].
//!
//! 让任意实现了 [`pcie_device_core::PcieDevice`] 的设备（如本仓库
//! 的 NVMe controller）通过 [vfio-user 协议][spec] 暴露成 UNIX-socket
//! server，被 QEMU `-device vfio-user-pci,socket=...` 直接接管。
//!
//! 与 OpenHCL pcie_remote backend 互为兄弟 transport：同一份 NvmeController
//! 跨 hypervisor 复用，验证 Phase T 抽象。
//!
//! 当前进度（U1–U5 全部 ✅）：
//! - **U1**：proto 数据结构 + 编解码 + 单测 ✅
//! - **U2**：UNIX socket framing + SCM_RIGHTS fd 传递 + 单测 ✅
//! - **U3**：VfioUserSession + REGION/INFO/RESET 命令派发到 PcieDevice ✅
//! - **U4**：DMA_MAP/UNMAP 表 + DMA_READ/WRITE server-initiated sync ✅
//! - **U5**：SET_IRQS eventfd → MSI-X 中断 + NVMe CLI demo（nvme_firmware
//!   `--vfio-user-sock`）+ 真 QEMU 11 guest-boot e2e ✅。
//!
//! 注：最初设想的独立 `VfioUserTransport` 类型**并未引入** ——
//! [`VfioUserSession`] 自身直接 `impl pcie_device_core::Transport`，设备经
//! `DeviceCtx` 反向发起的 DMA / 中断即走它（见 `session.rs`）。
//!
//! 完整 wire 参考：`docs/superpowers/specs/2026-06-04-vfio-user-wire-reference.md`。
//!
//! # unsafe 范围
//!
//! 本 crate 整体走 `#![deny(unsafe_code)]`（不是 forbid），仅两处
//! `#[allow(unsafe_code)]`，均由严密 SAFETY 注释保护 + 测试覆盖：
//! 1. [`framing::into_owned_fd`] 调 `OwnedFd::from_raw_fd` 将 `SCM_RIGHTS`
//!    收到的 RawFd 转 owned 句柄（[`framing::tests::roundtrip_with_one_fd`]）。
//! 2. `dma::map_dma_fd` 调 `memmap2::MmapOptions::map{,_mut}` 把 DMA_MAP 带来的
//!    client memfd 映射成零拷贝 DMA 内存（`dma::tests` mmap roundtrip 覆盖）。
//!
//! [spec]: https://github.com/nutanix/libvfio-user/blob/master/docs/vfio-user.rst

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod access;
pub mod config;
pub mod dma;
pub mod framing;
pub mod handshake;
pub mod irq;
pub mod proto;
pub mod server;
pub mod session;
pub mod transport;

pub use config::ConfigSpace;
pub use dma::DmaError;
pub use dma::DmaRegion;
pub use dma::DmaTable;
pub use framing::MAX_MSG_FDS;
pub use framing::Message;
pub use framing::read_message;
pub use framing::write_message;
pub use handshake::Negotiated;
pub use handshake::SERVER_CAPS_JSON;
pub use handshake::server_handshake;
pub use irq::IrqVectors;
pub use proto::Command;
pub use proto::HEADER_LEN;
pub use proto::Header;
pub use proto::HeaderFlags;
pub use proto::ProtoError;
pub use server::serve_unix;
pub use session::VfioUserSession;
pub use transport::NoopTransport;
