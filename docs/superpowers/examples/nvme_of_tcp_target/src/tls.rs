// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-tls-2** — 教学版 TLS 1.3 TlsAcceptor builder。
//!
//! 把 PEM 形式的 X.509 cert + PKCS#8 private key 喂 rustls，build 出
//! `tokio_rustls::TlsAcceptor`。bin 端 (V-followup-tls-3) 拿这个 acceptor
//! 在新 listener 上 `acceptor.accept(tcp_stream)` 产 `TlsStream<TcpStream>`，
//! 再喂 V-followup-tls-1 泛化后的 `AsyncSession<TlsStream<TcpStream>>`。
//!
//! **教学/生产边界**：本模块只做 server-auth (单向 TLS)：
//! - 不验 client cert（mTLS 留 V-followup-mtls）
//! - 不绑 host NQN ↔ TLS identity（留 V-followup-auth）
//! - 不做 PSK / DH-HMAC-CHAP（留 V-followup-tls-PSK / V-followup-auth）
//! - cert 链可以是 self-signed（无 CA root 校验）— 由 bin CLI 强制双 explicit
//!   consent `--tls-cert + --tls-i-trust-this-cert` 防误用
//!
//! 仍坚持 `#![forbid(unsafe_code)]`：rustls 内部有 unsafe，但 crate 边界 safe；
//! 本 module 0 unsafe block。

use anyhow::{Context as _, anyhow};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

/// **V-followup-tls-2** — 从 PEM cert + key 文件路径构造 `TlsAcceptor`。
///
/// # 参数
/// - `cert_path`: X.509 certificate chain PEM（PEM block label "CERTIFICATE"）；
///   单 cert 或 chain 都接受
/// - `key_path`: 私钥 PEM；接受 PKCS#8（"PRIVATE KEY"）/ PKCS#1 RSA
///   （"RSA PRIVATE KEY"）/ SEC1 EC（"EC PRIVATE KEY"）
///
/// # 返回
/// 配好 rustls 默认 cipher suites + TLS 1.2/1.3 的 `TlsAcceptor`。
///
/// # 失败
/// - cert / key 文件不可读
/// - PEM 解析错（malformed / 空文件 / 无相关 block）
/// - rustls `ServerConfig::with_single_cert` 拒收（cert/key 不匹配 / cert 损坏）
///
/// # spec / 教学
/// - 用 `ServerConfig::builder().with_no_client_auth()` — 不验 client cert
/// - 默认 cipher suites（TLS 1.2 + 1.3 standard set）；TLS 1.3 spec 锚 RFC 8446
/// - NVMe-oF spec § 8.13 要求 TLS 1.3 + 特定 cipher suites；本教学版不强制 1.3
///   only，方便 nvme-cli 老版本可连上。生产应 `.with_protocol_versions(&[
///   &rustls::version::TLS13])`
pub fn build_acceptor_from_pem(
    cert_path: impl AsRef<Path>,
    key_path: impl AsRef<Path>,
) -> anyhow::Result<TlsAcceptor> {
    let cert_path = cert_path.as_ref();
    let key_path = key_path.as_ref();

    // 读 cert PEM —— rustls_pemfile::certs 返 iter，失败 stop early。
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

    // 读 key PEM —— 接受 PKCS#8 / PKCS#1 / SEC1 三种 label。
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

    // 显式装 ring provider —— V-followup-tls-2 feature gating 时 default-features
    // off，必须 install_default 一次。多次调 install_default 第二次起返 Err，
    // 用 `.ok()` 容忍（其它测试 / bin 第一次启动可能已装）。
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("rustls ServerConfig 构建失败（cert/key 不匹配？）")?;
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
        // TlsAcceptor 无公开字段；构造成功本身即覆盖 happy path。
        drop(acceptor);
    }

    #[test]
    fn vt_tls_2_acceptor_rejects_missing_key_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(tmp.path().join("server.pem"), cert.cert.pem()).unwrap();
        // 故意不写 server.key
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
        // 写一个非 PEM 的乱字节当 key
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
        // cert 文件空
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
}
