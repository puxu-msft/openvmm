// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W6b Task 1.2 全双工 loopback 集成测：真 `vfio_user_transport::VfioUserSession` +
//! `MockDev` ⇄ client **split** 读写半（`VfioUserClient::into_channel`）。
//!
//! **价值证明**：串行 `VfioUserClient` 每个方法是「写一帧 → 立即读 reply」，无法在收
//! 任何 reply 之前连发两帧；本测先经 writer **连发两个** REGION_READ（msg_id 10 / 11），
//! 然后才经 reader 读两帧 reply，并**按 msg_id 路由**匹配到对应请求（而非按调用顺序）。
//! 这正是 W6b underhill worker `select!` 循环需要的全双工语义——firmware 慢回不会卡死
//! worker 的发送侧。
//!
//! socketpair，Linux 直跑，无需 VTL。server 半段同步（独立线程），client 段 async。

use pal_async::DefaultPool;
use pcie_device_core::BarKind;
use pcie_device_core::BarLayout;
use pcie_device_core::DeviceCtx;
use pcie_device_core::DeviceDescribe;
use pcie_device_core::PcieDevice;
use std::os::unix::net::UnixStream;
use std::thread;
use vfio_user_device::VfioUserClient;
use vfio_user_wire::proto::RegionAccessPayload;
use zerocopy::FromBytes;

/// 测试 MockDev：BAR0 8 KiB（u64 槽）+ 8 MSI-X + identity。与 loopback_region 一致。
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

/// 全双工：先经 writer 连发两个 REGION_READ（不等任何 reply），再经 reader 读两帧并
/// **按 msg_id 路由**匹配——串行 client 做不到（它每帧写后立即读）。
#[test]
fn loopback_channel_full_duplex_msg_id_routing() {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    let server = spawn_server(server_end);

    DefaultPool::run_with(async |driver| {
        let mut client =
            VfioUserClient::from_stream(&driver, client_end).expect("wrap client stream");
        client.handshake().await.expect("handshake");

        // 预置 BAR0：offset 0 / offset 8 写入不同已知 pattern（数据唯一使断言更强：
        // 证明 reply 不仅 msg_id 对得上，数据段也对得上对应请求）。串行 client 在
        // into_channel 之前写——into_channel 后 msg_id 分配权移交调用方。
        let pat0 = 0x1111_2222_3333_4444_u64.to_le_bytes();
        let pat1 = 0xAAAA_BBBB_CCCC_DDDD_u64.to_le_bytes();
        client.region_write(0, 0, &pat0).await.expect("seed BAR0@0");
        client.region_write(0, 8, &pat1).await.expect("seed BAR0@8");

        // 拆全双工读写半（消费 client）。msg_id 分配权移交本测（worker 角色）。
        let (mut writer, mut reader) = client.into_channel();

        // ── 关键：在读任何 reply 之前，连发两个 REGION_READ（msg_id 10 / 11）──
        // BAR0 region 0；msg_id 10 读 offset 0（count 8），msg_id 11 读 offset 8（count 8）。
        writer
            .send_region_read(10, 0, 0, 8)
            .await
            .expect("send REGION_READ msg_id=10");
        writer
            .send_region_read(11, 0, 8, 8)
            .await
            .expect("send REGION_READ msg_id=11");

        // ── 然后才读两帧 reply；按 msg_id 自行匹配 in_flight（非按调用顺序）──
        let mut seen_10 = false;
        let mut seen_11 = false;
        for _ in 0..2 {
            let reply = reader.recv_reply().await.expect("recv_reply");
            let id = reply.header.msg_id; // packed 字段先 copy
            // reply payload = RegionAccessPayload echo(16B) + count 字节数据。
            let echo = RegionAccessPayload::read_from_prefix(&reply.payload)
                .expect("decode RegionAccessPayload echo")
                .0;
            let echo_offset = echo.offset; // packed copy
            let echo_region = echo.region;
            let echo_count = echo.count;
            let data = &reply.payload[16..16 + echo_count as usize];
            match id {
                10 => {
                    assert!(!seen_10, "msg_id 10 重复");
                    assert_eq!(echo_region, 0);
                    assert_eq!(echo_offset, 0, "msg_id 10 应 echo offset 0");
                    assert_eq!(echo_count, 8);
                    assert_eq!(data, pat0, "msg_id 10 数据应 = BAR0@0 pattern");
                    seen_10 = true;
                }
                11 => {
                    assert!(!seen_11, "msg_id 11 重复");
                    assert_eq!(echo_region, 0);
                    assert_eq!(echo_offset, 8, "msg_id 11 应 echo offset 8");
                    assert_eq!(echo_count, 8);
                    assert_eq!(data, pat1, "msg_id 11 数据应 = BAR0@8 pattern");
                    seen_11 = true;
                }
                other => panic!("意外 msg_id：{other}"),
            }
        }
        assert!(seen_10 && seen_11, "两个 reply 都应按 msg_id 匹配到请求");

        // 关读写半 → server pump_one 返 Ok(false) 退出。drop 顺序无所谓（共享 socket）。
        drop(writer);
        drop(reader);
    });

    server.join().unwrap().expect("server loop");
}

/// 经 writer 的便捷 REGION_WRITE 帧（fire-and-forget），再用 reader 读其 reply 验证落地。
#[test]
fn loopback_channel_region_write_then_readback() {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    let server = spawn_server(server_end);

    DefaultPool::run_with(async |driver| {
        let mut client =
            VfioUserClient::from_stream(&driver, client_end).expect("wrap client stream");
        client.handshake().await.expect("handshake");
        let (mut writer, mut reader) = client.into_channel();

        // writer 发 REGION_WRITE（msg_id 20）+ REGION_READ（msg_id 21），都不等 reply。
        let pat = 0xFEED_FACE_0BAD_F00D_u64.to_le_bytes();
        writer
            .send_region_write(20, 0, 0, &pat)
            .await
            .expect("send REGION_WRITE msg_id=20");
        writer
            .send_region_read(21, 0, 0, 8)
            .await
            .expect("send REGION_READ msg_id=21");

        // 读两帧：write reply(echo only) + read reply(echo + data)。按 msg_id 匹配。
        let mut read_data: Option<Vec<u8>> = None;
        for _ in 0..2 {
            let reply = reader.recv_reply().await.expect("recv_reply");
            let id = reply.header.msg_id; // packed copy
            if id == 21 {
                read_data = Some(reply.payload[16..16 + 8].to_vec());
            }
        }
        assert_eq!(
            read_data.expect("应收到 msg_id 21 read reply"),
            pat.to_vec(),
            "REGION_WRITE 经 writer 落地后读回应一致"
        );

        drop(writer);
        drop(reader);
    });

    server.join().unwrap().expect("server loop");
}
