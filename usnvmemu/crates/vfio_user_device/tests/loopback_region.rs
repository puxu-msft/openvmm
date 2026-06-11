// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W2 loopback 集成测：真 `vfio_user_transport::VfioUserSession` + `MockDev` ⇄
//! client region wire（GET_INFO / GET_REGION_INFO / GET_IRQ_INFO / REGION_RW /
//! RESET）。socketpair，Linux 直跑，无需 VTL。剧本对标 interop_py enumerate_smoke。

use pcie_device_core::BarKind;
use pcie_device_core::BarLayout;
use pcie_device_core::DeviceCtx;
use pcie_device_core::DeviceDescribe;
use pcie_device_core::PcieDevice;
use std::os::unix::net::UnixStream;
use std::thread;
use vfio_user_device::VfioUserClient;

/// 复制自 vfio_user_transport session.rs 的测试 MockDev：BAR0 8 KiB + 8 MSI-X +
/// identity（vendor 0x1234 / device 0x5678 / class 0x010802）。
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
fn loopback_region_enumerate_and_rw() {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    let server = spawn_server(server_end);

    let mut client = VfioUserClient::from_stream(client_end);
    client.handshake().expect("handshake");

    // 1. DEVICE_GET_INFO：PCI 标准 num_regions=9 / num_irqs=5（常量，非 MockDev 派生）。
    let info = client.get_device_info().expect("get_device_info");
    let num_regions = info.num_regions; // packed copy
    let num_irqs = info.num_irqs;
    assert_eq!(num_regions, 9, "PCI num_regions");
    assert_eq!(num_irqs, 5, "PCI num_irqs");

    // 2. GET_REGION_INFO(BAR0=0)：size=8192。
    let bar0 = client.get_region_info(0).expect("get_region_info BAR0");
    let bar0_size = bar0.size; // packed copy
    assert_eq!(bar0_size, 8192, "BAR0 size");

    // 3. GET_REGION_INFO(CONFIG=7)：size=4096。
    let cfg = client.get_region_info(7).expect("get_region_info CONFIG");
    let cfg_size = cfg.size; // packed copy
    assert_eq!(cfg_size, 4096, "CONFIG size");

    // 4. GET_IRQ_INFO(MSIX=2)：count=8（MockDev msix_count）。
    let irq = client.get_irq_info(2).expect("get_irq_info MSIX");
    let irq_count = irq.count; // packed copy
    assert_eq!(irq_count, 8, "MSIX vector count");

    // 5. CONFIG region READ @ offset 0：identity dword0 = vendor(LE)+device(LE)。
    let id = client.region_read(7, 0, 4).expect("region_read CONFIG id");
    assert_eq!(id, vec![0x34, 0x12, 0x78, 0x56], "config dword0 identity LE");

    // 6. BAR0 REGION_WRITE then READ roundtrip @ offset 0（8 字节，对标 server
    //    region_write_then_read_roundtrip，避免 4-byte-in-u64-slot 巧合）。
    let pat = 0xCAFE_BABE_DEAD_BEEF_u64.to_le_bytes();
    client.region_write(0, 0, &pat).expect("region_write BAR0");
    let got = client.region_read(0, 0, 8).expect("region_read BAR0");
    assert_eq!(got, pat.to_vec(), "BAR0 8-byte write/read roundtrip");

    // 7. DEVICE_RESET（FLR）：不报错即可。
    client.reset().expect("reset");

    drop(client); // 关 socket → server pump_one 返 Ok(false) 退出循环。
    server.join().unwrap().expect("server loop");
}
