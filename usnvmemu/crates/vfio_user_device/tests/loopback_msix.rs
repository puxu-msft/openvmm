// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W4 loopback 集成测：client SET_IRQS 配 MSI-X eventfd → server device
//! fire_interrupt 写 eventfd → client read 验计数++（中断 firmware→client 半段
//! 送达）。对标 POC-2。socketpair + eventfd，Linux 直跑，无需 VTL。
//!
//! 注：`Interrupt::deliver`（注入 VTL0 guest）推迟 W6（需 underhill partition，
//! 非 standalone）。本测只验"中断写到 client eventfd"半段。
//!
//! W6a：client 改 async；client 段包进 `DefaultPool::run_with`，server 半段仍同步。

use nix::sys::eventfd::EfdFlags;
use nix::sys::eventfd::EventFd;
use pal_async::DefaultPool;
use pcie_device_core::BarKind;
use pcie_device_core::BarLayout;
use pcie_device_core::DeviceCtx;
use pcie_device_core::DeviceDescribe;
use pcie_device_core::PcieDevice;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::thread;
use vfio_user_device::VfioUserClient;
use vfio_user_wire::proto::pci_irq;

const DOORBELL: u64 = 0x1000;
const FIRE_VEC: u32 = 1; // device fire 的向量号

/// doorbell @ DOORBELL → ctx.fire_interrupt(FIRE_VEC)。
struct IrqTriggerDev;
impl PcieDevice for IrqTriggerDev {
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
            msix_count: 4,
            capabilities: vec![],
            cfg_write_side_effect_offsets: vec![],
        }
    }
    fn mmio_read(&mut self, _bar: u32, _offset: u64, _size: u32) -> u64 {
        0
    }
    fn mmio_write(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        _bar: u32,
        offset: u64,
        _size: u32,
        _value: u64,
    ) {
        if offset == DOORBELL {
            ctx.fire_interrupt(FIRE_VEC);
        }
    }
}

fn spawn_server(server_end: UnixStream) -> thread::JoinHandle<anyhow::Result<()>> {
    thread::spawn(move || -> anyhow::Result<()> {
        let mut s = server_end;
        let neg = vfio_user_transport::server_handshake(&mut s)?;
        let mut sess = vfio_user_transport::VfioUserSession::new(s, neg);
        let mut dev = IrqTriggerDev;
        while sess.pump_one(&mut dev)? {}
        Ok(())
    })
}

#[test]
fn loopback_msix_fire_signals_eventfd() {
    // client 建 4 个 eventfd（counter 起始 0）。
    let efds: Vec<EventFd> = (0..4)
        .map(|_| EventFd::from_value_and_flags(0, EfdFlags::empty()).unwrap())
        .collect();
    let borrowed: Vec<_> = efds.iter().map(|e| e.as_fd()).collect();

    let (server_end, client_end) = UnixStream::pair().unwrap();
    let server = spawn_server(server_end);

    DefaultPool::run_with(async |driver| {
        let mut client =
            VfioUserClient::from_stream(&driver, client_end).expect("wrap client stream");
        client.handshake().await.expect("handshake");

        // 1. SET_IRQS 配下 4 个 eventfd 给 MSI-X 向量 [0,4)。
        client
            .set_irqs(pci_irq::MSIX, 0, &borrowed)
            .await
            .expect("set_irqs assign");

        // 2. doorbell：BAR0 REGION_WRITE @ DOORBELL → server device ctx.fire_interrupt(FIRE_VEC)
        //    → server 写 client eventfd[FIRE_VEC]。fire 在 mmio_write 内（reply 前同步），
        //    故收到 region_write reply 时 eventfd 已 signaled。
        client
            .region_write(0, DOORBELL, &1u32.to_le_bytes())
            .await
            .expect("doorbell region_write");

        drop(client);
    });

    server.join().unwrap().expect("server loop");

    // 3. 验 fire 的向量 eventfd 计数 == 1（中断送达；fire 在 server 处理 doorbell 时
    //    同步发生，计数持久，join 后读取无误）。
    let mut buf = [0u8; 8];
    nix::unistd::read(&efds[FIRE_VEC as usize], &mut buf).expect("read fired eventfd");
    assert_eq!(u64::from_ne_bytes(buf), 1, "fire 的向量 eventfd 计数应 ==1");

    // 4. 其它向量未被误触发：nonblock dup 探测，未 fire → EAGAIN。
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    for (i, e) in efds.iter().enumerate() {
        if i == FIRE_VEC as usize {
            continue;
        }
        let probe = nix::unistd::dup(e.as_fd()).expect("dup probe");
        let cur = fcntl(probe.as_fd(), FcntlArg::F_GETFL).unwrap();
        fcntl(
            probe.as_fd(),
            FcntlArg::F_SETFL(OFlag::from_bits_truncate(cur) | OFlag::O_NONBLOCK),
        )
        .unwrap();
        let mut b = [0u8; 8];
        let r = nix::unistd::read(&probe, &mut b);
        assert!(
            matches!(r, Err(nix::errno::Errno::EAGAIN)),
            "向量 {i} 不应被触发（应 EAGAIN），got {r:?}"
        );
    }
}
