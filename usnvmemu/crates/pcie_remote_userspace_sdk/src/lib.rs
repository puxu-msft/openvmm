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
//! use pcie_remote_userspace_sdk::*;
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

mod device;
mod openhcl_transport;
mod run;
mod transport;

pub use device::DeviceCtx;
pub use device::PcieDevice;
pub use device::Transport;
pub use openhcl_transport::OpenhclVsockTransport;
// **Phase Q10** — re-export protocol types so example crates can use them
// in test fixtures (DeviceCtx::for_testing 输出 outbound 包).
pub use pcie_remote_protocol;
pub use pcie_remote_protocol::BarInfo;
pub use pcie_remote_protocol::CapabilityBlob;
pub use pcie_remote_protocol::DeviceDescribe;
pub use pcie_remote_protocol::ToOpenhcl;
pub use pcie_remote_protocol::bar_info::Kind as BarKind;
pub use run::RunOptions;
pub use run::run;
pub use transport::WireStream;
pub use transport::connect_tcp;
#[cfg(windows)]
pub use transport::connect_vsock;
