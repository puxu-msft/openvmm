// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-tls-4** — TLS-wrapped 应用层 byte-identical 反回归。
//!
//! 目标：让 TLS path (V-followup-tls-3) 与 plaintext path 对同样的 PDU 输入序列
//! 产生 **decrypt 后字节完全等价的应用层 wire**。TLS 只 wrap transport，不
//! 改任何 NVMe-oF PDU 字节；本 gate 防止未来给 TLS 路径偷偷加 in-band
//! transformation 而 plaintext 路径没加（或反之）。
//!
//! 设计：
//! - server side 各起一份 listener (plaintext / TLS) 共享同一 backing pattern
//! - client side 各起 plaintext TcpStream / TLS connector
//! - 输入同样 ICReq + Connect + Property Get + Identify Controller
//! - 收 server 回 stream 末尾后做 PDU-level 解析（与 V8e-7-followup
//!   byte-identical 同模式），比对 type / PSH / data
//!
//! 由于 TLS handshake 引入随机性（cert ephemeral key, ClientRandom,
//! ServerRandom），无法做 raw TCP byte-identical；只能比 decrypt 后的
//! 应用层 stream。

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::framing::Pdu;
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, decode_common_hdr, pdu_type};
use nvme_of_tcp_target::{
    SharedControllerInner, accept_and_handshake_async, build_acceptor_from_pem,
};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use std::sync::Arc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream as TokioStream;
use tokio_rustls::TlsConnector;
use zerocopy::IntoBytes;

#[derive(Debug)]
struct DangerousAllowAnyCert;

impl rustls::client::danger::ServerCertVerifier for DangerousAllowAnyCert {
    fn verify_server_cert(
        &self,
        _e: &CertificateDer<'_>,
        _i: &[CertificateDer<'_>],
        _n: &ServerName<'_>,
        _o: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
        ]
    }
}

fn make_backing(size: u64, pattern: u8) -> tempfile::NamedTempFile {
    use std::io::Write as _;
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

fn make_shared(backing: &std::path::Path) -> Arc<SharedControllerInner> {
    let path = backing.to_string_lossy().into_owned();
    let c = NvmeController::open(std::slice::from_ref(&path), 0x1414, 0, &[]).unwrap();
    Arc::new(SharedControllerInner::new(c))
}

fn build_test_cert() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_path = tmp.path().join("server.pem");
    let key_path = tmp.path().join("server.key");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
    (tmp, cert_path, key_path)
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

fn build_connect_bytes(cid: u16) -> Vec<u8> {
    use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
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
    out.extend_from_slice(cd.as_bytes());
    out
}

fn build_identify_ctrl_bytes(cid: u16) -> Vec<u8> {
    let mut sqe = [0u8; 64];
    sqe[0] = 0x06;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[40..44].copy_from_slice(&0x01u32.to_le_bytes());
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

fn decode_all_pdus(stream: &[u8]) -> Vec<Pdu> {
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
        let data = if plen > hlen {
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

/// 跑 plaintext path（async over plain TCP）；收 server 完整字节流
async fn run_plaintext(
    backing_path: &std::path::Path,
    requests: &[Vec<u8>],
    pumps: usize,
) -> Vec<u8> {
    let shared = make_shared(backing_path);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = Arc::clone(&shared);
    let server = tokio::spawn(async move {
        let (server, _) = listener.accept().await.unwrap();
        let mut sess = accept_and_handshake_async(server, s).await.unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        for _ in 0..pumps {
            match sess.pump_one_async(&mut rx).await {
                Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) => {
                    let _ = sess.dispatch_pdu_async(p).await;
                }
                _ => break,
            }
        }
    });
    let mut client = TokioStream::connect(addr).await.unwrap();
    for req in requests {
        client.write_all(req).await.unwrap();
    }
    client.shutdown().await.unwrap();
    let mut received = Vec::new();
    let _ = client.read_to_end(&mut received).await;
    drop(client);
    let _ = server.await;
    received
}

/// 跑 TLS path；同样请求 + 同 pump 次数；解密后返应用层字节
async fn run_tls(backing_path: &std::path::Path, requests: &[Vec<u8>], pumps: usize) -> Vec<u8> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let shared = make_shared(backing_path);
    let (_tmp, cert_path, key_path) = build_test_cert();
    let acceptor = build_acceptor_from_pem(&cert_path, &key_path).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = Arc::clone(&shared);
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(tcp).await.unwrap();
        let mut sess = accept_and_handshake_async(tls, s).await.unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        for _ in 0..pumps {
            match sess.pump_one_async(&mut rx).await {
                Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) => {
                    let _ = sess.dispatch_pdu_async(p).await;
                }
                _ => break,
            }
        }
    });
    let client_cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DangerousAllowAnyCert))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let tcp = TokioStream::connect(addr).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let mut client = connector.connect(name, tcp).await.unwrap();
    for req in requests {
        client.write_all(req).await.unwrap();
    }
    client.shutdown().await.unwrap();
    let mut received = Vec::new();
    let _ = client.read_to_end(&mut received).await;
    drop(client);
    let _ = server.await;
    received
}

fn compare_pdus(label: &str, plaintext: Vec<u8>, tls: Vec<u8>) {
    let p_pdus = decode_all_pdus(&plaintext);
    let t_pdus = decode_all_pdus(&tls);
    assert_eq!(
        p_pdus.len(),
        t_pdus.len(),
        "{label}: plaintext vs TLS 应用层 PDU 数应一致 (plain={}, tls={})",
        p_pdus.len(),
        t_pdus.len()
    );
    for (i, (a, b)) in p_pdus.iter().zip(t_pdus.iter()).enumerate() {
        assert_eq!(
            a.header.pdu_type, b.header.pdu_type,
            "{label} PDU#{i} type 不一致"
        );
        assert_eq!(a.psh, b.psh, "{label} PDU#{i} PSH bytes 不一致");
        assert_eq!(a.data, b.data, "{label} PDU#{i} data bytes 不一致");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_tls_4_app_layer_bytes_identical_connect_admin() {
    let bk_p = make_backing(1024 * 1024, 0xAA);
    let bk_t = make_backing(1024 * 1024, 0xAA);
    let reqs = vec![build_icreq_bytes(), build_connect_bytes(0x0001)];
    let plain = run_plaintext(bk_p.path(), &reqs, 1).await;
    let tls = run_tls(bk_t.path(), &reqs, 1).await;
    compare_pdus("Connect admin (TLS-vs-plain)", plain, tls);
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_tls_4_app_layer_bytes_identical_identify_controller() {
    let bk_p = make_backing(1024 * 1024, 0xAA);
    let bk_t = make_backing(1024 * 1024, 0xAA);
    let reqs = vec![
        build_icreq_bytes(),
        build_connect_bytes(0x0001),
        build_identify_ctrl_bytes(0x0010),
    ];
    let plain = run_plaintext(bk_p.path(), &reqs, 2).await;
    let tls = run_tls(bk_t.path(), &reqs, 2).await;
    compare_pdus("Identify Ctrl (TLS-vs-plain)", plain, tls);
}
