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

/// **V-followup-dhchap-2** — CHAP state machine 状态（per-session）。
///
/// 教学版只覆盖 host->target 单向认证（target 验 host）：
/// - `Idle`: 未启动；Connect 后视 `--host-secret` 是否注册而决定是否进入
///   `ChallengeNeeded`
/// - `ChallengeNeeded`: target 已认 Connect 但尚未挑战；下一个 AUTH_RECV
///   PDU 触发 target 生成 challenge 并 transition 到 `ChallengeSent`
/// - `ChallengeSent { challenge, hostnqn }`: 等 host 在 AUTH_SEND 内送回 HMAC
///   response；verify 通过 → `Authenticated`；失败 → `Failed`
/// - `Authenticated`: AUTH 通过，admin/IO cmd 可放行
/// - `Failed`: 校验失败 / 协议违例；caller 应关连接
/// - `Disabled`: bin 没启 CHAP（无 secret store 或 host 不在 store），后续
///   命令按 V-followup-auth / auth-2 既有规则走（保兼容）
#[derive(Debug, Clone)]
pub enum ChapStage {
    /// 初始未启动（构造时短暂状态；外部调用方一般不会观测到）。
    Idle,
    /// Connect 已通过 + secret 已注册；等下一个 AUTH_RECV 触发 issue_challenge。
    ChallengeNeeded {
        /// host NQN（来自 Connect.HOSTNQN，决定查 secret 用的 key）。
        hostnqn: String,
        /// subsystem NQN（来自 Connect.SUBNQN，参与 transcript 防 cross-protocol replay）。
        subnqn: String,
    },
    /// target 已发 challenge，等 host 在 AUTH_SEND 内送回 HMAC response。
    ChallengeSent {
        /// 本次 challenge bytes（issue_challenge 抽出的 nonce）。
        challenge: [u8; CHAP_CHALLENGE_LEN],
        /// host NQN（用于 verify 时定位 secret + transcript 参与 HMAC）。
        hostnqn: String,
        /// subsystem NQN（参与 transcript）。
        subnqn: String,
    },
    /// CHAP 通过，admin/IO cmd 可放行。
    Authenticated,
    /// CHAP 校验失败 / 协议违例；caller 应关连接。
    Failed,
    /// bin 未启 CHAP（无 secret store 或调用方未注入）；后续 cmd 按 V-followup-auth /
    /// auth-2 既有规则走（保兼容）。
    Disabled,
}

impl ChapStage {
    /// 当前是否已通过 (用于 admin / IO 命令的 gate)。
    pub fn is_authenticated(&self) -> bool {
        matches!(self, ChapStage::Authenticated | ChapStage::Disabled)
    }
    /// 当前是否已失败 (caller 应关连接)。
    pub fn is_failed(&self) -> bool {
        matches!(self, ChapStage::Failed)
    }
}

/// **V-followup-dhchap-2** — per-session CHAP negotiation 容器；包装 stage +
/// 共享 secret store。
#[derive(Debug, Clone)]
pub struct ChapNegotiation {
    /// 当前 CHAP 协商所处阶段。
    pub stage: ChapStage,
    /// 共享 secret store；多 session 共享同一 Arc，避免 clone 整张表。
    pub store: std::sync::Arc<ChapSecretStore>,
    /// **V-followup-dhchap-4** — wire 协议模式（首个 AUTH_SEND 检测后锁定）。
    pub wire_mode: ChapWireMode,
    /// **V-followup-dhchap-4** — spec NEGOTIATE 携带的 transaction id；
    /// target 在 CHALLENGE / SUCCESS1 / FAILURE 中需回填。
    pub spec_tid: u16,
}

/// **V-followup-dhchap-4** — CHAP wire 协议模式。
///
/// 教学库支持两套 wire：
/// - `Unknown`: 首个 AUTH_SEND 还没到，未确定
/// - `Simplified`: 老 V-followup-dhchap-3 路径，AUTH_RECV → 32B challenge raw
///   bytes，AUTH_SEND → 32B HMAC response raw bytes
/// - `Spec4Msg`: spec § 8.13.5 4-message 路径，AUTH_SEND data 以
///   `[0x01, MSG_NEGOTIATE]` 起头，target 用 spec wire 回 CHALLENGE / SUCCESS1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChapWireMode {
    /// 还没识别（无 AUTH_SEND 收到）
    Unknown,
    /// 教学版简化 wire（兼容 V-followup-dhchap-3 + Python test）
    Simplified,
    /// spec § 8.13.5 4-message wire (Linux nvme-cli compatible)
    Spec4Msg,
}

impl ChapNegotiation {
    /// Connect 完成后由 session 决定是否启动 CHAP。
    /// - store 空 → `Disabled`（兼容老路径）
    /// - hostnqn 不在 store → `Failed`（CHAP 启用就必须可校验，未配置 = 不放行）
    /// - hostnqn 在 store → `ChallengeNeeded`
    pub fn on_connect(store: std::sync::Arc<ChapSecretStore>, hostnqn: &str, subnqn: &str) -> Self {
        let stage = if store.is_empty() {
            ChapStage::Disabled
        } else if store.get(hostnqn).is_none() {
            ChapStage::Failed
        } else {
            ChapStage::ChallengeNeeded {
                hostnqn: hostnqn.to_string(),
                subnqn: subnqn.to_string(),
            }
        };
        Self {
            stage,
            store,
            wire_mode: ChapWireMode::Unknown,
            spec_tid: 0,
        }
    }

    /// target 端 issue challenge；调用者随后通过 wire 把 challenge 发回 host。
    /// 状态必须是 `ChallengeNeeded` 才生效，否则置 `Failed`。
    pub fn issue_challenge(&mut self) -> Option<[u8; CHAP_CHALLENGE_LEN]> {
        match std::mem::replace(&mut self.stage, ChapStage::Failed) {
            ChapStage::ChallengeNeeded { hostnqn, subnqn } => {
                let challenge = generate_challenge();
                self.stage = ChapStage::ChallengeSent {
                    challenge,
                    hostnqn,
                    subnqn,
                };
                Some(challenge)
            }
            _ => {
                // V-dhchap-4 reviewer H-3 修：之前误写 `self.stage = other` 再被
                // 覆盖；明确语义为"非 ChallengeNeeded 一律 Failed (caller 关连接)"。
                self.stage = ChapStage::Failed;
                None
            }
        }
    }

    /// host 通过 AUTH_SEND 送回 HMAC response；target 验签 → transition 终态。
    /// 返 `true` = 通过（state 已变 `Authenticated`），`false` = 失败
    /// （state 已变 `Failed`）。
    pub fn verify_host_response(&mut self, host_response: &[u8; HMAC_SHA256_LEN]) -> bool {
        let new_stage = match std::mem::replace(&mut self.stage, ChapStage::Failed) {
            ChapStage::ChallengeSent {
                challenge,
                hostnqn,
                subnqn,
            } => match self.store.get(&hostnqn) {
                Some(secret) => {
                    let expected = compute_response(secret, &challenge, &hostnqn, &subnqn);
                    if verify_response(host_response, &expected) {
                        ChapStage::Authenticated
                    } else {
                        ChapStage::Failed
                    }
                }
                None => ChapStage::Failed,
            },
            _ => ChapStage::Failed,
        };
        let ok = matches!(new_stage, ChapStage::Authenticated);
        self.stage = new_stage;
        ok
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

    // ---- V-followup-dhchap-2 state machine tests ----

    fn make_store_with(nqn: &str, secret: Vec<u8>) -> std::sync::Arc<ChapSecretStore> {
        let mut s = ChapSecretStore::new();
        s.insert(nqn.to_string(), secret);
        std::sync::Arc::new(s)
    }

    #[test]
    fn vt_dhchap2_on_connect_empty_store_disabled() {
        let store = std::sync::Arc::new(ChapSecretStore::new());
        let neg = ChapNegotiation::on_connect(store, "nqn.h", "nqn.s");
        assert!(matches!(neg.stage, ChapStage::Disabled));
        assert!(neg.stage.is_authenticated(), "Disabled 视为已通过 (兼容)");
    }

    #[test]
    fn vt_dhchap2_on_connect_unknown_host_failed() {
        let store = make_store_with("nqn.legit", vec![0x77u8; 32]);
        let neg = ChapNegotiation::on_connect(store, "nqn.evil", "nqn.s");
        assert!(matches!(neg.stage, ChapStage::Failed));
        assert!(neg.stage.is_failed());
    }

    #[test]
    fn vt_dhchap2_on_connect_known_host_challenge_needed() {
        let store = make_store_with("nqn.h", vec![0x77u8; 32]);
        let neg = ChapNegotiation::on_connect(store, "nqn.h", "nqn.s");
        assert!(matches!(neg.stage, ChapStage::ChallengeNeeded { .. }));
        assert!(!neg.stage.is_authenticated());
    }

    #[test]
    fn vt_dhchap2_issue_challenge_then_verify_correct_succeeds() {
        let secret = vec![0x77u8; 32];
        let store = make_store_with("nqn.h", secret.clone());
        let mut neg = ChapNegotiation::on_connect(store, "nqn.h", "nqn.s");
        let challenge = neg.issue_challenge().expect("应能 issue");
        let response = compute_response(&secret, &challenge, "nqn.h", "nqn.s");
        assert!(neg.verify_host_response(&response));
        assert!(neg.stage.is_authenticated());
    }

    #[test]
    fn vt_dhchap2_verify_wrong_response_fails() {
        let store = make_store_with("nqn.h", vec![0x77u8; 32]);
        let mut neg = ChapNegotiation::on_connect(store, "nqn.h", "nqn.s");
        let _ = neg.issue_challenge();
        let wrong = [0xFFu8; HMAC_SHA256_LEN];
        assert!(!neg.verify_host_response(&wrong));
        assert!(neg.stage.is_failed());
    }

    #[test]
    fn vt_dhchap2_issue_challenge_in_wrong_state_fails() {
        let store = make_store_with("nqn.h", vec![0x77u8; 32]);
        let mut neg = ChapNegotiation::on_connect(store, "nqn.h", "nqn.s");
        let _ = neg.issue_challenge();
        let second = neg.issue_challenge();
        assert!(second.is_none());
        assert!(neg.stage.is_failed());
    }

    #[test]
    fn vt_dhchap2_verify_without_challenge_fails() {
        let store = make_store_with("nqn.h", vec![0x77u8; 32]);
        let mut neg = ChapNegotiation::on_connect(store, "nqn.h", "nqn.s");
        let fake = [0u8; HMAC_SHA256_LEN];
        assert!(!neg.verify_host_response(&fake));
        assert!(neg.stage.is_failed());
    }
}

// ============================================================================
// V-followup-dhchap-4 — spec NVMe Base 2.0c § 8.13.5 wire format
// (DH-HMAC-CHAP 4-message: NEGOTIATE → CHALLENGE → REPLY → SUCCESS1 → SUCCESS2)
// ============================================================================
//
// 参 Linux kernel include/linux/nvme.h struct nvmf_auth_dhchap_*
// 实测兼容 Linux nvme-cli `--dhchap-secret` 路径。
//
// 教学限制 (相对完整 spec):
// - 不实现 DH ephemeral key exchange (dhgid=0 NULL = HMAC-only)
// - 不实现 bidirectional / mutual auth (SUCCESS1 不带 host-verify rval；
//   host 不会再发 SUCCESS2 — 我们当 SUCCESS1 SC=0 即终结)
// - 不实现 secure channel concat (sc_c=0 forced)
// - 只支持 SHA-256 hash (hashid=0x01)

/// V-dhchap-4 wire 常量 (spec § 8.13.5 + Linux include/linux/nvme.h)
pub mod wire {
    /// Auth Type: DHCHAP_MESSAGES (1 = DHCHAP family)
    pub const AUTH_TYPE_DHCHAP: u8 = 0x01;
    /// Message ID: NEGOTIATE (host → target, AUTH_SEND)
    pub const MSG_NEGOTIATE: u8 = 0x00;
    /// Message ID: CHALLENGE (target → host, AUTH_RECV)
    pub const MSG_CHALLENGE: u8 = 0x01;
    /// Message ID: REPLY (host → target, AUTH_SEND)
    pub const MSG_REPLY: u8 = 0x02;
    /// Message ID: SUCCESS1 (target → host, AUTH_RECV)
    pub const MSG_SUCCESS1: u8 = 0x03;
    /// Message ID: SUCCESS2 (host → target, mutual auth final ack)
    pub const MSG_SUCCESS2: u8 = 0x04;
    /// Message ID: FAILURE2 (host → target, abort)
    pub const MSG_FAILURE2: u8 = 0xf0;
    /// Message ID: FAILURE1 (target → host, abort)
    pub const MSG_FAILURE1: u8 = 0xf1;

    /// Protocol descriptor auth_id (in NEGOTIATE.auth_protocol[].authid)
    pub const AUTH_DHCHAP_AUTH_ID: u8 = 0x01;

    /// HMAC hash ID: SHA-256 (32 byte digest)
    pub const HASH_SHA256: u8 = 0x01;
    /// HMAC hash ID: SHA-384 (48 byte digest, 教学版不支持)
    pub const HASH_SHA384: u8 = 0x02;
    /// HMAC hash ID: SHA-512 (64 byte digest, 教学版不支持)
    pub const HASH_SHA512: u8 = 0x03;

    /// DH group IDs (NULL = no DH exchange, HMAC-only mode)
    pub const DHGROUP_NULL: u8 = 0x00;

    /// FAILURE rescode_exp: 通用 auth failure
    pub const FAIL_EXP_FAILED: u8 = 0x01;
    /// FAILURE rescode_exp: 没有可用的 protocol
    pub const FAIL_EXP_NOT_USABLE: u8 = 0x02;
    /// FAILURE rescode_exp: hash 算法不支持
    pub const FAIL_EXP_HASH_UNUSABLE: u8 = 0x04;
    /// FAILURE rescode_exp: DH group 不支持
    pub const FAIL_EXP_DHGROUP_UNUSABLE: u8 = 0x05;
    /// FAILURE rescode_exp: payload 解析失败
    pub const FAIL_EXP_INCORRECT_PAYLOAD: u8 = 0x06;
    /// FAILURE rescode_exp: 消息类型/顺序不对
    pub const FAIL_EXP_INCORRECT_MESSAGE: u8 = 0x07;
}

/// V-dhchap-4 — 解析 host 发来的 NEGOTIATE 消息 (AUTH_SEND data)。
///
/// 教学版限制 (reviewer H-2 备注):
/// - **只检查第一个 `auth_protocol[]` descriptor**。spec 允许 host 列多组
///   descriptors (e.g. DH-2048 + DH-NULL)；本实现拒掉第一个 descriptor 没
///   同时包含 SHA-256 + DHGROUP_NULL 的请求。Linux nvme-cli 默认只发一组，
///   所以实测可 interop；多 descriptor host 将被错拒。
/// - 不解析 napd > 1 的后续 descriptors (TODO(spec-full))。
///
/// wire layout (spec):
/// ```text
/// 0    auth_type   u8  = 0x01 (DHCHAP)
/// 1    auth_id     u8  = 0x00 (NEGOTIATE)
/// 2-3  rsvd        u16
/// 4-5  t_id        u16le (transaction id)
/// 6    sc_c        u8  (secure channel concat, 教学版要求 0)
/// 7    napd        u8  (number of auth protocol descriptors, >= 1)
/// 8+   auth_protocol[napd]
///   每个 protocol descriptor (≥ 8 byte):
///     0    authid    u8  (= 0x01 DHCHAP)
///     1    rsvd      u8
///     2    halen     u8  (number of hash IDs)
///     3    dhlen     u8  (number of DH group IDs)
///     4    idlist[halen + dhlen]   按 halen hash ids 后跟 dhlen dh ids
///     padding 到下一个 8-byte 边界
/// ```
///
/// 返 (t_id, has_sha256, has_dhnull)。我们只用 SHA-256 + DHGROUP_NULL。
pub fn parse_negotiate(data: &[u8]) -> anyhow::Result<(u16, bool, bool)> {
    use anyhow::{anyhow, bail};
    if data.len() < 8 {
        bail!("NEGOTIATE too short: {}", data.len());
    }
    if data[0] != wire::AUTH_TYPE_DHCHAP {
        bail!("auth_type {:#x} != DHCHAP (0x01)", data[0]);
    }
    if data[1] != wire::MSG_NEGOTIATE {
        bail!("auth_id {:#x} != NEGOTIATE (0x00)", data[1]);
    }
    let t_id = u16::from_le_bytes([data[4], data[5]]);
    let sc_c = data[6];
    let napd = data[7];
    if sc_c != 0 {
        bail!("sc_c = {sc_c}, 教学版仅支持 0 (no secure channel concat)");
    }
    if napd == 0 {
        bail!("napd = 0, 至少需 1 个 auth protocol descriptor");
    }
    // 解第一个 protocol descriptor (我们只看第一个; spec 允许 host 列多个)
    if data.len() < 8 + 4 {
        bail!("NEGOTIATE auth_protocol[0] header truncated");
    }
    let off = 8;
    let authid = data[off];
    let halen = data[off + 2] as usize;
    let dhlen = data[off + 3] as usize;
    if authid != wire::AUTH_DHCHAP_AUTH_ID {
        bail!("protocol authid {:#x} != DHCHAP (0x01)", authid);
    }
    if data.len() < off + 4 + halen + dhlen {
        bail!("auth_protocol idlist truncated");
    }
    let idlist = &data[off + 4..off + 4 + halen + dhlen];
    let hash_ids = &idlist[..halen];
    let dh_ids = &idlist[halen..];
    let has_sha256 = hash_ids.contains(&wire::HASH_SHA256);
    let has_dhnull = dh_ids.contains(&wire::DHGROUP_NULL);
    if !has_sha256 {
        return Err(anyhow!(
            "host 未列 SHA-256 (我们只支持此 hash)；提供 hash IDs = {hash_ids:?}"
        ));
    }
    if !has_dhnull {
        return Err(anyhow!(
            "host 未列 DH NULL (我们只支持 HMAC-only)；提供 DH IDs = {dh_ids:?}"
        ));
    }
    Ok((t_id, has_sha256, has_dhnull))
}

/// V-dhchap-4 — 构造 CHALLENGE 消息 (target 回 host AUTH_RECV C2HData)。
///
/// wire (spec):
/// ```text
/// 0    auth_type   = 0x01
/// 1    auth_id     = 0x01 CHALLENGE
/// 2-3  rsvd1       u16
/// 4-5  t_id        u16le
/// 6    hl          u8  (challenge bytes, = 32 for SHA-256)
/// 7    rsvd2       u8
/// 8    hashid      u8  (= 0x01 SHA-256)
/// 9    dhgid       u8  (= 0x00 NULL DH)
/// 10-11 dhvlen     u16le (= 0 for NULL DH)
/// 12-15 seqnum     u32le (我们用 1)
/// 16..16+hl  cval (challenge bytes)
/// ```
pub fn build_challenge(t_id: u16, challenge: &[u8; CHAP_CHALLENGE_LEN]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + CHAP_CHALLENGE_LEN);
    out.push(wire::AUTH_TYPE_DHCHAP);
    out.push(wire::MSG_CHALLENGE);
    out.extend_from_slice(&[0, 0]); // rsvd1
    out.extend_from_slice(&t_id.to_le_bytes());
    out.push(CHAP_CHALLENGE_LEN as u8); // hl
    out.push(0); // rsvd2
    out.push(wire::HASH_SHA256);
    out.push(wire::DHGROUP_NULL);
    out.extend_from_slice(&0u16.to_le_bytes()); // dhvlen = 0
    out.extend_from_slice(&1u32.to_le_bytes()); // seqnum = 1
    out.extend_from_slice(challenge);
    out
}

/// V-dhchap-4 — 解析 REPLY 消息 (host 发 AUTH_SEND data)。
///
/// wire (spec):
/// ```text
/// 0    auth_type   = 0x01
/// 1    auth_id     = 0x02 REPLY
/// 2-3  rsvd1       u16
/// 4-5  t_id        u16le
/// 6    hl          u8 (response bytes, = 32)
/// 7    rsvd2       u8
/// 8    cvalid      u8 bit0 = host-challenge response present (mutual auth)
/// 9    rsvd3       u8
/// 10-11 dhvlen     u16le (= 0 for NULL DH)
/// 12-15 seqnum     u32le
/// 16..16+hl  rval (HMAC response to target challenge)
/// 16+hl..  if cvalid: host-challenge (hl bytes) + dh_value (dhvlen bytes)
/// ```
pub fn parse_reply(data: &[u8]) -> anyhow::Result<(u16, Vec<u8>)> {
    use anyhow::bail;
    if data.len() < 16 + HMAC_SHA256_LEN {
        bail!("REPLY too short: {}", data.len());
    }
    if data[0] != wire::AUTH_TYPE_DHCHAP {
        bail!("auth_type != DHCHAP");
    }
    if data[1] != wire::MSG_REPLY {
        bail!("auth_id != REPLY");
    }
    let t_id = u16::from_le_bytes([data[4], data[5]]);
    let hl = data[6] as usize;
    if hl != HMAC_SHA256_LEN {
        bail!("REPLY hl={hl} != {}", HMAC_SHA256_LEN);
    }
    let rval = data[16..16 + HMAC_SHA256_LEN].to_vec();
    Ok((t_id, rval))
}

/// V-dhchap-4 — 构造 SUCCESS1 (target 回 host AUTH_RECV C2HData 第 2 次)。
///
/// 教学版 unidirectional auth: rvalid=0 (不带 mutual auth response)。
/// 完整 spec 双向时 rvalid=1 + 16+hl bytes of host-challenge verify response。
pub fn build_success1(t_id: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.push(wire::AUTH_TYPE_DHCHAP);
    out.push(wire::MSG_SUCCESS1);
    out.extend_from_slice(&[0, 0]); // rsvd1
    out.extend_from_slice(&t_id.to_le_bytes());
    out.push(HMAC_SHA256_LEN as u8); // hl
    out.push(0); // rsvd2
    out.push(0); // rvalid = 0 (no mutual auth)
    out.extend_from_slice(&[0u8; 7]); // rsvd3[7]
    debug_assert_eq!(out.len(), 16);
    out
}

/// V-dhchap-4 — 构造 FAILURE1/FAILURE2 (target 回 host wire-level fail)。
pub fn build_failure(t_id: u16, is_first: bool, rescode_exp: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.push(wire::AUTH_TYPE_DHCHAP);
    out.push(if is_first {
        wire::MSG_FAILURE1
    } else {
        wire::MSG_FAILURE2
    });
    out.extend_from_slice(&[0, 0]); // rsvd1
    out.extend_from_slice(&t_id.to_le_bytes());
    out.push(0x01); // rescode = 1 (FAILED)
    out.push(rescode_exp);
    out.extend_from_slice(&[0u8; 8]); // rsvd
    debug_assert_eq!(out.len(), 16);
    out
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    fn build_negotiate(t_id: u16, hash_ids: &[u8], dh_ids: &[u8]) -> Vec<u8> {
        let mut out = vec![
            wire::AUTH_TYPE_DHCHAP,
            wire::MSG_NEGOTIATE,
            0,
            0,
            t_id as u8,
            (t_id >> 8) as u8,
            0,
            1, // napd=1
        ];
        // protocol descriptor
        out.extend_from_slice(&[
            wire::AUTH_DHCHAP_AUTH_ID,
            0,                  // rsvd
            hash_ids.len() as u8,
            dh_ids.len() as u8,
        ]);
        out.extend_from_slice(hash_ids);
        out.extend_from_slice(dh_ids);
        out
    }

    #[test]
    fn vt_dhchap4_negotiate_parse_happy() {
        let data = build_negotiate(0x1234, &[wire::HASH_SHA256], &[wire::DHGROUP_NULL]);
        let (tid, sha, dh) = parse_negotiate(&data).unwrap();
        assert_eq!(tid, 0x1234);
        assert!(sha);
        assert!(dh);
    }

    #[test]
    fn vt_dhchap4_negotiate_rejects_no_sha256() {
        let data = build_negotiate(0x1234, &[wire::HASH_SHA512], &[wire::DHGROUP_NULL]);
        let err = parse_negotiate(&data).unwrap_err();
        assert!(format!("{err:#}").contains("SHA-256"));
    }

    #[test]
    fn vt_dhchap4_negotiate_rejects_no_dhnull() {
        // 教学版需 DHGROUP_NULL (HMAC-only)
        let data = build_negotiate(0x1234, &[wire::HASH_SHA256], &[0x01 /* DH 2048 */]);
        let err = parse_negotiate(&data).unwrap_err();
        assert!(format!("{err:#}").contains("DH NULL"));
    }

    #[test]
    fn vt_dhchap4_negotiate_rejects_sc_c() {
        let mut data = build_negotiate(0x1234, &[wire::HASH_SHA256], &[wire::DHGROUP_NULL]);
        data[6] = 1; // sc_c = 1
        let err = parse_negotiate(&data).unwrap_err();
        assert!(format!("{err:#}").contains("sc_c"));
    }

    #[test]
    fn vt_dhchap4_challenge_roundtrip() {
        let ch = [0xAAu8; CHAP_CHALLENGE_LEN];
        let wire = build_challenge(0xABCD, &ch);
        assert_eq!(wire[0], wire::AUTH_TYPE_DHCHAP);
        assert_eq!(wire[1], wire::MSG_CHALLENGE);
        assert_eq!(u16::from_le_bytes([wire[4], wire[5]]), 0xABCD);
        assert_eq!(wire[6], CHAP_CHALLENGE_LEN as u8);
        assert_eq!(wire[8], wire::HASH_SHA256);
        assert_eq!(wire[9], wire::DHGROUP_NULL);
        assert_eq!(u16::from_le_bytes([wire[10], wire[11]]), 0); // dhvlen=0
        assert_eq!(&wire[16..16 + CHAP_CHALLENGE_LEN], &ch);
    }

    #[test]
    fn vt_dhchap4_reply_parse_happy() {
        // 构造 REPLY: 16B header + 32B rval
        let mut data = vec![
            wire::AUTH_TYPE_DHCHAP,
            wire::MSG_REPLY,
            0,
            0,
            0xCD,
            0xAB,
            HMAC_SHA256_LEN as u8,
            0, // rsvd2
            0, // cvalid
            0, // rsvd3
            0,
            0, // dhvlen
            1,
            0,
            0,
            0, // seqnum
        ];
        data.extend_from_slice(&[0xFFu8; HMAC_SHA256_LEN]);
        let (tid, rval) = parse_reply(&data).unwrap();
        assert_eq!(tid, 0xABCD);
        assert_eq!(rval, vec![0xFFu8; HMAC_SHA256_LEN]);
    }

    #[test]
    fn vt_dhchap4_success1_builds_16_bytes_unidirectional() {
        let wire = build_success1(0x1234);
        assert_eq!(wire.len(), 16);
        assert_eq!(wire[1], wire::MSG_SUCCESS1);
        assert_eq!(wire[8], 0, "rvalid=0 unidirectional");
    }

    #[test]
    fn vt_dhchap4_failure_builds_16_bytes() {
        let f1 = build_failure(0xCAFE, true, wire::FAIL_EXP_HASH_UNUSABLE);
        assert_eq!(f1.len(), 16);
        assert_eq!(f1[1], wire::MSG_FAILURE1);
        assert_eq!(f1[7], wire::FAIL_EXP_HASH_UNUSABLE);
        let f2 = build_failure(0xCAFE, false, wire::FAIL_EXP_INCORRECT_PAYLOAD);
        assert_eq!(f2[1], wire::MSG_FAILURE2);
    }
}
