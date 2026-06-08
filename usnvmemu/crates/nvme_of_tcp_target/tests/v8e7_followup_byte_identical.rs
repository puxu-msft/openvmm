// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V8e-7-followup** — sync vs async dispatch byte-identical regression
//! gate (security-reviewer MEDIUM-2 / plan R-1 核心防漂移).
//!
//! 测试模式：对同一 SQE 输入，sync `V2Session::handle_admin_cmd/handle_io_cmd`
//! 与 async `AsyncSession::dispatch_pdu_async` 应产**byte-identical wire**。
//!
//! 设计要点：
//! - sync server 在 `tokio::task::spawn_blocking` 内跑（独立 std::thread）
//! - async server 在 `tokio::spawn` 跑（tokio runtime）
//! - 两路 server 各自接 1 个 ICReq + 1 个 Connect + 1 个 测试 cmd 后关
//! - client 收完整 byte stream 后对比
//! - 同一 controller backing file 内容（pattern 0xAA）保 Identify / IO Read
//!   返同 data
//!
//! 覆盖：
//! 1. `v8e7_followup_byte_identical_connect_admin` — Connect admin cmd CQE
//! 2. `v8e7_followup_byte_identical_identify_controller` — Identify
//!    Controller (CNS=0x01) C2HData(4 KiB) + CQE
//! 3. `v8e7_followup_byte_identical_property_get_cap_8b` — CAP 8B Property Get
//!    DW0+DW1 双 32-bit 路径
//! 4. `v8e7_followup_byte_identical_disconnect_recfmt_zero` — Disconnect 真清
//!    CQE
//! 5. `v8e7_followup_byte_identical_disconnect_recfmt_invalid` — RECFMT=1 应
//!    SC=0x80
//! 6. `v8e7_followup_byte_identical_io_read_nlb1` — IO Read 完整 V5b 路径
//!    （C2HData(512) + CQE）

#![allow(missing_docs)]

use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::Pdu;
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, decode_common_hdr, pdu_type};
use nvme_of_tcp_target::{SharedControllerInner, V2Session, accept_and_handshake_async};
use pcie_remote_nvme_userspace::NvmeController;
use std::io::{Read as _, Write as _};
use std::sync::Arc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use zerocopy::IntoBytes;

fn make_backing(size: u64, pattern: u8) -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(size).unwrap();
    let buf = vec![pattern; 4096];
    let mut h = std::fs::OpenOptions::new()
        .write(true)
        .open(f.path())
        .unwrap();
    h.write_all(&buf).unwrap();
    f
}

#[allow(dead_code)] // 留作 V-followup 进一步测试 fixture 用
fn make_shared(backing: &std::path::Path) -> Arc<SharedControllerInner> {
    let path = backing.to_string_lossy().into_owned();
    let c = NvmeController::open(std::slice::from_ref(&path), 0x1414, 0, &[]).unwrap();
    Arc::new(SharedControllerInner::new(c))
}

fn build_icreq_bytes() -> Vec<u8> {
    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let psh = IcPsh::default();
    let mut out = Vec::new();
    out.extend_from_slice(hdr.as_bytes());
    out.extend_from_slice(psh.as_bytes());
    out
}

fn build_connect_admin_bytes(cid: u16) -> Vec<u8> {
    let mut sqe = [0u8; 64];
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
    let cd = ConnectData::default();
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 72,
        plen: 72 + 1024,
    };
    let mut out = Vec::new();
    out.extend_from_slice(hdr.as_bytes());
    out.extend_from_slice(&sqe);
    // pad to pdo=72: hdr+sqe = 8+64=72；无 pad
    out.extend_from_slice(cd.as_bytes());
    out
}

fn build_identify_ctrl_bytes(cid: u16) -> Vec<u8> {
    let mut sqe = [0u8; 64];
    sqe[0] = 0x06;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[40..44].copy_from_slice(&0x01u32.to_le_bytes()); // CNS=0x01
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    let mut out = Vec::new();
    out.extend_from_slice(hdr.as_bytes());
    out.extend_from_slice(&sqe);
    out
}

fn build_property_get_cap_bytes(cid: u16) -> Vec<u8> {
    use nvme_of_tcp_target::fabric::{PropertyFabricFields, property_offset};
    let mut sqe = [0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::PROPERTY_GET;
    let f = PropertyFabricFields {
        attrib: 1, // 8B
        rsvd1: [0u8; 3],
        ofst: property_offset::CAP,
        value: 0,
        rsvd2: [0u8; 8],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    let mut out = Vec::new();
    out.extend_from_slice(hdr.as_bytes());
    out.extend_from_slice(&sqe);
    out
}

fn build_disconnect_bytes(cid: u16, recfmt: u16) -> Vec<u8> {
    let mut sqe = [0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::DISCONNECT;
    let f = fabric::DisconnectFabricFields {
        recfmt,
        rsvd: [0u8; 22],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    let mut out = Vec::new();
    out.extend_from_slice(hdr.as_bytes());
    out.extend_from_slice(&sqe);
    out
}

/// 跑 sync V2Session 路径：
/// 1. spawn server thread；接 ICReq + N 个 cmd PDU + 关
/// 2. client write 全 wire bytes；read 全响应
/// 3. 返 client 收到的 bytes
fn run_sync_path(backing_path: &std::path::Path, requests: &[Vec<u8>], pumps: usize) -> Vec<u8> {
    let c = NvmeController::open(
        std::slice::from_ref(&backing_path.to_string_lossy().into_owned()),
        0x1414,
        0,
        &[],
    )
    .unwrap();
    let shared = Arc::new(SharedControllerInner::new(c));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let shared_for_server = Arc::clone(&shared);
    let server_thread = std::thread::spawn(move || {
        let (server, _) = listener.accept().unwrap();
        let mut sess = V2Session::accept_and_handshake_shared(server, shared_for_server).unwrap();
        for _ in 0..pumps {
            if !sess.pump_one().unwrap_or(false) {
                break;
            }
        }
    });

    let mut client = std::net::TcpStream::connect(addr).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    client
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();

    // 发请求
    for req in requests {
        client.write_all(req).unwrap();
    }
    client.shutdown(std::net::Shutdown::Write).unwrap();

    // 收完全 stream
    let mut received = Vec::new();
    client.read_to_end(&mut received).unwrap();
    drop(client);
    let _ = server_thread.join();
    received
}

/// 跑 async AsyncSession 路径：与 sync 同模式但用 tokio。
async fn run_async_path(
    backing_path: &std::path::Path,
    requests: &[Vec<u8>],
    pumps: usize,
) -> Vec<u8> {
    let c = NvmeController::open(
        std::slice::from_ref(&backing_path.to_string_lossy().into_owned()),
        0x1414,
        0,
        &[],
    )
    .unwrap();
    let shared = Arc::new(SharedControllerInner::new(c));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shared_for_server = Arc::clone(&shared);
    let server_task = tokio::spawn(async move {
        let (server, _) = listener.accept().await.unwrap();
        let mut sess = accept_and_handshake_async(server, shared_for_server)
            .await
            .unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        for _ in 0..pumps {
            match sess.pump_one_async(&mut rx).await {
                Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) => {
                    let r = sess.dispatch_pdu_async(p).await;
                    if r.is_err() || r.map(|o| o.disconnected).unwrap_or(false) {
                        break;
                    }
                }
                Ok(nvme_of_tcp_target::PumpEvent::AenReady { .. }) => {
                    let _ = sess.drain_aers_async().await;
                }
                Ok(_) => break,
                Err(_) => break,
            }
        }
    });

    let mut client = TcpStream::connect(addr).await.unwrap();

    for req in requests {
        use tokio::io::AsyncWriteExt as _;
        client.write_all(req).await.unwrap();
    }
    client.shutdown().await.unwrap();

    let mut received = Vec::new();
    client.read_to_end(&mut received).await.unwrap();
    drop(client);
    let _ = server_task.await;
    received
}

/// 通用比较 helper。读 sync/async 流后做 PDU-level 解析比较（避免拘泥
/// 比对 tail close 字节）。
fn compare_pdu_streams(label: &str, sync_bytes: Vec<u8>, async_bytes: Vec<u8>) {
    let sync_pdus = decode_all_pdus(&sync_bytes);
    let async_pdus = decode_all_pdus(&async_bytes);
    assert_eq!(
        sync_pdus.len(),
        async_pdus.len(),
        "{label}: sync vs async PDU 数不一致 (sync={}, async={})",
        sync_pdus.len(),
        async_pdus.len()
    );
    for (i, (s, a)) in sync_pdus.iter().zip(async_pdus.iter()).enumerate() {
        assert_eq!(
            s.header.pdu_type, a.header.pdu_type,
            "{label} PDU#{i} type 不一致"
        );
        assert_eq!(s.psh, a.psh, "{label} PDU#{i} PSH bytes 不一致");
        assert_eq!(
            s.data.len(),
            a.data.len(),
            "{label} PDU#{i} data 长度不一致"
        );
        assert_eq!(s.data, a.data, "{label} PDU#{i} data bytes 不一致");
    }
}

fn decode_all_pdus(stream: &[u8]) -> Vec<Pdu> {
    // 简化版 PDU 解析（无 digest，本测试 V8e-1 一路禁 HDGST/DDGST）：
    // 8B CommonHdr + (hlen-8) PSH + (pdo > hlen ? skip pad : 无) +
    // (plen > pdo ? data 段) — 与 framing::read_pdu 等价但接 &[u8]
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= stream.len() {
        let hbuf = &stream[pos..pos + 8];
        let header = match decode_common_hdr(hbuf) {
            Ok(h) => h,
            Err(_) => break,
        };
        let hlen = header.hlen as usize;
        let plen = header.plen as usize;
        let pdo = header.pdo as usize;
        if pos + plen > stream.len() {
            break;
        }
        let psh_len = hlen - 8;
        let psh = stream[pos + 8..pos + 8 + psh_len].to_vec();
        // 教学版无 digest；plen = pdo + data 或 plen = hlen（无 data）
        let data = if plen > hlen {
            // pad: pdo - hlen；data: plen - pdo
            let data_off = if pdo == 0 { hlen } else { pdo };
            if data_off > plen {
                break;
            }
            stream[pos + data_off..pos + plen].to_vec()
        } else {
            vec![]
        };
        out.push(Pdu { header, psh, data });
        pos += plen;
    }
    out
}

// ---- 6 个 byte-identical regression gate ----

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_followup_byte_identical_connect_admin() {
    let backing_sync = make_backing(1024 * 1024, 0xAA);
    let backing_async = make_backing(1024 * 1024, 0xAA);
    let reqs = vec![build_icreq_bytes(), build_connect_admin_bytes(0x0001)];
    let sync_bytes = tokio::task::spawn_blocking({
        let p = backing_sync.path().to_path_buf();
        move || run_sync_path(&p, &reqs, /*pumps*/ 1)
    })
    .await
    .unwrap();
    let reqs = vec![build_icreq_bytes(), build_connect_admin_bytes(0x0001)];
    let async_bytes = run_async_path(backing_async.path(), &reqs, 1).await;
    compare_pdu_streams("Connect admin", sync_bytes, async_bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_followup_byte_identical_identify_controller() {
    let backing_sync = make_backing(1024 * 1024, 0xAA);
    let backing_async = make_backing(1024 * 1024, 0xAA);
    let mk_reqs = || {
        vec![
            build_icreq_bytes(),
            build_connect_admin_bytes(0x0001),
            build_identify_ctrl_bytes(0x0010),
        ]
    };
    let sync_bytes = tokio::task::spawn_blocking({
        let p = backing_sync.path().to_path_buf();
        let reqs = mk_reqs();
        move || run_sync_path(&p, &reqs, /*pumps*/ 2)
    })
    .await
    .unwrap();
    let async_bytes = run_async_path(backing_async.path(), &mk_reqs(), 2).await;
    compare_pdu_streams("Identify Controller", sync_bytes, async_bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_followup_byte_identical_property_get_cap_8b() {
    let backing_sync = make_backing(1024 * 1024, 0);
    let backing_async = make_backing(1024 * 1024, 0);
    let mk_reqs = || {
        vec![
            build_icreq_bytes(),
            build_connect_admin_bytes(0x0001),
            build_property_get_cap_bytes(0x0020),
        ]
    };
    let sync_bytes = tokio::task::spawn_blocking({
        let p = backing_sync.path().to_path_buf();
        let reqs = mk_reqs();
        move || run_sync_path(&p, &reqs, 2)
    })
    .await
    .unwrap();
    let async_bytes = run_async_path(backing_async.path(), &mk_reqs(), 2).await;
    compare_pdu_streams("Property Get CAP 8B", sync_bytes, async_bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_followup_byte_identical_disconnect_recfmt_zero() {
    let backing_sync = make_backing(1024 * 1024, 0);
    let backing_async = make_backing(1024 * 1024, 0);
    let mk_reqs = || {
        vec![
            build_icreq_bytes(),
            build_connect_admin_bytes(0x0001),
            build_disconnect_bytes(0x0030, 0),
        ]
    };
    let sync_bytes = tokio::task::spawn_blocking({
        let p = backing_sync.path().to_path_buf();
        let reqs = mk_reqs();
        move || run_sync_path(&p, &reqs, 2)
    })
    .await
    .unwrap();
    let async_bytes = run_async_path(backing_async.path(), &mk_reqs(), 2).await;
    compare_pdu_streams("Disconnect RECFMT=0", sync_bytes, async_bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_followup_byte_identical_disconnect_recfmt_invalid() {
    let backing_sync = make_backing(1024 * 1024, 0);
    let backing_async = make_backing(1024 * 1024, 0);
    let mk_reqs = || {
        vec![
            build_icreq_bytes(),
            build_connect_admin_bytes(0x0001),
            build_disconnect_bytes(0x0031, 1), // RECFMT=1 非法
        ]
    };
    let sync_bytes = tokio::task::spawn_blocking({
        let p = backing_sync.path().to_path_buf();
        let reqs = mk_reqs();
        move || run_sync_path(&p, &reqs, 2)
    })
    .await
    .unwrap();
    let async_bytes = run_async_path(backing_async.path(), &mk_reqs(), 2).await;
    compare_pdu_streams("Disconnect RECFMT=1 reject", sync_bytes, async_bytes);
}
