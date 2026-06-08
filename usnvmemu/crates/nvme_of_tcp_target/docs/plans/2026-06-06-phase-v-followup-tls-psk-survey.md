# V-followup-tls-psk-survey — rustls 0.23 external-PSK 现状调研 (2026-06-06)

## 1. 目标

NVMe-oF TP-8011 + spec § 8.13 强制要求 TLS 1.3 with **external Pre-Shared Key**
(retained PSK or DH-HMAC-CHAP-derived generated PSK)。本 phase 已实现
`src/tls_psk.rs` 全 deterministic 派生层 (digest + HKDF-Expand-Label + identity
string)，但**未注入 TLS 握手**，因为 rustls 0.23 没有公开 external-PSK API。

本文档调研三条候选路径 + 推荐选择。

## 2. 上游状态 (rustls/rustls upstream)

调研日期: 2026-06-06。

| 项 | 状态 |
|----|------|
| rustls 0.23.x 公开 `ExternalPsk` trait / PSK callback | ❌ 不存在 |
| Issue #174 "Status of PSK Support?" | 🟡 2018-06-03 开，仍 open（2026-05 还有动静）|
| PR #2424 "add TLS 1.3 PSK support" (SpiderOak 提) | 🔴 2025-06-13 closed without merge |
| PR #2524 "client: initial support for preshared keys" (djc 接手) | 🔴 closed |
| 维护者声明 | `ctz`: "Going to close this for now, pending future work on PSK support" |

**关键阻碍** (PR #2424 评论 + djc 反馈):
- semver: 加 `HandshakeKind` variant / 删 `PeerMisbehaved` variants 会 break compat
- 实现 quality: 巨 diff，需 client/server 分拆 + 干净 commit 历史
- 没有 dedicated maintainer 持续推

## 3. 候选路径对比

### 路径 A: 等 rustls upstream

- **代价**: 0（被动等）。
- **风险**: 时间线完全不可控，可能 2027+ 才有；NVMe-oF Linux nvme-cli `--tls`
  生态等不起。
- **副作用**: 我们的 `tls_psk.rs` 静态 anchor 等于死代码，无 e2e 路径可验。

### 路径 B: Fork rustls 加 PSK callback

- **代价**: 高。SpiderOak PR 是工程级 (~ 2k LOC)；维护成本 = 持续 rebase 到
  rustls 主线 + 重新 audit 每次 upstream 变更。
- **风险**: `#![forbid(unsafe_code)]` crate policy 仍可守，但 fork rustls 自身就
  有 unsafe；本 crate 边界依旧 0 unsafe，可接受。
- **副作用**: 多 1 个 dep。如改 fork 名（如 `rustls-with-psk`），下游 OpenVMM
  build 链需同步。

### 路径 C: 拿现有 PSK PR 当 branch dependency 试

- **代价**: 中。`SpiderOak/rustls` fork (PR #2424 源) 可作 git dep
  (`git = "https://github.com/SpiderOak/rustls", branch = "psk-tls13"`)，0 LOC
  本地 patch。
- **风险**: SpiderOak fork 自身 stale (~ 2024 中)；不会跟新 rustls 安全补丁。
  跑实验 / 教学 OK，**绝对不能进 prod**。
- **副作用**: 把 `tls_psk.rs` 接上后立刻可对 Linux nvme-cli `--tls` 实测，
  这是当前唯一能 close 真 interop 缺口的路径。

### 路径 D (创新但风险大): rustls-mbedtls / s2n-tls / openssl 桥

- 选别的 TLS 实现：`s2n-tls` (Amazon) 有 PSK callback；`rustls-mbedtls` 用
  mbedtls 后端；`openssl` crate 历史悠久但 unsafe + dep 重。
- 代价：本 crate 现状 100% rustls，换栈 = 重写 `tls.rs` + `tls_identity.rs` +
  所有 TLS test。**估 800-1500 LOC 改动 + 重新 reviewer pass**。

## 4. 推荐：路径 A + C 并行 (TODO 留下，主线不接)

短期 (本 fork 不动):
- 维持 `tls_psk.rs` 当前状态（deterministic crypto + 13 单元测试 + SHA-256/384
  双 dispatch e2e）。
- 在 `usnvmemu/docs/plans/2026-06-06-phase-v-followup-tls-psk-survey.md` 留
  路径 C 实验脚本：当用户想做真 Linux interop 时，可拿 SpiderOak fork rebuild。
- 不引 git dep 入主线 `Cargo.toml`，避免 fork stale 风险。

中期 (rustls 出官方 API):
- 监 rustls release notes；一旦 external PSK trait 进 stable，立刻接上：
  - `src/tls.rs` 加 `build_acceptor_with_psk(store: PskStore)`
  - 用 `tls_psk::generate_psk_digest` + `derive_tls_psk` + `build_psk_identity`
    填 rustls 提供的 callback
  - 加 `tests/vt_tls_psk_5_*` 系列 e2e (类似 V-followup-tls-3 的 5 个 file)
  - Python harness `scripts/interop_py/tls_psk_e2e.py` 走 nvme-cli `--tls`

长期 (真 interop 五元组 anchor):
- 写 out-of-tree Linux kernel probe module dump `nvme_auth_derive_tls_psk` 输出
  (它 `EXPORT_SYMBOL_GPL` 可挂)。
- 把 5 个 known-good 五元组烧进 `tls_psk.rs` 的 `vt_tlspsk_kernel_ci_vectors_*`
  conformance 测试。
- 之后 `tls_psk.rs` 升 production-grade，文档边界标 "self-consistent" → "kernel-conformant"。

## 5. 副产物 / 决策日志

- **不接路径 D**: 换 TLS 栈代价 > 30% codebase 重写，且新栈带来的 unsafe / dep
  量不抵 PSK 收益。
- **不接路径 B**: 我们没有 rustls maintainer bandwidth；fork 之后失同步 = 安全债。
- **L-6 (kernel CI 五元组)**: 与本调研独立，不依赖 rustls；任何时候用户跑了
  Linux 实测就能填，**优先级高于** PSK 注入 (因为它修复 `tls_psk.rs` 的
  conformance 问题，不需要等任何上游)。
