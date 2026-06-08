// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V8e-4 集成测试** — AER wakeup Notify channel + AsyncSession select! 3rd arm。
//!
//! V8e-4 把 AER notify 加到 `SharedControllerInner.aen_notify`；
//! `AsyncSession.pump_one_async` 加第 3 个 select! arm 监听 Notify，唤醒后
//! 自检 `nvme_pending_aer_count_for_conn(conn_id)` 过滤 spurious wakeup。
//!
//! 覆盖：
//! 1. `v8e4_notify_after_push_visible` — 在另一 task push AER + `notify_waiters`
//!    后 session pump_one_async 立刻返 `AenReady{pending: 1}`
//! 2. `v8e4_aer_emit_latency_under_50ms` — 从 push+notify 到 session 拿到 AenReady
//!    延迟 < 50ms（plan §1 表："< 10ms"在 GHC 上一般达到；CI 余量到 50ms）
//! 3. `v8e4_two_conns_per_conn_aer_self_check` — 共享 Notify 但 conn A push 后
//!    conn B 唤醒发现自己 pending=0，conn A 唤醒发现 pending=1（spurious 过滤验证）
//! 4. `v8e4_spurious_notify_returns_zero_pending` — 直接 `notify_aer()` 不 push
//!    AER，pump_one_async 返 `AenReady { pending: 0 }`（caller drain 时 no-op）
//! 5. `v8e4_notify_does_not_block_read_pdu` — Notify 已被 caller 消费后，read_pdu
//!    arm 仍可正常工作（不让 Notify 抢占 stream）

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::framing::{read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{PumpEvent, SharedControllerInner, accept_and_handshake_async};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use zerocopy::{FromZeros as _, IntoBytes as _};

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

/// 通过 controller dispatch_with_conn 路径 push 一条 AER 给指定 conn，并
/// notify wakeup 给 session。模拟 V8e-6 后真 dispatch 路径的行为。
fn push_aer_and_notify(shared: &Arc<SharedControllerInner>, cid: u16, conn_id: u32) {
    use nvme_firmware::cmd::Sqe;
    struct NullTransport;
    impl pcie_device_sdk::Transport for NullTransport {
        fn fire_interrupt(&mut self, _: u32) {}
        fn dma_write(&mut self, _: u64, _: Vec<u8>) -> u64 {
            0
        }
        fn dma_read(&mut self, _: u64, _: u32) -> u64 {
            0
        }
    }
    {
        let mut c = shared.controller.lock();
        let _ = c.nvme_install_admin_cq(
            nvme_of_tcp_target::CQ_BASE_GPA,
            nvme_of_tcp_target::ADMIN_CQ_SIZE,
        );
        let mut t = NullTransport;
        let mut ctx = pcie_device_sdk::DeviceCtx::new(&mut t);
        let mut sqe = Sqe::new_zeroed();
        sqe.cdw0 = ((cid as u32) << 16) | 0xC;
        let _ = c.nvme_admin_dispatch_with_conn(&mut ctx, sqe, cid, 0, conn_id);
    }
    shared.notify_aer();
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e4_notify_after_push_visible() {
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    let shared_for_task = Arc::clone(&shared);
    let h = tokio::spawn(async move {
        let mut sess = accept_and_handshake_async(server, shared_for_task)
            .await
            .unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        sess.pump_one_async(&mut rx).await
    });

    // 完成 ICReq/ICResp 让 server 进 pump_one_async
    send_icreq(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();

    // 给 spawn 一点时间走到 pump_one_async select!
    tokio::time::sleep(Duration::from_millis(50)).await;
    // server conn_id 由 SharedControllerInner.next_conn_id 单调；本测试之前没分
    // 别的 conn_id，所以唯一 session 拿到 conn_id=1
    push_aer_and_notify(&shared, 0xAA, 1);

    let r = h.await.unwrap().unwrap();
    match r {
        PumpEvent::AenReady { pending } => {
            assert_eq!(pending, 1, "push 1 条 AER 自检应见 pending=1");
        }
        other => panic!("expected AenReady, got {other:?}"),
    }
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e4_aer_emit_latency_under_50ms() {
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    let shared_for_task = Arc::clone(&shared);
    let h = tokio::spawn(async move {
        let mut sess = accept_and_handshake_async(server, shared_for_task)
            .await
            .unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        sess.pump_one_async(&mut rx).await
    });
    send_icreq(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let push_at = Instant::now();
    push_aer_and_notify(&shared, 0xBB, 1);
    let r = h.await.unwrap().unwrap();
    let elapsed = push_at.elapsed();

    assert!(matches!(r, PumpEvent::AenReady { pending: 1 }));
    assert!(
        elapsed < Duration::from_millis(50),
        "AER notify→session pickup 应 < 50ms，实测 {elapsed:?}"
    );
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e4_two_conns_per_conn_aer_self_check() {
    // 共享 Notify 但 push 进 conn_a，conn_b 唤醒后自检发现自己 pending=0
    let (shared, _backing) = make_shared();
    let (mut client_a, server_a) = tokio_pair().await;
    let (mut client_b, server_b) = tokio_pair().await;

    let s1 = Arc::clone(&shared);
    let ha = tokio::spawn(async move {
        let mut sess = accept_and_handshake_async(server_a, s1).await.unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        let event = sess.pump_one_async(&mut rx).await.unwrap();
        (sess.conn_id, event)
    });
    let s2 = Arc::clone(&shared);
    let hb = tokio::spawn(async move {
        let mut sess = accept_and_handshake_async(server_b, s2).await.unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        let event = sess.pump_one_async(&mut rx).await.unwrap();
        (sess.conn_id, event)
    });
    send_icreq(&mut client_a).await;
    send_icreq(&mut client_b).await;
    let _ = read_pdu_async(&mut client_a).await.unwrap();
    let _ = read_pdu_async(&mut client_b).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 只 push 给 conn 1
    push_aer_and_notify(&shared, 0xCC, 1);

    let (a_id, a_event) = ha.await.unwrap();
    let (b_id, b_event) = hb.await.unwrap();
    assert_ne!(a_id, b_id, "两 conn id 必不同");

    // 由 next_conn_id atomic 单调，conn_a 先 handshake 拿到 1，conn_b 拿 2
    // （但 spawn 顺序不严格保证 → match 任一 conn 拿 pending=1 即可）
    let (with_aer, no_aer) = if a_id == 1 {
        (a_event, b_event)
    } else {
        (b_event, a_event)
    };
    assert!(
        matches!(with_aer, PumpEvent::AenReady { pending: 1 }),
        "目标 conn 应见 pending=1，得 {with_aer:?}"
    );
    assert!(
        matches!(no_aer, PumpEvent::AenReady { pending: 0 }),
        "别 conn spurious wakeup 应见 pending=0，得 {no_aer:?}"
    );

    drop(client_a);
    drop(client_b);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e4_spurious_notify_returns_zero_pending() {
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    let s1 = Arc::clone(&shared);
    let h = tokio::spawn(async move {
        let mut sess = accept_and_handshake_async(server, s1).await.unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        sess.pump_one_async(&mut rx).await
    });
    send_icreq(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // notify 但不 push AER → spurious wakeup
    shared.notify_aer();

    let r = h.await.unwrap().unwrap();
    assert!(
        matches!(r, PumpEvent::AenReady { pending: 0 }),
        "spurious notify 应见 pending=0，得 {r:?}"
    );
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e4_notify_does_not_block_read_pdu() {
    // V8e-4 不该让 Notify arm 饥饿 read_pdu arm；本测试：notify 消费完后让
    // client 发一帧 PDU，session 应 pump 出 `PumpEvent::Pdu(_)`
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    let s1 = Arc::clone(&shared);
    let h = tokio::spawn(async move {
        let mut sess = accept_and_handshake_async(server, s1).await.unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        // 第 1 次：spurious notify
        let _ = sess.pump_one_async(&mut rx).await.unwrap();
        // 第 2 次：等真 PDU
        sess.pump_one_async(&mut rx).await
    });

    send_icreq(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 触发 1 次 spurious wakeup
    shared.notify_aer();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 再发一帧 ICReq（虽然语义错但 read_pdu_async 只看 framing），让 session
    // 从 PumpEvent::Pdu 路径返
    send_icreq(&mut client).await;

    let r = h.await.unwrap().unwrap();
    assert!(
        matches!(r, PumpEvent::Pdu(_)),
        "spurious notify 后 read_pdu arm 仍应工作，得 {r:?}"
    );
    drop(client);
}
