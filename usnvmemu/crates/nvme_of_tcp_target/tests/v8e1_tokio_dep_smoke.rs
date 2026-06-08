// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V8e-1 集成测试** — tokio dep + async framing 基础设施 smoke。
//!
//! V8e-1 在 crate 加 tokio runtime / framing async 版本而不动 bin / session
//! 行为。本文件验：
//!
//! 1. `v8e1_tokio_time_sleep_works` — `tokio::time::sleep(5ms).await` 验 time feature
//! 2. `v8e1_tokio_signal_module_available` — `tokio::signal::ctrl_c` symbol 存在
//! 3. `v8e1_watch_channel_shutdown_smoke` — tokio::sync::watch 作 shutdown 信号
//! 4. `v8e1_async_framing_read_write_roundtrip_via_duplex` — read_pdu_async /
//!    write_pdu_async 通过 `tokio::io::duplex` 全双工 pair byte-stream 等价
//! 5. `v8e1_async_framing_peer_closed_returns_framing_error` — half-close 后
//!    read_pdu_async 返 `FramingError::PeerClosed`，语义与 sync 版一致
//! 6. `v8e1_sync_vs_async_serialize_bytes_identical` — 同一 (hdr, psh, data)
//!    走 sync `write_pdu` 与 async `write_pdu_async` 后 byte stream 全等
//!
//! regression gate：跑完本文件应 baseline 124 active test 不动。

#![allow(missing_docs)]

use nvme_of_tcp_target::framing::FramingError;
use nvme_of_tcp_target::pdu::{CommonHdr, R2tPsh, pdu_type};
use nvme_of_tcp_target::{read_pdu_async, write_pdu, write_pdu_async};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;
use zerocopy::IntoBytes;

#[tokio::test(flavor = "current_thread")]
async fn v8e1_tokio_time_sleep_works() {
    let start = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(5),
        "sleep 应至少 5ms，实测 {elapsed:?}"
    );
}

/// `tokio::signal::ctrl_c` 是 V8e-2 shutdown 信号源；本测试只验 symbol 存在
/// 且能 spawn future（不实际触发信号，避免影响 test runner）。
#[tokio::test(flavor = "current_thread")]
async fn v8e1_tokio_signal_module_available() {
    // future 不 .await，仅验类型；timeout 后丢弃。
    let fut = tokio::signal::ctrl_c();
    let _ = tokio::time::timeout(Duration::from_millis(1), fut).await;
}

/// `tokio::sync::watch` 是 V8e-2 用来替代 V8f `AtomicBool running` 的 shutdown
/// 信号 channel；clone-able + 多消费者各自 `.changed().await`。
#[tokio::test(flavor = "current_thread")]
async fn v8e1_watch_channel_shutdown_smoke() {
    let (tx, mut rx1) = tokio::sync::watch::channel(false);
    let mut rx2 = rx1.clone();

    let t1 = tokio::spawn(async move {
        rx1.changed().await.unwrap();
        *rx1.borrow_and_update()
    });
    let t2 = tokio::spawn(async move {
        rx2.changed().await.unwrap();
        *rx2.borrow_and_update()
    });
    // delay 一下让两个 task park 在 changed().await
    tokio::time::sleep(Duration::from_millis(5)).await;
    tx.send(true).unwrap();

    assert!(t1.await.unwrap(), "task 1 应观察到 shutdown=true");
    assert!(t2.await.unwrap(), "task 2 同样");
}

#[tokio::test(flavor = "current_thread")]
async fn v8e1_async_framing_read_write_roundtrip_via_duplex() {
    // tokio::io::duplex 是内存 full-duplex pipe，省 TCP 也能测 async framing
    let (mut a, mut b) = tokio::io::duplex(8 * 1024);

    let hdr = CommonHdr {
        pdu_type: pdu_type::R2T,
        flags: 0,
        hlen: 24,
        pdo: 0,
        plen: 24,
    };
    let psh = R2tPsh {
        cccid: 0x1234,
        ttag: 0x5678,
        r2t_offset: 0,
        r2t_length: 4096,
        rsvd: [0u8; 4],
    };

    // writer task
    let writer = tokio::spawn(async move {
        write_pdu_async(&mut a, &hdr, psh.as_bytes(), &[])
            .await
            .unwrap();
        // close 写端让 reader 看到 EOF（如果后续还想 read）
        drop(a);
    });

    let pdu = read_pdu_async(&mut b).await.expect("read_pdu_async");
    writer.await.unwrap();

    assert_eq!(pdu.header.pdu_type, pdu_type::R2T);
    let r2t: R2tPsh = nvme_of_tcp_target::pdu::decode_psh(&pdu.psh).unwrap();
    let cccid = r2t.cccid;
    let ttag = r2t.ttag;
    let r2t_offset = r2t.r2t_offset;
    let r2t_length = r2t.r2t_length;
    assert_eq!(cccid, 0x1234);
    assert_eq!(ttag, 0x5678);
    assert_eq!(r2t_offset, 0);
    assert_eq!(r2t_length, 4096);
    assert!(pdu.data.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn v8e1_async_framing_peer_closed_returns_framing_error() {
    let (a, mut b) = tokio::io::duplex(64);
    drop(a); // 立刻 close 写端 → reader 应见 EOF
    let err = read_pdu_async(&mut b).await.expect_err("应返 PeerClosed");
    let downcast = err.downcast_ref::<FramingError>();
    assert!(
        matches!(downcast, Some(FramingError::PeerClosed { .. })),
        "应是 FramingError::PeerClosed，得 {err:?}"
    );
}

/// V8e-1 关键不变性：sync 与 async write_pdu 同输入应产出 byte-完全等同
/// 的 wire 帧。让 V8e-2/3 切换路径时不漂 protocol-level 行为。
#[tokio::test(flavor = "current_thread")]
async fn v8e1_sync_vs_async_serialize_bytes_identical() {
    let hdr = CommonHdr {
        pdu_type: pdu_type::C2H_DATA,
        flags: 0,
        hlen: 24,
        pdo: 24,
        plen: 24 + 32,
    };
    let psh = nvme_of_tcp_target::pdu::DataPsh {
        cccid: 0x0042,
        ttag_or_rsvd: 0,
        data_offset: 0,
        data_length: 32,
        rsvd: [0u8; 4],
    };
    let data: Vec<u8> = (0..32).collect();

    // sync 路径：用 std TCP pair + 读回 sink（sync IO 必须 spawn_blocking 才能跑在 async test 里）
    let psh_bytes = psh.as_bytes().to_vec();
    let data_clone = data.clone();
    let sync_bytes = tokio::task::spawn_blocking(move || {
        let (a, b) = tcp_pair();
        let mut a_sync = a;
        let mut b_sync = b;
        write_pdu(&mut a_sync, &hdr, &psh_bytes, &data_clone).unwrap();
        drop(a_sync);
        use std::io::Read as _;
        let mut sync_bytes = Vec::new();
        b_sync.read_to_end(&mut sync_bytes).unwrap();
        sync_bytes
    })
    .await
    .unwrap();

    // async 路径：tokio duplex + 读回 sink
    let async_bytes = {
        let (mut a, mut b) = tokio::io::duplex(8 * 1024);
        write_pdu_async(&mut a, &hdr, psh.as_bytes(), &data)
            .await
            .unwrap();
        drop(a);
        use tokio::io::AsyncReadExt as _;
        let mut buf = Vec::new();
        b.read_to_end(&mut buf).await.unwrap();
        buf
    };

    assert_eq!(
        sync_bytes, async_bytes,
        "sync 与 async write_pdu 应产同 byte stream；diff 会让 V8e-3 后 wire 不兼容"
    );
}

fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let h = std::thread::spawn(move || listener.accept().unwrap().0);
    let a = TcpStream::connect(addr).unwrap();
    let b = h.join().unwrap();
    (a, b)
}
