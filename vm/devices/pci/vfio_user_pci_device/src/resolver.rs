// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `VfioUserNvmeHandle` 的动态 resolver（W6b Task 1.6，Phase-1 收尾）。
//!
//! 这是 PCI 设备工厂：当 OpenHCL 的 VPCI 层 resolve 一个 [`VfioUserNvmeHandle`]
//! 时，本 resolver 从 boot 期由 connect 任务（Task 1.5）填好的 [`PreparedMap`]
//! 取出对应 [`PreparedVfioUserDevice`]，装配出可运行的 [`VfioUserPciDevice`]：
//! 构造 `MsixEmulator` + `DeviceBars` + `ConfigSpaceType0Emulator`，把 client
//! split 成全双工 worker channel，把 MSI-X eventfd 接到 `Interrupt::deliver`
//! （[`crate::irq::irq_wait_loop`]），并 spawn worker（[`crate::worker::Worker`]）。
//!
//! 相对模板 `pcie_remote_device::resolver` 的精简（W6b 单 handle / 无 DMA）：
//! - 删掉 Tcp/Vmbus 双 resolver 拆分 —— W6b 只有一个 [`VfioUserNvmeHandle`]。
//! - 删掉 `TransportSwapMap` / hotplug —— 重连是 W6d。
//! - 删掉 `derive_hardware_ids` —— prep 已携带解析好的 [`HardwareIds`]。
//! - 删掉 `guest_memory` / DMA 装配 —— DMA 走 `dma_map` 零拷贝是 Phase 3。
//!
//! 新增一个 boot-blocking await（`set_irqs`），**必须** timeout-guard（H-4）：
//! firmware 不可信，不能让 set_irqs 永久挂住 VM boot。

use crate::AbsentPcieDevice;
use crate::MSIX_BAR_INDEX;
use crate::VfioUserPciDevice;
use crate::irq::irq_wait_loop;
use crate::prepared::PreparedMap;
use crate::prepared::PreparedVfioUserDevice;
use crate::state::DeviceState;
use crate::state::SharedState;
use crate::worker::DeviceRequest;
use crate::worker::SharedWorkerStats;
use crate::worker::Worker;
use async_trait::async_trait;
use cvm_tracing::CVM_ALLOWED;
use mesh::CancelContext;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::wait::PolledWait;
use parking_lot::Mutex;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::cfg_space_emu::BarMemoryKind;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use std::sync::Arc;
use std::time::Duration;
use vfio_user_pci_resources::VfioUserNvmeHandle;
use vfio_user_wire::proto::pci_irq;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::kind::PciDeviceHandleKind;
use vmcore::interrupt::Interrupt;

// pci_resources types — 通过本 crate 的 pci_resources dependency 暴露。
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;

/// `set_irqs` 的 boot-blocking 超时上限（H-4）。
///
/// 这是装配阶段唯一的「向 firmware 同步发请求并等 reply」的 await。firmware 不可信
/// （另一个 VTL2 进程），若它故意不回 reply 会无限挂住本次 resolve → 卡死整个 VM
/// boot。5s 对 boot 期足够宽裕（firmware 应秒级响应），超时即放弃并发放
/// `AbsentPcieDevice` 兜底，绝不无限等待。
const SET_IRQS_TIMEOUT: Duration = Duration::from_secs(5);

/// 共享 worker tasks 持有器：`assemble_device` 时 push，调用方（underhill_core）
/// 持到进程结束。
///
/// **生命周期不变量**：drop `WorkerTasks` Arc 等于取消所有正在运行的 worker
/// task。调用方必须把 Arc 持到设备生命周期结束（实践中即进程退出），否则会
/// 半路杀死正在收发的 worker。
pub type WorkerTasks = Arc<Mutex<Vec<Task<()>>>>;

/// vfio-user NVMe 设备 resolver（单 handle，OpenHCL 路径）。
pub struct VfioUserPciResolver {
    prepared: PreparedMap,
    worker_tasks: WorkerTasks,
}

impl VfioUserPciResolver {
    /// 构造。`prepared` 由 boot 期 connect 任务（Task 1.5）填充；`worker_tasks`
    /// 由调用方持有到进程结束。
    pub fn new(prepared: PreparedMap, worker_tasks: WorkerTasks) -> Self {
        Self {
            prepared,
            worker_tasks,
        }
    }
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, VfioUserNvmeHandle> for VfioUserPciResolver {
    type Output = ResolvedPciDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _: &ResourceResolver,
        handle: VfioUserNvmeHandle,
        params: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(resolve_one(
            &self.prepared,
            &self.worker_tasks,
            handle.instance_id,
            params,
        )
        .await)
    }
}

/// 取 prepared 并装配；缺项 → `AbsentPcieDevice`（spec §3.10 layer-2，避免
/// host-controllable boot DoS）。
async fn resolve_one(
    prepared: &PreparedMap,
    worker_tasks: &WorkerTasks,
    instance_id: guid::Guid,
    params: ResolvePciDeviceHandleParams<'_>,
) -> ResolvedPciDevice {
    // `let Some(prep) = ...lock().remove() else` 的临时 lock guard 在 `;` 处即
    // 释放，`prep` 已 owned —— 故 lock **不**跨后续 `.await`（assemble 内有 await）。
    let Some(prep) = prepared.lock().remove(&instance_id) else {
        tracing::error!(
            CVM_ALLOWED,
            %instance_id,
            "vfio_user_pci prepared missing; serving AbsentPcieDevice"
        );
        return AbsentPcieDevice::new().into();
    };
    assemble_device(prep, worker_tasks, instance_id, params).await
}

/// 装配完整 `VfioUserPciDevice`：MsixEmulator + DeviceBars + cfg_space + MSI-X
/// eventfd 接线 + spawn worker / irq tasks。失败（set_irqs / PolledWait）一律
/// 发放 `AbsentPcieDevice` 兜底，绝不 panic boot。
async fn assemble_device(
    prep: PreparedVfioUserDevice,
    worker_tasks: &WorkerTasks,
    instance_id: guid::Guid,
    params: ResolvePciDeviceHandleParams<'_>,
) -> ResolvedPciDevice {
    let driver = params.driver_source.simple(); // VmTaskDriver：既是 Driver 又能 Spawn。
    let msix_count = prep.msix_count;

    // 1. MSI-X emulator（BAR4）。
    let (msix, msix_cap) = MsixEmulator::new(MSIX_BAR_INDEX, msix_count, params.msi_target);

    // 2. interrupts vec：每个 MSI-X 槽位一个 Interrupt 句柄。
    let interrupts: Vec<Interrupt> = (0..msix_count)
        .map(|i| msix.interrupt(i).expect("i < msix_count"))
        .collect();

    // 3. DeviceBars：
    //    - BAR0 = firmware 报告的 `bar0_size`。**M-1**：`DeviceBars::bar0` 被
    //      pci_core 自动呈现为 64-bit memory BAR（占用 BAR0+BAR1 寄存器对），这正
    //      是 NVMe 所要求的（NVMe controller registers 走 64-bit BAR0），无需特殊
    //      处理。
    //    - BAR4 = MSI-X table/PBA。
    let bars = DeviceBars::new()
        .bar0(
            prep.bar0_size,
            BarMemoryKind::Intercept(params.register_mmio.new_io_region("bar0", prep.bar0_size)),
        )
        .bar4(
            msix.bar_len(),
            BarMemoryKind::Intercept(params.register_mmio.new_io_region("msix", msix.bar_len())),
        );

    // 4. cfg_space（身份来自 prep.hardware_ids，已由 connect 任务从 CONFIG region
    //    解析；不经任何 protobuf）。
    let cfg_space = ConfigSpaceType0Emulator::new(
        prep.hardware_ids,
        vec![Box::new(msix_cap)],
        Vec::new(),
        bars,
    );

    // 5. MSI-X eventfd 接线（**H-4**：set_irqs 是装配期唯一 boot-blocking await，
    //    必须 timeout 包裹）。每个向量一个 eventfd，经 SCM_RIGHTS 发给 firmware；
    //    firmware fire 该向量时写对应 eventfd。
    let mut client = prep.client;
    let events: Vec<pal_event::Event> = (0..msix_count).map(|_| pal_event::Event::new()).collect();
    if msix_count > 0 {
        let fds: Vec<BorrowedFd<'_>> = events.iter().map(|e| e.as_fd()).collect();
        let mut ctx = CancelContext::new().with_timeout(SET_IRQS_TIMEOUT);
        match ctx
            .until_cancelled(client.set_irqs(pci_irq::MSIX, 0, &fds))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(
                    CVM_ALLOWED,
                    %instance_id,
                    error = %e,
                    "vfio_user_pci: set_irqs failed; serving AbsentPcieDevice"
                );
                return AbsentPcieDevice::new().into();
            }
            Err(_) => {
                tracing::error!(
                    CVM_ALLOWED,
                    %instance_id,
                    "vfio_user_pci: set_irqs timeout; serving AbsentPcieDevice"
                );
                return AbsentPcieDevice::new().into();
            }
        }
    }

    // 6. split client → 全双工 worker channel（writer 发请求帧 / reader 收 reply）。
    //    注意顺序：set_irqs 借用 `&mut client` 在前，into_channel consume client 在后。
    let (writer, reader) = client.into_channel();

    // 7. 每个 MSI-X 向量一个 eventfd-wait task：PolledWait(event) 唤醒 →
    //    interrupt.deliver()。PolledWait::new 失败（极罕见，epoll 注册错）→ 放弃
    //    装配，发 AbsentPcieDevice（优雅降级，不 panic boot）。
    let mut irq_tasks = Vec::new();
    for (i, event) in events.into_iter().enumerate() {
        let waiter = match PolledWait::new(&driver, event) {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(
                    CVM_ALLOWED,
                    %instance_id,
                    vector = i,
                    error = %e,
                    "vfio_user_pci: PolledWait::new failed; serving AbsentPcieDevice"
                );
                return AbsentPcieDevice::new().into();
            }
        };
        let interrupt = interrupts[i].clone();
        irq_tasks.push(driver.spawn(
            format!("vfio_user_irq_{instance_id}_{i}"),
            irq_wait_loop(waiter, interrupt),
        ));
    }

    // 8. device→worker channel + stats + state(Live) + spawn worker。
    let (to_worker, worker_inbox) = mesh::channel::<DeviceRequest>();
    let stats: SharedWorkerStats = Arc::new(Default::default());
    // prepared 在 map 时已 connect+handshake 完成，直接 Live。
    let state = SharedState::new(DeviceState::Live);
    // shutdown 通道 v1 不主动关 worker；靠 transport EOF / 设备 unbind。forget
    // sender 避免立刻 drop 让 worker 主循环误以为收到 shutdown。
    let (shutdown_tx, shutdown_rx) = mesh::channel::<()>();
    std::mem::forget(shutdown_tx);
    // interrupts 整体移交 worker 保活；irq tasks 已各自持一份 clone。
    let worker = Worker::new(
        writer,
        reader,
        state.clone(),
        worker_inbox,
        interrupts,
        irq_tasks,
        stats.clone(),
    );
    let task = driver.spawn(
        format!("vfio_user_worker_{instance_id}"),
        worker.run(shutdown_rx),
    );
    worker_tasks.lock().push(task);

    tracing::info!(
        CVM_ALLOWED,
        %instance_id,
        msix_count,
        bar0_size = prep.bar0_size,
        "vfio_user_pci: device assembled, worker + irq tasks spawned"
    );
    VfioUserPciDevice::new(state, to_worker, cfg_space, msix, stats).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_resource::ResourceId;

    /// handle 的 resource ID 契约：必须是 "vfio_user_nvme"（underhill 注册
    /// resolver 时按此 ID 路由 `VfioUserNvmeHandle`）。
    #[test]
    fn handle_resource_id_is_vfio_user_nvme() {
        let id = <VfioUserNvmeHandle as ResourceId<PciDeviceHandleKind>>::ID;
        assert_eq!(id, "vfio_user_nvme");
    }

    /// 文档契约：prepared map 缺项时 `resolve_one` 返回 `AbsentPcieDevice` 而非
    /// `bail!`（spec §3.10 layer-2，boot DoS 兜底）。
    ///
    /// 真正的 happy-path 装配（`assemble_device`）无法在单测构造：
    /// `ResolvePciDeviceHandleParams` 持 `&MsiTarget` / `&mut dyn
    /// RegisterMmioIntercept` / `&VmTaskDriverSource` / `&GuestMemory` 等运行期
    /// trait object 与 borrow，单测无法合成。这条路径由 Phase 2/3 的真 VM e2e
    /// 覆盖（guest 枚举并驱动设备），不是 unit test。此处仅以注释记录契约；
    /// missing→Absent 的代码路径见 `resolve_one` 的 `let Some(prep) = ... else`。
    #[test]
    fn missing_prep_serves_absent_contract_documented() {
        // `AbsentPcieDevice` 本身的 cfg-read-all-ones / cfg-write-ok 行为由
        // absent.rs 的单测覆盖；这里只断言它可被构造（resolve_one 的兜底产物），
        // 且能 `.into()` 成 `ResolvedPciDevice`（resolve_one 真正返回的类型）。
        let _resolved: ResolvedPciDevice = AbsentPcieDevice::new().into();
    }
}
