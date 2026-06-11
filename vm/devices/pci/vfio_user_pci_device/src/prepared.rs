// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connect 任务完成后的产物（W6b Task 1.5）。
//!
//! 设计要点（对标 `pcie_remote_device::prepared`，但 vfio-user **主动 connect**）：
//! - boot 早期，PCI resolver 装配设备之前，connect 任务先主动 `connect` 到
//!   firmware 的 vfio-user AF_UNIX socket，做 VERSION 握手，并读 firmware 身份
//!   （PCI config header + region/irq info）构造 [`HardwareIds`]。
//! - 结果（[`PreparedVfioUserDevice`]）塞进共享 map（[`PreparedMap`]），resolver
//!   （Task 1.6）后续取走装配 `ChipsetDevice`。
//! - `client` 已 connect+handshake，直接 by value 交给 worker（不需 `Arc<Mutex>`）；
//!   resolver consume 整个结构。

use guid::Guid;
use parking_lot::Mutex;
use pci_core::spec::hwid::HardwareIds;
use std::collections::HashMap;
use std::sync::Arc;
use vfio_user_device::VfioUserClient;

/// 一个已 connect+handshake+identity 的 vfio-user firmware 设备，等 resolver 取走装配。
///
/// `bar0_size` / `msix_count` 是 firmware 在 GET_REGION_INFO(BAR0) / GET_IRQ_INFO(MSIX)
/// 中报告的几何信息，resolver 据此构造 BAR window + MSI-X table 容量。
pub struct PreparedVfioUserDevice {
    /// 已 connect+握手的 async client，等 resolver 取走给 worker。
    pub client: VfioUserClient,
    /// 从 firmware CONFIG region 解析的 PCI 身份（vendor/device/class 等）。
    pub hardware_ids: HardwareIds,
    /// BAR0 大小（字节），firmware 在 GET_REGION_INFO(BAR0) 报告。
    pub bar0_size: u64,
    /// MSI-X 向量数，firmware 在 GET_IRQ_INFO(MSIX) 报告。
    pub msix_count: u16,
}

/// instance_id → prepared 设备。connect 任务写入，resolver consume。
///
/// 模板把 `PreparedMap` 定义在 resolver.rs；W6b 因 resolver（Task 1.6）尚未落地，
/// 先在 prepared.rs 定义，Task 1.6 直接复用此别名。
pub type PreparedMap = Arc<Mutex<HashMap<Guid, PreparedVfioUserDevice>>>;
