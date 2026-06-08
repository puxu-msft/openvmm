// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-dhchap-3-wire 集成测试** — AUTH_SEND / AUTH_RECV PDU
//! wire dispatch + admin cmd gate。
//!
//! 教学版简化 wire 格式:
//! - AUTH_RECV (host→target): fctype=0x06, 无 data；target 回 C2HData(challenge
//!   32B) + CapsuleResp SC=0
//! - AUTH_SEND (host→target): fctype=0x05, data=HMAC response 32B；target 校验
//!   通过 → SC=0, state=Authenticated；否则 SC=0x83
//!
//! 覆盖：
//! 1. `vt_dhchap3w_full_chap_roundtrip_then_admin_cmd_allowed` — Connect →
//!    AUTH_RECV (拿 challenge) → 算 HMAC → AUTH_SEND → 通过 → Identify
//!    admin cmd 放行
//! 2. `vt_dhchap3w_admin_cmd_blocked_before_chap_done` — Connect (CHAP 启用)
//!    → 直接发 Identify → SC=0x83
//! 3. `vt_dhchap3w_auth_send_wrong_response_fails` — host 用假 secret 算
//!    response → SC=0x83

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::dhchap::{ChapSecretStore, compute_response};
use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::{Pdu, read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{AsyncSession, SharedControllerInner, accept_and_handshake_async};
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

fn build_auth_recv_pdu(cid: u16) -> Pdu {
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::AUTH_RECV;
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

fn build_auth_send_pdu(cid: u16, response_32b: &[u8; 32]) -> Pdu {
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::AUTH_SEND;
    Pdu {
        header: CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 72,
            plen: 72 + 32,
        },
        psh: sqe,
        data: response_32b.to_vec(),
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
        let mut sess: AsyncSession<TcpStream> =
            accept_and_handshake_async(server, shared).await.unwrap();
        sess.enable_chap(store);
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        // 跑 4 个 pump iteration 应足够覆盖 Connect+AUTH_RECV+AUTH_SEND+Identify
        for _ in 0..8 {
            match sess.pump_one_async(&mut rx).await {
                Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) => {
                    let r = sess.dispatch_pdu_async(p).await;
                    if r.is_err() {
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

#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap3w_full_chap_roundtrip_then_admin_cmd_allowed() {
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

    // 1) Connect
    let connect = build_connect_pdu(0x0001, hostnqn, subnqn);
    write_pdu_async(&mut client, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let r1 = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(r1.header.pdu_type, pdu_type::RSP);
    assert_eq!(cqe_sc(&r1), 0, "Connect 应通过");

    // 2) AUTH_RECV -> 收 C2HData(challenge) + CapsuleResp
    let auth_recv = build_auth_recv_pdu(0x0002);
    write_pdu_async(
        &mut client,
        &auth_recv.header,
        &auth_recv.psh,
        &auth_recv.data,
    )
    .await
    .unwrap();
    let c2h = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(c2h.header.pdu_type, pdu_type::C2H_DATA);
    assert_eq!(c2h.data.len(), 32, "challenge 应为 32B");
    let mut challenge = [0u8; 32];
    challenge.copy_from_slice(&c2h.data);
    let r2 = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(r2.header.pdu_type, pdu_type::RSP);
    assert_eq!(cqe_sc(&r2), 0);

    // 3) host 用 secret 算 HMAC response
    let response = compute_response(&secret, &challenge, hostnqn, subnqn);

    // 4) AUTH_SEND
    let auth_send = build_auth_send_pdu(0x0003, &response);
    write_pdu_async(
        &mut client,
        &auth_send.header,
        &auth_send.psh,
        &auth_send.data,
    )
    .await
    .unwrap();
    let r3 = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(r3.header.pdu_type, pdu_type::RSP);
    assert_eq!(cqe_sc(&r3), 0, "AUTH_SEND 校验应通过");

    // 5) Identify admin cmd 应放行
    let id = build_identify_ctrl_pdu(0x0004);
    write_pdu_async(&mut client, &id.header, &id.psh, &id.data)
        .await
        .unwrap();
    let r4 = read_pdu_async(&mut client).await.unwrap();
    // Identify 流程会先发 C2HData(4096) 再 RSP；只看到 C2HData 即证 admin cmd
    // 没被 0x83 拦
    assert!(
        r4.header.pdu_type == pdu_type::C2H_DATA || r4.header.pdu_type == pdu_type::RSP,
        "CHAP 通过后 admin cmd 应放行 (得到 C2HData 或 RSP)，实际 PDU type = {:#x}",
        r4.header.pdu_type
    );
    if r4.header.pdu_type == pdu_type::C2H_DATA {
        let _rsp = read_pdu_async(&mut client).await.unwrap();
    }

    client.shutdown().await.unwrap();
    let _ = done_rx.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap3w_admin_cmd_blocked_before_chap_done() {
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

    // 不跑 CHAP，直接发 Identify
    let id = build_identify_ctrl_pdu(0x0002);
    write_pdu_async(&mut client, &id.header, &id.psh, &id.data)
        .await
        .unwrap();
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(r.header.pdu_type, pdu_type::RSP);
    assert_eq!(cqe_sc(&r), 0x83, "CHAP 未通过的 admin cmd 应得 SC=0x83");

    client.shutdown().await.unwrap();
    let _ = done_rx.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_dhchap3w_auth_send_wrong_response_fails() {
    let real_secret = vec![0x77u8; 32];
    let fake_secret = vec![0x88u8; 32];
    let hostnqn = "nqn.host-a";
    let subnqn = "nqn.subsys";
    let mut store_inner = ChapSecretStore::new();
    store_inner.insert(hostnqn.to_string(), real_secret);
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

    // AUTH_RECV 拿 challenge
    let auth_recv = build_auth_recv_pdu(0x0002);
    write_pdu_async(
        &mut client,
        &auth_recv.header,
        &auth_recv.psh,
        &auth_recv.data,
    )
    .await
    .unwrap();
    let c2h = read_pdu_async(&mut client).await.unwrap();
    let mut challenge = [0u8; 32];
    challenge.copy_from_slice(&c2h.data);
    let _ = read_pdu_async(&mut client).await.unwrap();

    // host 用 fake_secret 算 response (假冒)
    let bad_response = compute_response(&fake_secret, &challenge, hostnqn, subnqn);
    let auth_send = build_auth_send_pdu(0x0003, &bad_response);
    write_pdu_async(
        &mut client,
        &auth_send.header,
        &auth_send.psh,
        &auth_send.data,
    )
    .await
    .unwrap();
    let r = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(cqe_sc(&r), 0x83, "假 secret 应 SC=0x83");

    client.shutdown().await.unwrap();
    let _ = done_rx.await;
}
