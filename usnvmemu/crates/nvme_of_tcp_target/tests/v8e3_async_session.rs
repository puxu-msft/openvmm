// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V8e-3 集成测试** — `AsyncSession` 异步握手 + 简化 pump 骨架。
//!
//! V8e-3 把 async surface 加到位（[`accept_and_handshake_async`] /
//! [`AsyncSession::pump_one_async`]），不动 sync `V2Session`。
//!
//! 覆盖：
//! 1. `v8e3_async_handshake_roundtrip` — tokio TcpListener+TcpStream pair 完成
//!    ICReq/ICResp + conn_id 分配 + token slab
//! 2. `v8e3_async_handshake_shares_controller_with_sync_session` — 同一
//!    `SharedControllerInner` 既起 sync `V2Session` 又起 `AsyncSession`，两者
//!    conn_id 互不撞、各自 admin CQ install idempotent OK
//! 3. `v8e3_pump_one_async_returns_none_on_peer_close` — client drop 后 server
//!    pump_one_async 返 Ok(None)（peer closed 走正常退出）
//! 4. `v8e3_pump_one_async_shutdown_signal_unblocks_immediately` — watch::Sender
//!    send(true) 后 pump_one_async 立刻返 Ok(None) 不等 read_pdu
//! 5. `v8e3_async_session_drop_cleans_pending_aers` — Drop 调
//!    nvme_cleanup_conn_aers（与 sync V2Session V8c 行为一致）
//! 6. `v8e3_async_handshake_bad_pfv_rejected` — ICReq PFV≠0 应 Err

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::framing::{read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{
    AsyncSession, PumpEvent, SharedControllerInner, V2Session, accept_and_handshake_async,
};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use zerocopy::IntoBytes;

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
async fn v8e3_async_handshake_roundtrip() {
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    let h = tokio::spawn(accept_and_handshake_async(server, Arc::clone(&shared)));

    send_icreq(&mut client).await;
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(resp.header.pdu_type, pdu_type::ICRESP);

    let sess = h.await.unwrap().expect("handshake OK");
    assert!(sess.conn_id >= 1, "conn_id 应分到 ≥1，得 {}", sess.conn_id);
    assert!(
        sess.next_token >= 1u64 << 48,
        "next_token 应在 V8b token slab 起点之后，得 {:#x}",
        sess.next_token
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e3_async_handshake_shares_controller_with_sync_session() {
    // 共 SharedControllerInner：sync V2Session + AsyncSession 都跑一遍，验
    // conn_id 各自分到不同值，admin CQ idempotent 不冲突
    let (shared, _backing) = make_shared();

    // === sync V2Session 起一条（V8e-3 BC path 在 spawn_blocking 内）===
    let shared_sync = Arc::clone(&shared);
    let sync_conn_id = tokio::task::spawn_blocking(move || {
        let (mut client, server) = std_tcp_pair();
        let h =
            std::thread::spawn(move || V2Session::accept_and_handshake_shared(server, shared_sync));
        // 发 ICReq
        let hdr = CommonHdr {
            pdu_type: pdu_type::ICREQ,
            flags: 0,
            hlen: 128,
            pdo: 0,
            plen: 128,
        };
        let psh = IcPsh::default();
        nvme_of_tcp_target::framing::write_pdu(&mut client, &hdr, psh.as_bytes(), &[]).unwrap();
        let _ = nvme_of_tcp_target::framing::read_pdu(&mut client).unwrap();
        let sess = h.join().unwrap().unwrap();
        let id = sess.conn_id;
        drop(sess); // 触发 V8c Drop
        drop(client);
        id
    })
    .await
    .unwrap();

    // === AsyncSession 起一条 ===
    let (mut a_client, a_server) = tokio_pair().await;
    let ah = tokio::spawn(accept_and_handshake_async(a_server, Arc::clone(&shared)));
    send_icreq(&mut a_client).await;
    let _ = read_pdu_async(&mut a_client).await.unwrap();
    let async_sess = ah.await.unwrap().unwrap();

    assert_ne!(
        sync_conn_id, async_sess.conn_id,
        "sync 与 async 两条 conn 必拿到不同 conn_id，得 sync={sync_conn_id} async={}",
        async_sess.conn_id
    );
    assert!(sync_conn_id >= 1 && async_sess.conn_id >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e3_pump_one_async_returns_none_on_peer_close() {
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    let h = tokio::spawn(async move {
        let mut sess = accept_and_handshake_async(server, shared).await.unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        sess.pump_one_async(&mut rx).await
    });

    send_icreq(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();
    drop(client);

    let r = h.await.unwrap().unwrap();
    assert!(
        matches!(r, PumpEvent::PeerClosed),
        "peer close 应返 PumpEvent::PeerClosed，得 {r:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e3_pump_one_async_shutdown_signal_unblocks_immediately() {
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    let (tx, rx) = tokio::sync::watch::channel(false);
    let h = tokio::spawn(async move {
        let mut sess = accept_and_handshake_async(server, shared).await.unwrap();
        let mut rx = rx;
        let start = std::time::Instant::now();
        let r = sess.pump_one_async(&mut rx).await;
        (r, start.elapsed())
    });

    // 完成 ICReq/ICResp 让 spawn 进入 pump_one_async
    send_icreq(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();

    // 等 50ms 让 pump_one_async 真的开始 await
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    tx.send(true).unwrap();

    let (r, elapsed) = h.await.unwrap();
    assert!(
        matches!(r.unwrap(), PumpEvent::Shutdown),
        "shutdown 应返 PumpEvent::Shutdown"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "shutdown signal 应立刻 unblock pump_one_async，实测 {elapsed:?}"
    );

    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e3_async_session_drop_cleans_pending_aers() {
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    // 起 session 拿 conn_id
    let ah = tokio::spawn(accept_and_handshake_async(server, Arc::clone(&shared)));
    send_icreq(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();
    let sess = ah.await.unwrap().unwrap();
    let conn_id = sess.conn_id;

    // 走 controller dispatch push 两条本 conn AER；之前 V8c 测试已用相同技巧
    {
        let mut c = shared.controller.lock();
        let _ = c.nvme_install_admin_cq(
            nvme_of_tcp_target::CQ_BASE_GPA,
            nvme_of_tcp_target::ADMIN_CQ_SIZE,
        );
        let mut t = NullTransport;
        let mut ctx = pcie_device_sdk::DeviceCtx::new(&mut t);
        for cid in [1u16, 2u16] {
            let r = c.nvme_admin_dispatch_with_conn(&mut ctx, make_aer_sqe(cid), cid, 0, conn_id);
            assert!(r.is_none(), "AER should queue async");
        }
        // 别 conn 一条
        let r = c.nvme_admin_dispatch_with_conn(&mut ctx, make_aer_sqe(3), 3, 0, 9999);
        assert!(r.is_none());
    }

    drop(sess);
    drop(client);

    let c = shared.controller.lock();
    assert_eq!(
        c.nvme_pending_aer_count_for_conn(conn_id),
        0,
        "AsyncSession Drop 应清本 conn AER"
    );
    assert_eq!(
        c.nvme_pending_aer_count_for_conn(9999),
        1,
        "别 conn AER 应保留"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e3_async_handshake_bad_pfv_rejected() {
    let (shared, _backing) = make_shared();
    let (mut client, server) = tokio_pair().await;

    let h = tokio::spawn(accept_and_handshake_async(server, shared));

    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let psh = IcPsh {
        pfv: 1, // 非法
        hpda_or_cpda: 0,
        digest: 0,
        maxr2t_or_maxh2cdata: 0,
        rsvd: [0u8; 112],
    };
    write_pdu_async(&mut client, &hdr, psh.as_bytes(), &[])
        .await
        .unwrap();
    drop(client);

    let r = h.await.unwrap();
    assert!(
        r.is_err(),
        "PFV≠0 应让 handshake fail，得 {:?}",
        r.map(|_| "ok")
    );
}

// V8e-3 关键不变性：`AsyncSession` 公开 controller / next_token getter
// 让 V8e-6 后真 dispatch 接续 V8b token slab
#[tokio::test(flavor = "current_thread")]
async fn v8e3_async_session_getters_smoke() {
    // 仅编译期接口验：fn ptr 不调用，确认 getter 签名稳定
    #[allow(dead_code)]
    fn _assert_getters(s: &AsyncSession) -> (&Arc<SharedControllerInner>, u64) {
        (s.controller(), s.next_token())
    }
}

// helper: sync std TCP pair（spawn_blocking 内用）
fn std_tcp_pair() -> (std::net::TcpStream, std::net::TcpStream) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let h = std::thread::spawn(move || l.accept().unwrap().0);
    let a = std::net::TcpStream::connect(addr).unwrap();
    let b = h.join().unwrap();
    (a, b)
}

// helper: null transport for direct controller push (V8c/V8d 同模式)
struct NullTransport;
impl pcie_device_sdk::Transport for NullTransport {
    fn fire_interrupt(&mut self, _msix_index: u32) {}
    fn dma_write(&mut self, _gpa: u64, _data: Vec<u8>) -> u64 {
        0
    }
    fn dma_read(&mut self, _gpa: u64, _len: u32) -> u64 {
        0
    }
}

fn make_aer_sqe(cid: u16) -> nvme_firmware::cmd::Sqe {
    use zerocopy::FromZeros as _;
    let mut sqe = nvme_firmware::cmd::Sqe::new_zeroed();
    sqe.cdw0 = ((cid as u32) << 16) | 0xC;
    sqe
}
