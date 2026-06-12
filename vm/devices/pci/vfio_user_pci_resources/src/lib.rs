// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! vfio-user NVMe 设备的 resource handle（W6b）。underhill 把 firmware 的
//! unix socket 路径包成此 handle，resolver 据此 connect + 装配 ChipsetDevice。

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::ResourceId;
use vm_resource::kind::PciDeviceHandleKind;

/// 一个 vfio-user NVMe firmware 设备（VTL2 内独立进程，AF_UNIX 可达）。
#[derive(Debug, Clone, MeshPayload)]
pub struct VfioUserNvmeHandle {
    /// guest-visible 实例 ID。
    pub instance_id: guid::Guid,
    /// firmware vfio-user server 的 AF_UNIX socket 路径。
    pub unix_path: String,
    /// 声明给 guest 的 BAR0 窗口字节大小（CLI override；`None` = 用 resolver 内置
    /// 默认 `DEFAULT_BAR0_SIZE`）。identity 校验上界：firmware 实报 BAR0 超此值即
    /// 拒绝（决策 b，防 guest 拿到半映射控制器）。
    pub bar0_size: Option<u64>,
    /// 声明给 guest 的 MSI-X 向量数（CLI override；`None` = 用 resolver 内置默认
    /// `DEFAULT_MSIX_COUNT`）。identity 校验上界：firmware 实报 MSI-X 超此值即拒绝。
    pub msix_count: Option<u16>,
}

impl ResourceId<PciDeviceHandleKind> for VfioUserNvmeHandle {
    const ID: &'static str = "vfio_user_nvme";
}
