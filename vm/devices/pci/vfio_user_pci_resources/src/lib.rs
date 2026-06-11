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
}

impl ResourceId<PciDeviceHandleKind> for VfioUserNvmeHandle {
    const ID: &'static str = "vfio_user_nvme";
}
