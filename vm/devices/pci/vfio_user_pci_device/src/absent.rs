// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `AbsentPcieDevice`：纯本地 stub，无任何 host 协议 surface（照抄
//! `pcie_remote_device::absent`）。
//!
//! 用于：
//! 1. CVM 兜底（本特性在 CVM 下关闭，resolver 仍需给出一个无害设备）。
//! 2. connect/handshake 失败或超时的 boot DoS fallback（设备"缺席"而非挂起 boot）。
//! 3. 单元测试 fixture。
//!
//! cfg_read 全 1（guest 视为无设备）；cfg_write 静默丢弃。

use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use inspect::InspectMut;
use std::future::Future;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;

/// 纯本地 stub PCI 设备。
#[derive(InspectMut)]
pub struct AbsentPcieDevice {}

impl AbsentPcieDevice {
    /// 构造一个 absent 设备实例。
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for AbsentPcieDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangeDeviceState for AbsentPcieDevice {
    fn start(&mut self) {}

    fn stop(&mut self) -> impl Send + Future<Output = ()> {
        std::future::ready(())
    }

    fn reset(&mut self) -> impl Send + Future<Output = ()> {
        std::future::ready(())
    }
}

impl SaveRestore for AbsentPcieDevice {
    type SavedState = NoSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(NoSavedState)
    }

    fn restore(&mut self, _: Self::SavedState) -> Result<(), RestoreError> {
        Ok(())
    }
}

impl ChipsetDevice for AbsentPcieDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl PciConfigSpace for AbsentPcieDevice {
    fn pci_cfg_read(&mut self, _offset: u16, value: &mut u32) -> IoResult {
        *value = !0;
        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, _offset: u16, _value: u32) -> IoResult {
        IoResult::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfg_read_all_ones() {
        let mut d = AbsentPcieDevice::new();
        let mut v = 0u32;
        assert!(matches!(
            <AbsentPcieDevice as PciConfigSpace>::pci_cfg_read(&mut d, 0, &mut v),
            IoResult::Ok
        ));
        assert_eq!(v, !0u32);
    }

    #[test]
    fn cfg_write_silently_ok() {
        let mut d = AbsentPcieDevice::new();
        assert!(matches!(
            <AbsentPcieDevice as PciConfigSpace>::pci_cfg_write(&mut d, 0, 0xdead),
            IoResult::Ok
        ));
    }

    #[test]
    fn supports_pci_returns_self() {
        let mut d = AbsentPcieDevice::new();
        assert!(d.supports_pci().is_some());
    }
}
