// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W6b Task 1.5 loopback 集成测：真 `vfio_user_transport::VfioUserSession` +
//! `MockDev` 服务线程 ⇄ `prepare_from_client`（connect+handshake+identity）。
//!
//! socketpair，Linux 直跑，无需 VTL。用 `from_stream` + `prepare_from_client`
//! 绕过真实 listening unix path（`spawn_vfio_user_connects` 的 connect-by-path
//! 只是它外面的薄重试层）。MockDev 身份：vendor 0x1234 / device 0x5678 /
//! class 0x01_08_02 / BAR0 8 KiB / msix 8。

use pal_async::DefaultPool;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use pcie_device_core::BarKind;
use pcie_device_core::BarLayout;
use pcie_device_core::DeviceCtx;
use pcie_device_core::DeviceDescribe;
use pcie_device_core::PcieDevice;
use std::os::unix::net::UnixStream;
use std::thread;
use vfio_user_device::VfioUserClient;
use vfio_user_pci_device::spawn::prepare_from_client;

/// 复制自 vfio_user_device loopback_region.rs 的 MockDev：BAR0 8 KiB + 8 MSI-X +
/// identity（vendor 0x1234 / device 0x5678 / class 0x010802 / rev 1）。
struct MockDev {
    bar0: Vec<u64>,
    last_reset: u32,
}
impl MockDev {
    fn new() -> Self {
        Self {
            bar0: vec![0u64; 1024],
            last_reset: 0xFFFF_FFFF,
        }
    }
}
impl PcieDevice for MockDev {
    fn describe(&self) -> DeviceDescribe {
        DeviceDescribe {
            vendor_id: 0x1234,
            device_id: 0x5678,
            class_code: 0x01_08_02,
            revision: 1,
            subsystem_vendor: 0,
            subsystem_device: 0,
            bars: vec![BarLayout {
                index: 0,
                size: 8192,
                kind: BarKind::Mmio32,
                prefetchable: false,
            }],
            msix_count: 8,
            capabilities: vec![],
            cfg_write_side_effect_offsets: vec![],
        }
    }
    fn mmio_read(&mut self, _bar: u32, offset: u64, _size: u32) -> u64 {
        *self.bar0.get((offset / 8) as usize).unwrap_or(&0)
    }
    fn mmio_write(
        &mut self,
        _ctx: &mut DeviceCtx<'_>,
        _bar: u32,
        offset: u64,
        _size: u32,
        value: u64,
    ) {
        let idx = (offset / 8) as usize;
        if idx < self.bar0.len() {
            self.bar0[idx] = value;
        }
    }
    fn reset(&mut self, kind: u32) {
        self.last_reset = kind;
    }
}

/// 起 server 线程：handshake → session 循环 pump_one(MockDev) 直到 peer close。
fn spawn_server(server_end: UnixStream) -> thread::JoinHandle<anyhow::Result<()>> {
    thread::spawn(move || -> anyhow::Result<()> {
        let mut s = server_end;
        let neg = vfio_user_transport::server_handshake(&mut s)?;
        let mut sess = vfio_user_transport::VfioUserSession::new(s, neg);
        let mut dev = MockDev::new();
        while sess.pump_one(&mut dev)? {}
        Ok(())
    })
}

#[test]
fn prepare_from_client_reads_mockdev_identity() {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    let server = spawn_server(server_end);

    DefaultPool::run_with(async |driver| {
        let client = VfioUserClient::from_stream(&driver, client_end).expect("wrap client stream");
        let prep = prepare_from_client(client).await.expect("prepare ok");

        assert_eq!(prep.hardware_ids.vendor_id, 0x1234, "vendor");
        assert_eq!(prep.hardware_ids.device_id, 0x5678, "device");
        assert_eq!(
            prep.hardware_ids.base_class,
            ClassCode::from(0x01u8),
            "base_class NVMe = Mass Storage"
        );
        assert_eq!(
            prep.hardware_ids.sub_class,
            Subclass::from(0x08u8),
            "sub_class NVMe = NVM Controller"
        );
        assert_eq!(
            prep.hardware_ids.prog_if,
            ProgrammingInterface::from(0x02u8),
            "prog_if NVMe = NVM Express"
        );
        assert_eq!(prep.bar0_size, 8192, "BAR0 size");
        assert_eq!(prep.msix_count, 8, "MSI-X vector count");

        drop(prep); // 关 socket → server pump_one 返 Ok(false) 退循环。
    });

    server.join().unwrap().expect("server loop");
}
