// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Dynamic resolvers for `PcieRemoteTcpHandle` / `PcieRemoteVmbusHandle`.
//!
//! - Resolver 自持 `prepared_map`（spec §3.3 控制反转）。
//! - Resolver **不**持 worker task；Task 由调用方 Vec 持有。
//! - Resolver 找不到 prepared 时返回 `AbsentPcieDevice`（spec §3.10
//!   layer-2，避免 host-controllable boot DoS）。

use crate::AbsentPcieDevice;
use crate::PcieRemoteDevice;
use crate::PreparedPcieRemoteDevice;
use crate::state::DeviceState;
use crate::state::SharedState;
use async_trait::async_trait;
use parking_lot::Mutex;
use pcie_remote_protocol::DeviceDescribe;
use pcie_remote_resources::PcieRemoteTcpHandle;
use pcie_remote_resources::PcieRemoteVmbusHandle;
use std::collections::HashMap;
use std::sync::Arc;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::kind::PciDeviceHandleKind;

// pci_resources types — 通过 pcie_remote_device dependency 暴露。
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;

/// 共享 prepared map：调用方在 boot 期 populate，resolver 在 resolve 期 take。
pub type PreparedMap = Arc<Mutex<HashMap<guid::Guid, PreparedPcieRemoteDevice>>>;

/// TCP transport resolver（OpenVMM 路径）。
pub struct PcieRemoteTcpResolver {
    prepared: PreparedMap,
}

impl PcieRemoteTcpResolver {
    /// 构造。
    pub fn new(prepared: PreparedMap) -> Self {
        Self { prepared }
    }
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, PcieRemoteTcpHandle> for PcieRemoteTcpResolver {
    type Output = ResolvedPciDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _: &ResourceResolver,
        handle: PcieRemoteTcpHandle,
        _: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(resolve_one(&self.prepared, handle.instance_id))
    }
}

/// vsock transport resolver（OpenHCL 路径）。
pub struct PcieRemoteVmbusResolver {
    prepared: PreparedMap,
}

impl PcieRemoteVmbusResolver {
    /// 构造。
    pub fn new(prepared: PreparedMap) -> Self {
        Self { prepared }
    }
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, PcieRemoteVmbusHandle> for PcieRemoteVmbusResolver {
    type Output = ResolvedPciDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _: &ResourceResolver,
        handle: PcieRemoteVmbusHandle,
        _: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(resolve_one(&self.prepared, handle.instance_id))
    }
}

fn resolve_one(prepared: &PreparedMap, instance_id: guid::Guid) -> ResolvedPciDevice {
    let Some(prep) = prepared.lock().remove(&instance_id) else {
        tracing::error!(
            %instance_id,
            "pcie_remote handshake missing; serving AbsentPcieDevice"
        );
        return AbsentPcieDevice::new().into();
    };
    let dev = assemble_device(prep);
    dev.into()
}

fn assemble_device(mut prep: PreparedPcieRemoteDevice) -> PcieRemoteDevice {
    let cfg_local = build_initial_cfg(&prep.describe);
    let state = SharedState::new(DeviceState::Live);
    // worker_inbox 应该已经被 spawn 流程 take 走；这里 sanity check。
    let _ = prep.worker_inbox.take();
    PcieRemoteDevice::new(state, prep.to_worker, cfg_local)
}

/// 用 DeviceDescribe 填本地 cfg 镜像的前 3 dwords（vendor/device/status/class/revision）。
fn build_initial_cfg(d: &DeviceDescribe) -> [u32; 64] {
    let mut cfg = [0u32; 64];
    cfg[0] = (d.vendor_id) | (d.device_id << 16);
    cfg[1] = 0; // command=0 status=0
    cfg[2] = ((d.class_code & 0x00ff_ffff) << 8) | (d.revision & 0xff);
    cfg
}
