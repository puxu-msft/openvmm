// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Thin device shim（W6b 版，照抄 `pcie_remote_device::device` 并换 wire 层）。
//!
//! 关键设计（与模板一致 + vfio-user 模型差异）：
//! - cfg 100% **本地**，由 [`ConfigSpaceType0Emulator`] 承担 — host BAR 注册、
//!   command register MMIO enable bit、MSI-X cap、扩展 cap 全走标准实现。
//!   **vfio-user 模型下 config 永远本地仿真**：firmware 的 config 只在 spawn 时
//!   读一次取身份（vendor/device 等），之后 OpenHCL 侧本地仿真，**绝不**把
//!   cfg read/write 转发给 firmware —— 故删掉模板的 `CfgAccess` /
//!   `side_effect_offsets` / `CfgWriteSideEffect` 转发逻辑。
//! - MSI-X table/PBA 走专用 BAR4（[`MSIX_BAR_INDEX`]），MMIO 路由时在 device.rs
//!   **本地**调用 `MsixEmulator::read_u32 / write_u32` —— MSI-X 表数据永不离开
//!   OpenHCL。
//! - 其它 BAR（host-declared）的 MMIO：
//!   - read → [`ReqKind::MmioRead`] + `IoResult::Defer`，worker 把 REGION_READ
//!     发给 firmware，reply 到达后 complete token。
//!   - write → [`ReqKind::MmioWrite`] **fire-and-forget**，立即 `IoResult::Ok`
//!     让 guest driver 继续；worker 异步发 REGION_WRITE。
//! - C-2「config 恒呈真身份 + 仅 MMIO 按 Live 门控」（2026-06-12 真机 finding-③
//!   修订，见 `usnvmemu/experiments/2026-06-12-w6b-reconnect-real-vm/RESULT.md`；
//!   **Layer C C0 更新见下「offer 时机」**）：
//!   - **cfg_read/cfg_write 恒走真实 `cfg_space`**（不论 `Connecting`/`Live`/`Lost`），
//!     让 guest 在 **offer/枚举时刻**就看到真实 `VEN_1414&DEV_00A9` NVMe 控制器并
//!     加载 stornvme.sys。config 读是无副作用的身份/BAR-window 读，恒呈真只是让
//!     guest 能枚举，是安全的。
//!   - **mmio_read/mmio_write 按 `Live` 门控**：非 `Live`（`Connecting` 或 `Lost`）
//!     一律返 `Err(IoError::InvalidRegister)`，**绝不 `Defer`**（`Defer` 才是
//!     291d8645 OS-hang 的根因——driver 等不到完成 → IRP 卡死 → 整个 OS 停响应；
//!     `Err` 让 driver 读到全 1 / 收到错误 → init 失败或优雅重试，**不挂**）。只有
//!     `Live` 才把 MMIO defer/转发给 worker。这是标准的「设备在、控制器未就绪」
//!     行为（等同一块没响应的 NVMe 盘）。
//!   - MSI-X 专用 BAR（[`MSIX_BAR_INDEX`]）的 table/PBA 是**本地状态**，恒在本地
//!     服务（不依赖 backend，无 hang 风险），与「config 是本地仿真」同理。其余
//!     host-declared BAR 的 MMIO 才按 `Live` 门控转发 worker。
//!   - 净 guest 行为：枚举即加载 stornvme → driver 做 BAR0 MMIO（CAP/CSTS/CC）。
//!     若 Live（usnvmemu 已起）→ 真 NVMe 应答 → controller init → 盘 + IO；
//!     若非 Live → MMIO `Err` → driver init 失败/重试，非 hang；usnvmemu 起后转
//!     Live → 后续 MMIO 命中真 NVMe → 恢复（reconnect revive）。
//!
//!   **offer 时机（Layer C C0，2026-06-12）**：本 device shim 现在有两种装配路径。
//!   路径①是 W6b resolver 路径（`build_vpci_device`）：VPCI offer 在 `assemble_device`
//!   （state=`Connecting`）就发生，**cfg-恒呈真是这条路径的承重前提**（offer 时刻仍
//!   Connecting，若返 absent 则 guest 钉死 DEV_0000/Unknown 永不加载驱动）。
//!   路径②是 **Layer C C0 热插拔路径**（`underhill_core::emuplat::vfio_user_hotplug`）：
//!   device shim 经 `add_dyn_device` 长存，**VpciBus 只在 device 转 `Live` 时才经
//!   `add_dyn_device` offer**（Lost 时 `DynamicDeviceUnit::remove` 拆掉）。此路径下
//!   offer 时刻 device **已 Live**，guest 枚举即见就绪控制器 → 顺带解掉旧 disable/enable
//!   缺口（盘自动出现）。
//!   两条路径下 **cfg-恒呈真 + MMIO-Live-门控都保留不变**：C0 路径在「offer 后、
//!   worker 万一转 Lost」的竞态窗口里，仍靠 MMIO-Live-门控（非 Live 返 `Err` 不
//!   `Defer`）防 guest MMIO hang；cfg 恒呈真对 C0 无害（offer 时已 Live，恒呈真只是
//!   多余的安全冗余），对 resolver 路径仍是承重前提。
//!
//! 相对模板删掉：`next_seq`（vfio-user 的 msg_id 由 worker 分配，不在 device 侧）、
//! `side_effect_offsets`（无 cfg 转发）。

use crate::state::DeviceState;
use crate::state::SharedState;
use crate::worker::DeviceRequest;
use crate::worker::ReqKind;
use crate::worker::SharedWorkerStats;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::defer_read;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use device_emulators::ReadWriteRequestType;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
use inspect::InspectMut;
use mesh::Sender;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use std::future::Future;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;

/// MSI-X 表 + PBA 的 BAR index（"MSI-X 用 BAR4"，与模板一致）。
///
/// 模板从 handshake.rs 引入此常量；W6b 无 handshake 模块，故在此直接定义，
/// 值 4 与模板及 plan 约定一致。
pub const MSIX_BAR_INDEX: u8 = 4;

/// Thin device shim — `ChipsetDevice + PciConfigSpace + MmioIntercept`。
#[derive(InspectMut)]
pub struct VfioUserPciDevice {
    /// 当前 hotplug state（Connecting / Live / Lost）。
    /// 用 ohcldiag-dev inspect 暴露便于运行时 troubleshoot。
    state: SharedState,
    #[inspect(skip)]
    to_worker: Sender<DeviceRequest>,
    /// `ConfigSpaceType0Emulator` 持有 BAR window、command register、
    /// MSI-X capability、扩展 cap 等。
    cfg_space: ConfigSpaceType0Emulator,
    /// MSI-X 表/PBA 仿真器；与 cfg_space 中的 msix_cap 共享内部状态。
    #[inspect(skip)]
    msix: MsixEmulator,
    /// 共享 worker stats（worker 写，本字段 inspect 暴露）。
    worker_stats: SharedWorkerStats,
}

impl VfioUserPciDevice {
    /// 构造一个 device shim。
    pub fn new(
        state: SharedState,
        to_worker: Sender<DeviceRequest>,
        cfg_space: ConfigSpaceType0Emulator,
        msix: MsixEmulator,
        worker_stats: SharedWorkerStats,
    ) -> Self {
        Self {
            state,
            to_worker,
            cfg_space,
            msix,
            worker_stats,
        }
    }

    /// 返回设备状态机句柄的克隆（Connecting/Live/Lost）。
    ///
    /// Layer C C0 用：device shim 经 `add_dyn_device` 装配后长存，underhill 的热插拔
    /// reconcile 据此句柄观察 Live/Lost 边沿（配合 `with_edge_notifier` 注入的 channel）
    /// 并读当前态做幂等 add/remove 决策。`SharedState` 是 `Arc` 包裹，克隆共享同一状态。
    pub fn shared_state(&self) -> SharedState {
        self.state.clone()
    }
}

impl ChangeDeviceState for VfioUserPciDevice {
    fn start(&mut self) {}

    fn stop(&mut self) -> impl Send + Future<Output = ()> {
        std::future::ready(())
    }

    fn reset(&mut self) -> impl Send + Future<Output = ()> {
        std::future::ready(())
    }
}

impl SaveRestore for VfioUserPciDevice {
    type SavedState = NoSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(NoSavedState)
    }

    fn restore(&mut self, _: Self::SavedState) -> Result<(), RestoreError> {
        Ok(())
    }
}

impl ChipsetDevice for VfioUserPciDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }
}

impl PciConfigSpace for VfioUserPciDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        // C-2 修订（真机 finding-③）：config **恒呈真**，不论 Connecting/Live/Lost。
        // VPCI offer 在 assemble_device（Connecting 态）发生，guest 据 offer 时刻的
        // config 钉死 devnode 身份；若此刻返 Err/absent，guest 枚举成 DEV_0000/Unknown
        // → 永不加载 NVMe 驱动（之后 Live 也不重触 offer）。故 cfg 恒走真实
        // cfg_space —— config 读是无副作用的身份/BAR-window 读，恒呈真只让 guest 能
        // 枚举 VEN_1414&DEV_00A9 并加载 stornvme.sys，是安全的。
        //
        // 启动期半死 controller 的防护**移到 MMIO 门控**（见 mmio_read/mmio_write）：
        // 非 Live 时 MMIO 返 Err（而非旧设计让 cfg 整体 absent），driver init 优雅
        // 失败/重试而非 hang。vfio-user 模型：config 纯本地仿真，不转发 firmware。
        self.cfg_space.read_u32(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        // 同 cfg_read：config 恒走真实 cfg_space（不论状态）。写 BAR/command 等是
        // 本地仿真状态更新，无副作用转发 firmware（删掉模板的 side-effect CfgAccess
        // 转发）；让 guest 在 Connecting 态也能正常配置 BAR 窗口完成枚举。
        self.cfg_space.write_u32(offset, value)
    }
}

impl MmioIntercept for VfioUserPciDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        match self.cfg_space.find_bar(addr) {
            Some((MSIX_BAR_INDEX, offset)) => {
                // MSI-X 表/PBA 是**本地状态**，恒在本地服务（与 config 同理，不依赖
                // backend、无 hang 风险）；绝不转发给 firmware，也不按 Live 门控。
                read_as_u32_chunks(offset, data, |o| self.msix.read_u32(o));
                IoResult::Ok
            }
            Some((bar, offset)) => {
                // C-2 修订：其余 host-declared BAR 的 MMIO 才按 Live 门控。非 Live
                // （Connecting 或 Lost）→ 返 Err(InvalidRegister)，**绝不 Defer**：
                // backend 未联通时 driver 读到全 1 / 收错误 → init 失败或优雅重试，
                // **不挂**（Defer 才是 291d8645 OS-hang 根因——等不到完成 → IRP 卡死）。
                if !matches!(self.state.load(), DeviceState::Live) {
                    return IoResult::Err(IoError::InvalidRegister);
                }
                // Live：投递给 worker；defer 返回 token，firmware 回 REGION_READ
                // reply 后 complete。
                let access_size = data.len();
                if !matches!(access_size, 1 | 2 | 4 | 8) {
                    // PCIe MMIO 必须是 1/2/4/8 字节。
                    return IoResult::Err(IoError::InvalidAccessSize);
                }
                let (deferred, token) = defer_read();
                if self.to_worker.is_closed() {
                    return IoResult::Err(IoError::NoResponse);
                }
                self.to_worker.send(DeviceRequest {
                    kind: ReqKind::MmioRead {
                        bar: bar as u32,
                        offset,
                        size: access_size,
                        token: deferred,
                    },
                });
                IoResult::Defer(token)
            }
            None => IoResult::Err(IoError::InvalidRegister),
        }
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        match self.cfg_space.find_bar(addr) {
            Some((MSIX_BAR_INDEX, offset)) => {
                // MSI-X 写也本地处理（本地状态，恒服务、不门控）。
                write_as_u32_chunks(offset, data, |o, ty| match ty {
                    ReadWriteRequestType::Read => Some(self.msix.read_u32(o)),
                    ReadWriteRequestType::Write(val) => {
                        self.msix.write_u32(o, val);
                        None
                    }
                });
                IoResult::Ok
            }
            Some((bar, offset)) => {
                // C-2 修订：其余 BAR 的 MMIO write 按 Live 门控。非 Live → Err，
                // **绝不 Defer**（见 mmio_read 的 291d8645 说明）。
                if !matches!(self.state.load(), DeviceState::Live) {
                    return IoResult::Err(IoError::InvalidRegister);
                }
                let access_size = data.len();
                if !matches!(access_size, 1 | 2 | 4 | 8) {
                    return IoResult::Err(IoError::InvalidAccessSize);
                }
                // MMIO write 是 fire-and-forget：协议设计上 firmware 不 ack 写，
                // 立即 IoResult::Ok 让 guest driver 继续；否则 driver 永远
                // 等不到 IoResult 完成会 hang（实测 Windows nvme.sys 初始化
                // 写 CC.EN=0 后整个 OS 停在该 MMIO 上 —— commit 291d8645）。
                //
                // 把原始 data.to_vec() 传给 worker，由 worker 而非 device 拼
                // wire（REGION_WRITE 直接带原始字节）。
                if self.to_worker.is_closed() {
                    return IoResult::Err(IoError::NoResponse);
                }
                self.to_worker.send(DeviceRequest {
                    kind: ReqKind::MmioWrite {
                        bar: bar as u32,
                        offset,
                        data: data.to_vec(),
                    },
                });
                IoResult::Ok
            }
            None => IoResult::Err(IoError::InvalidRegister),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh::channel;
    use pci_core::cfg_space_emu::BarMemoryKind;
    use pci_core::cfg_space_emu::DeviceBars;
    use pci_core::spec::hwid::ClassCode;
    use pci_core::spec::hwid::HardwareIds;
    use pci_core::spec::hwid::ProgrammingInterface;
    use pci_core::spec::hwid::Subclass;

    fn build_test_device(state: DeviceState) -> VfioUserPciDevice {
        let msi_target = pci_core::msi::MsiTarget::disconnected();
        let (msix, msix_cap) = MsixEmulator::new(MSIX_BAR_INDEX, 1, &msi_target);
        let bars = DeviceBars::new()
            .bar0(4096, BarMemoryKind::Dummy)
            .bar4(msix.bar_len(), BarMemoryKind::Dummy);
        let cfg_space = ConfigSpaceType0Emulator::new(
            HardwareIds {
                // 自定身份：vendor=0x1414（Microsoft）/ device=0x00a9（NVMe-ish）。
                vendor_id: 0x1414,
                device_id: 0x00a9,
                revision_id: 1,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::NONE,
                base_class: ClassCode::UNCLASSIFIED,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![Box::new(msix_cap)],
            Vec::new(),
            bars,
        );
        let (tx, rx) = channel::<DeviceRequest>();
        // 防止 rx 被 drop 导致 is_closed() 立即返 true：forget 让 channel
        // 在测试函数生命周期内保持开。
        std::mem::forget(rx);
        let s = SharedState::new(state);
        let stats = std::sync::Arc::new(crate::worker::WorkerStats::default());
        VfioUserPciDevice::new(s, tx, cfg_space, msix, stats)
    }

    #[test]
    fn lost_cfg_read_returns_real_config() {
        // C-2 修订：config 恒呈真，Lost 态 cfg_read 也返真实 vendor/device（与 Live
        // 一致）。仅 MMIO 按 Live 门控；config 永远本地、无副作用、恒呈真。
        let mut dev = build_test_device(DeviceState::Lost);
        let mut v = 0u32;
        let r = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_read(&mut dev, 0, &mut v);
        assert!(
            matches!(r, IoResult::Ok),
            "Lost cfg 应恒呈真（Ok），got {r:?}"
        );
        assert_eq!(v & 0xffff, 0x1414, "Lost cfg vendor 应为真值 0x1414");
        assert_eq!(
            (v >> 16) & 0xffff,
            0x00a9,
            "Lost cfg device 应为真值 0x00a9"
        );
    }

    #[test]
    fn connecting_cfg_read_returns_real_config() {
        // C-2 修订（真机 finding-③）：Connecting 态 cfg_read 必须返**真实** config，
        // 使 VPCI offer（在 Connecting 态发生）捕获 DEV_00A9 → guest 加载 stornvme。
        // 这是对旧 C-2「Connecting show-absent」的修订：旧设计让 offer 捕获
        // DEV_0000/Unknown → 永不加载驱动。
        let mut dev = build_test_device(DeviceState::Connecting);
        let mut v = 0u32;
        let r = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_read(&mut dev, 0, &mut v);
        assert!(
            matches!(r, IoResult::Ok),
            "Connecting cfg 必须呈真让 guest 枚举（C-2 修订），got {r:?}"
        );
        assert_eq!(v & 0xffff, 0x1414, "Connecting cfg vendor 应为真值 0x1414");
        assert_eq!(
            (v >> 16) & 0xffff,
            0x00a9,
            "Connecting cfg device 应为真值 0x00a9"
        );
    }

    #[test]
    fn connecting_mmio_read_returns_err() {
        // C-2 修订：MMIO 仍按 Live 门控——Connecting 态对已映射 BAR 的 MMIO 返
        // Err（绝不 Defer，291d8645 hang-safety）。先程式化 BAR0 + 启 MEM，使
        // find_bar 命中 BAR0（而非走 unmapped-None 路径），才真正测到 Live-gate。
        let mut dev = build_test_device(DeviceState::Connecting);
        let bar_addr: u32 = 0x4000_0000;
        let _ = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 0x10, bar_addr);
        let _ = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 4, 0x0002);
        let mut buf = [0u8; 4];
        let r =
            <VfioUserPciDevice as MmioIntercept>::mmio_read(&mut dev, bar_addr as u64, &mut buf);
        assert!(
            matches!(r, IoResult::Err(IoError::InvalidRegister)),
            "Connecting 态映射 BAR 的 MMIO read 应 Err（Live-gate，非 Defer），got {r:?}"
        );
    }

    /// C-2 修订：MSI-X 专用 BAR（BAR4）的 table/PBA 是本地状态，**恒服务**——
    /// 即便 Connecting（backend 未联通）也直接本地读返 Ok，不按 Live 门控、不
    /// 转发 firmware（与「config 本地恒呈真」同理）。这条锁定"MSI-X-BAR 不被
    /// Live-gate 误伤"的行为。
    #[test]
    fn connecting_msix_bar_read_served_locally() {
        let mut dev = build_test_device(DeviceState::Connecting);
        // 程式化 BAR4（MSI-X，offset 0x20）+ 启 MEM，使 find_bar 命中 MSIX_BAR_INDEX。
        let msix_bar_addr: u32 = 0x5000_0000;
        let _ = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 0x20, msix_bar_addr);
        let _ = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 4, 0x0002);
        let mut buf = [0u8; 4];
        let r = <VfioUserPciDevice as MmioIntercept>::mmio_read(
            &mut dev,
            msix_bar_addr as u64,
            &mut buf,
        );
        assert!(
            matches!(r, IoResult::Ok),
            "Connecting 态 MSI-X BAR 本地读应恒服务返 Ok（不被 Live-gate），got {r:?}"
        );
    }

    #[test]
    fn live_cfg_read_vendor_device() {
        let mut dev = build_test_device(DeviceState::Live);
        let mut v = 0;
        let r = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_read(&mut dev, 0, &mut v);
        assert!(matches!(r, IoResult::Ok));
        // vendor (low 16) | device (high 16)
        assert_eq!(v & 0xffff, 0x1414);
        assert_eq!((v >> 16) & 0xffff, 0x00a9);
    }

    #[test]
    fn lost_cfg_write_is_real_ok() {
        // C-2 修订：config 恒呈真，Lost 态 cfg_write 走真实 cfg_space.write_u32
        // （不再静默丢弃），返 Ok。写命令寄存器（offset 4）是本地仿真状态更新。
        let mut dev = build_test_device(DeviceState::Lost);
        let r = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 4, 0xffff_ffff);
        assert!(
            matches!(r, IoResult::Ok),
            "Lost cfg_write 应真实落地返 Ok，got {r:?}"
        );
    }

    /// C-2 修订关键回归：not-Live 态 cfg_write 必须**真正写穿透**到 cfg_space
    /// （非旧设计的静默丢弃）。这是 VPCI offer 正确性的承重前提：VPCI 在
    /// assemble（Connecting）态经 `probe_bar_masks` 对 BAR 寄存器做
    /// write-all-1s → read-back → restore 来探 BAR 尺寸/类型掩码（源见
    /// `pci_core::chipset_device_ext::probe_hardware_ids`/`probe_bar_masks`），
    /// 若 Connecting cfg_write 被丢弃，则 BAR 程式化失效 → guest 误算 MMIO 窗口。
    /// 旧 `lost_cfg_write_is_real_ok` 只断言 Ok，区分不出"写穿透"与旧"静默丢弃"
    /// （两者都 Ok）；本测试直接读回 BAR0 证明写已落地。
    #[test]
    fn connecting_cfg_write_actually_writes_through() {
        let mut dev = build_test_device(DeviceState::Connecting);
        let bar_addr: u32 = 0x4000_0000;
        let _ = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 0x10, bar_addr);
        let mut v = 0u32;
        let _ = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_read(&mut dev, 0x10, &mut v);
        // BAR0 低位是类型 bits（64-bit mem），掩码后应反映写入的基址，
        // 证明 Connecting 态写已穿透到真实 cfg_space（而非被丢弃）。
        assert_eq!(
            v & 0xffff_fff0,
            bar_addr & 0xffff_fff0,
            "Connecting cfg_write 应真实落地（读回 BAR0 反映写入），got {v:#x}"
        );
    }

    /// 关键回归测试（commit 291d8645）：MMIO write 必须立即返
    /// `IoResult::Ok`，**不能** Defer。否则 guest driver 永远等不到
    /// 写完成 → IRP 卡死 → nvme.sys 写 CC.EN=0 后整个 OS 停响应。
    ///
    /// 测试用 cfg space write 给 BAR0 分配一个非零地址，再做 mmio_write，
    /// 应立即 Ok 不 Defer；不依赖 worker 真消费 frame。
    #[test]
    fn mmio_write_returns_ok_immediately_fire_and_forget() {
        let mut dev = build_test_device(DeviceState::Live);
        // 分配 BAR0 地址：写 cfg offset 0x10 (BAR0) 一个合法地址 + 写 cmd 启 MEM。
        let bar_addr: u32 = 0x4000_0000;
        let _ = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 0x10, bar_addr);
        // PCI cfg cmd (offset 4): bit 1 = MEM space enable
        let _ = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 4, 0x0002);

        // mmio_write 到 BAR0+0：必须立即 Ok（无 Defer 等 ack）。
        let r = <VfioUserPciDevice as MmioIntercept>::mmio_write(
            &mut dev,
            bar_addr as u64,
            &[0xde, 0xad, 0xbe, 0xef],
        );
        assert!(
            matches!(r, IoResult::Ok),
            "MMIO write must be fire-and-forget (Ok), got {r:?}; \
             defer would hang guest driver — see commit 291d8645"
        );
    }

    /// MMIO write 在 Lost 状态返 Err，不是 Ok / Defer（C-2 MMIO 门控）。
    #[test]
    fn mmio_write_lost_returns_err() {
        let mut dev = build_test_device(DeviceState::Lost);
        // 先程式化 BAR0 + 启 MEM，使 find_bar 命中 BAR0，才真正测到 Lost 的
        // Live-gate（而非走 unmapped-None 的 Err 路径）。cfg 恒呈真，Lost 也可写 BAR。
        let bar_addr: u32 = 0x4000_0000;
        let _ = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 0x10, bar_addr);
        let _ = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 4, 0x0002);
        let r =
            <VfioUserPciDevice as MmioIntercept>::mmio_write(&mut dev, bar_addr as u64, &[0; 4]);
        assert!(
            matches!(r, IoResult::Err(IoError::InvalidRegister)),
            "Lost 态映射 BAR 的 MMIO write 应 Err（Live-gate，非 Defer），got {r:?}"
        );
    }

    /// MMIO write 非法 size（如 3 字节）返 InvalidAccessSize，不 Ok 也不
    /// Defer。
    #[test]
    fn mmio_write_invalid_size_returns_err() {
        let mut dev = build_test_device(DeviceState::Live);
        let bar_addr: u32 = 0x4000_0000;
        let _ = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 0x10, bar_addr);
        let _ = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 4, 0x0002);

        let r = <VfioUserPciDevice as MmioIntercept>::mmio_write(
            &mut dev,
            bar_addr as u64,
            &[0; 3], // 3 字节非法
        );
        assert!(
            matches!(r, IoResult::Err(IoError::InvalidAccessSize)),
            "Invalid size must return Err(InvalidAccessSize), got {r:?}"
        );
    }
}
