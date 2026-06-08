// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-auth-2 集成测试** — NQN ↔ TLS cert SAN/CN 绑定。
//!
//! 覆盖：
//! 1. `vt_auth2_connect_passes_when_hostnqn_matches_cert_san_uri` — client
//!    cert SAN URI = hostnqn → Connect 通过
//! 2. `vt_auth2_connect_passes_when_hostnqn_matches_cn` — client cert CN =
//!    hostnqn (无 SAN URI) → Connect 通过
//! 3. `vt_auth2_connect_rejected_when_hostnqn_not_in_cert` — client cert
//!    完全不含 hostnqn → SC=0x84 CONNECT_INVALID_HOST

#![allow(missing_docs)]

use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::{read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{
    AsyncSession, SharedControllerInner, accept_and_handshake_async, build_acceptor_with_mtls,
    extract_host_identities,
};
use nvme_firmware::NvmeController;
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

struct Pki {
    server_cert_path: std::path::PathBuf,
    server_key_path: std::path::PathBuf,
    client_ca_path: std::path::PathBuf,
    client_cert_der: CertificateDer<'static>,
    client_key_der: PrivateKeyDer<'static>,
    _tmp: tempfile::TempDir,
}

/// 生成 server + CA + 一个由 CA 签的 client cert，cert SAN/CN 按参数注入。
fn make_pki_with_client_id(client_san_uris: Vec<String>, client_cn: Option<String>) -> Pki {
    use rcgen::{CertificateParams, KeyPair};
    let tmp = tempfile::tempdir().unwrap();
    let server = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let server_cert_path = tmp.path().join("server.pem");
    let server_key_path = tmp.path().join("server.key");
    std::fs::write(&server_cert_path, server.cert.pem()).unwrap();
    std::fs::write(&server_key_path, server.key_pair.serialize_pem()).unwrap();

    let mut ca_params = CertificateParams::new(vec!["client-ca".to_string()]).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let client_ca_path = tmp.path().join("client-ca.pem");
    std::fs::write(&client_ca_path, ca_cert.pem()).unwrap();

    // 构造 client cert params
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    for uri in client_san_uris {
        params
            .subject_alt_names
            .push(rcgen::SanType::URI(uri.try_into().unwrap()));
    }
    if let Some(cn) = client_cn {
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cn);
    }
    let client_key = KeyPair::generate().unwrap();
    let client_cert = params.signed_by(&client_key, &ca_cert, &ca_key).unwrap();
    let client_cert_der: CertificateDer<'static> = client_cert.der().clone();
    let client_key_pem = client_key.serialize_pem();
    let client_key_der: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut client_key_pem.as_bytes())
            .unwrap()
            .unwrap();

    Pki {
        server_cert_path,
        server_key_path,
        client_ca_path,
        client_cert_der,
        client_key_der,
        _tmp: tmp,
    }
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

async fn send_icreq<S>(s: &mut S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
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

/// Setup mTLS server + AsyncSession with bind_host_identities(leaf cert ids)
async fn run_server_with_binding(
    pki: &Pki,
    shared: Arc<SharedControllerInner>,
) -> (
    tokio::net::TcpListener,
    std::net::SocketAddr,
    tokio_rustls::TlsAcceptor,
) {
    let acceptor = build_acceptor_with_mtls(
        &pki.server_cert_path,
        &pki.server_key_path,
        &pki.client_ca_path,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _ = shared;
    (listener, addr, acceptor)
}

fn client_cfg(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> rustls::ClientConfig {
    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DangerousAllowAnyCert))
        .with_client_auth_cert(vec![cert], key)
        .unwrap()
}

async fn run_connect_e2e(pki: Pki, hostnqn: &str) -> u8 {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (shared, _backing) = make_shared();
    let (listener, addr, acceptor) = run_server_with_binding(&pki, Arc::clone(&shared)).await;
    let s = Arc::clone(&shared);
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(tcp).await.unwrap();
        // 从 TlsStream 抽 peer cert SAN/CN
        let (_t, sc) = tls.get_ref();
        let leaf = sc.peer_certificates().and_then(|c| c.first()).cloned();
        let ids = leaf
            .as_ref()
            .map(extract_host_identities)
            .transpose()
            .unwrap()
            .unwrap_or_default();
        let mut sess: AsyncSession<tokio_rustls::server::TlsStream<TokioStream>> =
            accept_and_handshake_async(tls, s).await.unwrap();
        sess.bind_host_identities(ids);
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        if let Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) = sess.pump_one_async(&mut rx).await {
            let _ = sess.dispatch_pdu_async(p).await;
        }
    });
    let cfg = client_cfg(pki.client_cert_der, pki.client_key_der);
    let connector = TlsConnector::from(Arc::new(cfg));
    let tcp = TokioStream::connect(addr).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let mut client = connector.connect(name, tcp).await.unwrap();
    send_icreq(&mut client).await;
    let _ic = read_pdu_async(&mut client).await.unwrap();
    let (hdr, sqe, data) = build_connect_pdu(0x0001, hostnqn);
    write_pdu_async(&mut client, &hdr, &sqe, &data)
        .await
        .unwrap();
    let resp = read_pdu_async(&mut client).await.unwrap();
    let cqe = &resp.psh;
    let status = u16::from_le_bytes([cqe[14], cqe[15]]);
    let sc = (status >> 1) & 0xFF;
    let _ = client.shutdown().await;
    let _ = server.await;
    sc as u8
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_auth2_connect_passes_when_hostnqn_matches_cert_san_uri() {
    let nqn = "nqn.2014-08.org.nvmexpress:uuid:host-x";
    let pki = make_pki_with_client_id(vec![nqn.to_string()], Some("client-x".to_string()));
    let sc = run_connect_e2e(pki, nqn).await;
    assert_ne!(sc, 0x84, "SAN URI 命中 hostnqn 应通过");
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_auth2_connect_passes_when_hostnqn_matches_cn() {
    let cn = "host-cn-only";
    let pki = make_pki_with_client_id(vec![], Some(cn.to_string()));
    let sc = run_connect_e2e(pki, cn).await;
    assert_ne!(sc, 0x84, "CN 命中 hostnqn 应通过");
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_auth2_connect_rejected_when_hostnqn_not_in_cert() {
    let pki = make_pki_with_client_id(
        vec!["nqn.legit-only".to_string()],
        Some("legit".to_string()),
    );
    let sc = run_connect_e2e(pki, "nqn.attacker-claim").await;
    assert_eq!(
        sc, 0x84,
        "hostnqn 不在 cert SAN/CN 应返 CONNECT_INVALID_HOST"
    );
}
