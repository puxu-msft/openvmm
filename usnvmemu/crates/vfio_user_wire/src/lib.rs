// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `vfio_user_wire` —— sans-IO vfio-user 线协议层。
//!
//! 抽自 `vfio_user_transport`（W0，2026-06-11）。本 crate 只含**无 IO**
//! 的协议事实：
//!
//! - [`proto`] —— Header / Command / Payload 编解码（zerocopy 字节布局）
//! - [`access`] —— REGION 访问的 sans-IO 对齐切分
//! - [`config`] —— PCIe Config Space 状态机（无 IO，输入 `DeviceDescribe`）
//! - [`handshake`] —— VERSION 协商纯决策 + server caps 常量
//! - [`irq`] —— `DEVICE_SET_IRQS` 的纯决策（assign/deassign/clear 等动作判定）
//! - [`framing`] —— sans-IO 线消息类型 `WireMessage`（header + payload，无 fd）
//!
//! IO（AF_UNIX 收发、SCM_RIGHTS、mmap、reactor）由调用方负责：
//! - server 端：`vfio_user_transport`（同仓）
//! - client 端：`vfio_user_device`（W1 起做，underhill 内）

#![deny(unsafe_op_in_unsafe_fn)]

pub mod access;
pub mod config;
pub mod framing;
pub mod handshake;
pub mod irq;
pub mod proto;
