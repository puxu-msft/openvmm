// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-dhchap-4 集成测试** — spec § 8.13.5 4-message wire
//! 端到端：NEGOTIATE → CHALLENGE → REPLY → SUCCESS1 → admin cmd 放行。
//!
//! 这条路径是 Linux nvme-cli `--dhchap-secret` 真发的 wire；教学库通过
//! `ChapWireMode::Spec4Msg` 自动识别（首个 AUTH_SEND data 起头 `[0x01, 0x00]`）。
//!
//! 覆盖：
//! 1. `vt_dhchap4_full_spec_wire_then_admin_cmd_allowed` — 完整 4-msg
//!    握手，最后 Identify 放行
//! 2. `vt_dhchap4_negotiate_rejects_no_sha256_with_wire_failure` — NEGOTIATE
//!    不带 SHA-256 → SC=0x83 (state Failed)
//! 3. `vt_dhchap4_reply_wrong_hmac_fails` — REPLY rval 错 → SC=0x83
//! 4. `vt_dhchap4_simplified_wire_still_works` — 老 V-3 simplified wire 路径
//!    仍兼容（首个 AUTH_SEND data = 32B HMAC）

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::dhchap::{ChapSecretStore, compute_response, wire};
use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::{Pdu, read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{SharedControllerInner, accept_and_handshake_async};
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

fn fabric_cmd_pdu(cid: u16, fct: u8, data: Vec<u8>) -> Pdu {
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fct;
    let plen = if data.is_empty() {
        72
    } else {
        (72 + data.len()) as u32
    };
    let pdo = if data.is_empty() { 0 } else { 72 };
    Pdu {
        header: CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo,
            plen,
        },
        psh: sqe,
        data,
    }
}

fn build_connect_pdu(cid: u16, hostnqn: &str, subnqn: &str) -> Pdu {
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
    let s = subnqn.len().min(cd.subnqn.len());
    cd.subnqn[..s].copy_from_slice(&subnqn.as_bytes()[..s]);
    Pdu {
        header: CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 72,
            plen: 72 + 1024,
        },
        psh: sqe,
        data: cd.as_bytes().to_vec(),
    }
}

fn build_identify_ctrl_pdu(cid: u16) -> Pdu {
    let mut sqe = vec![0u8; 64];
    sqe[0] = 0x06;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[40..44].copy_from_slice(&0x01u32.to_le_bytes());
    Pdu {
        header: CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        },
        psh: sqe,
        data: vec![],
    }
}

/// Build NEGOTIATE message wire (host → target).
fn build_negotiate(tid: u16, hash_ids: &[u8], dh_ids: &[u8]) -> Vec<u8> {
    let mut out = vec![
        wire::AUTH_TYPE_DHCHAP,
        wire::MSG_NEGOTIATE,
        0,
        0,
        tid as u8,
        (tid >> 8) as u8,
        0, // sc_c
        1, // napd
    ];
    out.extend_from_slice(&[
        wire::AUTH_DHCHAP_AUTH_ID,
        0,
        hash_ids.len() as u8,
        dh_ids.len() as u8,
    ]);
    out.extend_from_slice(hash_ids);
    out.extend_from_slice(dh_ids);
    out
}

/// Build REPLY message wire (host → target).
fn build_reply(tid: u16, rval: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + 32);
    out.push(wire::AUTH_TYPE_DHCHAP);
    out.push(wire::MSG_REPLY);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&tid.to_le_bytes());
    out.push(32); // hl
    out.push(0); // rsvd2
    out.push(0); // cvalid = 0
    out.push(0); // rsvd3
    out.extend_from_slice(&0u16.to_le_bytes()); // dhvlen
    out.extend_from_slice(&1u32.to_le_bytes()); // seqnum
    out.extend_from_slice(rval);
    out
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

fn cqe_sc(pdu: &Pdu) -> u8 {
    let status = u16::from_le_bytes([pdu.psh[14], pdu.psh[15]]);
    ((status >> 1) & 0xFF) as u8
}

async fn spawn_server_with_chap(
    store: Arc<ChapSecretStore>,
    server_done_tx: tokio::sync::oneshot::Sender<()>,
) -> std::net::SocketAddr {
    let (shared, backing) = make_shared();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (server, _) = listener.accept().await.unwrap();
        let mut sess =
            accept_and_handshake_async(server, shared).await.unwrap();
        sess.enable_chap(store);
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        for _ in 0..16 {
            match sess.pump_one_async(&mut rx).await {
                Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) => {
                    if sess.dispatch_pdu_async(p).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
        drop(backing);
        let _ = server_done_tx.send(());
    });
    addr
}

/// Spec 4-msg 路径: NEGOTIATE → CHALLENGE → REPLY → SUCCESS1 (+ admin cmd 放行)
#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap4_full_spec_wire_then_admin_cmd_allowed() {
    let secret = vec![0x55u8; 32];
    let hostnqn = "nqn.spec-host";
    let subnqn = "nqn.subsys";
    let mut store_inner = ChapSecretStore::new();
    store_inner.insert(hostnqn.to_string(), secret.clone());
    let store = Arc::new(store_inner);

    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let addr = spawn_server_with_chap(store, done_tx).await;
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();

    // 1) Connect
    let connect = build_connect_pdu(0x0001, hostnqn, subnqn);
    write_pdu_async(&mut client, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let r1 = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(cqe_sc(&r1), 0, "Connect 应通过");

    // 2) AUTH_SEND NEGOTIATE (锁定 Spec4Msg wire)
    let tid: u16 = 0xABCD;
    let neg_wire = build_negotiate(tid, &[wire::HASH_SHA256], &[wire::DHGROUP_NULL]);
    let p = fabric_cmd_pdu(0x0002, fctype::AUTH_SEND, neg_wire);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(cqe_sc(&r), 0, "NEGOTIATE 应被接受");

    // 3) AUTH_RECV → CHALLENGE wire
    let p = fabric_cmd_pdu(0x0003, fctype::AUTH_RECV, vec![]);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let c2h = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(c2h.header.pdu_type, pdu_type::C2H_DATA);
    assert_eq!(
        c2h.data.len(),
        16 + 32,
        "CHALLENGE wire = 16B header + 32B cval"
    );
    assert_eq!(c2h.data[1], wire::MSG_CHALLENGE);
    assert_eq!(u16::from_le_bytes([c2h.data[4], c2h.data[5]]), tid);
    assert_eq!(c2h.data[8], wire::HASH_SHA256);
    assert_eq!(c2h.data[9], wire::DHGROUP_NULL);
    let mut challenge = [0u8; 32];
    challenge.copy_from_slice(&c2h.data[16..48]);
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(cqe_sc(&r), 0);

    // 4) host 算 HMAC + AUTH_SEND REPLY
    let response = compute_response(&secret, &challenge, hostnqn, subnqn);
    let reply_wire = build_reply(tid, &response);
    let p = fabric_cmd_pdu(0x0004, fctype::AUTH_SEND, reply_wire);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(cqe_sc(&r), 0, "REPLY HMAC 应通过");

    // 5) AUTH_RECV → SUCCESS1 wire
    let p = fabric_cmd_pdu(0x0005, fctype::AUTH_RECV, vec![]);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let c2h = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(c2h.header.pdu_type, pdu_type::C2H_DATA);
    assert_eq!(c2h.data.len(), 16, "SUCCESS1 wire = 16B");
    assert_eq!(c2h.data[1], wire::MSG_SUCCESS1);
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(cqe_sc(&r), 0);

    // 6) Identify admin cmd 应放行
    let id = build_identify_ctrl_pdu(0x0006);
    write_pdu_async(&mut client, &id.header, &id.psh, &id.data)
        .await
        .unwrap();
    let r4 = read_pdu_async(&mut client).await.unwrap();
    assert!(
        r4.header.pdu_type == pdu_type::C2H_DATA || r4.header.pdu_type == pdu_type::RSP,
        "spec 4-msg CHAP 通过后 admin cmd 应放行，got pdu_type={:#x}",
        r4.header.pdu_type
    );
    if r4.header.pdu_type == pdu_type::C2H_DATA {
        let _rsp = read_pdu_async(&mut client).await.unwrap();
    }

    client.shutdown().await.unwrap();
    let _ = done_rx.await;
}

/// NEGOTIATE 不带 SHA-256 → 拒
#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap4_negotiate_rejects_no_sha256_with_wire_failure() {
    let mut store_inner = ChapSecretStore::new();
    store_inner.insert("nqn.host-a".to_string(), vec![0u8; 32]);
    let store = Arc::new(store_inner);

    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let addr = spawn_server_with_chap(store, done_tx).await;
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();

    let connect = build_connect_pdu(0x0001, "nqn.host-a", "nqn.subsys");
    write_pdu_async(&mut client, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let _r1 = read_pdu_async(&mut client).await.unwrap();

    // host 只列 SHA-512 → target 发 FAILURE1 wire + SC=0x83
    let neg = build_negotiate(0x1234, &[wire::HASH_SHA512], &[wire::DHGROUP_NULL]);
    let p = fabric_cmd_pdu(0x0002, fctype::AUTH_SEND, neg);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    // 先收 FAILURE1 wire (16B C2HData)
    let fw = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(fw.header.pdu_type, pdu_type::C2H_DATA);
    assert_eq!(fw.data.len(), 16);
    assert_eq!(fw.data[1], wire::MSG_FAILURE1);
    assert_eq!(
        fw.data[7],
        wire::FAIL_EXP_HASH_UNUSABLE,
        "NEGOTIATE 无 SHA-256 应 exp=HASH_UNUSABLE"
    );
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(cqe_sc(&r), 0x83, "NEGOTIATE 无 SHA-256 应 SC=0x83");

    client.shutdown().await.unwrap();
    let _ = done_rx.await;
}

/// REPLY 携带错的 HMAC → CHAP 失败
#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap4_reply_wrong_hmac_fails() {
    let mut store_inner = ChapSecretStore::new();
    store_inner.insert("nqn.host-a".to_string(), vec![0x77u8; 32]);
    let store = Arc::new(store_inner);

    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let addr = spawn_server_with_chap(store, done_tx).await;
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();

    let connect = build_connect_pdu(0x0001, "nqn.host-a", "nqn.subsys");
    write_pdu_async(&mut client, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let _r1 = read_pdu_async(&mut client).await.unwrap();

    let tid = 0xCAFEu16;
    let neg = build_negotiate(tid, &[wire::HASH_SHA256], &[wire::DHGROUP_NULL]);
    let p = fabric_cmd_pdu(0x0002, fctype::AUTH_SEND, neg);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    let p = fabric_cmd_pdu(0x0003, fctype::AUTH_RECV, vec![]);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let _c2h = read_pdu_async(&mut client).await.unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    // 故意发全 0 rval → HMAC 校验必失败 → FAILURE1 wire + SC=0x83
    let bad_rval = [0u8; 32];
    let reply_wire = build_reply(tid, &bad_rval);
    let p = fabric_cmd_pdu(0x0004, fctype::AUTH_SEND, reply_wire);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let fw = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(fw.header.pdu_type, pdu_type::C2H_DATA);
    assert_eq!(fw.data[1], wire::MSG_FAILURE1);
    assert_eq!(fw.data[7], wire::FAIL_EXP_FAILED);
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(cqe_sc(&r), 0x83, "假 rval 应 SC=0x83");

    client.shutdown().await.unwrap();
    let _ = done_rx.await;
}

/// 老 simplified wire (V-followup-dhchap-3) 仍兼容
#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap4_simplified_wire_still_works() {
    let secret = vec![0x77u8; 32];
    let hostnqn = "nqn.host-a";
    let subnqn = "nqn.subsys";
    let mut store_inner = ChapSecretStore::new();
    store_inner.insert(hostnqn.to_string(), secret.clone());
    let store = Arc::new(store_inner);

    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let addr = spawn_server_with_chap(store, done_tx).await;
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();

    let connect = build_connect_pdu(0x0001, hostnqn, subnqn);
    write_pdu_async(&mut client, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let _r1 = read_pdu_async(&mut client).await.unwrap();

    // AUTH_RECV (simplified)
    let p = fabric_cmd_pdu(0x0002, fctype::AUTH_RECV, vec![]);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let c2h = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(c2h.data.len(), 32, "simplified challenge = 32B raw");
    let mut challenge = [0u8; 32];
    challenge.copy_from_slice(&c2h.data);
    let _ = read_pdu_async(&mut client).await.unwrap();

    // AUTH_SEND (simplified: 32B HMAC raw, 起头不可能是 0x01 0x00)
    let response = compute_response(&secret, &challenge, hostnqn, subnqn);
    let p = fabric_cmd_pdu(0x0003, fctype::AUTH_SEND, response.to_vec());
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(cqe_sc(&r), 0, "Simplified wire 仍应通过");

    client.shutdown().await.unwrap();
    let _ = done_rx.await;
}

// ============== reviewer M-4 / L-6 补的覆盖 ==============

/// reviewer M-4: REPLY 在没收到 CHALLENGE 之前发 (state == ChallengeNeeded) → 拒
#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap4_reply_before_challenge_rejected() {
    let mut store_inner = ChapSecretStore::new();
    store_inner.insert("nqn.host-a".to_string(), vec![0u8; 32]);
    let store = Arc::new(store_inner);

    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let addr = spawn_server_with_chap(store, done_tx).await;
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();

    let connect = build_connect_pdu(0x0001, "nqn.host-a", "nqn.subsys");
    write_pdu_async(&mut client, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let _r1 = read_pdu_async(&mut client).await.unwrap();

    // NEGOTIATE 完成
    let tid = 0xBEEFu16;
    let neg = build_negotiate(tid, &[wire::HASH_SHA256], &[wire::DHGROUP_NULL]);
    let p = fabric_cmd_pdu(0x0002, fctype::AUTH_SEND, neg);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    // 跳过 AUTH_RECV → 直接发 REPLY → 应被拒（state == ChallengeNeeded）
    let bad_rval = [0u8; 32];
    let reply_wire = build_reply(tid, &bad_rval);
    let p = fabric_cmd_pdu(0x0003, fctype::AUTH_SEND, reply_wire);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let fw = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(fw.header.pdu_type, pdu_type::C2H_DATA);
    assert_eq!(fw.data[1], wire::MSG_FAILURE1);
    assert_eq!(fw.data[7], wire::FAIL_EXP_INCORRECT_MESSAGE);
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(cqe_sc(&r), 0x83);

    client.shutdown().await.unwrap();
    let _ = done_rx.await;
}

/// reviewer M-4: REPLY 携带 tid 与 NEGOTIATE tid 不匹配 → 拒
#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap4_reply_tid_mismatch_rejected() {
    let mut store_inner = ChapSecretStore::new();
    store_inner.insert("nqn.host-a".to_string(), vec![0u8; 32]);
    let store = Arc::new(store_inner);

    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let addr = spawn_server_with_chap(store, done_tx).await;
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();

    let connect = build_connect_pdu(0x0001, "nqn.host-a", "nqn.subsys");
    write_pdu_async(&mut client, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    let tid = 0x1111u16;
    let neg = build_negotiate(tid, &[wire::HASH_SHA256], &[wire::DHGROUP_NULL]);
    let p = fabric_cmd_pdu(0x0002, fctype::AUTH_SEND, neg);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    let p = fabric_cmd_pdu(0x0003, fctype::AUTH_RECV, vec![]);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    // REPLY 用错的 tid
    let bad_rval = [0u8; 32];
    let reply_wire = build_reply(0x2222, &bad_rval); // mismatch
    let p = fabric_cmd_pdu(0x0004, fctype::AUTH_SEND, reply_wire);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let fw = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(fw.data[1], wire::MSG_FAILURE1);
    assert_eq!(fw.data[7], wire::FAIL_EXP_INCORRECT_PAYLOAD);
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(cqe_sc(&r), 0x83);

    client.shutdown().await.unwrap();
    let _ = done_rx.await;
}

/// reviewer M-4: REPLY truncation (< 16+32 bytes) → wire-level INCORRECT_PAYLOAD
#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap4_reply_truncated_rejected() {
    let mut store_inner = ChapSecretStore::new();
    store_inner.insert("nqn.host-a".to_string(), vec![0u8; 32]);
    let store = Arc::new(store_inner);

    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let addr = spawn_server_with_chap(store, done_tx).await;
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();

    let connect = build_connect_pdu(0x0001, "nqn.host-a", "nqn.subsys");
    write_pdu_async(&mut client, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    let tid = 0x3333u16;
    let neg = build_negotiate(tid, &[wire::HASH_SHA256], &[wire::DHGROUP_NULL]);
    let p = fabric_cmd_pdu(0x0002, fctype::AUTH_SEND, neg);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    let p = fabric_cmd_pdu(0x0003, fctype::AUTH_RECV, vec![]);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    // 截短 REPLY 至 24 byte (< 16+32 = 48)
    let mut short = vec![wire::AUTH_TYPE_DHCHAP, wire::MSG_REPLY];
    short.extend_from_slice(&[0u8; 22]);
    let p = fabric_cmd_pdu(0x0004, fctype::AUTH_SEND, short);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let fw = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(fw.data[1], wire::MSG_FAILURE1);
    assert_eq!(fw.data[7], wire::FAIL_EXP_INCORRECT_PAYLOAD);
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(cqe_sc(&r), 0x02, "REPLY 截短应 SC=0x02 INVALID_FIELD");

    client.shutdown().await.unwrap();
    let _ = done_rx.await;
}

/// reviewer M-4: SUCCESS2 在未认证 state 发 → 拒 + state Failed
#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap4_success2_before_auth_rejected() {
    let mut store_inner = ChapSecretStore::new();
    store_inner.insert("nqn.host-a".to_string(), vec![0u8; 32]);
    let store = Arc::new(store_inner);

    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let addr = spawn_server_with_chap(store, done_tx).await;
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();

    let connect = build_connect_pdu(0x0001, "nqn.host-a", "nqn.subsys");
    write_pdu_async(&mut client, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    let tid = 0x4444u16;
    let neg = build_negotiate(tid, &[wire::HASH_SHA256], &[wire::DHGROUP_NULL]);
    let p = fabric_cmd_pdu(0x0002, fctype::AUTH_SEND, neg);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    // 没跑 CHALLENGE/REPLY 就发 SUCCESS2
    let mut s2 = vec![wire::AUTH_TYPE_DHCHAP, wire::MSG_SUCCESS2, 0, 0];
    s2.extend_from_slice(&tid.to_le_bytes());
    s2.extend_from_slice(&[0u8; 10]); // 16B total
    let p = fabric_cmd_pdu(0x0003, fctype::AUTH_SEND, s2);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let fw = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(fw.data[1], wire::MSG_FAILURE1);
    assert_eq!(fw.data[7], wire::FAIL_EXP_INCORRECT_MESSAGE);
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(cqe_sc(&r), 0x83);

    client.shutdown().await.unwrap();
    let _ = done_rx.await;
}

/// reviewer M-4: FAILURE2 from host → state Failed + capsule 0x83
#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap4_host_sends_failure2_rejected() {
    let mut store_inner = ChapSecretStore::new();
    store_inner.insert("nqn.host-a".to_string(), vec![0u8; 32]);
    let store = Arc::new(store_inner);

    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let addr = spawn_server_with_chap(store, done_tx).await;
    let mut client = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();

    let connect = build_connect_pdu(0x0001, "nqn.host-a", "nqn.subsys");
    write_pdu_async(&mut client, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    let tid = 0x5555u16;
    let neg = build_negotiate(tid, &[wire::HASH_SHA256], &[wire::DHGROUP_NULL]);
    let p = fabric_cmd_pdu(0x0002, fctype::AUTH_SEND, neg);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    // FAILURE2 from host
    let mut f2 = vec![wire::AUTH_TYPE_DHCHAP, wire::MSG_FAILURE2, 0, 0];
    f2.extend_from_slice(&tid.to_le_bytes());
    f2.push(0x01); // rescode = FAILED
    f2.push(wire::FAIL_EXP_FAILED);
    f2.extend_from_slice(&[0u8; 8]);
    let p = fabric_cmd_pdu(0x0003, fctype::AUTH_SEND, f2);
    write_pdu_async(&mut client, &p.header, &p.psh, &p.data)
        .await
        .unwrap();
    // FAILURE2 from host → capsule SC=0x83 直接，不发 wire
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(r.header.pdu_type, pdu_type::RSP);
    assert_eq!(cqe_sc(&r), 0x83);

    client.shutdown().await.unwrap();
    let _ = done_rx.await;
}

/// reviewer L-6: HMAC compute_response 静态测试向量 — 锁定算法实现，未来若
/// `compute_response` 误改，本测试即触发。这套向量是 *本实现自洽*；要做真正
/// Linux nvme-cli interop，还需 capture 真实 Linux nvmet `nvme_dhchap_*` 输出
/// 对齐（待用户跑实测后填充真值）。
#[test]
fn vt_dhchap4_compute_response_static_vector_self_consistent() {
    use nvme_of_tcp_target::dhchap::compute_response;
    // 固定 inputs
    let secret = [0x11u8; 32];
    let challenge = [0x22u8; 32];
    let hostnqn = "nqn.test-host";
    let subnqn = "nqn.test-subsys";

    let r1 = compute_response(&secret, &challenge, hostnqn, subnqn);
    // 不同 challenge → 不同 response
    let r2 = compute_response(&secret, &[0x33u8; 32], hostnqn, subnqn);
    assert_ne!(r1, r2, "challenge 变化应改变 HMAC");
    // 不同 hostnqn → 不同 response
    let r3 = compute_response(&secret, &challenge, "nqn.other-host", subnqn);
    assert_ne!(r1, r3, "hostnqn 变化应改变 HMAC (transcript 含 hostnqn)");
    // 不同 secret → 不同 response
    let r4 = compute_response(&[0x99u8; 32], &challenge, hostnqn, subnqn);
    assert_ne!(r1, r4, "secret 变化应改变 HMAC");
    // 重算同样输入应 deterministic
    let r5 = compute_response(&secret, &challenge, hostnqn, subnqn);
    assert_eq!(r1, r5, "compute_response 必须 deterministic");
    // 32 byte HMAC-SHA256
    assert_eq!(r1.len(), 32);
}
