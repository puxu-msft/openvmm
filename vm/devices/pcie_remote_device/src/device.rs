// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Thin device shim (spec §3.5 / §3.6 完整版)。
//!
//! v2 重构关键设计：
//! - cfg 100% sync，由 `ConfigSpaceType0Emulator` 承担 — host BAR 注册、
//!   command register MMIO enable bit、MSI-X cap、扩展 cap 等全部走标准实现
//! - MSI-X table/PBA 走专用 BAR4（`MSIX_BAR_INDEX`，与 nvme/pci.rs 一致），
//!   MMIO 路由时在 device.rs **本地**调用 `MsixEmulator::read_u32 / write_u32`
//!   —— **K-NEW-A: MSI-X 表数据永远不离开 OpenHCL**
//! - 其它 BAR (host-declared) 的 MMIO 走 `DeviceRequest` + `IoResult::Defer`
//!   投递给 worker，worker 把 `MmioAccess` 帧发给 host
//! - cfg_write 命中 `cfg_write_side_effect_offsets` → fire-and-forget 发
//!   `CfgAccess` 给 host（Q6）
//! - Lost 状态：cfg_read 返回 `Err(InvalidRegister)`（让 vpci `compute_config_writes`
//!   的 `now_or_never` 走 fill(!0) 路径）；MMIO 同样 Err；cfg_write 静默 Ok。

use crate::state::DeviceState;
use crate::state::SharedState;
use crate::worker::DeviceRequest;
use crate::worker::InFlight;
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
use pcie_remote_protocol::CfgAccess;
use pcie_remote_protocol::MmioAccess;
use pcie_remote_protocol::ToHost;
use pcie_remote_protocol::to_host::Body;
use std::collections::HashSet;
use std::future::Future;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;

/// MSI-X 表 + PBA 的 BAR index（与 handshake.rs::MSIX_BAR_INDEX 一致）。
pub use crate::handshake::MSIX_BAR_INDEX;

/// Thin device shim — `ChipsetDevice + PciConfigSpace + MmioIntercept`。
#[derive(InspectMut)]
pub struct PcieRemoteDevice {
    /// 当前 K-20 hotplug state（Connecting / Live / Lost）。
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
    /// `cfg_write_side_effect_offsets` 集合（来自 DeviceDescribe）。
    #[inspect(skip)]
    side_effect_offsets: HashSet<u32>,
    /// 序号生成器（device→worker→host 的 MmioAccess / CfgAccess 帧 seq）。
    next_seq: u64,
    /// 共享 worker stats（worker 写，本字段 inspect 暴露）。
    worker_stats: SharedWorkerStats,
}

impl PcieRemoteDevice {
    /// 构造一个 device shim。
    pub fn new(
        state: SharedState,
        to_worker: Sender<DeviceRequest>,
        cfg_space: ConfigSpaceType0Emulator,
        msix: MsixEmulator,
        side_effect_offsets: HashSet<u32>,
        worker_stats: SharedWorkerStats,
    ) -> Self {
        Self {
            state,
            to_worker,
            cfg_space,
            msix,
            side_effect_offsets,
            next_seq: 1,
            worker_stats,
        }
    }

    /// 取下一个 seq（用于 device-initiated frame）。
    ///
    /// seq 空间约定（与 worker.rs `next_dma_seq` 配合）：
    /// - device.rs 用低半 u64（从 1 起，单调递增到 2^63-1）
    /// - worker.rs DMA reply 用高半（`1 << 63` 起）
    ///
    /// 双方互不重叠，便于排查日志中 frame 来源。实践中 2^63 帧不可达。
    fn next_seq(&mut self) -> u64 {
        debug_assert!(
            self.next_seq < (1u64 << 63),
            "device.rs seq overflowed into DMA-reply half"
        );
        let s = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        s
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

    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }
}

impl PciConfigSpace for PcieRemoteDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        match self.state.load() {
            DeviceState::Lost => IoResult::Err(IoError::InvalidRegister),
            DeviceState::Connecting | DeviceState::Live => self.cfg_space.read_u32(offset, value),
        }
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        match self.state.load() {
            DeviceState::Lost => IoResult::Ok,
            _ => {
                let r = self.cfg_space.write_u32(offset, value);
                // Q6: 命中 host 声明的 side-effect offset → 发 CfgAccess 给 host
                // （fire-and-forget；如果 worker 已断开 receiver，send 仍 OK 返回
                // 但消息被丢弃，下次 MMIO 用 50ms timeout 自然把 Lost 标出来）。
                //
                // rust-reviewer #8 (HIGH): 仅在 cfg_space.write_u32 成功时 forward。
                // 失败的 write 在 chipset 角度从未发生过，host 不应收到事件 —
                // 否则违反 spec §3.6 "host 只看到成功的 cfg writes" 不变量，
                // 导致 OpenHCL / host 状态分歧。
                if matches!(r, IoResult::Ok) && self.side_effect_offsets.contains(&(offset as u32))
                {
                    let seq = self.next_seq();
                    let frame = ToHost {
                        seq,
                        body: Some(Body::CfgWriteSideEffect(CfgAccess {
                            offset: offset as u32,
                            size: 4,
                            value,
                        })),
                    };
                    // 这条 send 是 fire-and-forget（不需 is_closed precheck）；
                    // mesh::Sender::send 在 receiver 关闭时不 panic，消息丢弃。
                    self.to_worker.send(DeviceRequest {
                        seq,
                        frame,
                        pending: None,
                    });
                }
                r
            }
        }
    }
}

impl MmioIntercept for PcieRemoteDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        if matches!(self.state.load(), DeviceState::Lost) {
            return IoResult::Err(IoError::InvalidRegister);
        }
        match self.cfg_space.find_bar(addr) {
            Some((MSIX_BAR_INDEX, offset)) => {
                // K-NEW-A: MSI-X 表/PBA 永远在本地仿真；绝不转发给 host。
                read_as_u32_chunks(offset, data, |o| self.msix.read_u32(o));
                IoResult::Ok
            }
            Some((bar, offset)) => {
                // 其它 BAR → 投递给 worker；defer 返回 token，host 回 MmioReadResult 后 complete。
                let access_size = data.len();
                if !matches!(access_size, 1 | 2 | 4 | 8) {
                    // K-18: PCIe MMIO 必须是 1/2/4/8 字节。
                    return IoResult::Err(IoError::InvalidAccessSize);
                }
                let (deferred, token) = defer_read();
                let seq = self.next_seq();
                let frame = ToHost {
                    seq,
                    body: Some(Body::MmioRead(MmioAccess {
                        bar: bar as u32,
                        offset,
                        size: access_size as u32,
                        value: 0,
                    })),
                };
                if self.to_worker.is_closed() {
                    return IoResult::Err(IoError::NoResponse);
                }
                self.to_worker.send(DeviceRequest {
                    seq,
                    frame,
                    pending: Some(InFlight::Read {
                        token: deferred,
                        access_size,
                    }),
                });
                IoResult::Defer(token)
            }
            None => IoResult::Err(IoError::InvalidRegister),
        }
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        if matches!(self.state.load(), DeviceState::Lost) {
            return IoResult::Err(IoError::InvalidRegister);
        }
        match self.cfg_space.find_bar(addr) {
            Some((MSIX_BAR_INDEX, offset)) => {
                // K-NEW-A: MSI-X 写也本地处理。
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
                // 拼成 u64 little-endian。
                let mut buf = [0u8; 8];
                buf[..access_size].copy_from_slice(data);
                let value = u64::from_le_bytes(buf);

                // MMIO write 是 fire-and-forget：协议设计上 host 不 ack 写，
                // 立即 IoResult::Ok 让 guest driver 继续；否则 driver 永远
                // 等不到 IoResult 完成会 hang（实测 Windows nvme.sys 初始化
                // 写 CC.EN=0 后整个 OS 停在该 MMIO 上）。
                let seq = self.next_seq();
                let frame = ToHost {
                    seq,
                    body: Some(Body::MmioWrite(MmioAccess {
                        bar: bar as u32,
                        offset,
                        size: access_size as u32,
                        value,
                    })),
                };
                if self.to_worker.is_closed() {
                    return IoResult::Err(IoError::NoResponse);
                }
                self.to_worker.send(DeviceRequest {
                    seq,
                    frame,
                    pending: None, // 不等 host ack；写完即返
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

    fn build_test_device(state: DeviceState) -> PcieRemoteDevice {
        let msi_target = pci_core::msi::MsiTarget::disconnected();
        let (msix, msix_cap) = MsixEmulator::new(MSIX_BAR_INDEX, 1, &msi_target);
        let bars = DeviceBars::new()
            .bar0(4096, BarMemoryKind::Dummy)
            .bar4(msix.bar_len(), BarMemoryKind::Dummy);
        let cfg_space = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: 0x1414,
                device_id: 0xc0de,
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
        PcieRemoteDevice::new(s, tx, cfg_space, msix, HashSet::new(), stats)
    }

    #[test]
    fn lost_cfg_read_returns_err() {
        let mut dev = build_test_device(DeviceState::Lost);
        let mut v = 0;
        let r = <PcieRemoteDevice as PciConfigSpace>::pci_cfg_read(&mut dev, 0, &mut v);
        assert!(matches!(r, IoResult::Err(IoError::InvalidRegister)));
    }

    #[test]
    fn live_cfg_read_vendor_device() {
        let mut dev = build_test_device(DeviceState::Live);
        let mut v = 0;
        let r = <PcieRemoteDevice as PciConfigSpace>::pci_cfg_read(&mut dev, 0, &mut v);
        assert!(matches!(r, IoResult::Ok));
        // vendor (low 16) | device (high 16)
        assert_eq!(v & 0xffff, 0x1414);
        assert_eq!((v >> 16) & 0xffff, 0xc0de);
    }

    #[test]
    fn lost_cfg_write_is_ok() {
        let mut dev = build_test_device(DeviceState::Lost);
        let r = <PcieRemoteDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 4, 0xffff_ffff);
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
        let _ = <PcieRemoteDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 0x10, bar_addr);
        // PCI cfg cmd (offset 4): bit 1 = MEM space enable
        let _ = <PcieRemoteDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 4, 0x0002);

        // mmio_write 到 BAR0+0：必须立即 Ok（无 Defer 等 ack）。
        let r = <PcieRemoteDevice as MmioIntercept>::mmio_write(
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
        let r = <PcieRemoteDevice as MmioIntercept>::mmio_write(
            &mut dev,
            0x4000_0000,
            &[0; 4],
        );
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
        let _ = <PcieRemoteDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 0x10, bar_addr);
        let _ = <PcieRemoteDevice as PciConfigSpace>::pci_cfg_write(&mut dev, 4, 0x0002);

        let r = <PcieRemoteDevice as MmioIntercept>::mmio_write(
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
