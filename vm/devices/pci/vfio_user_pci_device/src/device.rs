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
//! - C-2「show-absent-until-Live」：backend（usnvmemu）真正 connect 成功
//!   （state 进 `Live`）之前，对 guest 一律呈「设备不存在」。`Connecting` 与
//!   `Lost` 在 cfg/MMIO 表面行为**完全一致**：cfg_read 返
//!   `Err(InvalidRegister)`（让 vpci `compute_config_writes` 的 `now_or_never`
//!   走 fill(!0) 路径，guest 读到全 1）；MMIO 同样 Err；cfg_write 静默 Ok。
//!   只有 `Live` 才放行真实 cfg_space / MMIO 路径。理由见 `pci_cfg_read` 注释
//!   的 291d8645 OS-hang 说明：若启动期就让 guest 看到合法身份，NVMe 驱动会
//!   bind 到尚未联通的 controller，首个 MMIO 永远等不到应答 → OS 停响应。
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
        match self.state.load() {
            // C-2 不变量「show-absent-until-Live」：在 backend（usnvmemu）真正
            // connect 成功（state 进 Live）之前，对 guest 呈「设备不存在」。
            // Connecting 必须与 Lost 表现完全一致 —— cfg read 返
            // Err(InvalidRegister)，让 vpci `compute_config_writes` 的
            // `now_or_never` 走 fill(!0) 路径（guest 读到全 1 = 设备消失）。
            //
            // 若 Connecting 落到真实 cfg_space.read_u32，guest 会看到合法
            // vendor/device 而把 NVMe 驱动 bind 到一个尚未联通的 controller，
            // 随后首个 MMIO（如写 CC.EN）永远等不到 firmware 应答 → IRP 卡死 →
            // 整个 OS 停响应（291d8645 OS-hang 类故障）。故只有 Live 走真实 cfg。
            DeviceState::Connecting | DeviceState::Lost => IoResult::Err(IoError::InvalidRegister),
            // vfio-user 模型：config 纯本地仿真，不转发 firmware。
            DeviceState::Live => self.cfg_space.read_u32(offset, value),
        }
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        match self.state.load() {
            // 同 C-2：未 Live 之前静默丢弃写（Ok 但不落地），与 Lost 行为一致。
            DeviceState::Connecting | DeviceState::Lost => IoResult::Ok,
            // 纯本地，不转发 firmware（删掉模板的 side-effect CfgAccess 转发）。
            DeviceState::Live => self.cfg_space.write_u32(offset, value),
        }
    }
}

impl MmioIntercept for VfioUserPciDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        // C-2「show-absent-until-Live」：只有 Live 才让 MMIO 走真实路径。
        // Connecting 与 Lost 一律 Err —— backend 未联通时绝不让 guest 驱动
        // 通过 MMIO 触达半死 controller（见 pci_cfg_read 的 291d8645 说明）。
        if !matches!(self.state.load(), DeviceState::Live) {
            return IoResult::Err(IoError::InvalidRegister);
        }
        match self.cfg_space.find_bar(addr) {
            Some((MSIX_BAR_INDEX, offset)) => {
                // MSI-X 表/PBA 永远在本地仿真；绝不转发给 firmware。
                read_as_u32_chunks(offset, data, |o| self.msix.read_u32(o));
                IoResult::Ok
            }
            Some((bar, offset)) => {
                // 其它 BAR → 投递给 worker；defer 返回 token，firmware 回
                // REGION_READ reply 后 complete。
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
        // C-2「show-absent-until-Live」：同 mmio_read，只有 Live 放行。
        if !matches!(self.state.load(), DeviceState::Live) {
            return IoResult::Err(IoError::InvalidRegister);
        }
        match self.cfg_space.find_bar(addr) {
            Some((MSIX_BAR_INDEX, offset)) => {
                // MSI-X 写也本地处理。
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
    fn lost_cfg_read_returns_err() {
        let mut dev = build_test_device(DeviceState::Lost);
        let mut v = 0;
        let r = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_read(&mut dev, 0, &mut v);
        assert!(matches!(r, IoResult::Err(IoError::InvalidRegister)));
    }

    #[test]
    fn connecting_cfg_read_returns_err_like_lost() {
        let mut dev = build_test_device(DeviceState::Connecting);
        let mut v = 0u32;
        let r = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_read(&mut dev, 0, &mut v);
        assert!(
            matches!(r, IoResult::Err(IoError::InvalidRegister)),
            "Connecting 必须对 guest 呈不存在（C-2），got {r:?}"
        );
    }

    #[test]
    fn connecting_mmio_read_returns_err() {
        let mut dev = build_test_device(DeviceState::Connecting);
        let mut buf = [0u8; 4];
        let r = <VfioUserPciDevice as MmioIntercept>::mmio_read(&mut dev, 0x4000_0000, &mut buf);
        assert!(matches!(r, IoResult::Err(IoError::InvalidRegister)));
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
    fn lost_cfg_write_is_ok() {
        let mut dev = build_test_device(DeviceState::Lost);
        let r = <VfioUserPciDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 4, 0xffff_ffff);
        assert!(matches!(r, IoResult::Ok));
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

    /// MMIO write 在 Lost 状态返 Err，不是 Ok / Defer。
    #[test]
    fn mmio_write_lost_returns_err() {
        let mut dev = build_test_device(DeviceState::Lost);
        // 即使 BAR 没分配（Lost 状态优先检查），mmio_write 都直接 Err。
        let r = <VfioUserPciDevice as MmioIntercept>::mmio_write(&mut dev, 0x4000_0000, &[0; 4]);
        assert!(
            matches!(r, IoResult::Err(IoError::InvalidRegister)),
            "Lost state must return Err, got {r:?}"
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
