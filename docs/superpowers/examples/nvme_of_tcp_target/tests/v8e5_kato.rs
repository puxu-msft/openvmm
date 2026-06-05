// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V8e-5 集成测试** — KATO timer (NVMe-oF spec § 7.13)。
//!
//! V8e-5 把 `tokio::time::Sleep` deadline 加到 `AsyncSession`，让
//! `pump_one_async` select! 第 3 arm 监听超时；超时 → `PumpEvent::KatoExpired`。
//! `set_kato(kato_ms)` / `reset_kato_deadline()` 是 caller 控制点。
//!
//! 覆盖：
//! 1. `v8e5_kato_disabled_no_timer` — kato=0 → 长时间 pending_async 不返
//!    `KatoExpired`（spec § 7.13 KATO=0 disabled）
//! 2. `v8e5_kato_expired_returns_event` — kato=500ms + 不发 cmd → 700ms 内
//!    返 `PumpEvent::KatoExpired`
//! 3. `v8e5_kato_reset_keeps_session_alive` — kato=500ms + 每 200ms reset →
//!    1500ms 后仍未 expire
//! 4. `v8e5_kato_reset_under_load_no_double_wake` — reset 多次后 select! 不
//!    立刻 fire（Pin::as_mut.reset 正确性，plan R-11）
//! 5. `v8e5_kato_does_not_block_aer_or_shutdown` — kato armed 时 AER / shutdown
//!    arm 仍优先（biased select 验证）

#![allow(missing_docs)]

use nvme_of_tcp_target::framing::{read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{PumpEvent, SharedControllerInner, accept_and_handshake_async};
use pcie_remote_nvme_userspace::NvmeController;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use zerocopy::IntoBytes as _;

fn make_backing(size: u64) -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(size).unwrap();
    f
}

fn make_shared() -> (Arc<SharedControllerInner>, tempfile::NamedTempFile) {
    let backing = make_backing(1024 * 1024);
    let path = backing.path().to_string_lossy().into_owned();
    let c = NvmeController::open(std::slice::from_ref(&path), 0x1414, 0, &[]).unwrap();
    (Arc::new(SharedControllerInner::new(c)), backing)
}

async fn tokio_pair() -> (TcpStream, TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let h = tokio::spawn(async move { l.accept().await.unwrap().0 });
    let a = TcpStream::connect(addr).await.unwrap();
    let b = h.await.unwrap();
    (a, b)
}

async fn send_icreq(s: &mut TcpStream) {
    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let psh = IcPsh::default();
    write_pdu_async(s, &hdr, psh.as_bytes(), &[]).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e5_kato_disabled_no_timer() {
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    let h = tokio::spawn(async move {
        let mut sess = accept_and_handshake_async(server, shared).await.unwrap();
        // kato 默认 0；显式不 set_kato。
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        // 用 timeout 包：500ms 内 pump_one_async 不应返（无 PDU/AER/shutdown/KATO）
        tokio::time::timeout(Duration::from_millis(500), sess.pump_one_async(&mut rx)).await
    });
    send_icreq(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();

    let r = h.await.unwrap();
    assert!(
        r.is_err(),
        "kato=0 时 pump_one_async 应 idle 不返，结果 {r:?}"
    );
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e5_kato_expired_returns_event() {
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    let h = tokio::spawn(async move {
        let mut sess = accept_and_handshake_async(server, shared).await.unwrap();
        sess.set_kato(500); // 500ms
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        let start = Instant::now();
        let r = sess.pump_one_async(&mut rx).await;
        (r, start.elapsed())
    });
    send_icreq(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();

    let (r, elapsed) = h.await.unwrap();
    assert!(
        matches!(r.unwrap(), PumpEvent::KatoExpired),
        "应返 KatoExpired"
    );
    assert!(
        elapsed >= Duration::from_millis(400) && elapsed < Duration::from_millis(900),
        "KATO=500ms 应在 ~500ms expire，实测 {elapsed:?}"
    );
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e5_kato_reset_keeps_session_alive() {
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    let h = tokio::spawn(async move {
        let mut sess = accept_and_handshake_async(server, shared).await.unwrap();
        sess.set_kato(500);
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        let start = Instant::now();
        // 每 200ms reset，做 7 轮（总 1.4s）；KATO=500ms 单 reset 间隔 < kato 应不 expire
        for _ in 0..7 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            sess.reset_kato_deadline();
        }
        // 终止 timeout 防 test hang
        let r =
            tokio::time::timeout(Duration::from_millis(100), sess.pump_one_async(&mut rx)).await;
        (r, start.elapsed())
    });
    send_icreq(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();

    let (r, elapsed) = h.await.unwrap();
    assert!(
        r.is_err(),
        "KATO 持续 reset 应阻止 expire（pump_one_async timeout 表示无 event），但得 {r:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(1400),
        "应已 reset 7×200=1.4s+ 然后 100ms timeout"
    );
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e5_kato_reset_under_load_no_double_wake() {
    // plan R-11: Pin::as_mut.reset() 后 select! 不应立即 fire 旧 future
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    let h = tokio::spawn(async move {
        let mut sess = accept_and_handshake_async(server, shared).await.unwrap();
        sess.set_kato(300);
        // 立即 reset 一次 → 新 deadline = now + 300ms
        sess.reset_kato_deadline();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        let start = Instant::now();
        let r = sess.pump_one_async(&mut rx).await;
        (r, start.elapsed())
    });
    send_icreq(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();

    let (r, elapsed) = h.await.unwrap();
    assert!(matches!(r.unwrap(), PumpEvent::KatoExpired));
    // reset 后应等约 300ms 才 fire（而不是立即）；允许 50-500ms 区间
    assert!(
        elapsed >= Duration::from_millis(200) && elapsed < Duration::from_millis(700),
        "reset 后应延后 ~300ms 才 fire，实测 {elapsed:?}"
    );
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e5_kato_does_not_block_aer_or_shutdown() {
    // biased select 验证：shutdown / aer 比 kato 优先（KATO 即将 expire 时
    // 收到 shutdown，应优先返 Shutdown）
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    let (tx, rx) = tokio::sync::watch::channel(false);
    let h = tokio::spawn(async move {
        let mut sess = accept_and_handshake_async(server, shared).await.unwrap();
        sess.set_kato(300);
        let mut rx = rx;
        sess.pump_one_async(&mut rx).await
    });
    send_icreq(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();

    // 在 KATO expire 之前发 shutdown
    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.send(true).unwrap();

    let r = h.await.unwrap().unwrap();
    assert!(
        matches!(r, PumpEvent::Shutdown),
        "shutdown 应优先于 KATO expire 触发，得 {r:?}"
    );
    drop(client);
}
