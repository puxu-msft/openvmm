// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `VfioUserNvmeHandle` 的动态 resolver（W6b A2.3，assemble-always + 持久重连）。
//!
//! 这是 PCI 设备工厂：当 OpenHCL 的 VPCI 层 resolve 一个 [`VfioUserNvmeHandle`]
//! 时，本 resolver **总是**装配出一个 [`VfioUserPciDevice`]（不再依赖 boot 期预填的
//! prepared map）：
//! - 用 [`DeclaredGeometry`]（handle 的 CLI override 或内置默认）构造 `MsixEmulator`
//!   + `DeviceBars` + `ConfigSpaceType0Emulator`（身份用 [`declared_hardware_ids`]）；
//! - 创建持久 MSI-X eventfd（owned `pal_event::Event`），一份留给 irq task
//!   （[`crate::irq::irq_wait_loop`]）做 VTL0 注入，一份 clone 给 connector（C-3：
//!   每次重连重新 `set_irqs` 同一批 eventfd）；
//! - 起 [`crate::worker::Worker`]（初始 `Connecting`，C-2 show-absent-until-Live）+
//!   持久重连引擎 [`crate::reconnect::reconnect_loop`]（首次连接 + 后续掉线重连），
//!   两者经 `reconnect`/`lost` 两条 channel 协作。
//!
//! 相对 A1 一次性 connect 的改造（A2.3）：
//! - **删** prepared map / set_irqs / into_channel：connect、握手、identity 校验、
//!   set_irqs、into_channel 全下沉到 [`reconnect_loop`]，resolver 只负责 guest-facing
//!   仿真器装配 + spawn 两个 task。
//! - **assemble-always**：resolve 时立即返回 live 设备（初始 Connecting，对 guest
//!   show-absent），backend 真正连上才转 Live。只有装配本身失败（如 `PolledWait::new`
//!   epoll 注册错）才发 `AbsentPcieDevice` 兜底。

use crate::AbsentPcieDevice;
use crate::MSIX_BAR_INDEX;
use crate::VfioUserPciDevice;
use crate::identity::DeclaredGeometry;
use crate::identity::declared_hardware_ids;
use crate::irq::irq_wait_loop;
use crate::reconnect::ReconnectChannels;
use crate::reconnect::reconnect_loop;
use crate::state::DeviceState;
use crate::state::SharedState;
use crate::worker::DeviceRequest;
use crate::worker::ReconnectEvent;
use crate::worker::SharedWorkerStats;
use crate::worker::Worker;
use async_trait::async_trait;
use cvm_tracing::CVM_ALLOWED;
use guestmem::ShareableRegion;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::wait::PolledWait;
use pal_event::Event;
use parking_lot::Mutex;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::cfg_space_emu::BarMemoryKind;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use std::sync::Arc;
use vfio_user_pci_resources::VfioUserNvmeHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::kind::PciDeviceHandleKind;
use vmcore::interrupt::Interrupt;

// pci_resources types — 通过本 crate 的 pci_resources dependency 暴露。
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;

/// 共享 worker tasks 持有器：`assemble_device` 时 push（worker + 重连引擎），
/// 调用方（underhill_core）持到进程结束。
///
/// **生命周期不变量**：drop `WorkerTasks` Arc 等于取消所有正在运行的 task。调用方
/// 必须把 Arc 持到设备生命周期结束（实践中即进程退出），否则会半路杀死正在收发的
/// worker / 正在重连的 connector。
pub type WorkerTasks = Arc<Mutex<Vec<Task<()>>>>;

/// vfio-user NVMe 设备 resolver（单 handle，OpenHCL 路径）。
///
/// A2.3：不再持 prepared map —— connect 下沉到 [`reconnect_loop`]，resolver 只持
/// `worker_tasks` 给装配出的 worker / connector task 保活。
pub struct VfioUserPciResolver {
    worker_tasks: WorkerTasks,
}

impl VfioUserPciResolver {
    /// 构造。`worker_tasks` 由调用方持有到进程结束（保活 worker + 重连引擎 task）。
    pub fn new(worker_tasks: WorkerTasks) -> Self {
        Self { worker_tasks }
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
        Ok(resolve_one(&self.worker_tasks, handle, params).await)
    }
}

/// assemble-always：总是装配 live 设备（初始 Connecting）；仅装配本身失败才发
/// `AbsentPcieDevice`（spec §3.10 layer-2，避免 host-controllable boot DoS）。
async fn resolve_one(
    worker_tasks: &WorkerTasks,
    handle: VfioUserNvmeHandle,
    params: ResolvePciDeviceHandleParams<'_>,
) -> ResolvedPciDevice {
    assemble_device(worker_tasks, handle, params).await
}

/// 装配完整 `VfioUserPciDevice`：MsixEmulator + DeviceBars + cfg_space + 持久 MSI-X
/// eventfd（irq task + connector 各持一份）+ spawn worker / irq tasks / 重连引擎。
///
/// 设备初始状态 `Connecting`（C-2 show-absent-until-Live）；connector 首次连接成功
/// 后经 `reconnect` channel 把 transport 交给 worker → worker 转 Live。装配失败
/// （`PolledWait::new`）一律发放 `AbsentPcieDevice` 兜底，绝不 panic boot。
async fn assemble_device(
    worker_tasks: &WorkerTasks,
    handle: VfioUserNvmeHandle,
    params: ResolvePciDeviceHandleParams<'_>,
) -> ResolvedPciDevice {
    let driver = params.driver_source.simple(); // VmTaskDriver：既是 Driver 又能 Spawn。
    let instance_id = handle.instance_id;

    // 声明几何（CLI override 或内置默认）。connector 据此做 identity 校验上界；
    // resolver 据 declared.bar0_size / declared.msix_count 构造 BAR window + MSI-X 容量。
    let declared = DeclaredGeometry::new(handle.bar0_size, handle.msix_count);
    let msix_count = declared.msix_count;

    // 1. MSI-X emulator（BAR4）。
    let (msix, msix_cap) = MsixEmulator::new(MSIX_BAR_INDEX, msix_count, params.msi_target);

    // 2. interrupts vec：每个 MSI-X 槽位一个 Interrupt 句柄。
    let interrupts: Vec<Interrupt> = (0..msix_count)
        .map(|i| msix.interrupt(i).expect("i < msix_count"))
        .collect();

    // 3. DeviceBars：
    //    - BAR0 = 声明的 `bar0_size`。**M-1**：`DeviceBars::bar0` 被 pci_core 自动
    //      呈现为 64-bit memory BAR（占用 BAR0+BAR1 寄存器对），这正是 NVMe 所要求
    //      的（NVMe controller registers 走 64-bit BAR0），无需特殊处理。
    //    - BAR4 = MSI-X table/PBA。
    let bars = DeviceBars::new()
        .bar0(
            declared.bar0_size,
            BarMemoryKind::Intercept(
                params
                    .register_mmio
                    .new_io_region("bar0", declared.bar0_size),
            ),
        )
        .bar4(
            msix.bar_len(),
            BarMemoryKind::Intercept(params.register_mmio.new_io_region("msix", msix.bar_len())),
        );

    // 4. cfg_space（身份用声明默认值；连接器另读 firmware 真身份做校验，但 guest-facing
    //    身份固定 = declared，避免 backend drift 改变 guest 看到的 PCI 身份）。
    let cfg_space = ConfigSpaceType0Emulator::new(
        declared_hardware_ids(),
        vec![Box::new(msix_cap)],
        Vec::new(),
        bars,
    );

    // 5. 持久 MSI-X eventfd（**C-3**）：每向量一个 owned `Event`。
    //    - `events`：移交给 irq task（PolledWait + irq_wait_loop，VTL0 注入）。
    //    - `connector_events`：clone（dup 底层 eventfd）给 connector，每次重连重新
    //      `set_irqs` 同一批 fd。`Event` 的 Clone 复制 fd，故两侧指向同一内核 eventfd：
    //      firmware 写 → irq task 唤醒。
    let events: Vec<Event> = (0..msix_count).map(|_| Event::new()).collect();
    let connector_events: Vec<Event> = events.iter().map(Clone::clone).collect();

    // 6. 两条 channel：
    //    - reconnect（connector → worker）：投递新连接（writer+reader 对，C-1）。
    //    - lost（worker → connector）：掉线边沿信号（驱动重连）。
    let (reconnect_tx, reconnect_rx) = mesh::channel::<ReconnectEvent>();
    let (lost_tx, lost_rx) = mesh::channel::<()>();

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

    // 8. device→worker channel + stats + state(Connecting, C-2) + spawn worker。
    let (to_worker, worker_inbox) = mesh::channel::<DeviceRequest>();
    let stats: SharedWorkerStats = Arc::new(Default::default());
    // C-2 初始 Connecting：backend 未连上前对 guest show-absent；connector 连上
    // 后经 reconnect channel 交付 transport，worker 转 Live。
    let state = SharedState::new(DeviceState::Connecting);
    // shutdown 通道：v1 不主动关 worker；靠 reconnect channel 关闭 / 设备 unbind。
    // forget sender 避免立刻 drop 让 worker 主循环误以为收到 shutdown。
    let (shutdown_tx, shutdown_rx) = mesh::channel::<()>();
    std::mem::forget(shutdown_tx);
    // interrupts 整体移交 worker 保活；irq tasks 已各自持一份 clone。
    let worker = Worker::new(
        state.clone(),
        worker_inbox,
        interrupts,
        irq_tasks,
        stats.clone(),
        reconnect_rx,
        lost_tx,
    );
    let worker_task = driver.spawn(
        format!("vfio_user_worker_{instance_id}"),
        worker.run(shutdown_rx),
    );

    // 8.5. W6c finding-④：取 VTL0 guest RAM 的可共享区间（逐 ram() 段），交给
    //      connector，每次重连经 vfio-user DMA_MAP 把每段 ship 给 VTL2 server，
    //      实现零拷贝 DMA。
    //      - 非隔离 VTL0 的 `sharing()` 返回 `Some`（mapping.rs no-bitmap 路径）；
    //      - 理论上非隔离不会是 `None`，但稳健起见：`None` → 空 vec + warn（设备
    //        仍装配，guest 可枚举，只是 DMA 不可用），绝不 panic boot。
    let dma_regions: Vec<ShareableRegion> = match params.guest_memory.sharing() {
        Some(sharing) => match sharing.get_regions().await {
            Ok(regions) => {
                tracing::info!(
                    CVM_ALLOWED,
                    %instance_id,
                    region_count = regions.len(),
                    "vfio_user_pci: collected {} shareable guest-RAM region(s) for DMA_MAP",
                    regions.len()
                );
                regions
            }
            Err(e) => {
                tracing::warn!(
                    CVM_ALLOWED,
                    %instance_id,
                    error = e.as_ref() as &dyn std::error::Error,
                    "vfio_user_pci: get_regions failed; device assembled without DMA mapping"
                );
                Vec::new()
            }
        },
        None => {
            tracing::warn!(
                CVM_ALLOWED,
                %instance_id,
                "vfio_user_pci: guest_memory.sharing() returned None; device assembled without DMA mapping (DMA unavailable)"
            );
            Vec::new()
        }
    };

    // 9. spawn 持久重连引擎（首次连接 + 后续掉线重连）。connector 持 connector_events
    //    做 C-3 set_irqs + dma_regions 做 W6c finding-④ DMA_MAP；与 worker 经
    //    reconnect/lost 两条 channel 协作。
    let connector_task = driver.spawn(
        format!("vfio_user_reconnect_{instance_id}"),
        reconnect_loop(
            driver.clone(),
            handle.unix_path.clone(),
            declared,
            connector_events,
            dma_regions,
            ReconnectChannels {
                reconnect_tx,
                lost_rx,
            },
        ),
    );

    // worker + connector 都进 worker_tasks 保活（调用方持到进程退出）。
    {
        let mut guard = worker_tasks.lock();
        guard.push(worker_task);
        guard.push(connector_task);
    }

    tracing::info!(
        CVM_ALLOWED,
        %instance_id,
        msix_count,
        bar0_size = declared.bar0_size,
        unix_path = handle.unix_path.as_str(),
        "vfio_user_pci: device assembled (Connecting), worker + irq + reconnect tasks spawned"
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

    /// 文档契约：装配失败时发 `AbsentPcieDevice`（而非 panic），boot DoS 兜底。
    ///
    /// 真正的 happy-path 装配（`assemble_device`）无法在单测构造：
    /// `ResolvePciDeviceHandleParams` 持 `&MsiTarget` / `&mut dyn
    /// RegisterMmioIntercept` / `&VmTaskDriverSource` / `&GuestMemory` 等运行期
    /// trait object 与 borrow，单测无法合成。这条路径由 Phase 2/3 的真 VM e2e
    /// 覆盖（guest 枚举并驱动设备），不是 unit test。此处仅以注释记录契约；
    /// 装配失败 → Absent 的代码路径见 `assemble_device` 的 `PolledWait::new` 分支。
    #[test]
    fn assembly_failure_serves_absent_contract_documented() {
        // `AbsentPcieDevice` 本身的 cfg-read-all-ones / cfg-write-ok 行为由
        // absent.rs 的单测覆盖；这里只断言它可被构造（assemble_device 的兜底产物），
        // 且能 `.into()` 成 `ResolvedPciDevice`（resolve_one 真正返回的类型）。
        let _resolved: ResolvedPciDevice = AbsentPcieDevice::new().into();
    }
}
