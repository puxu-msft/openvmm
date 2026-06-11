// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `vfio_user_device` —— vfio-user **client**（underhill 侧）。
//!
//! 与 `vfio_user_transport`（firmware server）对接：connect AF_UNIX socket、
//! VERSION 握手协商，后续（W2+）PCIe 呈现 / REGION_RW / DMA_MAP / MSI-X。
//! 复用 `vfio_user_wire` sans-IO 协议层（编解码 + 握手纯决策）。
//!
//! **W1 范围**：client wire + AF_UNIX + 握手（loopback 单测）。同步阻塞收发
//! （握手无 fd → 纯 `UnixStream` read/write，无 recvmsg/SCM_RIGHTS/unsafe）；
//! fd-capable framing 留 W3，async 化留 W6。

#![deny(unsafe_code)]

mod client;
mod framing;

pub use client::VfioUserClient;
pub use vfio_user_wire::handshake::NegotiatedClient;
