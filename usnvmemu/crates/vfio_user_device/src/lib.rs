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
//! - **W6b Task 1.2**：全双工通道（`VfioUserClient::into_channel` →
//!   `VfioUserWriter` + `VfioUserReader`），供 underhill worker `select!` 并发收发
//!
//! **async 收发**（W6a，2026-06-12）：迁到 pal_async `PolledSocket`，照搬
//! `vhost_user_protocol::socket` 的 SCM_RIGHTS 混合范式（PolledSocket readiness + 裸
//! libc cmsg fd 传递）。**unsafe 隔离在 [`async_socket`] 一个模块**；`framing`/`client`
//! 模块级 `#![deny(unsafe_code)]` 维持纯 safe。**新增模块默认须加模块级
//! `#![deny(unsafe_code)]`，仅 async_socket 例外**（crate 级 deny 已移除，防回归）。
//! 为 W6b/c/d 真 underhill 集成（pcie_remote 风格 async worker）铺路。
//!
//! **W6b 全双工**（Task 1.2，2026-06-12）：串行 `AsyncSocket`（`Mutex<PolledSocket>`，
//! request/reply 串行）对 underhill worker 不够——worker 需在 `select!` 里发请求帧的
//! 同时收 reply 帧，否则 firmware 一慢就卡死整个循环（连 shutdown 都处理不了）。故
//! `into_channel` 经 `PolledSocket::split()` 拆读写半（共享 socket 但不跨内核调用持锁，
//! send/recv 真并发），msg_id 分配权移交调用方（worker 持计数器 + in-flight 表）。串行
//! `VfioUserClient` 维持不变（W5a harness / spawn identity 读仍用），`into_channel` 是
//! additive。详见 [`channel`] 模块文档。
//! guest-facing PCIe 呈现（pci_core adapter）+ eventfd→`Interrupt::deliver` VTL0 注入
//! 留 W6b 后续 task（需 underhill partition，非 standalone 可测）。

mod async_socket;
mod channel;
mod client;
mod framing;

pub use channel::VfioUserReader;
pub use channel::VfioUserWriter;
pub use client::VfioUserClient;
pub use vfio_user_wire::handshake::NegotiatedClient;
