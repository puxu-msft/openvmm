// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-mtls 集成测试** — 双向 TLS。
//!
//! 覆盖：
//! 1. `vt_mtls_handshake_with_valid_client_cert` — client 出示 trusted cert
//!    → handshake 成功 → AsyncSession ICReq/ICResp 端到端
//! 2. `vt_mtls_handshake_rejected_when_client_has_no_cert` — client 用普通
//!    TlsConnector 不带 cert → server `acceptor.accept` 返 Err
//! 3. `vt_mtls_handshake_rejected_when_client_cert_signed_by_unknown_ca` —
//!    client 出示另一 CA 签的 cert → server reject
//!
//! 教学要点：mTLS 只验 cert chain trust，不绑 NQN identity（spec § 8.13
//! 强制；留 V-followup-auth）。

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::framing::{read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{
    AsyncSession, SharedControllerInner, accept_and_handshake_async, build_acceptor_with_mtls,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use std::sync::Arc;
use tokio::io::AsyncWriteExt as _;
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

fn make_shared() -> (Arc<SharedControllerInner>, tempfile::NamedTempFile) {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(1024 * 1024).unwrap();
    let path = f.path().to_string_lossy().into_owned();
    let c = NvmeController::open(std::slice::from_ref(&path), 0x1414, 0, &[]).unwrap();
    (Arc::new(SharedControllerInner::new(c)), f)
}

/// 一对 (CA cert, CA key, ca_path)；CA 用以签 client cert。
struct Pki {
    server_cert_path: std::path::PathBuf,
    server_key_path: std::path::PathBuf,
    /// CA bundle 包含 client cert 的签发者 CA
    client_ca_path: std::path::PathBuf,
    /// 由该 CA 签的 client cert + key (用作 client identity)
    client_cert_der: CertificateDer<'static>,
    client_key_der: PrivateKeyDer<'static>,
    /// 由 **另一个** unrelated CA 签的 client cert + key (用作 mTLS reject 测试)
    unknown_client_cert_der: CertificateDer<'static>,
    unknown_client_key_der: PrivateKeyDer<'static>,
    _tmp: tempfile::TempDir,
}

fn make_pki() -> Pki {
    use rcgen::{CertificateParams, KeyPair};
    let tmp = tempfile::tempdir().unwrap();
    // server self-signed
    let server = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let server_cert_path = tmp.path().join("server.pem");
    let server_key_path = tmp.path().join("server.key");
    std::fs::write(&server_cert_path, server.cert.pem()).unwrap();
    std::fs::write(&server_key_path, server.key_pair.serialize_pem()).unwrap();

    // client CA：作为 client cert 的 issuer
    let mut ca_params = CertificateParams::new(vec!["client-ca".to_string()]).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let client_ca_path = tmp.path().join("client-ca.pem");
    std::fs::write(&client_ca_path, ca_cert.pem()).unwrap();

    // client cert 由 ca_cert 签
    let client_params = CertificateParams::new(vec!["client-1".to_string()]).unwrap();
    let client_key = KeyPair::generate().unwrap();
    let client_cert = client_params
        .signed_by(&client_key, &ca_cert, &ca_key)
        .unwrap();
    let client_cert_der: CertificateDer<'static> = client_cert.der().clone();
    let client_key_pem = client_key.serialize_pem();
    let client_key_der: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut client_key_pem.as_bytes())
            .unwrap()
            .unwrap();

    // unknown CA + 它签的 client cert (server 不认它)
    let mut unk_ca_params = CertificateParams::new(vec!["unknown-ca".to_string()]).unwrap();
    unk_ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let unk_ca_key = KeyPair::generate().unwrap();
    let unk_ca_cert = unk_ca_params.self_signed(&unk_ca_key).unwrap();
    let unk_client_params = CertificateParams::new(vec!["evil".to_string()]).unwrap();
    let unk_client_key = KeyPair::generate().unwrap();
    let unk_client_cert = unk_client_params
        .signed_by(&unk_client_key, &unk_ca_cert, &unk_ca_key)
        .unwrap();
    let unknown_client_cert_der: CertificateDer<'static> = unk_client_cert.der().clone();
    let unk_client_key_pem = unk_client_key.serialize_pem();
    let unknown_client_key_der: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut unk_client_key_pem.as_bytes())
            .unwrap()
            .unwrap();

    Pki {
        server_cert_path,
        server_key_path,
        client_ca_path,
        client_cert_der,
        client_key_der,
        unknown_client_cert_der,
        unknown_client_key_der,
        _tmp: tmp,
    }
}

fn make_client_config(
    client_cert: CertificateDer<'static>,
    client_key: PrivateKeyDer<'static>,
) -> rustls::ClientConfig {
    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DangerousAllowAnyCert))
        .with_client_auth_cert(vec![client_cert], client_key)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_mtls_handshake_with_valid_client_cert() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pki = make_pki();
    let (shared, _backing) = make_shared();
    let acceptor = build_acceptor_with_mtls(
        &pki.server_cert_path,
        &pki.server_key_path,
        &pki.client_ca_path,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = Arc::clone(&shared);
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(tcp).await.expect("mTLS handshake 应成功");
        let mut sess: AsyncSession<tokio_rustls::server::TlsStream<TokioStream>> =
            accept_and_handshake_async(tls, s).await.unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        // 不实际 dispatch，只验 NVMe ICReq/ICResp 走过 mTLS 通道
        if let Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) = sess.pump_one_async(&mut rx).await {
            let _ = sess.dispatch_pdu_async(p).await;
        }
    });
    let client_cfg =
        make_client_config(pki.client_cert_der.clone(), pki.client_key_der.clone_key());
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let tcp = TokioStream::connect(addr).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let mut client = connector
        .connect(name, tcp)
        .await
        .expect("client TLS connect 应成功");
    // ICReq
    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let psh = IcPsh::default();
    write_pdu_async(&mut client, &hdr, psh.as_bytes(), &[])
        .await
        .unwrap();
    let ic_resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(ic_resp.header.pdu_type, pdu_type::ICRESP);
    client.shutdown().await.unwrap();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_mtls_handshake_rejected_when_client_has_no_cert() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pki = make_pki();
    let (shared, _backing) = make_shared();
    let acceptor = build_acceptor_with_mtls(
        &pki.server_cert_path,
        &pki.server_key_path,
        &pki.client_ca_path,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _s = Arc::clone(&shared);
    // server 端：accept 必返 Err（mTLS 强制 client cert，缺即 reject）
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), acceptor.accept(tcp)).await;
        match r {
            Ok(Ok(_)) => false, // 不应到这
            Ok(Err(_)) => true, // 预期
            Err(_) => false,    // 超时也算不该
        }
    });
    // client 用普通 server-auth-only TlsConnector（不带 cert）
    // TLS 1.3 下 client.connect 可能 "成功"（乐观发 Finished），但后续读必失败。
    let client_cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DangerousAllowAnyCert))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let tcp = TokioStream::connect(addr).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    // 不 assert client.connect 成功与否（TLS 1.3 可能乐观成功）；只 assert server 拒
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        connector.connect(name, tcp),
    )
    .await;
    let server_rejected = server.await.unwrap();
    assert!(server_rejected, "mTLS server 应拒收 no-cert client");
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_mtls_handshake_rejected_when_client_cert_signed_by_unknown_ca() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pki = make_pki();
    let (shared, _backing) = make_shared();
    let acceptor = build_acceptor_with_mtls(
        &pki.server_cert_path,
        &pki.server_key_path,
        &pki.client_ca_path,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _s = Arc::clone(&shared);
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), acceptor.accept(tcp)).await;
        match r {
            Ok(Ok(_)) => false,
            Ok(Err(_)) => true,
            Err(_) => false,
        }
    });
    // client 出示由 unknown CA 签的 cert
    let client_cfg = make_client_config(
        pki.unknown_client_cert_der.clone(),
        pki.unknown_client_key_der.clone_key(),
    );
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let tcp = TokioStream::connect(addr).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        connector.connect(name, tcp),
    )
    .await;
    let server_rejected = server.await.unwrap();
    assert!(server_rejected, "mTLS server 应拒收 unknown-CA client cert");
}
