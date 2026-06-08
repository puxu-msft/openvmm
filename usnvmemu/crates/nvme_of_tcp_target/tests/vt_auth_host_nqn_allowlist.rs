// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-auth 集成测试** — host NQN 白名单。
//!
//! 覆盖：
//! 1. `vt_auth_allows_listed_hostnqn` — allowlist set 内 Connect 通过 → RSP
//!    SC=0
//! 2. `vt_auth_rejects_unlisted_hostnqn` — allowlist 外 Connect → RSP SC=
//!    0x84 (CONNECT_INVALID_HOST)
//! 3. `vt_auth_disabled_when_allowlist_none` — accept_and_handshake_async
//!    (无 auth) 任意 hostnqn 都通过 (V8 兼容)

#![allow(missing_docs)]

use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::{read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{
    AsyncSession, SharedControllerInner, accept_and_handshake_async,
    accept_and_handshake_async_with_auth,
};
use nvme_firmware::NvmeController;
use std::collections::HashSet;
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

#[tokio::test(flavor = "multi_thread")]
async fn vt_auth_allows_listed_hostnqn() {
    let (shared, _backing) = make_shared();
    let mut set = HashSet::new();
    set.insert("nqn.2014-08.org.nvmexpress:uuid:host-allowed".to_string());
    let allow = Arc::new(set);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = Arc::clone(&shared);
    let a = Arc::clone(&allow);
    let server = tokio::spawn(async move {
        let (server, _) = listener.accept().await.unwrap();
        let mut sess: AsyncSession<TcpStream> = accept_and_handshake_async_with_auth(server, s, a)
            .await
            .unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        if let Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) = sess.pump_one_async(&mut rx).await {
            let _ = sess.dispatch_pdu_async(p).await.unwrap();
        }
    });
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();
    let (hdr, sqe, data) =
        build_connect_pdu(0x0001, "nqn.2014-08.org.nvmexpress:uuid:host-allowed");
    write_pdu_async(&mut client, &hdr, &sqe, &data)
        .await
        .unwrap();
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(resp.header.pdu_type, pdu_type::RSP);
    // CQE DW3 高 16-bit 含 status field (Phase + SC + SCT)
    // 此处仅看 SC 字段不出 0x84 即视作通过 (allowed)
    let cqe = &resp.psh;
    let status = u16::from_le_bytes([cqe[14], cqe[15]]);
    let sc = (status >> 1) & 0xFF;
    assert_ne!(sc, 0x84, "allowed hostnqn 不应返 CONNECT_INVALID_HOST");
    client.shutdown().await.unwrap();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_auth_rejects_unlisted_hostnqn() {
    let (shared, _backing) = make_shared();
    let mut set = HashSet::new();
    set.insert("nqn.allowed-only-this".to_string());
    let allow = Arc::new(set);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = Arc::clone(&shared);
    let a = Arc::clone(&allow);
    let server = tokio::spawn(async move {
        let (server, _) = listener.accept().await.unwrap();
        let mut sess: AsyncSession<TcpStream> = accept_and_handshake_async_with_auth(server, s, a)
            .await
            .unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        if let Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) = sess.pump_one_async(&mut rx).await {
            let _ = sess.dispatch_pdu_async(p).await;
        }
    });
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();
    let (hdr, sqe, data) = build_connect_pdu(0x0001, "nqn.evil-impersonator");
    write_pdu_async(&mut client, &hdr, &sqe, &data)
        .await
        .unwrap();
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(resp.header.pdu_type, pdu_type::RSP);
    let cqe = &resp.psh;
    let status = u16::from_le_bytes([cqe[14], cqe[15]]);
    let sc = (status >> 1) & 0xFF;
    assert_eq!(
        sc, 0x84,
        "未在白名单的 hostnqn 应返 CONNECT_INVALID_HOST (0x84); status={status:#06x} sc={sc:#04x}"
    );
    client.shutdown().await.unwrap();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_auth_disabled_when_allowlist_none() {
    let (shared, _backing) = make_shared();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = Arc::clone(&shared);
    let server = tokio::spawn(async move {
        let (server, _) = listener.accept().await.unwrap();
        let mut sess: AsyncSession<TcpStream> =
            accept_and_handshake_async(server, s).await.unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        if let Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) = sess.pump_one_async(&mut rx).await {
            let _ = sess.dispatch_pdu_async(p).await;
        }
    });
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();
    // 任意 hostnqn 都接受
    let (hdr, sqe, data) = build_connect_pdu(0x0001, "nqn.anything-goes");
    write_pdu_async(&mut client, &hdr, &sqe, &data)
        .await
        .unwrap();
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(resp.header.pdu_type, pdu_type::RSP);
    let cqe = &resp.psh;
    let status = u16::from_le_bytes([cqe[14], cqe[15]]);
    let sc = (status >> 1) & 0xFF;
    assert_ne!(sc, 0x84, "无白名单时不应返 CONNECT_INVALID_HOST");
    client.shutdown().await.unwrap();
    let _ = server.await;
}
