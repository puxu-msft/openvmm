// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Thin device shim (spec §3.5)。
//!
//! 关键设计：
//! - cfg 100% sync（因为 vpci `compute_config_writes` 用 now_or_never，
//!   Lost 状态 cfg_read 必须返回 Err(InvalidRegister) 让 vpci 走 fill(!0) 路径）。
//! - MMIO 走 IoResult::Defer（Phase 5+：通过 DeviceRequest 投递给 worker）。
//! - 不持 transport，不直接做 I/O。

use crate::state::DeviceState;
use crate::state::SharedState;
use crate::worker::DeviceRequest;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use inspect::InspectMut;
use mesh::Sender;
use std::future::Future;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;

/// Thin device shim。
#[derive(InspectMut)]
pub struct PcieRemoteDevice {
    #[inspect(skip)]
    state: SharedState,
    #[inspect(skip)]
    to_worker: Sender<DeviceRequest>,
    /// 本地 cfg 镜像（64 dwords = 256 字节标准 cfg）。
    #[inspect(skip)]
    cfg_local: [u32; 64],
    #[inspect(skip)]
    next_seq: u64,
}

impl PcieRemoteDevice {
    /// 构造一个 device shim。
    pub fn new(
        state: SharedState,
        to_worker: Sender<DeviceRequest>,
        cfg_local: [u32; 64],
    ) -> Self {
        Self {
            state,
            to_worker,
            cfg_local,
            next_seq: 1,
        }
    }

    /// 暴露 to_worker（用于 fire-and-forget；Phase 6+ 可能要用）。
    pub fn sender(&self) -> Sender<DeviceRequest> {
        self.to_worker.clone()
    }
}

impl ChangeDeviceState for PcieRemoteDevice {
    fn start(&mut self) {}

    fn stop(&mut self) -> impl Send + Future<Output = ()> {
        std::future::ready(())
    }

    fn reset(&mut self) -> impl Send + Future<Output = ()> {
        std::future::ready(())
    }
}

impl SaveRestore for PcieRemoteDevice {
    type SavedState = NoSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(NoSavedState)
    }

    fn restore(&mut self, _: Self::SavedState) -> Result<(), RestoreError> {
        Ok(())
    }
}

impl ChipsetDevice for PcieRemoteDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl PciConfigSpace for PcieRemoteDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        match self.state.load() {
            DeviceState::Lost => IoResult::Err(IoError::InvalidRegister),
            DeviceState::Connecting | DeviceState::Live => {
                let idx = (offset >> 2) as usize;
                if idx < self.cfg_local.len() {
                    *value = self.cfg_local[idx];
                } else {
                    *value = !0;
                }
                IoResult::Ok
            }
        }
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        match self.state.load() {
            DeviceState::Lost => IoResult::Ok,
            _ => {
                let idx = (offset >> 2) as usize;
                if idx < self.cfg_local.len() {
                    self.cfg_local[idx] = value;
                }
                // cfg_write_side_effect forward 将在 Phase 6+ 启用，
                // 通过 cfg_local 镜像 + host 声明的 cfg_write_side_effect_offsets。
                self.next_seq = self.next_seq.wrapping_add(1);
                IoResult::Ok
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh::channel;

    #[test]
    fn lost_returns_err_invalid_register() {
        let s = SharedState::new(DeviceState::Lost);
        let (tx, _rx) = channel::<DeviceRequest>();
        let mut dev = PcieRemoteDevice::new(s, tx, [0u32; 64]);
        let mut v = 0;
        let r = <PcieRemoteDevice as PciConfigSpace>::pci_cfg_read(&mut dev, 0, &mut v);
        assert!(matches!(r, IoResult::Err(IoError::InvalidRegister)));
    }

    #[test]
    fn live_cfg_read_returns_local_value() {
        let s = SharedState::new(DeviceState::Live);
        let (tx, _rx) = channel::<DeviceRequest>();
        let mut cfg = [0u32; 64];
        cfg[0] = 0x80861234;
        let mut dev = PcieRemoteDevice::new(s, tx, cfg);
        let mut v = 0;
        assert!(matches!(
            <PcieRemoteDevice as PciConfigSpace>::pci_cfg_read(&mut dev, 0, &mut v),
            IoResult::Ok
        ));
        assert_eq!(v, 0x80861234);
    }

    #[test]
    fn cfg_write_updates_local_mirror() {
        let s = SharedState::new(DeviceState::Live);
        let (tx, _rx) = channel::<DeviceRequest>();
        let mut dev = PcieRemoteDevice::new(s, tx, [0u32; 64]);
        let _ = <PcieRemoteDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 0, 0xabcd);
        let mut v = 0;
        let _ = <PcieRemoteDevice as PciConfigSpace>::pci_cfg_read(&mut dev, 0, &mut v);
        assert_eq!(v, 0xabcd);
    }
}
