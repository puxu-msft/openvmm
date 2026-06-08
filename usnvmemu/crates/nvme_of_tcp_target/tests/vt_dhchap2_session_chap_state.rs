// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-dhchap-2 集成测试** — AsyncSession 端 CHAP 状态机集成。
//!
//! 覆盖：
//! 1. `vt_dhchap2_session_chap_disabled_by_default` — 默认无 store 时
//!    sess.chap = None；Connect 正常通过
//! 2. `vt_dhchap2_session_chap_known_host_enters_challenge_needed` —
//!    enable_chap(store) 后 Connect 已知 host → chap.stage = ChallengeNeeded
//! 3. `vt_dhchap2_session_chap_unknown_host_enters_failed` —
//!    enable_chap(store) 后 Connect 未知 host → chap.stage = Failed
//! 4. `vt_dhchap2_session_chap_empty_store_disabled` — enable_chap(空 store)
//!    Connect → chap.stage = Disabled (兼容)

#![allow(missing_docs)]

use nvme_of_tcp_target::dhchap::{ChapSecretStore, ChapStage};
use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::{read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{AsyncSession, SharedControllerInner, accept_and_handshake_async};
use nvme_firmware::NvmeController;
use std::sync::Arc;
use tokio::io::AsyncWriteExt as _;
use tokio::net::{TcpListener, TcpStream};
use zerocopy::IntoBytes;

fn make_shared() -> (Arc<SharedControllerInner>, tempfile::NamedTempFile) {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(1024 * 1024).unwrap();
    let path = f.path().to_string_lossy().into_owned();
    let c = NvmeController::open(std::slice::from_ref(&path), 0x1414, 0, &[]).unwrap();
    (Arc::new(SharedControllerInner::new(c)), f)
}

fn build_connect_pdu(cid: u16, hostnqn: &str) -> (CommonHdr, Vec<u8>, Vec<u8>) {
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::CONNECT;
    let f = ConnectFabricFields {
        recfmt: 0,
        qid: 0,
        sqsize: 31,
        cattr: 0,
        rsvd1: 0,
        kato: 0,
        rsvd2: [0u8; 12],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let mut cd = ConnectData::default();
    let n = hostnqn.len().min(cd.hostnqn.len());
    cd.hostnqn[..n].copy_from_slice(&hostnqn.as_bytes()[..n]);
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 72,
        plen: 72 + 1024,
    };
    (hdr, sqe, cd.as_bytes().to_vec())
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

/// 通用 server-side：handshake → enable_chap (option) → 跑一次 Connect →
/// 返 chap stage Debug 字符串供 caller assert
async fn run_server_capture_stage(
    addr_tx: tokio::sync::oneshot::Sender<std::net::SocketAddr>,
    hostnqn: String,
    store: Option<Arc<ChapSecretStore>>,
) -> Option<String> {
    let (shared, _backing) = make_shared();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    addr_tx.send(addr).unwrap();
    let (server, _) = listener.accept().await.unwrap();
    let mut sess: AsyncSession<TcpStream> =
        accept_and_handshake_async(server, shared).await.unwrap();
    if let Some(s) = store {
        sess.enable_chap(s);
    }
    let _ = hostnqn; // 用 client 端发 Connect.hostnqn 决定真实路径
    let (_tx, mut rx) = tokio::sync::watch::channel(false);
    if let Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) = sess.pump_one_async(&mut rx).await {
        let _ = sess.dispatch_pdu_async(p).await;
    }
    sess.chap.as_ref().map(|c| format!("{:?}", c.stage))
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap2_session_chap_disabled_by_default() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(run_server_capture_stage(tx, "nqn.h".to_string(), None));
    let addr = rx.await.unwrap();
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();
    let (hdr, sqe, data) = build_connect_pdu(0x0001, "nqn.h");
    write_pdu_async(&mut client, &hdr, &sqe, &data)
        .await
        .unwrap();
    let _resp = read_pdu_async(&mut client).await.unwrap();
    client.shutdown().await.unwrap();
    let stage = server.await.unwrap();
    assert!(stage.is_none(), "未 enable_chap 时 sess.chap 应为 None");
}

async fn run_with_store(hostnqn_in_connect: &str, store: Arc<ChapSecretStore>) -> String {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let host = hostnqn_in_connect.to_string();
    let server = tokio::spawn(run_server_capture_stage(tx, host.clone(), Some(store)));
    let addr = rx.await.unwrap();
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();
    let (hdr, sqe, data) = build_connect_pdu(0x0001, hostnqn_in_connect);
    write_pdu_async(&mut client, &hdr, &sqe, &data)
        .await
        .unwrap();
    let _resp = read_pdu_async(&mut client).await.unwrap();
    client.shutdown().await.unwrap();
    server.await.unwrap().expect("chap 应已初始化")
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap2_session_chap_known_host_enters_challenge_needed() {
    let mut store = ChapSecretStore::new();
    store.insert("nqn.allowed".to_string(), vec![0x77u8; 32]);
    let stage = run_with_store("nqn.allowed", Arc::new(store)).await;
    assert!(
        stage.contains("ChallengeNeeded"),
        "已知 host 应进 ChallengeNeeded，实际: {stage}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap2_session_chap_unknown_host_enters_failed() {
    let mut store = ChapSecretStore::new();
    store.insert("nqn.legit".to_string(), vec![0x77u8; 32]);
    let stage = run_with_store("nqn.evil", Arc::new(store)).await;
    assert!(
        stage.contains("Failed"),
        "未知 host 应进 Failed，实际: {stage}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap2_session_chap_empty_store_disabled() {
    let store = ChapSecretStore::new(); // 空
    let stage = run_with_store("nqn.anything", Arc::new(store)).await;
    assert!(
        stage.contains("Disabled"),
        "空 store 应进 Disabled (兼容)，实际: {stage}"
    );
    // 校验 is_authenticated() 同步逻辑：Disabled 视为已通过
    assert!(ChapStage::Disabled.is_authenticated());
}
