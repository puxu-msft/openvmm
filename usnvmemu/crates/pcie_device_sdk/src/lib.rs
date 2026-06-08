// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![allow(clippy::disallowed_macros)] // futures::pin_mut / select_biased 惯用法

//! Userspace SDK for implementing PCIe devices over the pcie_remote vsock protocol.
//!
//! 让任意用户态程序通过 `vsock` 连入 OpenHCL VTL2 的 `pcie_remote_device`，
//! 给 guest 暴露一个完整可工作的 PCIe 设备。用户只需实现 [`PcieDevice`]
//! 7 个方法即可，wire-protocol / handshake / select loop / 退出处理全部
//! 由本 SDK 处理。
//!
//! # 设计
//!
//! - **零内核态**：纯 userspace，AF_HYPERV vsock，不需 KMDF / vfio。
//! - **同步语义**：所有回调（cfg/MMIO）签名同步；DMA 是 async 因为需要 RTT
//!   到 OpenHCL；interrupt 是 fire-and-forget。
//! - **单 worker 线程模型**：本 SDK 自带的 `run` 循环串行驱动用户实现，
//!   用户不需考虑并发；用户内部如果要后台任务，请自己 spawn。
//! - **Reconnect 友好**：transport EOF / error 时 `run` 返回；调用方可外
//!   层 reconnect。这对应 OpenHCL 侧 K-20 hotplug。
//! - **Phase T transport 抽象**：[`DeviceCtx`] 内部持 `&mut dyn Transport`，
//!   pcie_remote 协议路径走 [`OpenhclVsockTransport`]；Phase U/V 加
//!   vfio-user / NVMe-oF TCP 时只需新增 `impl Transport`，controller 0 改动。
//!
//! # 用法
//!
//! ```ignore
//! use pcie_device_sdk::*;
//!
//! struct MyDevice { /* state */ }
//!
//! #[async_trait::async_trait(?Send)]
//! impl PcieDevice for MyDevice {
//!     fn describe(&self) -> DeviceDescribe { /* ... */ }
//!     fn cfg_write_side_effect(&mut self, offset: u32, value: u32) { /* ... */ }
//!     fn mmio_read(&mut self, bar: u32, offset: u64, size: u32) -> u64 { /* ... */ }
//!     fn mmio_write(&mut self, bar: u32, offset: u64, size: u32, value: u64) { /* ... */ }
//!     fn reset(&mut self, kind: u32) { /* ... */ }
//!     fn tick(&mut self, ctx: &mut DeviceCtx<'_>) { /* periodic */ }
//! }
//!
//! #[cfg(windows)]
//! fn main() -> anyhow::Result<()> {
//!     pal_async::DefaultPool::run_with(|driver| async move {
//!         let vm_id: guid::Guid = "...".parse().unwrap();
//!         loop {
//!             let transport = connect_vsock(&driver, vm_id, 50000).await?;
//!             let device = MyDevice::new();
//!             run(&driver, transport, device, RunOptions::default()).await?;
//!             // EOF / error → reconnect
//!         }
//!     })
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod openhcl_transport;
mod run;
mod transport;

// **Phase W2 (ADR-010)** — device 模型移到 `pcie_device_core`（零 wire / 零 runtime
// 依赖）。本 crate 现在是 **openhcl (pcie_remote vsock) transport adapter**：实现
// `OpenhclVsockTransport` + 主循环 + connect。re-export core 类型让 openhcl bin
// 单路径 import；wire 类型（pcie_remote_protocol）**不再** pub —— 撤掉了泄漏，
// adapter 内部仍用它做 wire 编码。
pub use openhcl_transport::OpenhclVsockTransport;
pub use pcie_device_core::BarKind;
pub use pcie_device_core::BarLayout;
pub use pcie_device_core::Capability;
pub use pcie_device_core::CaptureTransport;
pub use pcie_device_core::DeviceCtx;
pub use pcie_device_core::DeviceDescribe;
pub use pcie_device_core::PcieDevice;
pub use pcie_device_core::Transport;
pub use pcie_device_core::TransportEvent;
pub use pcie_device_core::describe;
pub use run::RunOptions;
pub use run::run;
pub use transport::WireStream;
pub use transport::connect_tcp;
#[cfg(windows)]
pub use transport::connect_vsock;
