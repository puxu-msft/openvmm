// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end integration: spawn_tcp_handshakes + minimal in-process client +
//! resolver.resolve all in one process. 验证整条 wire 协议 + handshake + state
//! 在真实 socket 上工作（不需要 KVM / 完整 OpenVMM）。

use futures::AsyncReadExt;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::socket::PolledSocket;
use pcie_remote_device::PreparedMap;
use pcie_remote_device::handshake_spawn::spawn_tcp_handshakes;
use pcie_remote_protocol::BarInfo;
use pcie_remote_protocol::DeviceDescribe;
use pcie_remote_protocol::Hello;
use pcie_remote_protocol::HelloAck;
use pcie_remote_protocol::PROTOCOL_MAGIC;
use pcie_remote_protocol::bar_info::Kind;
use pcie_remote_protocol::codec;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// 启动 OpenVMM 侧的 listener（server），然后用 std::net::TcpStream 模拟 host
/// 客户端连进去，完成 Hello/HelloAck 流程。
#[async_test]
async fn handshake_e2e_via_tcp_loopback(driver: DefaultDriver) {
    let port = 41000u16;
    let id = guid::Guid {
        data1: 0xdead_beef,
        data2: 0x1234,
        data3: 0x5678,
        data4: [0; 8],
    };

    let prepared: PreparedMap = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr = format!("127.0.0.1:{port}");
    let instances = vec![(id, addr.clone(), Duration::from_secs(5))];
    let _listener_tasks =
        spawn_tcp_handshakes(driver.clone(), driver.clone(), instances, prepared.clone());

    // 等 server bind 后，作为 client connect 上去
    pal_async::timer::PolledTimer::new(&driver)
        .sleep(Duration::from_millis(200))
        .await;

    let stream = std::net::TcpStream::connect(&addr).expect("client connect");
    stream.set_nonblocking(true).unwrap();
    let mut polled = PolledSocket::new(&driver, stream).unwrap();

    // server 端会先发 Hello
    let hello: Hello = codec::read_frame(&mut polled).await.expect("recv hello");
    assert_eq!(hello.magic, PROTOCOL_MAGIC);
    assert_eq!(hello.instance_id.len(), 16);

    // 回 HelloAck
    let ack = HelloAck {
        ok: true,
        reason: String::new(),
        device: Some(DeviceDescribe {
            vendor_id: 0x1414,
            device_id: 0xc0de,
            class_code: 0x010802,
            revision: 1,
            subsystem_vendor: 0,
            subsystem_device: 0,
            bars: vec![BarInfo {
                index: 0,
                size: 4096,
                kind: Kind::Mmio32 as i32,
                prefetchable: false,
            }],
            msix_count: 1,
            capabilities: vec![],
            cfg_write_side_effect_offsets: vec![],
        }),
    };
    codec::write_frame(&mut polled, &ack)
        .await
        .expect("send ack");

    // 等 handshake task 把 prepared 填好
    for _ in 0..40 {
        if prepared.lock().contains_key(&id) {
            break;
        }
        pal_async::timer::PolledTimer::new(&driver)
            .sleep(Duration::from_millis(50))
            .await;
    }
    assert!(
        prepared.lock().contains_key(&id),
        "handshake did not populate prepared_map"
    );

    // 验证 DeviceDescribe 真的进了 prepared
    let prep = prepared.lock().remove(&id).unwrap();
    let d = prep.describe;
    assert_eq!(d.vendor_id, 0x1414);
    assert_eq!(d.device_id, 0xc0de);
    assert_eq!(d.bars.len(), 1);
    assert_eq!(d.bars[0].size, 4096);

    // 保持 reader 一会儿避免 worker EOF 立即 Lost
    let _ = polled.read(&mut [0u8; 1]);
}

/// host stub 永远不来 → 总超时后 prepared_map 仍为空（绝不 boot fail）。
#[async_test]
async fn handshake_timeout_leaves_prepared_empty(driver: DefaultDriver) {
    let port = 41001u16;
    let id = guid::Guid {
        data1: 0xfeed_face,
        ..Default::default()
    };
    let prepared: PreparedMap = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let _listener_tasks = spawn_tcp_handshakes(
        driver.clone(),
        driver.clone(),
        vec![(id, format!("127.0.0.1:{port}"), Duration::from_millis(500))],
        prepared.clone(),
    );
    // 等超时
    pal_async::timer::PolledTimer::new(&driver)
        .sleep(Duration::from_millis(800))
        .await;
    assert!(
        !prepared.lock().contains_key(&id),
        "prepared_map should remain empty after handshake timeout"
    );
}

/// bind 失败（端口被占）→ task 立刻退出，prepared 不被改动。
#[async_test]
async fn bind_failure_does_not_panic(driver: DefaultDriver) {
    let port = 41002u16;
    let _hog = std::net::TcpListener::bind(("127.0.0.1", port)).expect("hog bind");
    let id = guid::Guid {
        data1: 0xabad_1dea,
        ..Default::default()
    };
    let prepared: PreparedMap = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let _listener_tasks = spawn_tcp_handshakes(
        driver.clone(),
        driver.clone(),
        vec![(id, format!("127.0.0.1:{port}"), Duration::from_millis(500))],
        prepared.clone(),
    );
    pal_async::timer::PolledTimer::new(&driver)
        .sleep(Duration::from_millis(200))
        .await;
    assert!(!prepared.lock().contains_key(&id));
}
