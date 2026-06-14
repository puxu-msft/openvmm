// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-tls-3 集成测试** — TLS 1.3 handshake → ICReq/ICResp
//! 端到端：起一个 tokio TcpListener 喂 `TlsAcceptor`，client 用 `TlsConnector`
//! + dangerous certificate verifier（仅测试内）跳过 chain validation，跑
//!   AsyncSession over TlsStream。
//!
//! 覆盖：
//! 1. `vt_tls_3_handshake_then_icreq_roundtrip` — TLS handshake 完成后
//!    AsyncSession<TlsStream<TcpStream>> 跑 ICReq/ICResp + Connect admin
//!    + Property Get 端到端
//! 2. `vt_tls_3_handshake_aborted_drops_conn` — client 直接 plaintext 发
//!    ICReq 到 TLS port，server 应 fail（不 fallback plaintext）

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::framing::{Pdu, read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{
    SharedControllerInner, accept_and_handshake_async, build_acceptor_from_pem,
};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use std::sync::Arc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream as TokioStream;
use tokio_rustls::TlsConnector;
use zerocopy::IntoBytes;

/// 测试用：把任何 cert 视为有效（**仅测试**；生产严禁）。
#[derive(Debug)]
struct DangerousAllowAnyCert;

impl rustls::client::danger::ServerCertVerifier for DangerousAllowAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

fn make_shared() -> (Arc<SharedControllerInner>, tempfile::NamedTempFile) {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(1024 * 1024).unwrap();
    let path = f.path().to_string_lossy().into_owned();
    let c = NvmeController::open(std::slice::from_ref(&path), 0x1414, 0, &[]).unwrap();
    (Arc::new(SharedControllerInner::new(c)), f)
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

fn build_connect_pdu(cid: u16) -> Pdu {
    use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
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
    let cd = ConnectData::default();
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

fn build_property_get_cap_pdu(cid: u16) -> Pdu {
    use nvme_of_tcp_target::fabric::{self, PropertyFabricFields, fctype, property_offset};
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::PROPERTY_GET;
    let f = PropertyFabricFields {
        attrib: 1,
        rsvd1: [0u8; 3],
        ofst: property_offset::CAP,
        value: 0,
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

#[tokio::test(flavor = "multi_thread")]
async fn vt_tls_3_handshake_then_icreq_roundtrip() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (shared, _backing) = make_shared();
    let (_tmp, cert_path, key_path) = build_test_cert();
    let acceptor = build_acceptor_from_pem(&cert_path, &key_path).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // server task：accept TCP → TLS handshake → AsyncSession over TlsStream
    let s = Arc::clone(&shared);
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(tcp).await.unwrap();
        let mut sess =
            accept_and_handshake_async(tls, s).await.unwrap();
        // 跑 2 个 cmd: Connect + Property Get
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        for _ in 0..2 {
            match sess.pump_one_async(&mut rx).await.unwrap() {
                nvme_of_tcp_target::PumpEvent::Pdu(p) => {
                    let _ = sess.dispatch_pdu_async(p).await.unwrap();
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
    });

    // client：dangerous verifier + TLS connect
    let client_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DangerousAllowAnyCert))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));
    let tcp = TokioStream::connect(addr).await.unwrap();
    let server_name = ServerName::try_from("localhost").unwrap();
    let mut client = connector.connect(server_name, tcp).await.unwrap();

    // ICReq
    let icreq_hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let psh = IcPsh::default();
    write_pdu_async(&mut client, &icreq_hdr, psh.as_bytes(), &[])
        .await
        .unwrap();
    let ic_resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(ic_resp.header.pdu_type, pdu_type::ICRESP);

    // Connect
    let connect = build_connect_pdu(0x0001);
    write_pdu_async(&mut client, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let resp1 = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(resp1.header.pdu_type, pdu_type::RSP);

    // Property Get CAP
    let pg = build_property_get_cap_pdu(0x0010);
    write_pdu_async(&mut client, &pg.header, &pg.psh, &pg.data)
        .await
        .unwrap();
    let resp2 = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(resp2.header.pdu_type, pdu_type::RSP);

    client.shutdown().await.unwrap();
    drop(client);
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_tls_3_handshake_aborted_drops_conn() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (shared, _backing) = make_shared();
    let (_tmp, cert_path, key_path) = build_test_cert();
    let acceptor = build_acceptor_from_pem(&cert_path, &key_path).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // server：accept TCP → TLS handshake，必失败（plain peer 不发 ClientHello）
    let _s = Arc::clone(&shared);
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        // 30s 是 bin 的 timeout，这里 1s 即可（plain peer 发 NVMe ICReq 给
        // TLS port，rustls 应快速 reject 而非超时 30s）
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), acceptor.accept(tcp)).await;
        match r {
            Ok(Ok(_)) => panic!("TLS handshake unexpectedly succeeded on plaintext peer"),
            Ok(Err(_e)) => {
                // 预期：rustls 收到非 ClientHello 应 reject
            }
            Err(_elapsed) => panic!("TLS handshake should fail fast on plaintext bytes"),
        }
    });

    // plain client 直接送 NVMe ICReq 字节到 TLS port
    let mut plain = TokioStream::connect(addr).await.unwrap();
    let icreq_hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let psh = IcPsh::default();
    // write_pdu_async 在 plain client 上写明文 ICReq；server 端应作为
    // ClientHello 解析失败 → rustls reject → 不 fallback。
    let _ = write_pdu_async(&mut plain, &icreq_hdr, psh.as_bytes(), &[]).await;
    // server 不会回 ICResp（成功路径）；client 读应得 EOF / Err
    let mut buf = [0u8; 16];
    let r = tokio::time::timeout(std::time::Duration::from_secs(5), plain.read(&mut buf)).await;
    // 几种合法路径：rustls 发 TLS Alert → buf 含 alert bytes / 0 bytes / Err
    // 关键：server 决不发 ICResp（pdu_type=0x01），plain peer 不会拿到一个
    // 合法的 NVMe-oF 握手响应。
    match r {
        Ok(Ok(0)) => { /* EOF: rustls 已 drop */ }
        Ok(Ok(_n)) => {
            // 若有字节，绝不应是 NVMe ICResp PDU type (0x01)
            assert_ne!(
                buf[0],
                pdu_type::ICRESP,
                "TLS port 绝不应回 NVMe ICResp（防 downgrade）"
            );
        }
        Ok(Err(_)) => { /* IO error, OK */ }
        Err(_) => panic!("plain peer 在 TLS port 上的握手应快速失败"),
    }
    server.await.unwrap();
}
