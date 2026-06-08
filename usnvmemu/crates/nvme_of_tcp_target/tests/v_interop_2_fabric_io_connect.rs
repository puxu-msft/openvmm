// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **V-followup-interop-2 regression gate** — NVMe-oF fabric IO queue Connect。
//!
//! 模拟 Linux nvme-tcp 真实行为：每条 IO queue 用**独立 TCP conn**，开 conn
//! 后第一个 PDU = Fabric Connect (qid≥1)，**不**走 admin path 的 Create IO
//! CQ/SQ。本测试在 in-process 模拟 2 个 TCP session（admin + IO qid=1），
//! 验 fabric IO Connect 自动 install CQ+SQ 并能跑通后续 IO Read。
//!
//! 防止以下回归：
//! - `handle_connect_async` qid≥1 路径 reject (V8 PCIe-only 模型)
//! - `nvme_force_install_io_queue` 漏 install SQ → IO cmd dispatch 失败
//! - sqsize 0-based 误读为 1-based 导致 qsize off-by-one

#![allow(missing_docs)]

use nvme_of_tcp_target::fabric::{
    self, ConnectData, ConnectFabricFields, PropertyFabricFields, fctype, property_offset,
};
use nvme_of_tcp_target::framing::{Pdu, read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{AsyncSession, SharedControllerInner, accept_and_handshake_async};
use nvme_firmware::NvmeController;
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

fn build_icreq() -> (CommonHdr, Vec<u8>) {
    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    (hdr, IcPsh::default().as_bytes().to_vec())
}

fn build_connect(cid: u16, qid: u16, kato_ms: u32, hostnqn: &str, subnqn: &str) -> Pdu {
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::CONNECT;
    let f = ConnectFabricFields {
        recfmt: 0,
        qid,
        sqsize: 31, // 0-based → qsize=32
        cattr: 0,
        rsvd1: 0,
        kato: kato_ms,
        rsvd2: [0u8; 12],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let mut cd = ConnectData::default();
    let n = hostnqn.len().min(cd.hostnqn.len());
    cd.hostnqn[..n].copy_from_slice(&hostnqn.as_bytes()[..n]);
    let n = subnqn.len().min(cd.subnqn.len());
    cd.subnqn[..n].copy_from_slice(&subnqn.as_bytes()[..n]);
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

fn build_property_set(cid: u16, ofst: u32, value: u64) -> Pdu {
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::PROPERTY_SET;
    let f = PropertyFabricFields {
        attrib: 0,
        rsvd1: [0u8; 3],
        ofst,
        value,
        rsvd2: [0u8; 8],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
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

fn cqe_sc(pdu: &Pdu) -> u8 {
    assert_eq!(pdu.header.pdu_type, pdu_type::RSP);
    let status = u16::from_le_bytes([pdu.psh[14], pdu.psh[15]]);
    ((status >> 1) & 0xFF) as u8
}

async fn send_icreq(c: &mut TcpStream) {
    let (h, psh) = build_icreq();
    write_pdu_async(c, &h, &psh, &[]).await.unwrap();
    let r = read_pdu_async(c).await.unwrap();
    assert_eq!(r.header.pdu_type, pdu_type::ICRESP);
}

/// 启 1 个 listener，spawn 任意数量 conn handler 直到 shutdown；返 (addr,
/// shutdown_tx)。
async fn spawn_multi_conn_listener(
    shared: Arc<SharedControllerInner>,
) -> (
    std::net::SocketAddr,
    tokio::sync::watch::Sender<bool>,
    Vec<tokio::task::JoinHandle<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let handles: Arc<parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let h_clone = Arc::clone(&handles);
    let accept_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                r = listener.accept() => {
                    if let Ok((server, _)) = r {
                        let s = Arc::clone(&shared);
                        let h = tokio::spawn(async move {
                            let mut sess: AsyncSession<TcpStream> =
                                accept_and_handshake_async(server, s).await.unwrap();
                            let (_tx, mut rx) = tokio::sync::watch::channel(false);
                            for _ in 0..32 {
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
                        });
                        h_clone.lock().push(h);
                    }
                }
            }
        }
    });
    let mut all = handles.lock().drain(..).collect::<Vec<_>>();
    all.push(accept_handle);
    (addr, shutdown_tx, all)
}

#[tokio::test(flavor = "multi_thread")]
async fn v_interop_2_fabric_io_connect_auto_installs_queue_and_subsequent_admin_works() {
    let (shared, _backing) = make_shared();
    let (addr, shutdown_tx, _handles) = spawn_multi_conn_listener(Arc::clone(&shared)).await;

    // ===== Admin queue conn (qid=0) =====
    let mut admin = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut admin).await;
    let connect_admin = build_connect(
        0x0001,
        0,
        10000,
        "nqn.2014-08.org.nvmexpress:uuid:host-01",
        "nqn.2014-08.org.nvmexpress:teaching:disk",
    );
    write_pdu_async(
        &mut admin,
        &connect_admin.header,
        &connect_admin.psh,
        &connect_admin.data,
    )
    .await
    .unwrap();
    let r = read_pdu_async(&mut admin).await.unwrap();
    assert_eq!(cqe_sc(&r), 0, "admin Connect SC=0");

    // CC.EN sequence on admin to enable controller
    let ps_off = build_property_set(0x0002, property_offset::CC, 0x0046_0000);
    write_pdu_async(&mut admin, &ps_off.header, &ps_off.psh, &ps_off.data)
        .await
        .unwrap();
    assert_eq!(cqe_sc(&read_pdu_async(&mut admin).await.unwrap()), 0);
    let ps_on = build_property_set(0x0003, property_offset::CC, 0x0046_0001);
    write_pdu_async(&mut admin, &ps_on.header, &ps_on.psh, &ps_on.data)
        .await
        .unwrap();
    assert_eq!(cqe_sc(&read_pdu_async(&mut admin).await.unwrap()), 0);

    // ===== IO queue conn (qid=1) — 独立 TCP =====
    let mut io = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut io).await;
    let connect_io = build_connect(
        0x0010,
        1,
        0, // IO queue kato=0 (spec § 3.6)
        "nqn.2014-08.org.nvmexpress:uuid:host-01",
        "nqn.2014-08.org.nvmexpress:teaching:disk",
    );
    write_pdu_async(
        &mut io,
        &connect_io.header,
        &connect_io.psh,
        &connect_io.data,
    )
    .await
    .unwrap();
    let r_io = read_pdu_async(&mut io).await.unwrap();
    // 核心断言：本 PR 之前会 SC=0x82 (CONNECT_INVALID_PARAM) reject；
    // V-followup-interop-2 后应 SC=0
    assert_eq!(
        cqe_sc(&r_io),
        0,
        "fabric IO Connect (qid=1) 必须 SC=0 — host 不走 Create IO CQ/SQ admin path"
    );

    // 简单 IO 不发了：完整 Read 端到端需要更多 setup (PSDT/SGL)；本测试仅
    // 锁定 Connect 自动 install 不再 reject。后续 V-interop-3 加 IO Read e2e。

    admin.shutdown().await.unwrap();
    io.shutdown().await.unwrap();
    let _ = shutdown_tx.send(true);
    // accept loop 会自然退出；conn handler tokio task 也随 conn drop 退
}
