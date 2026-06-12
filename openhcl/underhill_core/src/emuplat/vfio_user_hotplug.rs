// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Layer C C0：emulated vfio-user NVMe 设备的 **guest 运行时热插拔**。
//!
//! # 目标（C0，最小机制门）
//!
//! 让 vfio_user NVMe 设备的 **VpciBus 层**随 usnvmemu（VTL2 内 vfio-user server）的
//! Live/Lost 在 guest 里动态 add/remove：
//! - usnvmemu 起 → device 转 `Live` → guest 里 PnP 出现 NVMe 盘；
//! - usnvmemu 停（`pkill`）→ device 转 `Lost` → guest PnP 移盘；
//! - usnvmemu 重起 → re-add bus → 盘回来。
//!
//! 而 **device shim**（`VfioUserPciDevice` + worker + reconnect connector + irq tasks）
//! **boot 时装配一次、长存**（不随 add/remove 重建）。每周期重建 device shim 会丢
//! in-flight read / MSI-X 表 / 重启 worker+connector+eventfd（毁 C-3 reconnect 状态）。
//! 这是 Layer C 区别于 `vpci_relay`（每次全新设备）的核心。
//!
//! # 架构（照 `vpci_relay`，**不是** `netvsp`）
//!
//! reconcile 作为 **dispatch 主循环的一个 select 臂**运行（见 `dispatch/mod.rs`：
//! `wait_event()` 在 select 臂、`process(...)` 在 match 臂），**不是**独立 spawned task。
//! 理由：
//! - `ChipsetDevices::add_dyn_device(&self, units: &StateUnits, ...)` + 之后
//!   `StateUnits::start_stopped_units(&mut self)` 需要对 dispatch 长存的
//!   `chipset_devices` / `state_units` 的（可变）访问。`StateUnits` **非 `Clone`**，
//!   无法搬进独立 task。dispatch loop **单线程独占**两者 → 在它的一个 select 臂里跑
//!   reconcile 是唯一能干净拿到 `&ChipsetDevices` + `&mut StateUnits` 的地方，**零并发危险**。
//! - `netvsp` 的 worker 是独立 task，用 `VpciBusControl::offer_device()/revoke`，**从不**
//!   碰 ChipsetDevices/StateUnits —— 那是 VF 中继模型，不能 `add_dyn_device`，是错误范本。
//!   `vpci_relay`（同样用 `add_dyn_device`/`DynamicDeviceUnit::remove` 做 runtime
//!   add/remove）才是对的范本。
//!
//! # MSI 链 + 虚拟设备的生命周期（C0 真机 POC 修正）
//!
//! device shim 的 `MsixEmulator` 装配时把一个 [`MsiTarget`] clone 进每个 table entry。
//! `MsiTarget` 内部是 `Arc<RwLock<..>>`。本模块在 **assemble 时一次性**：
//! 1. `partition.new_virtual_device()?.build(Vtl0, device_id)` 造虚拟设备
//!    （`Arc<dyn SignalMsi> + MapVpciInterrupt`），**device_id 注册只此一次**；
//! 2. `msi_conn.connect(virtual_device)` 把 MSI 投递指向它（持久，之后不再 connect）；
//! 3. 存下 [`VpciInterruptMapper`]（内部 `Arc`，`Clone`）。
//!
//! 之后每个 Live 边沿 `add_bus` 只 `interrupt_mapper.clone()` 交给新建的 `VpciBus`，
//! **不**重建虚拟设备。**为什么必须一次性**：虚拟设备 + device_id 是分区级资源；若每个
//! Live 重建，旧的仍被持久 `msi_conn`/`interrupt_mapper` 持有（device_id 未释放），
//! re-add 时 `build(device_id)` 撞 "device id already in use" —— C0 真机 POC 正是
//! 撞到这个（kill→重起 usnvmemu 后盘不回来）。故虚拟设备与 device shim 同生命周期长存，
//! 只有薄壳 `VpciBus` 随 add/remove；`build_vpci_device`（boot 路径）本就一次性建好，
//! 语义一致。
//!
//! # C0 范围
//!
//! **无 debounce**（C2 才加）：先把机制跑通。Live→add，Lost→remove，串行（单一
//! reconcile 入口，dispatch loop 天然串行）。拆除**必须**用
//! [`DynamicDeviceUnit::remove`]（连 chipset device unit + 2 个 MMIO config 区域一起拆，
//! drop `VpciBus` → `SimpleDeviceHandle` Drop = rescind），**绝不**用
//! `SimpleDeviceHandle::revoke`（只 await offer task，泄漏 device unit + MMIO）。

#![cfg(feature = "vpci")]

use anyhow::Context as _;
use chipset_device::ChipsetDevice;
use closeable_mutex::CloseableMutex;
use cvm_tracing::CVM_ALLOWED;
use futures::StreamExt as _;
use guestmem::GuestMemory;
use pci_core::bus_range::AssignedBusRange;
use pci_core::msi::MsiConnection;
use state_unit::StateUnits;
use std::sync::Arc;
use vfio_user_pci_device::DeviceState;
use vfio_user_pci_device::SharedState;
use vfio_user_pci_device::VfioUserPciDevice;
use vfio_user_pci_device::WorkerTasks;
use vfio_user_pci_resources::VfioUserNvmeHandle;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vpci_msi::VpciInterruptMapper;
use vmotherboard::ChipsetDevices;
use vmotherboard::DynamicDeviceUnit;

/// 由 boot 路径（`worker.rs`）造好交给 dispatch 的一条 vfio_user 热插拔上下文。
///
/// 持有**长存** device shim（device unit + Arc + SharedState）+ 重建 VpciBus 所需的
/// 全部「重建上下文」（持久 MsiConnection、vmbus、vtom、instance_id、driver_source、
/// partition）+ 当前 bus unit（`Option`，Lost 时为 `None`）+ Live/Lost 边沿 receiver。
pub struct VfioUserHotplug {
    /// vpci bus_instance_id（offer 给 guest 的 GUID）。
    instance_id: guid::Guid,

    /// **长存** device shim 句柄（`add_dyn_device` 产物）。coerce 成
    /// `dyn ChipsetDevice` 交给 `VpciBus::new`。boot 装配一次，整个 VM 生命周期不重建。
    device: Arc<CloseableMutex<VfioUserPciDevice>>,

    /// device shim 对应的长存 device unit。**只在 VM teardown 时随本结构 drop**
    /// （drop = `SpawnedUnit` Drop，移除 unit）；add/remove 周期中**绝不**动它。
    /// 字段保活，无运行时读取，故 `_` 前缀。
    _device_unit: DynamicDeviceUnit,

    /// device 状态机句柄（Connecting/Live/Lost）。reconcile 据 `load()` 读当前态做幂等决策。
    state: SharedState,

    /// 持久 MSI 连接（与 device shim 的 MsixEmulator 共享同一 `MsiTarget`）。assemble
    /// 时 `connect(虚拟设备)` **一次**，之后只为保活该连接而持有（故 `_` 前缀，无运行时
    /// 读取）。虚拟设备/device_id 长存，不随 bus add/remove 重建。
    _msi_conn: MsiConnection,

    /// vmbus 控制句柄（offer VpciBus channel 用）。
    vmbus: Arc<vmbus_server::VmbusServerControl>,

    /// vtom（隔离偏移；非隔离为 `None`）。透传给 `VpciBus::new`。
    vtom: Option<u64>,

    /// 任务驱动源（spawn offer task + VpciBus 内部 task 用）。
    driver_source: VmTaskDriverSource,

    /// 持久 `VpciInterruptMapper`：assemble 时**一次性**造虚拟设备得到，跨 add/remove
    /// 周期复用 `clone()`。**虚拟设备 + 其 device_id 注册只建一次**——若每个 Live 边沿
    /// 重建会撞 "device id already in use"（C0 真机 POC 实测：kill→重起 usnvmemu 后
    /// re-add 时 `new_virtual_device().build(device_id)` 因 device_id 仍被持久 msi_conn
    /// 持有而失败）。虚拟设备是**分区级资源**，应与 device shim 同生命周期长存，而非
    /// 随 bus add/remove。`VpciInterruptMapper` 内部 = `Arc<dyn DynMapVpciInterrupt>`，
    /// `Clone` 即增引用，每个 add_bus clone 一份交给新 VpciBus。
    interrupt_mapper: VpciInterruptMapper,

    /// 当前 VpciBus 的 unit；`Some` = 已 add（盘对 guest 可见），`None` = 未 add（Lost / 初始）。
    bus_unit: Option<DynamicDeviceUnit>,

    /// device 的 Live/Lost 边沿通知 receiver（device shim 的 `SharedState` 在转
    /// Live/Lost 时投递新状态）。
    edge_rx: mesh::Receiver<DeviceState>,
}

/// 抽掉 `UhPartition` 的具体类型：reconcile 只需要「能造虚拟设备」这一能力。
///
/// 用 trait object 持有，避免 underhill_core 在本模块处处写死 `virt_mshv_vtl::UhPartition`
/// 泛型（worker.rs 处也是 `Arc<UhPartition>`，构造时即可 erase 成本 trait）。
pub trait DeviceBuilderPartition: Send + Sync {
    /// 造一个 VTL0 虚拟设备：返回 `(SignalMsi 句柄, VpciInterruptMapper)`，与
    /// `build_vpci_device` 的 `new_virtual_device` 闭包同义。
    fn build_virtual_device(
        &self,
        device_id: u64,
    ) -> anyhow::Result<(Arc<dyn pci_core::msi::SignalMsi>, VpciInterruptMapper)>;
}

/// 为任意实现了 `virt::Hv1`（且 `Device: MapVpciInterrupt + SignalMsi`）的分区
/// 实现 [`DeviceBuilderPartition`]，逻辑照搬 `worker.rs:3489-3496` 的
/// `new_virtual_device` 闭包。
impl<P> DeviceBuilderPartition for P
where
    P: virt::Hv1 + Send + Sync,
    P::Device: vmcore::vpci_msi::MapVpciInterrupt + pci_core::msi::SignalMsi + 'static,
{
    fn build_virtual_device(
        &self,
        device_id: u64,
    ) -> anyhow::Result<(Arc<dyn pci_core::msi::SignalMsi>, VpciInterruptMapper)> {
        let device = self
            .new_virtual_device()
            .context("vpci is not supported by this hypervisor")?
            .build(hvdef::Vtl::Vtl0, device_id)
            .context("failed to build virtual device")?;
        let device = Arc::new(device);
        Ok((device.clone(), VpciInterruptMapper::new(device)))
    }
}

impl VfioUserHotplug {
    /// 在 dispatch loop 的 `chipset_devices`/`state_units` 上下文里，为一条 vfio_user
    /// 配置装配**长存** device shim，并返回热插拔上下文。
    ///
    /// 必须在 `chipset_builder.build()` 之后调用（`add_dyn_device` 是 `ChipsetDevices`
    /// 的运行时 API）。boot 时 usnvmemu 未起也照样装配 device shim（初始 Connecting，
    /// 对 guest show-absent）—— bus 在首个 Live 边沿才 add（冷插由后续 C3 完整化；
    /// C0 只要 Live→add / Lost→remove 机制通）。
    ///
    /// `worker_tasks`：worker/connector/irq task 保活容器（调用方持到进程退出）。
    #[expect(clippy::too_many_arguments)]
    pub async fn assemble(
        chipset_devices: &ChipsetDevices,
        state_units: &StateUnits,
        driver_source: &VmTaskDriverSource,
        worker_tasks: &WorkerTasks,
        handle: VfioUserNvmeHandle,
        guest_memory: &GuestMemory,
        vmbus: Arc<vmbus_server::VmbusServerControl>,
        partition: Arc<dyn DeviceBuilderPartition>,
        vtom: Option<u64>,
    ) -> anyhow::Result<Self> {
        let instance_id = handle.instance_id;

        // 持久 MSI 连接：device shim 的 MsixEmulator 绑定它的 `target()`；每个 Live
        // 边沿 `connect(新虚拟设备)`。devfn=0 / 空 bus_range 与 `build_vpci_device`
        // （device_builder.rs:61）一致——VPCI 设备的 BDF 身份不经此路径承载。
        let msi_conn = MsiConnection::new(AssignedBusRange::new(), 0);

        // device 的 Live/Lost 边沿 channel：device shim 的 SharedState 转 Live/Lost 时
        // 投递新状态，reconcile（dispatch 臂）据此 add/remove bus。
        let (edge_tx, edge_rx) = mesh::channel::<DeviceState>();

        // 经 add_dyn_device 把 device shim 登记为**长存** chipset device unit（不挂
        // PCI bus，单独存在合法——等同 build_vpci_device 的 `with_external_pci`）。
        // 闭包内拿到的 `register_mmio` 与 resolver 路径 `services.register_mmio()` 同义，
        // 交给可复用的 `build_device_shim` 装配 device（含 spawn worker/irq/connector）。
        // `build_device_shim` 也返回 SharedState，但闭包只能返回 device 本体；故装配后
        // 经 `device.lock().shared_state()` 取同一状态句柄（避免 Mutex 走私）。
        let msi_target = msi_conn.target().clone();
        let device_name: Arc<str> = format!("vfio_user_nvme:device-{instance_id}").into();
        let (device_unit, device) = chipset_devices
            .add_dyn_device(
                driver_source,
                state_units,
                device_name,
                async |register_mmio| {
                    let (device, _state) = vfio_user_pci_device::build_device_shim(
                        worker_tasks,
                        &handle,
                        register_mmio,
                        &msi_target,
                        driver_source,
                        guest_memory,
                        Some(edge_tx),
                    )
                    .await?;
                    anyhow::Ok(device)
                },
            )
            .await
            .context("failed to assemble long-lived vfio_user device shim")?;

        // 取与 device shim（及其 worker）共享的状态句柄：reconcile 据它读 Live/Lost。
        let state = device.lock().shared_state();

        // **一次性**造虚拟设备并把持久 `msi_conn` 指向它。device_id 注册只此一次、与
        // device shim 同生命周期长存，跨 add/remove 周期复用 `interrupt_mapper.clone()`
        // —— 避免每个 Live 边沿重建撞 "device id already in use"（C0 真机 POC 实测的
        // 根因）。device_id 与 `build_vpci_device`（device_builder.rs:66）一致。
        let device_id = (instance_id.data2 as u64) << 16 | (instance_id.data3 as u64 & 0xfff8);
        let (msi_controller, interrupt_mapper) = partition
            .build_virtual_device(device_id)
            .context("failed to create virtual device for vfio_user hotplug")?;
        msi_conn.connect(msi_controller);

        tracing::info!(
            CVM_ALLOWED,
            %instance_id,
            "vfio_user hotplug: long-lived device shim assembled (bus not yet offered)"
        );

        Ok(Self {
            instance_id,
            device,
            _device_unit: device_unit,
            state,
            _msi_conn: msi_conn,
            vmbus,
            vtom,
            driver_source: driver_source.clone(),
            interrupt_mapper,
            bus_unit: None,
            edge_rx,
        })
    }

    /// 等待下一个 Live/Lost 边沿（dispatch loop select 臂用；可取消）。
    ///
    /// `None`（channel 关闭，device shim 整体退出）→ 永远 pending，让本臂不再唤醒。
    pub async fn wait_event(&mut self) -> DeviceState {
        match self.edge_rx.next().await {
            Some(state) => state,
            None => std::future::pending().await,
        }
    }

    /// 处理一次边沿：按 device 的**当前**状态收敛 bus 的 add/remove（幂等）。
    ///
    /// **不可取消**（与 `vpci_relay::process` 同纪律：add/remove 半途取消会泄漏/撕裂）。
    /// 串行：本方法只从 dispatch loop 单点调用，不并发。
    ///
    /// - `Live` 且当前无 bus → `add_bus`（造虚拟设备 + connect + add_dyn_device(VpciBus)
    ///   + `start_stopped_units`）。
    /// - 非 `Live`（`Lost`/`Connecting`）且当前有 bus → `remove_bus`
    ///   （`DynamicDeviceUnit::remove`）。
    /// - 其余（状态与 bus 已一致）→ no-op（幂等吸收抖动/冗余通知）。
    pub async fn process(
        &mut self,
        chipset_devices: &ChipsetDevices,
        state_units: &mut StateUnits,
    ) -> anyhow::Result<()> {
        let live = matches!(self.state.load(), DeviceState::Live);
        match (live, self.bus_unit.is_some()) {
            (true, false) => self.add_bus(chipset_devices, state_units).await?,
            (false, true) => self.remove_bus().await,
            _ => {}
        }
        Ok(())
    }

    /// Live 边沿：造虚拟设备 + connect MSI + `add_dyn_device` 构造 VpciBus（捕获**已存在**
    /// 的 device shim Arc + 新 interrupt_mapper）+ `start_stopped_units`。
    async fn add_bus(
        &mut self,
        chipset_devices: &ChipsetDevices,
        state_units: &mut StateUnits,
    ) -> anyhow::Result<()> {
        // coerce 长存 device shim → dyn ChipsetDevice（VpciBus::new 的入参类型）。
        let device: Arc<CloseableMutex<dyn ChipsetDevice>> = self.device.clone();
        // 复用 assemble 时**一次性**造好的持久 interrupt_mapper（`clone()` = 增 Arc
        // 引用，**不**重建虚拟设备、**不**重注册 device_id）。虚拟设备 + msi 连接在
        // assemble 已建好长存，跨 add/remove 周期不变（修 C0 真机 POC 暴露的
        // "device id already in use" re-add 失败）。
        let interrupt_mapper = self.interrupt_mapper.clone();

        let instance_id = self.instance_id;
        let driver_source = &self.driver_source;
        let vmbus = self.vmbus.clone();
        let vtom = self.vtom;
        let bus_name: Arc<str> = format!("vfio_user_nvme:vpci-{instance_id}").into();

        let (bus_unit, _bus) = chipset_devices
            .add_dyn_device(
                driver_source,
                state_units,
                bus_name,
                async |register_mmio| {
                    let bus = vpci::bus::VpciBus::new(
                        driver_source,
                        instance_id,
                        device,
                        register_mmio,
                        vmbus.as_ref(),
                        interrupt_mapper,
                        vtom,
                    )
                    .await?;
                    anyhow::Ok(bus)
                },
            )
            .await
            .context("failed to add vfio_user VpciBus")?;

        self.bus_unit = Some(bus_unit);
        // add_dyn_device 加入的 unit 初始 stopped；若 VM 在跑则启动它（VpciBus offer
        // 在 new 内已发生，这里启动 channel state unit）。
        state_units.start_stopped_units().await;

        tracing::info!(
            CVM_ALLOWED,
            %instance_id,
            "vfio_user hotplug: VpciBus offered (device Live, disk should appear in guest)"
        );
        Ok(())
    }

    /// Lost 边沿：用 [`DynamicDeviceUnit::remove`] 拆 VpciBus（连 chipset device unit +
    /// MMIO config 区域一起拆 → 无泄漏；drop VpciBus → SimpleDeviceHandle Drop = rescind
    /// → guest PnP 移盘）。**绝不**用 `revoke`。
    async fn remove_bus(&mut self) {
        if let Some(bus_unit) = self.bus_unit.take() {
            bus_unit.remove().await;
            tracing::info!(
                CVM_ALLOWED,
                instance_id = %self.instance_id,
                "vfio_user hotplug: VpciBus removed (device Lost, disk should disappear from guest)"
            );
        }
    }
}
