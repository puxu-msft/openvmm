// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Layer C：emulated vfio-user NVMe 设备在 OpenHCL guest 的呈现 —— **Option B：把 usnvmemu
//! 重启建模为 transient 后端停顿，而非设备热插拔**（真机定稿 2026-06-13）。
//!
//! # 目标与模型
//!
//! 设备在 guest 出现，并在 usnvmemu（VTL2 内 vfio-user server）重启时**透明恢复**：
//! - usnvmemu 起（首次 / boot-absent 后起）→ device 转 `Live` → guest PnP 出现 NVMe 盘（冷插）；
//! - usnvmemu 停（`pkill`）→ device 转 `Lost` → **设备对 guest 恒在（不移盘）**，仅底层 MMIO/DMA
//!   在 Lost 窗口返 Err（C-2 门控）；
//! - usnvmemu 重起 → **C-3 reconnect 透明重连**（重发 set_irqs+dma_map）→ 控制器恢复，如真硬件
//!   NVMe controller 短暂 reset：**快重启盘不消失、在途 IO 超时重试后恢复**。
//!
//! **为何是 transient-stall 而非 hot-remove/re-add**（真机 finding-⑦ 深挖坐实）：Windows pci.sys
//! 只在 guest **自己**重上电 bus FDO（`FDO_D0_EXIT`→`FDO_D0_ENTRY`）时才重枚举 VPCI 子设备；VSP 侧
//! 任何 push（C0 同-instance re-offer / C2-1 unsolicited `BUS_RELATIONS2` / C2-1-fix `INVALIDATE_BUS`
//! / Option A 换新 instance_id）都不能可靠让 guest 在运行时重出盘。故「Lost→hot-remove、Live→
//! hot-re-add」模型**在 Windows 不可闭环**，改用 transient-stall 绕开。完整方案对照（含 C0/C2-0/
//! C2-1/Option A 各自真机结果）见 `docs/superpowers/plans/2026-06-12-layer-c-vpci-hotplug.md`
//! 「已试方案与去留」表 + `usnvmemu/experiments/2026-06-12-layer-c-c0-real-vm/RESULT.md`。
//!
//! **已知限制**：长停顿（usnvmemu 停 > guest IO 超时窗口 ~8s）→ guest 自身**有序**移盘（无⑥崩，
//! guest 主导非 surprise-rescind）；之后恢复需 guest 侧介入（reboot / 设备管理器 rescan /
//! disable-enable）—— 与真硬件 NVMe 一致，纯 VSP 侧无法自动补（Option A POC 已证）。
//!
//! # device shim + VpciBus 通道：都长存
//!
//! **device shim**（`VfioUserPciDevice` + worker + reconnect connector + irq tasks）
//! **boot 时装配一次、长存**（不随状态重建）。每周期重建会丢 in-flight read / MSI-X 表 /
//! 重启 worker+connector+eventfd（毁 C-3 reconnect 状态）。**VpciBus channel** 亦**首个 Live
//! 边沿 add 一次、VM 全程长存**（Option B 不在运行时拆通道，Lost 是 no-op）。
//!
//! 生命周期对照：
//! - **device shim**（含虚拟设备 + device_id + MSI 连接）：boot 装配一次，VM 全程长存。
//! - **VpciBus channel + cmd_tx**：**首个 Live 边沿 add 一次**，VM 全程长存（不随 Lost 拆）。
//! - Lost 边沿 = **no-op**（设备恒在，C-3 reconnect 恢复）。`SetPresent`（`device_count` 0↔1）
//!   与 `hide_device`（graceful EJECT，**修 finding-⑥** 挂载脏卷 surprise-remove 崩）保留作
//!   **未来 operator 显式永久移除**的 scaffolding —— 它们是 C2-1 路径 B 的遗留机制，在 Option B
//!   下运行时不触发（device_count 恒 1）。
//!
//! # 架构（照 `vpci_relay`，**不是** `netvsp`）
//!
//! reconcile 作为 **dispatch 主循环的一个 select 臂**运行（见 `dispatch/mod.rs`：
//! `wait_event()` 在 select 臂、`process(...)` 在 match 臂），**不是**独立 spawned task。
//! 理由：`ChipsetDevices::add_dyn_device(&self, units: &StateUnits, ...)` + 之后
//! `StateUnits::start_stopped_units(&mut self)` 需要对 dispatch 长存的 `chipset_devices` /
//! `state_units` 的（可变）访问。`StateUnits` **非 `Clone`**，无法搬进独立 task。dispatch
//! loop **单线程独占**两者 → 在它的一个 select 臂里跑 reconcile 是唯一能干净拿到
//! `&ChipsetDevices` + `&mut StateUnits` 的地方，**零并发危险**。
//!
//! 注意 C2-1 后，**只有首个 Live 边沿**真正需要 `add_dyn_device`（add 通道）；后续边沿
//! 只发 `SetPresent` 命令（不碰 chipset/state_units）。`add_dyn_device` 仍必须在 dispatch
//! 上下文里做，故 reconcile 仍留在 dispatch 臂。
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
//! `add_bus`（仅首个 Live 边沿调用一次）`interrupt_mapper.clone()` 交给新建的 `VpciBus`，
//! **不**重建虚拟设备。**为什么必须一次性**：虚拟设备 + device_id 是分区级资源；若重建，
//! 旧的仍被持久 `msi_conn`/`interrupt_mapper` 持有（device_id 未释放），撞 "device id
//! already in use"（C0 真机 POC 实测）。C2-1 通道恒在后，连这条路径都不再有 re-add 风险了
//! —— `add_bus` 全程只跑一次。
//!
//! # 拆除（仅 VM teardown）
//!
//! C2-1 不再在运行时拆 VpciBus。VpciBus unit（`bus_unit`）与 device shim unit
//! （`_device_unit`）一样持有到 VM teardown（随 `VfioUserHotplug` drop = `SpawnedUnit`
//! Drop → 移除 unit + drop VpciBus → `SimpleDeviceHandle` Drop = rescind + drop config
//! MMIO 区域 → unmap，无泄漏）。运行时设备进出全靠 `SetPresent`，**绝不**用 `revoke`
//! （只 await offer task，泄漏 device unit + MMIO）。

#![cfg(feature = "vpci")]

use anyhow::Context as _;
use chipset_device::ChipsetDevice;
use closeable_mutex::CloseableMutex;
use cvm_tracing::CVM_ALLOWED;
use futures::FutureExt as _;
use futures::StreamExt as _;
use guestmem::GuestMemory;
use pal_async::timer::PolledTimer;
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
/// 持有**长存** device shim（device unit + Arc + SharedState）、构造 VpciBus 所需的
/// 全部上下文（持久 MsiConnection、vmbus、vtom、instance_id、driver_source、partition）、
/// **长存** VpciBus unit（`Option`，首个 Live 边沿 add 后恒 `Some`）、命令 sender、
/// 编排侧 device_present 视图，以及 Live/Lost 边沿 receiver。
pub struct VfioUserHotplug {
    /// vpci bus_instance_id（offer 给 guest 的 GUID）。
    instance_id: guid::Guid,

    /// **长存** device shim 句柄（`add_dyn_device` 产物）。coerce 成
    /// `dyn ChipsetDevice` 交给 `VpciBus::new`。boot 装配一次，整个 VM 生命周期不重建。
    device: Arc<CloseableMutex<VfioUserPciDevice>>,

    /// device shim 对应的长存 device unit。**只在 VM teardown 时随本结构 drop**
    /// （drop = `SpawnedUnit` Drop，移除 unit）；运行时**绝不**动它。
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

    /// 持久 `VpciInterruptMapper`：assemble 时**一次性**造虚拟设备得到。**虚拟设备 +
    /// 其 device_id 注册只建一次**，与 device shim 同生命周期长存（避免撞 "device id
    /// already in use"，C0 真机 POC 实测根因）。`VpciInterruptMapper` 内部 =
    /// `Arc<dyn DynMapVpciInterrupt>`，`Clone` 即增引用，`add_bus`（仅一次）clone 一份
    /// 交给 VpciBus。
    interrupt_mapper: VpciInterruptMapper,

    /// **长存** VpciBus unit；`None` = 尚未 add（boot 起 usnvmemu 未起 / 首个 Live 之前），
    /// `Some` = 已 add（**此后恒 `Some`，VM 全程不拆**——C2-1 路径 B 的核心：通道恒在）。
    /// 运行时设备进出靠 `SetPresent` 切 `device_count`，不拆此 unit。随本结构 drop（VM
    /// teardown）时才移除。
    bus_unit: Option<DynamicDeviceUnit>,

    /// VpciBus 通道的运行时命令 sender（graceful EJECT + SetPresent）。
    ///
    /// **`add_bus` 建一次后长存**（与 `bus_unit` 同步）：sender 存这里、receiver 经
    /// `VpciBus::new(.., Some(cmd_rx))` 交给 VpciChannel。`hide_device` 经它发
    /// [`vpci::HotplugCommand::Eject`] + `SetPresent(false)`；re-add 经它发
    /// `SetPresent(true)`。`None` = 尚未 add（与 `bus_unit` 同步）。
    /// 与 C2-0 不同：通道恒在 → 命令 channel 也恒在，**不**每周期重建。
    cmd_tx: Option<mesh::Sender<vpci::HotplugCommand>>,

    /// **编排侧**对「设备当前是否呈现给 guest（`device_count=1`）」的视图。
    ///
    /// 与通道内 `ReadyState::device_present` 镜像，但由编排方独立维护，用于让 `process`
    /// 的 Live/Lost 收敛**幂等**：仅在真正发生 false↔true 跃迁时才发 `SetPresent`/`Eject`，
    /// 吸收冗余/抖动的 Live/Lost 通知（C2-2 debounce 之前的基本幂等保证）。
    /// `add_bus` 后置 `true`（通道初始 `device_count=1`）。
    device_present: bool,

    /// device 的 Live/Lost 边沿通知 receiver（device shim 的 `SharedState` 在转
    /// Live/Lost 时投递新状态）。
    edge_rx: mesh::Receiver<DeviceState>,
}

/// graceful EJECT 等 `EJECT_COMPLETE` 的超时上限。
///
/// guest 驱动卡住 / 后端（usnvmemu）已死导致 flush 收尾迟滞时，**绝不能**让 reconcile
/// 无限等待（会卡死整条 dispatch 主循环）。超时后退化为直接走 rescind 拆除——graceful
/// 尝试尽力而为，超时即放弃。1.5s 取自 finding-⑥ 真机观测的 query-remove 量级（NTFS
/// flush+dismount 通常亚秒级；留余量但不至于拖垮热插拔响应）。C2-2 抖动加固时可再调。
const EJECT_COMPLETE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

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
    /// 对 guest show-absent）—— 通道在首个 Live 边沿才 add（之后恒在）；冷插由后续 C3
    /// 完整化。
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
            cmd_tx: None,
            // 尚未 add 通道 → 设备未呈现给 guest。`add_bus` 后置 true。
            device_present: false,
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

    /// 处理一次边沿：按 device 的**当前**状态收敛设备对 guest 的呈现（幂等）。
    ///
    /// **不可取消**（与 `vpci_relay::process` 同纪律：add/SetPresent 半途取消会撕裂状态）。
    /// 串行：本方法只从 dispatch loop 单点调用，不并发。
    ///
    /// C2-1 路径 B（通道恒在，`device_count` 0↔1）的收敛表：
    /// - `Live` 且**通道未 add** → [`add_bus`](Self::add_bus)：offer 通道一次
    ///   （`device_count` 初始 1 → guest 首次枚举出盘）。此后通道恒在。
    /// - `Live` 且**通道已 add** 且**当前未呈现** → `SetPresent(true)`
    ///   （`device_count` 0→1 → guest 重枚举，**修⑦**）。
    /// - 非 `Live`（`Lost`/`Connecting`）且**通道已 add** 且**当前呈现** →
    ///   [`hide_device`](Self::hide_device)：graceful `EJECT` + `SetPresent(false)`
    ///   （`device_count` 1→0 → guest PnP 移除，**修⑥**）。
    /// - 其余（状态与呈现已一致 / 通道未 add 且非 Live）→ no-op（幂等吸收抖动/冗余通知）。
    ///
    /// 用编排侧 `device_present` 视图判跃迁（而非只看 `bus_unit`），因为通道恒在后
    /// `bus_unit.is_some()` 不再随设备进出变化；真正的「呈现/隐藏」边沿由 `device_present`
    /// 标记。
    pub async fn process(
        &mut self,
        chipset_devices: &ChipsetDevices,
        state_units: &mut StateUnits,
    ) -> anyhow::Result<()> {
        let live = matches!(self.state.load(), DeviceState::Live);
        match (live, self.bus_unit.is_some(), self.device_present) {
            // 首个 Live：add 通道一次（device_count 初始 1，盘出现）。
            (true, false, _) => self.add_bus(chipset_devices, state_units).await?,
            // re-add（通道已在、当前未呈现）：device_count 0→1，guest 重枚举（修⑦）。
            (true, true, false) => self.set_present(true).await,
            // Lost（通道已在、当前呈现）——**Option B：transient 后端停顿模型**：
            //
            // 真机 POC 确证（finding-⑦ 深挖）：Windows pci.sys 只在 guest **自己**重上电
            // bus FDO（D0Exit→D0Entry）时才重枚举 VPCI 子设备；VSP 侧任何 push（同-instance
            // 通道 re-offer / unsolicited BUS_RELATIONS2 / INVALIDATE_BUS）都不触发重枚举。
            // 故「Lost 时 hot-remove，Live 时 hot-re-add」的模型在 Windows 上**无法闭环**
            // （remove 走 EJECT 可行，但 re-add 让 guest 重出盘不可行）。
            //
            // 改用 transient-stall 模型：usnvmemu 停（Lost）**不**移除设备——设备对 guest
            // 恒在，仅底层 MMIO/DMA 在 Lost 窗口返 Err（C-2 门控）。usnvmemu 重启后由长存
            // device shim 的 **C-3 reconnect**（worker + connector + 持久 eventfd/region）
            // 透明重连，重发 set_irqs + dma_map，控制器恢复——如真硬件 NVMe controller 短暂
            // reset：盘不从 guest 消失，在途 IO 超时重试后恢复。这样**根本不触发**需要 guest
            // 重枚举的 re-add 路径，绕开⑦。
            //
            // （永久移除 = operator 显式意图，仍可用 `hide_device` 的 graceful EJECT；那是
            // 未来的 operator-workflow，不在「usnvmemu 进程进出」的 reactive 模型内。）
            (false, true, true) => {
                tracing::info!(
                    CVM_ALLOWED,
                    instance_id = %self.instance_id,
                    "vfio_user hotplug: backend Lost — keeping device present (transient stall; C-3 reconnect will recover, no hot-remove)"
                );
            }
            // 其余：状态与呈现已一致，或通道未 add 且非 Live（boot 起 usnvmemu 未起）。
            _ => {}
        }
        Ok(())
    }

    /// 经命令 channel 发 [`vpci::HotplugCommand::SetPresent`]，切换 guest 侧 `device_count`，
    /// 并更新编排侧 `device_present` 视图。通道恒在，故只发命令、不动 `bus_unit`。
    ///
    /// 仅在通道已 add（`cmd_tx` 为 `Some`）时有效；否则记一条 warning（不应发生：调用方
    /// 已据 `bus_unit.is_some()` 门控）。
    async fn set_present(&mut self, present: bool) {
        if let Some(cmd_tx) = self.cmd_tx.as_ref() {
            cmd_tx.send(vpci::HotplugCommand::SetPresent(present));
            self.device_present = present;
            tracing::info!(
                CVM_ALLOWED,
                instance_id = %self.instance_id,
                present,
                "vfio_user hotplug: SetPresent (device_count toggled, channel stays offered)"
            );
        } else {
            tracelimit::warn_ratelimited!(
                instance_id = %self.instance_id,
                present,
                "vfio_user hotplug: SetPresent with no command channel (bus not added); ignoring"
            );
        }
    }

    /// 首个 Live 边沿（仅一次）：造命令 channel + `add_dyn_device` 构造 VpciBus（捕获
    /// **已存在**的 device shim Arc + interrupt_mapper）+ `start_stopped_units`。
    ///
    /// C2-1 路径 B：本方法**全程只跑一次**（`process` 据 `bus_unit.is_none()` 门控）。
    /// 通道一旦 offer 即恒在，后续设备进出靠 `SetPresent`，不再 add/remove 通道。
    /// VpciChannel 的 `device_present` 初始 `true`（见 `device.rs`），故首次 offer 即
    /// `device_count=1` → guest 枚举出盘。
    async fn add_bus(
        &mut self,
        chipset_devices: &ChipsetDevices,
        state_units: &mut StateUnits,
    ) -> anyhow::Result<()> {
        // coerce 长存 device shim → dyn ChipsetDevice（VpciBus::new 的入参类型）。
        let device: Arc<CloseableMutex<dyn ChipsetDevice>> = self.device.clone();
        // 复用 assemble 时**一次性**造好的持久 interrupt_mapper（`clone()` = 增 Arc
        // 引用，**不**重建虚拟设备、**不**重注册 device_id）。
        let interrupt_mapper = self.interrupt_mapper.clone();

        let instance_id = self.instance_id;
        let driver_source = &self.driver_source;
        let vmbus = self.vmbus.clone();
        let vtom = self.vtom;
        let bus_name: Arc<str> = format!("vfio_user_nvme:vpci-{instance_id}").into();

        // 建运行时命令 channel：sender 存进 self（供 `hide_device`/`set_present` 用），
        // receiver 交给 VpciChannel。**C2-1 通道恒在 → 此 channel 也恒在，只建一次**
        // （与 C2-0 每周期重建不同）。
        let (cmd_tx, cmd_rx) = mesh::channel::<vpci::HotplugCommand>();

        let (bus_unit, _bus) = chipset_devices
            .add_dyn_device(
                driver_source,
                state_units,
                bus_name,
                async |register_mmio| {
                    let bus = vpci::bus::VpciBus::new(
                        driver_source,
                        vpci::bus::VpciBusConfig {
                            instance_id,
                            vtom,
                            vnode: None,
                        },
                        device,
                        register_mmio,
                        vmbus.as_ref(),
                        interrupt_mapper,
                        // 启用运行时热插拔命令（graceful EJECT + SetPresent）。
                        Some(cmd_rx),
                    )
                    .await?;
                    anyhow::Ok(bus)
                },
            )
            .await
            .context("failed to add vfio_user VpciBus")?;

        self.bus_unit = Some(bus_unit);
        self.cmd_tx = Some(cmd_tx);
        // 通道初始 device_count=1 → 设备已呈现给 guest。记录编排侧视图。
        self.device_present = true;
        // add_dyn_device 加入的 unit 初始 stopped；若 VM 在跑则启动它（VpciBus offer
        // 在 new 内已发生，这里启动 channel state unit）。
        state_units.start_stopped_units().await;

        tracing::info!(
            CVM_ALLOWED,
            %instance_id,
            "vfio_user hotplug: VpciBus offered once (channel stays for VM lifetime; disk should appear in guest)"
        );
        Ok(())
    }

    /// Lost 边沿：**先**经命令 channel 发 graceful `EJECT` 给 guest 并等 `EJECT_COMPLETE`
    /// （带超时），**再**发 `SetPresent(false)`（`device_count` 1→0 → guest PnP 移除）。
    /// **C2-1 路径 B：不再拆 VpciBus**（通道恒在）—— 仅切 `device_count`。
    ///
    /// # 为什么要 graceful EJECT 前置（finding-⑥）
    ///
    /// C0 真机暴露：直接 surprise-removal 一个**挂载 + 脏数据**的 NTFS 卷会让 guest 偶发
    /// BSOD→reboot。graceful EJECT 给 guest 一个 query-remove 窗口先 flush + dismount 卷，
    /// 再降 `device_count`，避免 surprise。`device_count=0` 本身不保证优雅，故 EJECT 仍是
    /// **必要前置**（与 C2-0 一致，路径 B 不取消此前置）。
    ///
    /// # ⑥ 最微妙点：后端已死时的收尾
    ///
    /// ⑥ 触发时 usnvmemu（vfio-user server）通常已被 kill（device shim 转 Lost）。此时
    /// guest 的 flush 会打到 Lost device shim（C-2 MMIO 门控返 Err），**flush 必然失败**。
    /// 但 graceful EJECT 仍给 guest 一个**有序 dismount** 的机会（即便数据 flush 失败，
    /// 卷状态机能干净走完 remove，不至于 surprise-removal 崩内核）。
    ///
    /// # 超时退化
    ///
    /// 若 guest 在 [`EJECT_COMPLETE_TIMEOUT`] 内不回 `EJECT_COMPLETE`（驱动卡死 / 不支持），
    /// 放弃等待直接降 `device_count`——graceful 是尽力而为，**绝不**因 guest 不配合而挂死
    /// reconcile。
    ///
    /// # 当前去留（Option B）
    ///
    /// Option B（transient-stall 模型）下 `process` 的 Lost 边沿**不再**调用本方法（设备恒在、
    /// 靠 C-3 reconnect 恢复，绕开⑦）。本方法保留作**未来 operator 显式永久移除**的 graceful
    /// 路径（先 EJECT 给 guest 有序 dismount 窗口再降 device_count），尚未接线 → `dead_code`
    /// 暂允许（scaffolding，非过时残留）。
    #[expect(
        dead_code,
        reason = "Option B 下 Lost 边沿不再调用本方法；保留作未来 operator 显式永久移除的 graceful EJECT 路径（见上方 doc），尚未接线"
    )]
    async fn hide_device(&mut self) {
        // 先尝试 graceful EJECT（仅当确有命令 channel，即通道已 add）。
        // 注意：通道恒在 → `cmd_tx` 不 take（后续 re-add 的 SetPresent(true) 还要用它）。
        if let Some(cmd_tx) = self.cmd_tx.as_ref() {
            let (done_tx, done_rx) = mesh::oneshot::<()>();
            cmd_tx.send(vpci::HotplugCommand::Eject { done: done_tx });

            // 等 EJECT_COMPLETE 或超时，二者先到先得。超时兜底**必须有**。
            let mut timer = PolledTimer::new(&self.driver_source.simple());
            let timed_out = futures::select_biased! {
                r = done_rx.fuse() => {
                    // `Ok(())` = guest 回了 EJECT_COMPLETE；`Err` = 通道在确认前退出
                    //（sender drop），也视作「不必再等」。
                    if r.is_err() {
                        tracelimit::warn_ratelimited!(
                            instance_id = %self.instance_id,
                            "vfio_user hotplug: EJECT channel closed before EJECT_COMPLETE; proceeding to hide"
                        );
                    }
                    false
                }
                _ = timer.sleep(EJECT_COMPLETE_TIMEOUT).fuse() => true,
            };

            if timed_out {
                tracing::warn!(
                    CVM_ALLOWED,
                    instance_id = %self.instance_id,
                    timeout_ms = EJECT_COMPLETE_TIMEOUT.as_millis() as u64,
                    "vfio_user hotplug: graceful EJECT timed out; proceeding to set device_count=0 (guest may surprise-remove)"
                );
            } else {
                tracing::info!(
                    CVM_ALLOWED,
                    instance_id = %self.instance_id,
                    "vfio_user hotplug: graceful EJECT acknowledged by guest"
                );
            }
        }

        // 降 device_count 1→0：guest PnP 移除（通道不动）。`set_present` 更新编排侧视图。
        self.set_present(false).await;
        tracing::info!(
            CVM_ALLOWED,
            instance_id = %self.instance_id,
            "vfio_user hotplug: device hidden (device_count=0, channel stays offered)"
        );
    }
}
