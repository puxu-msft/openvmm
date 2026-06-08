// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-tls-psk (TP-8011)** — NVMe TLS PSK 派生层 (deterministic
//! crypto only)。
//!
//! 本模块实现 NVMe Base Spec § 8.13 + TP-8011 + Linux kernel
//! `drivers/nvme/common/auth.c` 里的 PSK 派生：
//!
//! 1. **PSK Digest** (`nvme_auth_generate_digest`):
//!    ```text
//!    digest_bin = HMAC-SHA{256|384}(key = retained_psk,
//!                                   data = hostnqn || " " || subsysnqn || " NVMe-over-Fabrics")
//!    digest_str = base64(digest_bin)   // 44 chars for SHA-256, 64 for SHA-384
//!    ```
//!
//! 2. **TLS PSK** (`nvme_auth_derive_tls_psk`):
//!    ```text
//!    salt = [0u8; hash_len]
//!    PRK = HKDF-Extract(salt, retained_psk)
//!    context = format!("{:02} {}", hmac_id, digest_str)
//!    tls_psk = HKDF-Expand-Label(PRK, "nvme-tls-psk", context, hash_len)
//!    ```
//!    其中 HKDF-Expand-Label 遵 RFC 8446 § 7.1：
//!    ```text
//!    HkdfLabel = struct {
//!      uint16 length = hash_len;                       // 2 byte big-endian
//!      opaque label<7..255> = "tls13 " + "nvme-tls-psk"; // 1B len + bytes
//!      opaque context<0..255> = context;               // 1B len + bytes
//!    }
//!    info = HkdfLabel
//!    out = HMAC(PRK, info || 0x01)[..hash_len]
//!    ```
//!
//! 3. **PSK Identity** (`nvme_tls_psk_refresh`):
//!    ```text
//!    "NVMe%u%c%02u %s %s [%s]"
//!     |   | |   |  |  |   |
//!     | ver(0/1)
//!     |     'G' generated | 'R' retained
//!     |       hmac_id 01/02
//!     |          hostnqn
//!     |             subsysnqn
//!     |                [digest_str when ver==1]
//!    ```
//!    版本 0 = retained PSK + 不带 digest，识别字符串例：
//!    `"NVMe0R01 nqn.host nqn.subsys"`。
//!    版本 1 = generated（DH-HMAC-CHAP 通过后派生） + 带 digest，例：
//!    `"NVMe1G01 nqn.host nqn.subsys <digest>"`。
//!
//! **教学/生产边界**：
//! - 本模块仅 deterministic crypto；产 raw TLS PSK 字节 + PSK identity 字符串。
//! - 实际把 PSK 注入 TLS 1.3 ClientHello / ServerHello 需要 rustls 提供
//!   external-PSK API — rustls 0.23 **尚无公开 API**（issue 上有 work-in-progress
//!   PR）。Linux nvme-cli `--tls` 路径要真 interop，需 rustls upstream 完成
//!   `ExternalPsk` trait，或本仓 fork rustls 加 PSK callback。
//! - 本模块的 lib 测试覆盖 deterministic 派生 — 算法正确就锁定，不需要等
//!   rustls upstream。
//!
//! **TODO(kernel-CI vector, reviewer L-4)**：当前测试是 self-consistent —
//! 算法对自身 deterministic / 字段敏感性正确，但没有真 Linux nvmet/nvme-cli
//! 输出的 `(retained_psk, hostnqn, subsysnqn, expected_digest, expected_tls_psk)`
//! 五元组 anchor。`nvme_auth_derive_tls_psk` 在 kernel 是 EXPORT_SYMBOL_GPL，
//! 出 tree probe 模块可 dump；待用户跑 Linux interop 时回填。
//!
//! 仍坚持 `#![forbid(unsafe_code)]`。

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::{Sha256, Sha384};

/// HMAC-SHA256 输出长度
pub const SHA256_LEN: usize = 32;
/// HMAC-SHA384 输出长度
pub const SHA384_LEN: usize = 48;

/// NVMe TLS PSK 支持的 hash 算法 ID (spec § 8.13 / Linux NVME_AUTH_HASH_*)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsPskHash {
    /// HMAC-SHA-256 (hmac_id=1, 32 byte)
    Sha256,
    /// HMAC-SHA-384 (hmac_id=2, 48 byte)
    Sha384,
}

impl TlsPskHash {
    /// NVMe spec hmac_id (1 / 2)
    pub fn hmac_id(&self) -> u8 {
        match self {
            TlsPskHash::Sha256 => 1,
            TlsPskHash::Sha384 => 2,
        }
    }
    /// 输出 byte 长度 (= hash digest size)。
    ///
    /// 命名为 `output_len` 而非 `len` 是有意的：避免与集合语义的 `.len()`
    /// 混淆，也回避 clippy::len_without_is_empty (hash 输出永远 ≠ 0)。
    pub fn output_len(&self) -> usize {
        match self {
            TlsPskHash::Sha256 => SHA256_LEN,
            TlsPskHash::Sha384 => SHA384_LEN,
        }
    }
    /// 兼容别名 — 旧调用方依赖 `.len()`，等迁完移除。
    #[deprecated(note = "用 output_len() 取代")]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.output_len()
    }
}

/// NVMe TLS PSK 版本：retained (用户配置长期) vs generated (DH-HMAC-CHAP 推出)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsPskVersion {
    /// 版本 0: retained PSK，identity 不带 digest
    Ver0Retained,
    /// 版本 1: generated PSK (DH-HMAC-CHAP 后派生)，identity 带 digest
    Ver1Generated,
}

impl TlsPskVersion {
    fn version_digit(&self) -> u8 {
        match self {
            TlsPskVersion::Ver0Retained => 0,
            TlsPskVersion::Ver1Generated => 1,
        }
    }
    fn type_char(&self) -> char {
        match self {
            TlsPskVersion::Ver0Retained => 'R',
            TlsPskVersion::Ver1Generated => 'G',
        }
    }
}

/// **TP-8011 step 1** — 生 PSK digest (Base64-encoded HMAC over NQNs)。
///
/// 对应 Linux `nvme_auth_generate_digest`：
/// ```text
/// digest = HMAC(psk, hostnqn || " " || subsysnqn || " NVMe-over-Fabrics")
/// return base64(digest)
/// ```
///
/// **调用方责任 (reviewer L-3)**：本函数不校验 NQN 合法性 — 调用方必须先用
/// 上层 NQN allowlist / length check 确保 `hostnqn` / `subsysnqn` 非空且符合
/// NVMe NQN 规范 (≤ 223 char, 起头 "nqn.")。Kernel `nvme_auth_generate_digest`
/// 有 `WARN_ON(!subsysnqn || !hostnqn)` 但我们这里保持纯函数语义。
pub fn generate_psk_digest(
    psk: &[u8],
    hostnqn: &str,
    subsysnqn: &str,
    hash: TlsPskHash,
) -> anyhow::Result<String> {
    let mut input = Vec::with_capacity(hostnqn.len() + subsysnqn.len() + 22);
    input.extend_from_slice(hostnqn.as_bytes());
    input.push(b' ');
    input.extend_from_slice(subsysnqn.as_bytes());
    input.extend_from_slice(b" NVMe-over-Fabrics");
    let digest = match hash {
        TlsPskHash::Sha256 => {
            let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(psk)
                .context("HMAC-SHA256 init 失败 (key 长度异常)")?;
            mac.update(&input);
            mac.finalize().into_bytes().to_vec()
        }
        TlsPskHash::Sha384 => {
            let mut mac = <Hmac<Sha384> as Mac>::new_from_slice(psk)
                .context("HMAC-SHA384 init 失败 (key 长度异常)")?;
            mac.update(&input);
            mac.finalize().into_bytes().to_vec()
        }
    };
    Ok(base64::engine::general_purpose::STANDARD.encode(&digest))
}

/// HKDF-Extract: PRK = HMAC(salt, IKM)
fn hkdf_extract(salt: &[u8], ikm: &[u8], hash: TlsPskHash) -> Vec<u8> {
    match hash {
        TlsPskHash::Sha256 => {
            let mut mac =
                <Hmac<Sha256> as Mac>::new_from_slice(salt).expect("HMAC accepts any key length");
            mac.update(ikm);
            mac.finalize().into_bytes().to_vec()
        }
        TlsPskHash::Sha384 => {
            let mut mac =
                <Hmac<Sha384> as Mac>::new_from_slice(salt).expect("HMAC accepts any key length");
            mac.update(ikm);
            mac.finalize().into_bytes().to_vec()
        }
    }
}

/// HKDF-Expand-Label per RFC 8446 § 7.1：构造 HkdfLabel + 单 block 输出。
///
/// info = uint16(length) || opaque label || opaque context
///   - label = "tls13 " + raw_label
///   - opaque = 1 byte length prefix + bytes
///
/// out = HMAC(PRK, info || 0x01)[..length]
///
/// 仅支持单 block 输出 (length <= hash_len)；TP-8011 PSK 派生只需要 1 block。
fn hkdf_expand_label(
    prk: &[u8],
    raw_label: &str,
    context: &[u8],
    length: usize,
    hash: TlsPskHash,
) -> anyhow::Result<Vec<u8>> {
    if length > hash.output_len() {
        bail!(
            "hkdf_expand_label: length {length} 超过单 block hash_len {}",
            hash.output_len()
        );
    }
    let mut full_label = Vec::with_capacity(7 + raw_label.len());
    full_label.extend_from_slice(b"tls13 ");
    full_label.extend_from_slice(raw_label.as_bytes());
    if full_label.len() > 255 {
        bail!("hkdf_expand_label: label 过长 ({} > 255)", full_label.len());
    }
    if context.len() > 255 {
        bail!("hkdf_expand_label: context 过长 ({} > 255)", context.len());
    }
    let len_u16 =
        u16::try_from(length).map_err(|_| anyhow!("hkdf length {length} 不能放入 u16"))?;
    let mut info = Vec::with_capacity(2 + 1 + full_label.len() + 1 + context.len());
    info.extend_from_slice(&len_u16.to_be_bytes());
    info.push(full_label.len() as u8);
    info.extend_from_slice(&full_label);
    info.push(context.len() as u8);
    info.extend_from_slice(context);

    // HMAC(PRK, info || 0x01)
    let mut block = info.clone();
    block.push(0x01);
    let out_full = match hash {
        TlsPskHash::Sha256 => {
            let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(prk)
                .context("HKDF-Expand HMAC-SHA256 init 失败")?;
            mac.update(&block);
            mac.finalize().into_bytes().to_vec()
        }
        TlsPskHash::Sha384 => {
            let mut mac = <Hmac<Sha384> as Mac>::new_from_slice(prk)
                .context("HKDF-Expand HMAC-SHA384 init 失败")?;
            mac.update(&block);
            mac.finalize().into_bytes().to_vec()
        }
    };
    Ok(out_full[..length].to_vec())
}

/// **TP-8011 step 2** — 派生 TLS PSK (raw bytes)。
///
/// 对应 Linux `nvme_auth_derive_tls_psk`：
/// ```text
/// salt = [0; hash_len]
/// PRK = HKDF-Extract(salt, retained_psk)
/// context = sprintf("%02d %s", hmac_id, digest)
/// tls_psk = HKDF-Expand-Label(PRK, "nvme-tls-psk", context, hash_len)
/// ```
pub fn derive_tls_psk(
    retained_psk: &[u8],
    digest_str: &str,
    hash: TlsPskHash,
) -> anyhow::Result<Vec<u8>> {
    let salt = vec![0u8; hash.output_len()];
    let prk = hkdf_extract(&salt, retained_psk, hash);
    let context = format!("{:02} {}", hash.hmac_id(), digest_str);
    hkdf_expand_label(
        &prk,
        "nvme-tls-psk",
        context.as_bytes(),
        hash.output_len(),
        hash,
    )
}

/// **TP-8011 step 3** — 构造 PSK identity (TLS ClientHello / ServerHello 用)。
///
/// 对应 Linux `nvme_tls_psk_refresh` (ver=1) / `nvme_tls_psk_lookup` (ver=0):
/// ```text
/// ver=0 retained: "NVMe0R%02d %s %s"            (hmac_id, hostnqn, subsysnqn)
/// ver=1 generated: "NVMe1G%02d %s %s %s"        (hmac_id, hostnqn, subsysnqn, digest)
/// ```
pub fn build_psk_identity(
    version: TlsPskVersion,
    hash: TlsPskHash,
    hostnqn: &str,
    subsysnqn: &str,
    digest_str: Option<&str>,
) -> anyhow::Result<String> {
    let id = match (version, digest_str) {
        (TlsPskVersion::Ver0Retained, _) => format!(
            "NVMe{ver}{ty}{hmac:02} {host} {sub}",
            ver = version.version_digit(),
            ty = version.type_char(),
            hmac = hash.hmac_id(),
            host = hostnqn,
            sub = subsysnqn,
        ),
        (TlsPskVersion::Ver1Generated, Some(digest)) => format!(
            "NVMe{ver}{ty}{hmac:02} {host} {sub} {digest}",
            ver = version.version_digit(),
            ty = version.type_char(),
            hmac = hash.hmac_id(),
            host = hostnqn,
            sub = subsysnqn,
            digest = digest,
        ),
        (TlsPskVersion::Ver1Generated, None) => {
            bail!("Ver1Generated 必须提供 digest")
        }
    };
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vt_tlspsk_hash_id_and_len_match_spec() {
        assert_eq!(TlsPskHash::Sha256.hmac_id(), 1);
        assert_eq!(TlsPskHash::Sha256.output_len(), 32);
        assert_eq!(TlsPskHash::Sha384.hmac_id(), 2);
        assert_eq!(TlsPskHash::Sha384.output_len(), 48);
    }

    #[test]
    fn vt_tlspsk_generate_psk_digest_sha256_format() {
        // 已知 inputs
        let psk = [0x11u8; 32];
        let digest = generate_psk_digest(&psk, "nqn.host", "nqn.subsys", TlsPskHash::Sha256)
            .expect("digest 应能生");
        // SHA-256 -> 32 byte HMAC -> base64 = 44 char (含尾 '=')
        assert_eq!(digest.len(), 44, "Base64 of 32 byte = 44 chars");
        // deterministic
        let d2 = generate_psk_digest(&psk, "nqn.host", "nqn.subsys", TlsPskHash::Sha256).unwrap();
        assert_eq!(digest, d2);
    }

    #[test]
    fn vt_tlspsk_generate_psk_digest_sha384_format() {
        let psk = [0x22u8; 48];
        let digest = generate_psk_digest(&psk, "nqn.host", "nqn.subsys", TlsPskHash::Sha384)
            .expect("digest 应能生");
        // SHA-384 -> 48 byte HMAC -> base64 = 64 char
        assert_eq!(digest.len(), 64, "Base64 of 48 byte = 64 chars");
    }

    #[test]
    fn vt_tlspsk_digest_changes_with_nqn() {
        let psk = [0x33u8; 32];
        let d1 = generate_psk_digest(&psk, "nqn.h1", "nqn.s", TlsPskHash::Sha256).unwrap();
        let d2 = generate_psk_digest(&psk, "nqn.h2", "nqn.s", TlsPskHash::Sha256).unwrap();
        let d3 = generate_psk_digest(&psk, "nqn.h1", "nqn.s2", TlsPskHash::Sha256).unwrap();
        assert_ne!(d1, d2, "hostnqn 变 → digest 变");
        assert_ne!(d1, d3, "subsysnqn 变 → digest 变");
    }

    #[test]
    fn vt_tlspsk_derive_tls_psk_sha256_length_and_determinism() {
        let psk = [0x44u8; 32];
        let digest =
            generate_psk_digest(&psk, "nqn.host", "nqn.subsys", TlsPskHash::Sha256).unwrap();
        let tls_psk = derive_tls_psk(&psk, &digest, TlsPskHash::Sha256).unwrap();
        assert_eq!(tls_psk.len(), 32, "TLS PSK SHA-256 应 32 byte");
        let tls_psk2 = derive_tls_psk(&psk, &digest, TlsPskHash::Sha256).unwrap();
        assert_eq!(tls_psk, tls_psk2, "派生必须 deterministic");
    }

    #[test]
    fn vt_tlspsk_derive_tls_psk_sha384_length() {
        let psk = [0x55u8; 48];
        let digest =
            generate_psk_digest(&psk, "nqn.host", "nqn.subsys", TlsPskHash::Sha384).unwrap();
        let tls_psk = derive_tls_psk(&psk, &digest, TlsPskHash::Sha384).unwrap();
        assert_eq!(tls_psk.len(), 48, "TLS PSK SHA-384 应 48 byte");
    }

    #[test]
    fn vt_tlspsk_derive_changes_with_digest() {
        let psk = [0x66u8; 32];
        let p1 = derive_tls_psk(&psk, "digestA", TlsPskHash::Sha256).unwrap();
        let p2 = derive_tls_psk(&psk, "digestB", TlsPskHash::Sha256).unwrap();
        assert_ne!(p1, p2, "digest 变 → TLS PSK 变");
    }

    #[test]
    fn vt_tlspsk_identity_ver0_retained_format() {
        let id = build_psk_identity(
            TlsPskVersion::Ver0Retained,
            TlsPskHash::Sha256,
            "nqn.host",
            "nqn.subsys",
            None,
        )
        .unwrap();
        assert_eq!(id, "NVMe0R01 nqn.host nqn.subsys");
    }

    #[test]
    fn vt_tlspsk_identity_ver0_retained_sha384_format() {
        let id = build_psk_identity(
            TlsPskVersion::Ver0Retained,
            TlsPskHash::Sha384,
            "nqn.host",
            "nqn.subsys",
            None,
        )
        .unwrap();
        assert_eq!(id, "NVMe0R02 nqn.host nqn.subsys");
    }

    #[test]
    fn vt_tlspsk_identity_ver1_generated_with_digest() {
        let id = build_psk_identity(
            TlsPskVersion::Ver1Generated,
            TlsPskHash::Sha256,
            "nqn.h",
            "nqn.s",
            Some("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+/abcdefgh"),
        )
        .unwrap();
        assert_eq!(
            id,
            "NVMe1G01 nqn.h nqn.s ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+/abcdefgh"
        );
    }

    #[test]
    fn vt_tlspsk_identity_ver1_without_digest_rejected() {
        let err = build_psk_identity(
            TlsPskVersion::Ver1Generated,
            TlsPskHash::Sha256,
            "nqn.h",
            "nqn.s",
            None,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("Ver1Generated"));
    }

    #[test]
    fn vt_tlspsk_hkdf_expand_label_known_vector_rfc_format() {
        // 锁定 HKDF-Expand-Label 实现 — info 结构 anchor。
        // 我们不能从 RFC 直接拿到 NVMe TLS 向量 (kernel CI 才有)，所以用
        // self-consistent 锁定 + 不变量。
        let prk = vec![0x77u8; 32];
        // 1) 输出长度
        let out =
            hkdf_expand_label(&prk, "nvme-tls-psk", b"01 digest", 32, TlsPskHash::Sha256).unwrap();
        assert_eq!(out.len(), 32);
        // 2) deterministic
        let out2 =
            hkdf_expand_label(&prk, "nvme-tls-psk", b"01 digest", 32, TlsPskHash::Sha256).unwrap();
        assert_eq!(out, out2);
        // 3) label 变 → 输出变
        let out3 =
            hkdf_expand_label(&prk, "other-label", b"01 digest", 32, TlsPskHash::Sha256).unwrap();
        assert_ne!(out, out3);
        // 4) context 变 → 输出变
        let out4 =
            hkdf_expand_label(&prk, "nvme-tls-psk", b"02 digest", 32, TlsPskHash::Sha256).unwrap();
        assert_ne!(out, out4);
        // 5) length 超 hash_len 应 reject (单 block only)
        let err =
            hkdf_expand_label(&prk, "nvme-tls-psk", b"x", 64, TlsPskHash::Sha256).unwrap_err();
        assert!(format!("{err:#}").contains("超过单 block"));
    }

    /// 端到端 self-consistent: digest → derive → identity 三步链路
    #[test]
    fn vt_tlspsk_end_to_end_self_consistent() {
        let retained_psk = vec![0x88u8; 32];
        let hostnqn = "nqn.2014-08.org.nvmexpress:uuid:test-host";
        let subsysnqn = "nqn.2014-08.org.nvmexpress:teaching:disk";

        let digest =
            generate_psk_digest(&retained_psk, hostnqn, subsysnqn, TlsPskHash::Sha256).unwrap();
        let tls_psk = derive_tls_psk(&retained_psk, &digest, TlsPskHash::Sha256).unwrap();
        assert_eq!(tls_psk.len(), 32);
        let identity = build_psk_identity(
            TlsPskVersion::Ver1Generated,
            TlsPskHash::Sha256,
            hostnqn,
            subsysnqn,
            Some(&digest),
        )
        .unwrap();
        // identity 应以 "NVMe1G01 " 起头
        assert!(identity.starts_with("NVMe1G01 "));
        // 应含 hostnqn + subsysnqn + digest
        assert!(identity.contains(hostnqn));
        assert!(identity.contains(subsysnqn));
        assert!(identity.contains(&digest));
    }

    /// 端到端 SHA-384 — 锁 dispatch table 第二臂 (reviewer L-2)
    #[test]
    fn vt_tlspsk_end_to_end_self_consistent_sha384() {
        let retained_psk = vec![0xAAu8; 48];
        let hostnqn = "nqn.2014-08.org.nvmexpress:uuid:test-host-384";
        let subsysnqn = "nqn.2014-08.org.nvmexpress:teaching:disk-384";

        let digest =
            generate_psk_digest(&retained_psk, hostnqn, subsysnqn, TlsPskHash::Sha384).unwrap();
        assert_eq!(digest.len(), 64, "SHA-384 digest base64 = 64 char");
        let tls_psk = derive_tls_psk(&retained_psk, &digest, TlsPskHash::Sha384).unwrap();
        assert_eq!(tls_psk.len(), 48, "SHA-384 TLS PSK = 48 byte");
        let identity = build_psk_identity(
            TlsPskVersion::Ver1Generated,
            TlsPskHash::Sha384,
            hostnqn,
            subsysnqn,
            Some(&digest),
        )
        .unwrap();
        assert!(identity.starts_with("NVMe1G02 "), "SHA-384 → hmac_id=02");
        assert!(identity.contains(&digest));
    }
}
