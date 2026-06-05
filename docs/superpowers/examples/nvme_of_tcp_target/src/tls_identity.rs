// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-auth-2** — TLS leaf cert 提取 host identity（SAN URI/DNS
//! + CN）用于 NQN ↔ TLS identity binding。
//!
//! 设计：mTLS handshake 完成后，`TlsStream` 内含 peer cert chain。bin 端拿
//! chain 的 **leaf**（chain[0]）调本 module `extract_host_identities` 解出全部
//! candidate identity 字符串 → 作 `AsyncSession.bound_host_identities` 注入。
//! Connect 时校验 `hostnqn` 必须 ∈ identities。
//!
//! identity 来源覆盖（spec section 8.13 / RFC 6125 best practice）:
//! 1. SAN `URI`（推荐：URI 格式 `nqn.xxxx.xxx`）
//! 2. SAN `DNS`（常见 ops 习惯：把 NQN 当 DNS name）
//! 3. CN（fallback；RFC 2818 deprecated 但 nvme-cli 等老 client 仍出 CN）
//!
//! 仍坚持 `#![forbid(unsafe_code)]`：x509-parser 内部 nom + std；本 module
//! 0 unsafe block。

use anyhow::{Context as _, anyhow};
use rustls::pki_types::CertificateDer;
use std::collections::HashSet;
use x509_parser::prelude::*;

/// 从 leaf cert（chain[0]）DER 提取全部 candidate host identity 字符串。
///
/// # 来源
/// - SAN URI（每条都加入；spec section 8.13 推荐）
/// - SAN DNS（每条都加入）
/// - Subject CN（fallback）
///
/// # 返回
/// 至少 1 条 identity 即返 Ok(set)；leaf 解析失败返 Err（让上层拒 conn）。
/// 重复 identity 自动去重（HashSet）。
pub fn extract_host_identities(leaf_cert: &CertificateDer<'_>) -> anyhow::Result<HashSet<String>> {
    let (_, cert) = X509Certificate::from_der(leaf_cert.as_ref())
        .context("x509-parser: leaf cert 解析失败（DER 非法 / 截断）")?;
    let mut identities: HashSet<String> = HashSet::new();

    // SAN extension 提取
    if let Ok(Some(san_ext)) = cert.subject_alternative_name() {
        for name in &san_ext.value.general_names {
            match name {
                GeneralName::URI(uri) => {
                    identities.insert(uri.to_string());
                }
                GeneralName::DNSName(dns) => {
                    identities.insert(dns.to_string());
                }
                _ => {}
            }
        }
    }

    // Subject CN（fallback）— 遍历 RDN 找 CN attr
    for cn in cert.subject().iter_common_name() {
        if let Ok(s) = cn.as_str() {
            identities.insert(s.to_string());
        }
    }

    if identities.is_empty() {
        return Err(anyhow!(
            "leaf cert 不含可用 host identity（无 SAN URI/DNS 也无 Subject CN）"
        ));
    }
    Ok(identities)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rcgen_cert_with(
        san_dns: Vec<String>,
        san_uris: Vec<String>,
        cn: Option<String>,
    ) -> CertificateDer<'static> {
        use rcgen::{CertificateParams, KeyPair};
        let mut sans: Vec<rcgen::SanType> = san_dns
            .into_iter()
            .map(|d| rcgen::SanType::DnsName(d.try_into().unwrap()))
            .collect();
        for uri in san_uris {
            sans.push(rcgen::SanType::URI(uri.try_into().unwrap()));
        }
        let mut params = if sans.is_empty() {
            CertificateParams::new(vec![cn.clone().unwrap_or_else(|| "x".into())]).unwrap()
        } else {
            // CertificateParams::new 接 DNS only；其他 SAN 通过 subject_alt_names 字段加
            let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
            p.subject_alt_names = sans;
            p
        };
        if let Some(cn) = cn {
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, cn);
        }
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        cert.der().clone()
    }

    #[test]
    fn vt_auth2_extract_san_uri() {
        let cert = rcgen_cert_with(
            vec![],
            vec!["nqn.2014-08.org.nvmexpress:uuid:host-a".to_string()],
            Some("client-a".to_string()),
        );
        let ids = extract_host_identities(&cert).unwrap();
        assert!(
            ids.contains("nqn.2014-08.org.nvmexpress:uuid:host-a"),
            "应抽出 SAN URI"
        );
        assert!(ids.contains("client-a"), "应也抽出 CN");
    }

    #[test]
    fn vt_auth2_extract_san_dns() {
        let cert = rcgen_cert_with(
            vec!["nqn.host-b".to_string()],
            vec![],
            Some("host-b".to_string()),
        );
        let ids = extract_host_identities(&cert).unwrap();
        assert!(ids.contains("nqn.host-b"), "应抽出 SAN DNS");
        assert!(ids.contains("host-b"), "应抽出 CN");
    }

    #[test]
    fn vt_auth2_extract_cn_only() {
        let cert = rcgen_cert_with(
            vec!["fallback-dns".to_string()],
            vec![],
            Some("just-cn".to_string()),
        );
        let ids = extract_host_identities(&cert).unwrap();
        assert!(ids.contains("just-cn"));
        assert!(ids.contains("fallback-dns"));
    }
}
