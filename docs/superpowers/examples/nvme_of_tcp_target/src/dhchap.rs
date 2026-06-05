// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-dhchap-1** — DH-HMAC-CHAP 教学版构件（HMAC-only，spec
//! section 8.13.5 简化）。
//!
//! **目标**：把 spec 的 4-message DH-HMAC-CHAP 协议拆出可独立测试的 HMAC
//! 构件（challenge 生成 / response 计算 / 校验），让后续 phase
//! (V-followup-dhchap-2) 把这些 building block 串到 AUTH_SEND / AUTH_RECV PDU
//! 状态机（fabric.rs `fctype::AUTH_SEND=0x05` / `AUTH_RECV=0x06`）。
//!
//! **当前 phase 边界**：
//! - ✅ HMAC-SHA256 challenge / response 计算 + 验证（spec section 8.13.5.2）
//! - ✅ secret 解码（HEX string -> raw bytes，长度校验）
//! - ❌ **不** 做 DH 增量（spec section 8.13.5.4 的 ephemeral DH 留生产版；
//!   教学版仅 HMAC-only mode = T_REQ.dhgid=0）
//! - ❌ **不** 做完整 4-message 状态机 wire（留 V-followup-dhchap-2）
//! - ❌ **不** 做 mutual auth（host -> target 单向；留 V-followup-dhchap-3）
//!
//! **教学/生产边界警示**：HMAC-only 模式相对于完整 DH-HMAC-CHAP 的弱点：
//! - 中间人重放保护减弱（DH 提供 forward-secrecy；HMAC-only 不提供）
//! - 生产环境**必须**叠加 TLS（V-followup-tls-3+）防 challenge 泄漏
//!
//! 仍坚持 `#![forbid(unsafe_code)]`：hmac/sha2/rand/hex 全 safe，本 module
//! 0 unsafe block。

use anyhow::{Context as _, anyhow};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use std::collections::HashMap;

/// HMAC-SHA256 输出长度。
pub const HMAC_SHA256_LEN: usize = 32;

/// CHAP challenge nonce 长度（spec section 8.13.5.2 推荐 ≥ 16 字节）。
pub const CHAP_CHALLENGE_LEN: usize = 32;

/// secret 最小长度（HEX 解码后；教学版要求 ≥ 32 字节 = 256-bit entropy）。
pub const CHAP_SECRET_MIN_LEN: usize = 32;

/// 把 hex string secret 解码为 raw bytes，并做长度校验。
///
/// # 参数
/// - `hex_secret`: 形如 `"ab12cd34..."`（不区分大小写，无前缀）
///
/// # 返回
/// 解码后的 bytes（≥ [`CHAP_SECRET_MIN_LEN`]）；不合法返 Err。
pub fn decode_secret(hex_secret: &str) -> anyhow::Result<Vec<u8>> {
    let bytes = hex::decode(hex_secret).context("CHAP secret 不是合法 hex string")?;
    if bytes.len() < CHAP_SECRET_MIN_LEN {
        return Err(anyhow!(
            "CHAP secret 长度 {} 字节 < 最小 {} 字节",
            bytes.len(),
            CHAP_SECRET_MIN_LEN
        ));
    }
    Ok(bytes)
}

/// 生成一个 random CHAP challenge nonce（spec section 8.13.5.2）。
///
/// 用 `rand::rngs::OsRng` 直接从 OS entropy 抽（教学版接受 cost；生产可换
/// `ring::rand` 或 thread-local CSPRNG）。
pub fn generate_challenge() -> [u8; CHAP_CHALLENGE_LEN] {
    let mut buf = [0u8; CHAP_CHALLENGE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf
}

/// 计算 CHAP response = HMAC-SHA256(secret, challenge || hostnqn || subnqn)。
///
/// spec section 8.13.5.3 要求 response 把 host / subsystem identity 作为
/// transcript 一部分以防 cross-protocol replay；本教学版用 `||` 简单
/// 串接（生产应按 spec wire format 串）。
pub fn compute_response(
    secret: &[u8],
    challenge: &[u8],
    hostnqn: &str,
    subnqn: &str,
) -> [u8; HMAC_SHA256_LEN] {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(challenge);
    mac.update(hostnqn.as_bytes());
    mac.update(subnqn.as_bytes());
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; HMAC_SHA256_LEN];
    out.copy_from_slice(&result);
    out
}

/// constant-time verify response = expected HMAC（防 timing leak）。
pub fn verify_response(actual: &[u8; HMAC_SHA256_LEN], expected: &[u8; HMAC_SHA256_LEN]) -> bool {
    // hmac::Mac::verify_slice 内部 ConstantTimeEq；这里手摇 trivial xor + zero check
    // 等价（且不依赖 hmac::Mac 重建）；保 constant-time。
    let mut diff = 0u8;
    for (a, b) in actual.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// CHAP secret store：host NQN -> raw secret bytes。
///
/// bin 端从 CLI `--host-secret <NQN>=<HEX>` 重复 build；session 端在 AUTH_SEND
/// 收到 host CHAP-1 时查表，缺 host NQN → reject。
#[derive(Debug, Default, Clone)]
pub struct ChapSecretStore {
    secrets: HashMap<String, Vec<u8>>,
}

impl ChapSecretStore {
    /// 构造空 store。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册 `(host_nqn, secret_bytes)`；重复 NQN 后注册覆盖前面。
    pub fn insert(&mut self, host_nqn: String, secret: Vec<u8>) {
        self.secrets.insert(host_nqn, secret);
    }

    /// 查找 host NQN 对应 secret；None = 未注册。
    pub fn get(&self, host_nqn: &str) -> Option<&[u8]> {
        self.secrets.get(host_nqn).map(|v| v.as_slice())
    }

    /// 是否空 store（无任何 NQN 注册）。
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// 当前注册的 host NQN 数。
    pub fn len(&self) -> usize {
        self.secrets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vt_dhchap_decode_secret_happy_path() {
        let hex_str = "0123456789abcdef".repeat(4); // 64 hex chars = 32 bytes
        let bytes = decode_secret(&hex_str).unwrap();
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn vt_dhchap_decode_secret_rejects_too_short() {
        let hex_str = "abcd".repeat(4); // 16 bytes < 32
        let err = decode_secret(&hex_str).unwrap_err();
        assert!(format!("{err:#}").contains("< 最小 32 字节"));
    }

    #[test]
    fn vt_dhchap_decode_secret_rejects_non_hex() {
        let err = decode_secret("not hex at all").unwrap_err();
        assert!(format!("{err:#}").contains("不是合法 hex"));
    }

    #[test]
    fn vt_dhchap_challenge_is_random_per_call() {
        let c1 = generate_challenge();
        let c2 = generate_challenge();
        assert_ne!(c1, c2, "两次 challenge 应不同 (OsRng 应足够随机)");
    }

    #[test]
    fn vt_dhchap_compute_response_deterministic_for_same_inputs() {
        let secret = vec![0xABu8; 32];
        let challenge = [0x42u8; 32];
        let r1 = compute_response(&secret, &challenge, "nqn.h", "nqn.s");
        let r2 = compute_response(&secret, &challenge, "nqn.h", "nqn.s");
        assert_eq!(r1, r2, "相同输入应产相同 HMAC");
    }

    #[test]
    fn vt_dhchap_compute_response_changes_with_challenge() {
        let secret = vec![0xABu8; 32];
        let c1 = [0x42u8; 32];
        let mut c2 = c1;
        c2[0] = 0x43;
        let r1 = compute_response(&secret, &c1, "h", "s");
        let r2 = compute_response(&secret, &c2, "h", "s");
        assert_ne!(r1, r2);
    }

    #[test]
    fn vt_dhchap_compute_response_changes_with_identity() {
        let secret = vec![0xABu8; 32];
        let challenge = [0x42u8; 32];
        let r1 = compute_response(&secret, &challenge, "h1", "s");
        let r2 = compute_response(&secret, &challenge, "h2", "s");
        assert_ne!(r1, r2, "hostnqn 不同应产不同 HMAC (防 cross-host replay)");
    }

    #[test]
    fn vt_dhchap_verify_response_accepts_correct() {
        let secret = vec![0xABu8; 32];
        let challenge = [0x42u8; 32];
        let response = compute_response(&secret, &challenge, "h", "s");
        let expected = compute_response(&secret, &challenge, "h", "s");
        assert!(verify_response(&response, &expected));
    }

    #[test]
    fn vt_dhchap_verify_response_rejects_wrong() {
        let secret = vec![0xABu8; 32];
        let challenge = [0x42u8; 32];
        let response = compute_response(&secret, &challenge, "h", "s");
        let evil_secret = vec![0xCCu8; 32];
        let wrong = compute_response(&evil_secret, &challenge, "h", "s");
        assert!(!verify_response(&response, &wrong));
    }

    #[test]
    fn vt_dhchap_secret_store_lookup() {
        let mut store = ChapSecretStore::new();
        store.insert("nqn.h-1".to_string(), vec![0xAAu8; 32]);
        store.insert("nqn.h-2".to_string(), vec![0xBBu8; 32]);
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("nqn.h-1"), Some([0xAAu8; 32].as_slice()));
        assert_eq!(store.get("nqn.h-2"), Some([0xBBu8; 32].as_slice()));
        assert!(store.get("nqn.unknown").is_none());
    }

    #[test]
    fn vt_dhchap_full_round_trip_succeeds() {
        // 模拟完整 host <-> target CHAP exchange (无 wire format，仅算法)
        let shared_secret = vec![0x77u8; 32];
        let mut store = ChapSecretStore::new();
        store.insert("nqn.host-a".to_string(), shared_secret.clone());

        // target -> host CHAP-1: challenge (省略 wire encode)
        let target_challenge = generate_challenge();

        // host -> target CHAP-2: compute response 用同样 secret
        let host_response = compute_response(
            &shared_secret,
            &target_challenge,
            "nqn.host-a",
            "nqn.subsys",
        );

        // target 接收 host_response → 查 secret → 算 expected → verify
        let secret = store.get("nqn.host-a").expect("host 已注册");
        let expected = compute_response(secret, &target_challenge, "nqn.host-a", "nqn.subsys");
        assert!(
            verify_response(&host_response, &expected),
            "完整 round-trip 应通过"
        );
    }

    #[test]
    fn vt_dhchap_full_round_trip_fails_when_host_uses_wrong_secret() {
        let real_secret = vec![0x77u8; 32];
        let fake_secret = vec![0x88u8; 32];
        let mut store = ChapSecretStore::new();
        store.insert("nqn.host-a".to_string(), real_secret);

        let target_challenge = generate_challenge();
        let host_response_with_fake =
            compute_response(&fake_secret, &target_challenge, "nqn.host-a", "nqn.subsys");
        let secret = store.get("nqn.host-a").unwrap();
        let expected = compute_response(secret, &target_challenge, "nqn.host-a", "nqn.subsys");
        assert!(
            !verify_response(&host_response_with_fake, &expected),
            "假 secret 应被拒"
        );
    }
}
