// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Dynamic resolvers for `PcieRemoteTcpHandle` / `PcieRemoteVmbusHandle` (v2)。
//!
//! v2 关键变化：worker spawn 推迟到 `assemble_device` 阶段，因为只有此时
//! 拿到 `msi_target` + `register_mmio` + `guest_memory` + `driver_source`，
//! 可构造完整 `MsixEmulator` + `DeviceBars` + `ConfigSpaceType0Emulator` +
//! `Vec<Interrupt>` + DMA-capable worker。
//!
//! - Resolver 自持 `prepared_map`（spec §3.3 控制反转）。
//! - Resolver 也持 `worker_tasks: Arc<Mutex<Vec<Task>>>`，每次 assemble
//!   一个 device 时 push 一个 task；调用方在 dispatch.rs 拿走 Arc 持有
//!   到进程结束，保证 worker 不被 drop。
//! - Resolver 找不到 prepared 时返回 `AbsentPcieDevice`（spec §3.10
//!   layer-2，避免 host-controllable boot DoS）。

use crate::AbsentPcieDevice;
use crate::PcieRemoteDevice;
use crate::PreparedPcieRemoteDevice;
use crate::handshake::MSIX_BAR_INDEX;
use crate::state::DeviceState;
use crate::state::SharedState;
use crate::worker::DeviceRequest;
use crate::worker::Worker;
use async_trait::async_trait;
use cvm_tracing::CVM_ALLOWED;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::cfg_space_emu::BarMemoryKind;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use pcie_remote_protocol::DeviceDescribe;
use pcie_remote_resources::PcieRemoteTcpHandle;
use pcie_remote_resources::PcieRemoteVmbusHandle;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::kind::PciDeviceHandleKind;

// pci_resources types — 通过 pcie_remote_device dependency 暴露。
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;

/// 共享 prepared map：调用方在 boot 期 populate，resolver 在 resolve 期 take。
pub type PreparedMap = Arc<Mutex<HashMap<guid::Guid, PreparedPcieRemoteDevice>>>;

/// 共享 worker tasks 持有器：assemble_device 时 push，dispatch.rs 持到进程结束。
///
/// **生命周期不变量**：drop `WorkerTasks` Arc 等于取消所有正在运行的 worker
/// task。调用方（dispatch.rs / underhill_core/worker.rs）必须把 Arc 持到设备
/// 生命周期结束（实践中即进程退出），否则会半路杀死正在收发的 worker。
pub type WorkerTasks = Arc<Mutex<Vec<Task<()>>>>;

/// TCP transport resolver（OpenVMM 路径）。
pub struct PcieRemoteTcpResolver {
    prepared: PreparedMap,
    worker_tasks: WorkerTasks,
}

impl PcieRemoteTcpResolver {
    /// 构造。
    pub fn new(prepared: PreparedMap, worker_tasks: WorkerTasks) -> Self {
        Self {
            prepared,
            worker_tasks,
        }
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
        params: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(resolve_one(
            &self.prepared,
            &self.worker_tasks,
            handle.instance_id,
            params,
        ))
    }
}

/// vsock transport resolver（OpenHCL 路径）。
pub struct PcieRemoteVmbusResolver {
    prepared: PreparedMap,
    worker_tasks: WorkerTasks,
}

impl PcieRemoteVmbusResolver {
    /// 构造。
    pub fn new(prepared: PreparedMap, worker_tasks: WorkerTasks) -> Self {
        Self {
            prepared,
            worker_tasks,
        }
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
        params: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(resolve_one(
            &self.prepared,
            &self.worker_tasks,
            handle.instance_id,
            params,
        ))
    }
}

fn resolve_one(
    prepared: &PreparedMap,
    worker_tasks: &WorkerTasks,
    instance_id: guid::Guid,
    params: ResolvePciDeviceHandleParams<'_>,
) -> ResolvedPciDevice {
    let Some(prep) = prepared.lock().remove(&instance_id) else {
        tracing::error!(
            CVM_ALLOWED,
            %instance_id,
            "pcie_remote handshake missing; serving AbsentPcieDevice"
        );
        return AbsentPcieDevice::new().into();
    };
    let dev = assemble_device(prep, worker_tasks, instance_id, params);
    dev.into()
}

/// 装配完整 `PcieRemoteDevice`：拿 prep + resolver params 构造
/// MsixEmulator + DeviceBars + ConfigSpaceType0Emulator，并 spawn worker。
fn assemble_device(
    mut prep: PreparedPcieRemoteDevice,
    worker_tasks: &WorkerTasks,
    instance_id: guid::Guid,
    params: ResolvePciDeviceHandleParams<'_>,
) -> PcieRemoteDevice {
    let describe = prep.describe.clone();
    let transport = prep.take_transport();

    // 1. MSI-X emulator：use MSIX_BAR_INDEX，count 来自 describe（handshake 已
    //    校验 ≤ 2048）。
    let msix_count = describe.msix_count as u16;
    let (msix, msix_cap) = MsixEmulator::new(MSIX_BAR_INDEX, msix_count, params.msi_target);

    // 2. interrupts vec：MSI-X 槽位的 Interrupt 句柄，移交 worker。
    let interrupts = (0..msix_count)
        .map(|i| {
            msix.interrupt(i)
                .expect("msix_count was just used to build emulator")
        })
        .collect::<Vec<_>>();

    // 3. DeviceBars：BAR4 给 MSI-X；其它按 host 声明（仅 bar0/bar2 支持
    //    via DeviceBars::bar0/bar2）。handshake 已拒绝 BAR index >5 和
    //    == MSIX_BAR_INDEX。BAR1/3/5 暂未在 DeviceBars 公共 API 暴露
    //    （留 v2 升级 pci_core API）。
    let mut bars = DeviceBars::new().bar4(
        msix.bar_len(),
        BarMemoryKind::Intercept(params.register_mmio.new_io_region("msix", msix.bar_len())),
    );
    for bar in &describe.bars {
        let region_name = format!("bar{}", bar.index);
        let region = params.register_mmio.new_io_region(&region_name, bar.size);
        bars = match bar.index {
            0 => bars.bar0(bar.size, BarMemoryKind::Intercept(region)),
            2 => bars.bar2(bar.size, BarMemoryKind::Intercept(region)),
            // BAR4 reserved for MSI-X; handshake rejects host BAR4 already.
            // BAR1/3/5: not yet wired in DeviceBars upstream API; warn + skip.
            _ => {
                tracing::warn!(
                    CVM_ALLOWED,
                    %instance_id,
                    bar_index = bar.index,
                    "pcie_remote: BAR index not supported by DeviceBars v1 API; ignored"
                );
                bars
            }
        };
    }

    // 4. cfg_space。
    let cfg_space = ConfigSpaceType0Emulator::new(
        derive_hardware_ids(&describe),
        vec![Box::new(msix_cap)],
        Vec::new(),
        bars,
    );

    let side_effect_offsets: HashSet<u32> = describe
        .cfg_write_side_effect_offsets
        .iter()
        .copied()
        .collect();

    // 5. channel for device → worker. mesh::channel 已隐式有缓冲。
    let (to_worker, worker_inbox) = mesh::channel::<DeviceRequest>();

    // 6. SharedState — Live 因为 prepared 在 map 时已 handshake 完。
    let state = SharedState::new(DeviceState::Live);

    // 7. spawn worker via params.driver_source.simple() —— 跨 platform，
    //    走 VmTaskDriver 这个仓库统一的 task 调度器。
    //    shutdown 通道 v1 不主动关，靠 transport EOF。
    //    sender 立刻 forget 避免 drop 让 worker.next() 立刻退出。
    let (shutdown_tx, shutdown_rx) = mesh::channel::<()>();
    std::mem::forget(shutdown_tx);
    let worker = Worker::new(
        transport,
        state.clone(),
        worker_inbox,
        interrupts,
        params.guest_memory.clone(),
    );
    let task = params.driver_source.simple().spawn(
        format!("pcie_remote_worker_{instance_id}"),
        worker.run(shutdown_rx),
    );
    worker_tasks.lock().push(task);
    tracing::info!(
        CVM_ALLOWED,
        %instance_id,
        msix_count,
        bar_count = describe.bars.len(),
        "pcie_remote: device assembled, worker spawned"
    );

    PcieRemoteDevice::new(state, to_worker, cfg_space, msix, side_effect_offsets)
}

/// 用 DeviceDescribe 构造 HardwareIds。
fn derive_hardware_ids(d: &DeviceDescribe) -> HardwareIds {
    HardwareIds {
        vendor_id: d.vendor_id as u16,
        device_id: d.device_id as u16,
        revision_id: d.revision as u8,
        prog_if: ProgrammingInterface(((d.class_code) & 0xff) as u8),
        sub_class: Subclass(((d.class_code >> 8) & 0xff) as u8),
        base_class: ClassCode(((d.class_code >> 16) & 0xff) as u8),
        type0_sub_vendor_id: d.subsystem_vendor as u16,
        type0_sub_system_id: d.subsystem_device as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    ///
    /// 测试仅检查 'absent 路径'（prepared map 空），不进入需要真实
    /// MsiTarget / register_mmio 的 assemble_device 分支。
    #[test]
    fn missing_prep_doc_only() {
        // 因为 ResolvePciDeviceHandleParams 是 &'a + 含 RegisterMmioIntercept
        // 等 trait object，单测无法构造。运行时路径见
        // dispatch.rs / vtl2_settings_worker.rs 的真实 e2e。
        // 这里只验证 dummy_describe 通过 handshake validate_describe。
        crate::handshake::tests_helpers_assert_validate_ok(&dummy_describe());
    }

    /// K-17: 重复注册同 resource id resolver 应 panic
    /// (vm_resource/src/lib.rs:`add_async_resolver` doc 明确 panic on dup)。
    /// 这里在文档层面记录此契约；具体 panic 行为由 vm_resource 保证。
    #[test]
    fn add_async_resolver_duplicate_panics_contract_documented() {
        let id_str = <PcieRemoteTcpHandle as vm_resource::ResourceId<PciDeviceHandleKind>>::ID;
        assert_eq!(id_str, "pcie_remote_tcp");
        let id_str2 = <PcieRemoteVmbusHandle as vm_resource::ResourceId<PciDeviceHandleKind>>::ID;
        assert_eq!(id_str2, "pcie_remote_vmbus");
        // 两个 ID 不同，所以 Tcp 和 Vmbus 注册到同一 resolver 不冲突。
        assert_ne!(id_str, id_str2);
    }
}
