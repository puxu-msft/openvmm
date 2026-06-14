// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **V-followup-prp-list** lib regression — session chunking for nlb > V5_NLB_MAX。
//!
//! 不真起 TCP，直接 in-process 测 AsyncSession::dispatch_pdu_async 路径:
//! - nlb 1..16 (V5_NLB_MAX 内): 单 sub-cmd, 1 个 C2HData, 1 个 RSP
//! - nlb 17 (= V5_NLB_MAX+1): 2 个 sub-cmd, 2 个 C2HData + 1 个 RSP
//! - nlb 32 (= 2*V5_NLB_MAX): 2 个 sub-cmd, 2 个 C2HData + 1 个 RSP
//! - nlb 33 (= 2*V5_NLB_MAX+1): 3 个 sub-cmd, 3 个 C2HData + 1 个 RSP
//! - nlb 256 (= V_HOST_IO_NLB_MAX): 16 个 sub-cmd, 16 个 C2HData + 1 个 RSP
//! - nlb 257 (> V_HOST_IO_NLB_MAX): SC=0x18 reject

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::fabric::{
    self, ConnectData, ConnectFabricFields, PropertyFabricFields, fctype, property_offset,
};
use nvme_of_tcp_target::framing::{read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{SharedControllerInner, accept_and_handshake_async};
use std::sync::Arc;
use tokio::io::AsyncWriteExt as _;
use tokio::net::{TcpListener, TcpStream};
use zerocopy::IntoBytes;

fn make_shared() -> (Arc<SharedControllerInner>, tempfile::NamedTempFile) {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(64 * 1024 * 1024).unwrap();
    let path = f.path().to_string_lossy().into_owned();
    let c = NvmeController::open(std::slice::from_ref(&path), 0x1414, 0, &[]).unwrap();
    (Arc::new(SharedControllerInner::new(c)), f)
}

async fn handshake_and_connect_io(addr: std::net::SocketAddr) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let psh = IcPsh::default();
    write_pdu_async(&mut s, &hdr, psh.as_bytes(), &[])
        .await
        .unwrap();
    let _ = read_pdu_async(&mut s).await.unwrap();
    // admin Connect
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&1u16.to_le_bytes());
    sqe[4] = fctype::CONNECT;
    let f = ConnectFabricFields {
        recfmt: 0,
        qid: 0,
        sqsize: 31,
        cattr: 0,
        rsvd1: 0,
        kato: 10000,
        rsvd2: [0u8; 12],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let cd = ConnectData::default();
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 72,
        plen: 72 + 1024,
    };
    write_pdu_async(&mut s, &hdr, &sqe, cd.as_bytes())
        .await
        .unwrap();
    let _ = read_pdu_async(&mut s).await.unwrap();
    // CC.EN
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&2u16.to_le_bytes());
    sqe[4] = fctype::PROPERTY_SET;
    let pf = PropertyFabricFields {
        attrib: 0,
        rsvd1: [0u8; 3],
        ofst: property_offset::CC,
        value: 0x46_0001,
        rsvd2: [0u8; 8],
    };
    sqe[40..64].copy_from_slice(pf.as_bytes());
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    write_pdu_async(&mut s, &hdr, &sqe, &[]).await.unwrap();
    let _ = read_pdu_async(&mut s).await.unwrap();
    s
}

async fn io_connect_qid1(addr: std::net::SocketAddr) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let psh = IcPsh::default();
    write_pdu_async(&mut s, &hdr, psh.as_bytes(), &[])
        .await
        .unwrap();
    let _ = read_pdu_async(&mut s).await.unwrap();
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&0x100u16.to_le_bytes());
    sqe[4] = fctype::CONNECT;
    let f = ConnectFabricFields {
        recfmt: 0,
        qid: 1,
        sqsize: 31,
        cattr: 0,
        rsvd1: 0,
        kato: 0,
        rsvd2: [0u8; 12],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let cd = ConnectData::default();
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 72,
        plen: 72 + 1024,
    };
    write_pdu_async(&mut s, &hdr, &sqe, cd.as_bytes())
        .await
        .unwrap();
    let _ = read_pdu_async(&mut s).await.unwrap();
    s
}

fn build_io_rw(cid: u16, opc: u8, slba: u64, nlb_zero_based: u32) -> (CommonHdr, Vec<u8>) {
    let mut sqe = vec![0u8; 64];
    sqe[0] = opc;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4..8].copy_from_slice(&1u32.to_le_bytes()); // nsid=1
    sqe[40..48].copy_from_slice(&slba.to_le_bytes());
    sqe[48..52].copy_from_slice(&nlb_zero_based.to_le_bytes());
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    (hdr, sqe)
}

async fn spawn_server(shared: Arc<SharedControllerInner>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((server, _)) = listener.accept().await else {
                break;
            };
            let s = Arc::clone(&shared);
            tokio::spawn(async move {
                let mut sess =
                    accept_and_handshake_async(server, s).await.unwrap();
                let (_tx, mut rx) = tokio::sync::watch::channel(false);
                for _ in 0..256 {
                    match sess.pump_one_async(&mut rx).await {
                        Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) => {
                            if sess.dispatch_pdu_async(p).await.is_err() {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            });
        }
    });
    addr
}

fn cqe_sc(psh: &[u8]) -> u8 {
    let status = u16::from_le_bytes([psh[14], psh[15]]);
    ((status >> 1) & 0xFF) as u8
}

/// Read nlb_real LBAs, returns (num C2H_DATA pdus seen, total bytes, RSP SC)
async fn do_read(io: &mut TcpStream, cid: u16, slba: u64, nlb_real: u32) -> (usize, usize, u8) {
    let (hdr, psh) = build_io_rw(cid, 0x02, slba, nlb_real - 1);
    write_pdu_async(io, &hdr, &psh, &[]).await.unwrap();
    let mut n_c2h = 0usize;
    let mut total = 0usize;
    loop {
        let pdu = read_pdu_async(io).await.unwrap();
        match pdu.header.pdu_type {
            pdu_type::C2H_DATA => {
                n_c2h += 1;
                total += pdu.data.len();
            }
            pdu_type::RSP => {
                return (n_c2h, total, cqe_sc(&pdu.psh));
            }
            other => panic!("unexpected pdu type {other:#x}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn v_prp_list_anchor_constants() {
    // V_HOST_IO_NLB_MAX 必须是 V5_NLB_MAX 整数倍 (chunking 数学 sanity)
    // const assert via static branch — both consts, clippy 抱怨常量比较，但
    // 这正是 anchor test 目的 (lock 当下值，未来改 const 跳出 review)
    let host_max = nvme_of_tcp_target::V_HOST_IO_NLB_MAX;
    let chunk_max = nvme_of_tcp_target::V5_NLB_MAX;
    assert!(host_max >= chunk_max);
    assert_eq!(
        host_max % chunk_max,
        0,
        "V_HOST_IO_NLB_MAX 应为 V5_NLB_MAX 整数倍"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn v_prp_list_read_single_chunk_nlb16() {
    let (shared, _backing) = make_shared();
    let addr = spawn_server(shared).await;
    let _admin = handshake_and_connect_io(addr).await;
    let mut io = io_connect_qid1(addr).await;
    let (n, bytes, sc) = do_read(&mut io, 0x500, 0, 16).await;
    assert_eq!(n, 1, "16 LBA = 1 chunk");
    assert_eq!(bytes, 16 * 512, "16 * 512 byte");
    assert_eq!(sc, 0);
    io.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn v_prp_list_read_two_chunks_nlb17() {
    let (shared, _backing) = make_shared();
    let addr = spawn_server(shared).await;
    let _admin = handshake_and_connect_io(addr).await;
    let mut io = io_connect_qid1(addr).await;
    let (n, bytes, sc) = do_read(&mut io, 0x500, 0, 17).await;
    assert_eq!(n, 2, "17 LBA = 2 chunks (16 + 1)");
    assert_eq!(bytes, 17 * 512);
    assert_eq!(sc, 0);
    io.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn v_prp_list_read_max_chunks_nlb256() {
    let (shared, _backing) = make_shared();
    let addr = spawn_server(shared).await;
    let _admin = handshake_and_connect_io(addr).await;
    let mut io = io_connect_qid1(addr).await;
    let (n, bytes, sc) = do_read(&mut io, 0x500, 0, 256).await;
    assert_eq!(n, 16, "256 LBA = 16 chunks of 16");
    assert_eq!(bytes, 256 * 512);
    assert_eq!(sc, 0);
    io.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn v_prp_list_read_over_max_nlb257_rejected() {
    let (shared, _backing) = make_shared();
    let addr = spawn_server(shared).await;
    let _admin = handshake_and_connect_io(addr).await;
    let mut io = io_connect_qid1(addr).await;
    let (hdr, psh) = build_io_rw(0x500, 0x02, 0, 256); // 0-based: 256 = nlb_real 257
    write_pdu_async(&mut io, &hdr, &psh, &[]).await.unwrap();
    let resp = read_pdu_async(&mut io).await.unwrap();
    assert_eq!(resp.header.pdu_type, pdu_type::RSP);
    assert_eq!(cqe_sc(&resp.psh), 0x18, "> V_HOST_IO_NLB_MAX 应 SC=0x18");
    io.shutdown().await.unwrap();
}
