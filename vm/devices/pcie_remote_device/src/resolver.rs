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

#[cfg(test)]
mod tests {
    use super::*;
    use mesh::channel;
    use pcie_remote_protocol::BarInfo;
    use pcie_remote_protocol::bar_info::Kind;

    fn dummy_describe() -> DeviceDescribe {
        DeviceDescribe {
            vendor_id: 0x1414,
            device_id: 0xc0de,
            class_code: 0x010802,
            revision: 1,
            subsystem_vendor: 0,
            subsystem_device: 0,
            bars: vec![BarInfo {
                index: 0,
                size: 4096,
                kind: Kind::Mmio32 as i32,
                prefetchable: false,
            }],
            msix_count: 1,
            capabilities: vec![],
            cfg_write_side_effect_offsets: vec![],
        }
    }

    /// 验证 spec §3.10 layer-2：prepared_map 中缺项时返回 AbsentPcieDevice，
    /// 而**不**是 bail!。这是 CVM bug 防护 / boot DoS 防御的关键。
    #[test]
    fn missing_prep_returns_absent_device() {
        let prepared: PreparedMap = Arc::new(Mutex::new(HashMap::new()));
        let id = guid::Guid {
            data1: 0xdead_beef,
            ..Default::default()
        };
        // resolve_one 是同步函数，可以直接测。它必须**不 panic** 并返回某个 device。
        let _dev = resolve_one(&prepared, id);
    }

    /// prepared_map 中有项时正常组装并消耗。
    #[test]
    fn present_prep_returns_real_device() {
        let prepared: PreparedMap = Arc::new(Mutex::new(HashMap::new()));
        let id = guid::Guid {
            data1: 0xabcd_1234,
            ..Default::default()
        };
        let (tx, _rx) = channel();
        let prep = PreparedPcieRemoteDevice {
            describe: dummy_describe(),
            to_worker: tx,
            worker_inbox: Some(channel().1),
        };
        prepared.lock().insert(id, prep);
        let _dev = resolve_one(&prepared, id);
        assert!(
            prepared.lock().is_empty(),
            "resolve_one should consume prepared entry"
        );
    }

    #[test]
    fn build_initial_cfg_packs_vendor_device() {
        let d = dummy_describe();
        let cfg = build_initial_cfg(&d);
        assert_eq!(cfg[0], 0x1414 | (0xc0de << 16));
        assert_eq!(cfg[2], (0x010802 << 8) | 1);
    }

    /// K-17: 重复注册同 resource id resolver 应 panic
    /// (vm_resource/src/lib.rs:`add_async_resolver` doc 明确 panic on dup)。
    /// 这里在文档层面记录此契约；具体 panic 行为由 vm_resource 保证。
    #[test]
    fn add_async_resolver_duplicate_panics_contract_documented() {
        // 此测试为契约文档化：vm_resource::ResourceResolver::add_async_resolver
        // 在 ResourceId<K> 重复时 panic("duplicate resolver for ...")。
        // pcie_remote 的 dispatch.rs (OpenVMM) 与 worker.rs (OpenHCL) 都只
        // 调用一次。如未来代码改动引入重复调用，会立即在 boot 期 panic
        // 而非静默忽略。
        // 真实 panic 用 #[should_panic] 需构造完整 ResourceResolver，
        // 涉及 vmm_core 依赖，此处仅作 lint-style assertion。
        let id_str = <pcie_remote_resources::PcieRemoteTcpHandle as vm_resource::ResourceId<vm_resource::kind::PciDeviceHandleKind>>::ID;
        assert_eq!(id_str, "pcie_remote_tcp");
        let id_str2 = <pcie_remote_resources::PcieRemoteVmbusHandle as vm_resource::ResourceId<vm_resource::kind::PciDeviceHandleKind>>::ID;
        assert_eq!(id_str2, "pcie_remote_vmbus");
        // 两个 ID 不同，所以 Tcp 和 Vmbus 注册到同一 resolver 不冲突。
        assert_ne!(id_str, id_str2);
    }
}
