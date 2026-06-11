// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `vfio_user_device` —— vfio-user **client**（underhill 侧）。
//!
//! 与 `vfio_user_transport`（firmware server）对接，复用 `vfio_user_wire` sans-IO
//! 协议层（编解码 + 握手/irq 纯决策）。已实现：
//! - **W1**：connect AF_UNIX + VERSION 握手协商（`VfioUserClient::{connect, handshake}`）
//! - **W2**：REGION wire（`get_device_info` / `get_region_info` / `get_irq_info` /
//!   `region_read` / `region_write` / `reset`）
//! - **W3**：DMA_MAP fd-passing 零拷贝（`dma_map` / `dma_unmap` / `dma_unmap_all`，
//!   经 SCM_RIGHTS 传 region fd → server mmap）
//! - **W4**：MSI-X SET_IRQS eventfd（`set_irqs` / `set_irqs_deassign` /
//!   `set_irqs_clear`，多 fd 经 SCM_RIGHTS → server fire 写 eventfd）
//!
//! **同步阻塞**收发（loopback 单测对接真 server）；发 fd 全 safe nix（收 fd 才需
//! unsafe，client 只发），保持 `#![deny(unsafe_code)]`。async 化留 W6；guest-facing
//! PCIe 呈现（pci_core adapter）+ eventfd→`Interrupt::deliver` VTL0 注入留 W6
//! （需 underhill partition，非 standalone 可测）。

#![deny(unsafe_code)]

mod client;
mod framing;

pub use client::VfioUserClient;
pub use vfio_user_wire::handshake::NegotiatedClient;
