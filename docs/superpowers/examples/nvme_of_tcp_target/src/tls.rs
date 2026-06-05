// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-tls-2 / V-followup-mtls** — 教学版 TLS 1.3 TlsAcceptor builder。
//!
//! 把 PEM 形式的 X.509 cert + PKCS#8 private key 喂 rustls，build 出
//! `tokio_rustls::TlsAcceptor`。bin 端 (V-followup-tls-3 / V-followup-mtls)
//! 拿这个 acceptor 在新 listener 上 `acceptor.accept(tcp_stream)` 产
//! `TlsStream<TcpStream>`，再喂 V-followup-tls-1 泛化后的
//! `AsyncSession<TlsStream<TcpStream>>`。
//!
//! **教学/生产边界**：本模块支持：
//! - server-auth (单向 TLS) — [`build_acceptor_from_pem`]
//! - mTLS (双向 TLS，强制 client cert) — [`build_acceptor_with_mtls`]
//!
//! 仍 **未** 做：
//! - cert chain 内的 hostname / SAN 校验（rustls server 端按 spec 不做 client
//!   SNI 校验；mTLS 路径只校验 client cert chain 是否 trust，不绑 NQN）
//! - host NQN ↔ TLS identity binding（spec § 8.13 强制；留 V-followup-auth）
//! - PSK / DH-HMAC-CHAP（留 V-followup-tls-PSK / V-followup-auth）
//!
//! 仍坚持 `#![forbid(unsafe_code)]`：rustls 内部有 unsafe，但 crate 边界 safe；
//! 本 module 0 unsafe block。

use anyhow::{Context as _, anyhow};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

/// 内部 helper：从 cert + key PEM 路径解 (Vec<CertificateDer>, PrivateKeyDer)。
fn load_cert_and_key(
    cert_path: &Path,
    key_path: &Path,
) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_bytes = std::fs::read(cert_path)
        .with_context(|| format!("读 TLS cert 文件失败: {}", cert_path.display()))?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_bytes.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("解析 TLS cert PEM 失败: {}", cert_path.display()))?;
    if certs.is_empty() {
        return Err(anyhow!(
            "TLS cert PEM 不含 CERTIFICATE block: {}",
            cert_path.display()
        ));
    }
    let key_bytes = std::fs::read(key_path)
        .with_context(|| format!("读 TLS key 文件失败: {}", key_path.display()))?;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_bytes.as_slice())
        .with_context(|| format!("解析 TLS private key PEM 失败: {}", key_path.display()))?
        .ok_or_else(|| {
            anyhow!(
                "TLS key PEM 不含可识别的 PRIVATE KEY block: {}",
                key_path.display()
            )
        })?;
    Ok((certs, key))
}

/// 内部 helper：从 CA bundle PEM 路径解出全部 CertificateDer。
fn load_ca_bundle(ca_path: &Path) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let bytes = std::fs::read(ca_path)
        .with_context(|| format!("读 client CA bundle 失败: {}", ca_path.display()))?;
    let cas: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut bytes.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("解析 client CA bundle PEM 失败: {}", ca_path.display()))?;
    if cas.is_empty() {
        return Err(anyhow!(
            "client CA bundle 不含 CERTIFICATE block: {}",
            ca_path.display()
        ));
    }
    Ok(cas)
}

fn ensure_ring_provider_installed() {
    // **V-followup-tls-2** — feature gating 时 default-features off，必须
    // install_default 一次。多次调 install_default 第二次起返 Err，用 `.ok()`
    // 容忍（其它测试 / bin 第一次启动可能已装）。
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// **V-followup-tls-2** — 从 PEM cert + key 文件路径构造 server-auth `TlsAcceptor`
/// （不验 client cert）。
///
/// # 参数
/// - `cert_path`: X.509 certificate chain PEM（PEM block label "CERTIFICATE"）；
///   单 cert 或 chain 都接受
/// - `key_path`: 私钥 PEM；接受 PKCS#8（"PRIVATE KEY"）/ PKCS#1 RSA
///   （"RSA PRIVATE KEY"）/ SEC1 EC（"EC PRIVATE KEY"）
///
/// # 失败
/// - cert / key 文件不可读
/// - PEM 解析错（malformed / 空文件 / 无相关 block）
/// - rustls `ServerConfig::with_single_cert` 拒收（cert/key 不匹配 / cert 损坏）
pub fn build_acceptor_from_pem(
    cert_path: impl AsRef<Path>,
    key_path: impl AsRef<Path>,
) -> anyhow::Result<TlsAcceptor> {
    let (certs, key) = load_cert_and_key(cert_path.as_ref(), key_path.as_ref())?;
    ensure_ring_provider_installed();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("rustls ServerConfig 构建失败（cert/key 不匹配？）")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// **V-followup-mtls** — 构造 **强制 client cert** 的 `TlsAcceptor`（mTLS）。
///
/// # 参数
/// - `cert_path` / `key_path`: server cert chain + key（同 [`build_acceptor_from_pem`]）
/// - `client_ca_path`: 受信 client CA bundle（多 cert 串接的 PEM 即 chain）；
///   `WebPkiClientVerifier` 用其作 trust anchor 校验 client cert 链
///
/// # 行为
/// - rustls `WebPkiClientVerifier::builder(...).build()` 强制 client 必发 cert，
///   且 chain 必须能 anchor 到本 bundle
/// - client 不带 cert / cert 不在 trust 内 → handshake fail，`TlsAcceptor::accept`
///   返 Err
/// - 同 server-auth 路径不绑 NQN identity（V-followup-auth 才做）
///
/// # 失败
/// - 三 PEM 任一不可读 / 解析错 / 空 block
/// - rustls `WebPkiClientVerifier::builder` 拒收（CA 损坏 / 不支持的 cert format）
/// - rustls `with_single_cert` 拒收（server cert/key 不匹配）
pub fn build_acceptor_with_mtls(
    cert_path: impl AsRef<Path>,
    key_path: impl AsRef<Path>,
    client_ca_path: impl AsRef<Path>,
) -> anyhow::Result<TlsAcceptor> {
    let (certs, key) = load_cert_and_key(cert_path.as_ref(), key_path.as_ref())?;
    let client_cas = load_ca_bundle(client_ca_path.as_ref())?;
    ensure_ring_provider_installed();

    let mut roots = rustls::RootCertStore::empty();
    for ca in client_cas {
        roots
            .add(ca)
            .context("把 client CA cert 加入 RootCertStore 失败")?;
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .context("构造 WebPkiClientVerifier 失败")?;

    let config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .context("rustls mTLS ServerConfig 构建失败（cert/key 不匹配？）")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用 rcgen 在 tempdir 生成 self-signed cert + PKCS#8 key 喂 builder 正路径。
    #[test]
    fn vt_tls_2_acceptor_built_from_rcgen_self_signed() {
        let tmp = tempfile::tempdir().unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_pem = cert.cert.pem();
        let key_pem = cert.key_pair.serialize_pem();
        let cert_path = tmp.path().join("server.pem");
        let key_path = tmp.path().join("server.key");
        std::fs::write(&cert_path, cert_pem).unwrap();
        std::fs::write(&key_path, key_pem).unwrap();

        let acceptor = build_acceptor_from_pem(&cert_path, &key_path).expect("正路径应成功");
        drop(acceptor);
    }

    #[test]
    fn vt_tls_2_acceptor_rejects_missing_key_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(tmp.path().join("server.pem"), cert.cert.pem()).unwrap();
        let res =
            build_acceptor_from_pem(tmp.path().join("server.pem"), tmp.path().join("server.key"));
        let err = res.err().expect("应失败");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("读 TLS key 文件失败"),
            "应给清晰错误信息, got: {msg}"
        );
    }

    #[test]
    fn vt_tls_2_acceptor_rejects_malformed_pem() {
        let tmp = tempfile::tempdir().unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(tmp.path().join("server.pem"), cert.cert.pem()).unwrap();
        std::fs::write(
            tmp.path().join("server.key"),
            b"this is not a PEM file at all\n",
        )
        .unwrap();
        let res =
            build_acceptor_from_pem(tmp.path().join("server.pem"), tmp.path().join("server.key"));
        let err = res.err().expect("应失败");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("不含可识别的 PRIVATE KEY block")
                || msg.contains("解析 TLS private key PEM 失败"),
            "应给清晰错误信息, got: {msg}"
        );
    }

    #[test]
    fn vt_tls_2_acceptor_rejects_empty_cert_pem() {
        let tmp = tempfile::tempdir().unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(tmp.path().join("server.pem"), b"").unwrap();
        std::fs::write(tmp.path().join("server.key"), cert.key_pair.serialize_pem()).unwrap();
        let res =
            build_acceptor_from_pem(tmp.path().join("server.pem"), tmp.path().join("server.key"));
        let err = res.err().expect("应失败");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("不含 CERTIFICATE block"),
            "应给清晰错误信息, got: {msg}"
        );
    }

    /// **V-followup-mtls** — 正路径：server cert + client CA → acceptor build 成功
    #[test]
    fn vt_mtls_acceptor_built_with_client_ca() {
        let tmp = tempfile::tempdir().unwrap();
        let server = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let client_ca = rcgen::generate_simple_self_signed(vec!["client-ca".to_string()]).unwrap();
        std::fs::write(tmp.path().join("server.pem"), server.cert.pem()).unwrap();
        std::fs::write(
            tmp.path().join("server.key"),
            server.key_pair.serialize_pem(),
        )
        .unwrap();
        std::fs::write(tmp.path().join("client-ca.pem"), client_ca.cert.pem()).unwrap();
        let acceptor = build_acceptor_with_mtls(
            tmp.path().join("server.pem"),
            tmp.path().join("server.key"),
            tmp.path().join("client-ca.pem"),
        )
        .expect("mTLS 正路径应成功");
        drop(acceptor);
    }

    #[test]
    fn vt_mtls_acceptor_rejects_missing_ca() {
        let tmp = tempfile::tempdir().unwrap();
        let server = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(tmp.path().join("server.pem"), server.cert.pem()).unwrap();
        std::fs::write(
            tmp.path().join("server.key"),
            server.key_pair.serialize_pem(),
        )
        .unwrap();
        // 不写 client-ca.pem
        let res = build_acceptor_with_mtls(
            tmp.path().join("server.pem"),
            tmp.path().join("server.key"),
            tmp.path().join("client-ca.pem"),
        );
        let err = res.err().expect("应失败");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("读 client CA bundle 失败"),
            "应给清晰错误信息, got: {msg}"
        );
    }

    #[test]
    fn vt_mtls_acceptor_rejects_empty_ca_pem() {
        let tmp = tempfile::tempdir().unwrap();
        let server = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(tmp.path().join("server.pem"), server.cert.pem()).unwrap();
        std::fs::write(
            tmp.path().join("server.key"),
            server.key_pair.serialize_pem(),
        )
        .unwrap();
        std::fs::write(tmp.path().join("client-ca.pem"), b"").unwrap();
        let res = build_acceptor_with_mtls(
            tmp.path().join("server.pem"),
            tmp.path().join("server.key"),
            tmp.path().join("client-ca.pem"),
        );
        let err = res.err().expect("应失败");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("不含 CERTIFICATE block"),
            "应给清晰错误信息, got: {msg}"
        );
    }
}
