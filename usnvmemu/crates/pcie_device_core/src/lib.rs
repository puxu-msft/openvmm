// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # pcie_device_core
//!
//! **Phase W2 (ADR-010)** — transport-neutral PCIe 设备 domain model。
//!
//! 本 crate 是 hexagonal 架构的 **domain core**：定义"一个 PCIe 设备*是什么*"
//! （[`PcieDevice`] trait + 中立 [`DeviceDescribe`]）与"设备如何反向发起 DMA /
//! 中断"（[`Transport`] trait + [`DeviceCtx`]）。它**零 wire 依赖**（不 import
//! 任何 protobuf / pcie_remote 协议）、**零 runtime 依赖**（不 import pal_async /
//! tokio / vmsocket）。
//!
//! 每个 transport adapter（`pcie_transport_openhcl` vsock / `vfio_user_transport` /
//! `nvme_of_tcp_target`）在自己的 crate 里 `impl Transport` 并把 device 跑起来；
//! wire 类型只活在各 adapter 内，永不泄漏进本 domain core。
//!
//! 测试用 [`CaptureTransport`] 捕获 device 的 outbound 原语成中立
//! [`TransportEvent`]，无需 live socket / wire 编码。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod describe;
mod device;
mod noop_transport;
mod shared_ram;
mod transport_capture;

pub use describe::BarKind;
pub use describe::BarLayout;
pub use describe::Capability;
pub use describe::DeviceDescribe;
pub use device::DeviceCtx;
pub use device::PcieDevice;
pub use device::Transport;
pub use noop_transport::NoopTransport;
pub use shared_ram::SharedRamRegion;
pub use shared_ram::VecRamRegion;
pub use transport_capture::CaptureTransport;
pub use transport_capture::TransportEvent;
