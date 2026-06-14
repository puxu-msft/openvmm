// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Windows transport-SGL Connect data（2026-06-14 真 WS2025 互通实测固化）** — regression gate。
//!
//! 真 Windows Server 2025 inbox NVMe-oF initiator（`stornvmeofi`）的 IO-queue Fabric Connect
//! 与 Linux 不同：
//! 1. Connect data（1024B）**不放 in-capsule**，而是用 **Transport SGL**（SGL Identifier
//!    byte `0x5A` = type 0x5 Transport Data Block / subtype 0xA），controller 须发 **R2T**、
//!    host 回 **H2CData** 推数据（[`fabric::decode_connect_data_source`]）。
//! 2. Windows 的 H2CData **PLEN 设为 HLEN（仅头）**，data 长度由 PSH DATAL 给（Linux/spec 把
//!    data 计进 PLEN）。target 据 DATAL 补读（`await_host_data_async` 的 quirk 处理）。
//!
//! 本测试用**精确的 Windows wire**（SGL 0x5A + H2CData PLEN=HLEN）驱真 `AsyncSession`，
//! 锁定两处修复；并验畸形 H2CData（DATAL 超 R2T 范围）被拒。修复前 IO Connect 在
//! `data.len()!=1024` 处被 SC=0x82 拒，Windows 永远建不起 IO 队列、不出盘。

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::{Pdu, read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{SharedControllerInner, accept_and_handshake_async};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use zerocopy::IntoBytes;

const SUBNQN: &str = "nqn.2014-08.org.nvmexpress:teaching:disk";
const HOSTNQN: &str = "nqn.2014-08.org.nvmexpress:uuid:win-host";

fn make_shared() -> (Arc<SharedControllerInner>, tempfile::NamedTempFile) {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(16 * 1024 * 1024).unwrap();
    let path = f.path().to_string_lossy().into_owned();
    let c = NvmeController::open(std::slice::from_ref(&path), 0x1414, 0, &[]).unwrap();
    (Arc::new(SharedControllerInner::new(c)), f)
}

async fn send_icreq(c: &mut TcpStream) {
    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    write_pdu_async(c, &hdr, IcPsh::default().as_bytes(), &[])
        .await
        .unwrap();
    assert_eq!(read_pdu_async(c).await.unwrap().header.pdu_type, pdu_type::ICRESP);
}

/// Connect SQE，用 **Transport SGL**（SGL1 identifier=0x5A，length=1024）—— Windows IO Connect
/// 的真实形态：data 不 in-capsule，经 R2T 取。
fn build_connect_sqe_transport_sgl(cid: u16, qid: u16) -> Vec<u8> {
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::CONNECT;
    // SGL1 @ sqe[24..40]: length @ [32..36], identifier @ [39].
    sqe[32..36].copy_from_slice(&(fabric::CONNECT_DATA_SIZE as u32).to_le_bytes());
    sqe[39] = 0x5A; // type 0x5 Transport Data Block / subtype 0xA
    let f = ConnectFabricFields {
        recfmt: 0,
        qid,
        sqsize: 31,
        cattr: 0,
        rsvd1: 0,
        kato: if qid == 0 { 10000 } else { 0 },
        rsvd2: [0u8; 12],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    sqe
}

fn connect_data_bytes(cntlid: u16) -> Vec<u8> {
    let mut cd = ConnectData {
        cntlid,
        ..ConnectData::default()
    };
    let n = HOSTNQN.len().min(cd.hostnqn.len());
    cd.hostnqn[..n].copy_from_slice(&HOSTNQN.as_bytes()[..n]);
    let n = SUBNQN.len().min(cd.subnqn.len());
    cd.subnqn[..n].copy_from_slice(&SUBNQN.as_bytes()[..n]);
    cd.as_bytes().to_vec()
}

/// 发一条 CapsuleCmd（仅 SQE，无 in-capsule data）—— transport-SGL Connect 形态（plen=72）。
async fn send_capsule_cmd_no_data(c: &mut TcpStream, sqe: &[u8]) {
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    write_pdu_async(c, &hdr, sqe, &[]).await.unwrap();
}

/// 解 R2T PSH → (cccid, ttag, offset, length)。
fn parse_r2t(p: &Pdu) -> (u16, u16, u32, u32) {
    assert_eq!(p.header.pdu_type, pdu_type::R2T, "expected R2T");
    let psh = &p.psh;
    (
        u16::from_le_bytes([psh[0], psh[1]]),
        u16::from_le_bytes([psh[2], psh[3]]),
        u32::from_le_bytes([psh[4], psh[5], psh[6], psh[7]]),
        u32::from_le_bytes([psh[8], psh[9], psh[10], psh[11]]),
    )
}

/// 发一条 **Windows-style H2CData**：CH PLEN = HLEN（仅头，24）—— Windows quirk；data 长度由
/// PSH DATAL 给。`windows_plen_quirk=false` 则发 spec-conformant（PLEN=hlen+data）。
async fn send_h2cdata(
    c: &mut TcpStream,
    cccid: u16,
    ttag: u16,
    datao: u32,
    data: &[u8],
    windows_plen_quirk: bool,
) {
    let mut psh = [0u8; 16];
    psh[0..2].copy_from_slice(&cccid.to_le_bytes());
    psh[2..4].copy_from_slice(&ttag.to_le_bytes());
    psh[4..8].copy_from_slice(&datao.to_le_bytes());
    psh[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes()); // DATAL
    let hlen = 24u8;
    let plen: u32 = if windows_plen_quirk {
        hlen as u32 // Windows: PLEN = HLEN（不计 data）
    } else {
        hlen as u32 + data.len() as u32 // spec-conformant
    };
    // 手搓 PDU（write_pdu_async 会按 plen 计算，这里要精确控制 plen，故直接写 socket）。
    use tokio::io::AsyncWriteExt as _;
    let mut buf = vec![pdu_type::H2C_DATA, 0x04, hlen, 24];
    buf.extend_from_slice(&plen.to_le_bytes());
    buf.extend_from_slice(&psh);
    buf.extend_from_slice(data); // data 始终在 wire 上（Windows quirk：plen 不计它）
    c.write_all(&buf).await.unwrap();
}

fn cqe_sc(p: &Pdu) -> u8 {
    assert_eq!(p.header.pdu_type, pdu_type::RSP);
    ((u16::from_le_bytes([p.psh[14], p.psh[15]]) >> 1) & 0xFF) as u8
}

async fn spawn_one_conn(shared: Arc<SharedControllerInner>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((server, _)) = listener.accept().await {
            let mut sess =
                accept_and_handshake_async(server, shared).await.unwrap();
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
        }
    });
    addr
}

/// **核心**：transport-SGL Connect（Windows）→ target 发 R2T → Windows-style H2CData(PLEN=HLEN)
/// 推 1024B Connect data → CapsuleResp SC=0。修复前此路被 SC=0x82 拒。
#[tokio::test(flavor = "multi_thread")]
async fn windows_transport_sgl_connect_with_plen_quirk_succeeds() {
    let (shared, _b) = make_shared();
    let addr = spawn_one_conn(Arc::clone(&shared)).await;
    let mut c = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut c).await;

    // admin Connect（qid=0），transport SGL，无 in-capsule data。
    send_capsule_cmd_no_data(&mut c, &build_connect_sqe_transport_sgl(0x0001, 0)).await;
    // target 应发 R2T 取 Connect data。
    let r2t = read_pdu_async(&mut c).await.unwrap();
    let (cccid, ttag, off, len) = parse_r2t(&r2t);
    assert_eq!(cccid, 0x0001, "R2T cccid 回 Connect cid");
    assert_eq!(off, 0);
    assert_eq!(len, fabric::CONNECT_DATA_SIZE as u32, "R2T 请求 1024B");
    // 回 Windows-style H2CData（PLEN=HLEN quirk）。
    send_h2cdata(&mut c, cccid, ttag, 0, &connect_data_bytes(0xFFFF), true).await;
    // 期望 Connect 成功。
    let resp = read_pdu_async(&mut c).await.unwrap();
    assert_eq!(cqe_sc(&resp), 0, "transport-SGL Connect (Windows H2CData quirk) 应 SC=0");
}

/// spec-conformant H2CData（PLEN=hlen+data）走同一路径也成功（Linux 风格也能用 transport SGL）。
#[tokio::test(flavor = "multi_thread")]
async fn transport_sgl_connect_conformant_h2cdata_succeeds() {
    let (shared, _b) = make_shared();
    let addr = spawn_one_conn(Arc::clone(&shared)).await;
    let mut c = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut c).await;
    send_capsule_cmd_no_data(&mut c, &build_connect_sqe_transport_sgl(0x0002, 0)).await;
    let r2t = read_pdu_async(&mut c).await.unwrap();
    let (cccid, ttag, _, _) = parse_r2t(&r2t);
    send_h2cdata(&mut c, cccid, ttag, 0, &connect_data_bytes(0xFFFF), false).await;
    assert_eq!(cqe_sc(&read_pdu_async(&mut c).await.unwrap()), 0);
}

/// 畸形：H2CData DATAL 超 R2T 范围（1024）→ target 拒（不补读越界）→ 连接错误（C2HTerm）。
#[tokio::test(flavor = "multi_thread")]
async fn transport_sgl_connect_oversized_h2cdata_rejected() {
    let (shared, _b) = make_shared();
    let addr = spawn_one_conn(Arc::clone(&shared)).await;
    let mut c = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut c).await;
    send_capsule_cmd_no_data(&mut c, &build_connect_sqe_transport_sgl(0x0003, 0)).await;
    let r2t = read_pdu_async(&mut c).await.unwrap();
    let (cccid, ttag, _, _) = parse_r2t(&r2t);
    // DATAL 标 2048（> R2T 1024），但只发 1024 真实字节 + Windows plen quirk。
    let mut over = [0u8; 16];
    over[0..2].copy_from_slice(&cccid.to_le_bytes());
    over[2..4].copy_from_slice(&ttag.to_le_bytes());
    over[8..12].copy_from_slice(&2048u32.to_le_bytes()); // DATAL 越界
    use tokio::io::AsyncWriteExt as _;
    let mut buf = vec![pdu_type::H2C_DATA, 0x04, 24, 24];
    buf.extend_from_slice(&24u32.to_le_bytes()); // plen=hlen (quirk)
    buf.extend_from_slice(&over);
    buf.extend_from_slice(&[0u8; 1024]);
    c.write_all(&buf).await.unwrap();
    // target 应不补读越界 DATAL、reassembler 报错 → C2HTerm 或连接断（非 SC=0 success）。
    match read_pdu_async(&mut c).await {
        Ok(p) => assert_ne!(
            p.header.pdu_type,
            pdu_type::RSP,
            "越界 DATAL 不该得 Connect success；应 C2HTerm"
        ),
        Err(_) => { /* 连接断也可接受 */ }
    }
}
