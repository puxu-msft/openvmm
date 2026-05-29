// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

//! Resource definitions for the PCIe remote experimental device (spec v3.1).
//!
//! 三种 handle:
//! - `PcieRemoteHandle`（已废弃，OpenVMM 旧 CLI 兼容占位）
//! - `PcieRemoteTcpHandle`（OpenVMM 路径，TCP loopback）
//! - `PcieRemoteVmbusHandle`（OpenHCL 路径，vsock）

use mesh::MeshPayload;
use vm_resource::ResourceId;
use vm_resource::kind::PciDeviceHandleKind;

/// 默认 TCP 地址（仅 OpenVMM 旧 CLI 兼容；v3.1 起仅允许 loopback）。
pub const DEFAULT_SOCKET_ADDR: &str = "127.0.0.1:48914";

/// 旧 handle，保留作为 OpenVMM CLI 兼容；新代码请用 `PcieRemoteTcpHandle`
/// 或 `PcieRemoteVmbusHandle`。
#[allow(deprecated)]
#[derive(MeshPayload)]
#[deprecated(note = "use PcieRemoteTcpHandle (OpenVMM) or PcieRemoteVmbusHandle (OpenHCL)")]
pub struct PcieRemoteHandle {
    /// 实例唯一标识。
    pub instance_id: guid::Guid,
    /// 可选 TCP 地址；不填时用 [`DEFAULT_SOCKET_ADDR`]。
    pub socket_addr: Option<String>,
    /// Host Unit 编号（多 PCIe IP 块场景）。
    pub hu: u16,
    /// Controller 编号（单 HU 内多 link 场景）。
    pub controller: u16,
}

#[allow(deprecated)]
impl PcieRemoteHandle {
    /// 返回 socket 地址；缺省时用 [`DEFAULT_SOCKET_ADDR`]。
    pub fn socket_addr(&self) -> &str {
        self.socket_addr.as_deref().unwrap_or(DEFAULT_SOCKET_ADDR)
    }
}

#[allow(deprecated)]
impl ResourceId<PciDeviceHandleKind> for PcieRemoteHandle {
    const ID: &'static str = "pcie_remote";
}

/// OpenVMM 路径：通过 TCP loopback 连 host 用户态实验程序。
///
/// `socket_addr` 必须是 loopback（127.0.0.1 / ::1）；resolver 会校验。
#[derive(Debug, Clone, MeshPayload)]
pub struct PcieRemoteTcpHandle {
    /// 实例唯一标识。
    pub instance_id: guid::Guid,
    /// TCP socket 地址（仅允许 loopback）。
    pub socket_addr: String,
    /// 握手超时（毫秒）。
    pub handshake_timeout_ms: u32,
}

impl ResourceId<PciDeviceHandleKind> for PcieRemoteTcpHandle {
    const ID: &'static str = "pcie_remote_tcp";
}

/// OpenHCL 路径：通过 vsock (AF_VSOCK ↔ AF_HYPERV) 连 host 用户态实验程序。
#[derive(Debug, Clone, MeshPayload)]
pub struct PcieRemoteVmbusHandle {
    /// 实例唯一标识。
    pub instance_id: guid::Guid,
    /// vsock 端口（必须不在端口黑名单内，由 OpenHCL CLI 校验）。
    pub vsock_port: u32,
    /// 握手超时（毫秒）。
    pub handshake_timeout_ms: u32,
}

impl ResourceId<PciDeviceHandleKind> for PcieRemoteVmbusHandle {
    const ID: &'static str = "pcie_remote_vmbus";
}
