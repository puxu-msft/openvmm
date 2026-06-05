# Phase V-followup TLS — NVMe-oF TCP target 加 TLS 1.3 wrap

> 日期：2026-06-06
> Plan 作者：planner subagent
> 前置 HEAD：`afb90654`（V8e-7 完成；197 active test，clippy 0 warning）
> 目标：把 TLS 1.3 wrap 引入 NVMe-oF TCP target，保留全部 plaintext 路径与现有测试

---

## 1. 总览

### 1.1 当前状态（V8e-7 完成时）

- bin 全 async：`tokio::main` + `tokio::net::TcpListener` + `tokio::spawn(handle_conn_async)`
- `AsyncSession.stream: tokio::net::TcpStream`（**hardcoded**，本 phase 关键改动点）
- `read_pdu_async` / `write_pdu_async` 已是 `S: AsyncRead/AsyncWrite + Unpin` 泛型（**TLS 喂入 zero-touch**）
- workspace 不含 rustls / tokio-rustls / rcgen — 本 phase 首次引入
- 已知 caveat（每 commit message 重复）：
  > 本 bin 无 TLS / 无 in-band auth / 无 host NQN 白名单；默认 listen `127.0.0.1`，
  > 非 loopback 必须显式 `--i-know-this-is-insecure`

### 1.2 V-followup TLS 目标

最小可用 TLS：**Server-auth X.509 自带 cert/key listen + handshake → 把
TlsStream 喂入泛化后的 AsyncSession**。教学价值优先，spec TP-8011
PSK 全套（PSKI + HKDF-Expand-Label NVMe-specific labels + NQN binding）
分到 V-followup-tls-PSK / V-followup-auth。

### 1.3 与 V8e-7 关系

- **保留全部 plaintext 路径**：CLI 不传 `--tls-cert/--tls-key` 时行为 100% 同 V8e-7
- 197 active test 全部继续跑（无任何 TLS 干扰）
- 新增 TLS 测试在 `tests/vt_tls_*.rs`，独立 file
- `byte-identical` gate（V8e-7-followup）不动：比 `serialize_pdu` 出参，TLS 是 transport wrap，不改 wire
- README 更新 "TLS 教学开关" 段落，标注 "教学 cert，不要用于生产"

### 1.4 spec 锚

- NVMe Base Spec 2.0c § 8.13（NVMe over Fabrics TCP TLS Transport Binding）
- TP-8011（PSK PSKI / HKDF-Expand-Label NVMe labels）
- RFC 8446（TLS 1.3）

---

## 2. 风险表

| ID | 风险 | 影响 | 缓解 |
|----|------|------|------|
| **R-1** | `AsyncSession.stream` 由 `tokio::net::TcpStream` 改泛型，breaking change 波及全部 V8e-3..V8e-7 内部代码 + 200+ test | 编译爆 + 隐式 await-holding-lock 漏掉 | V-followup-tls-1 单独 phase 只做泛型化；不引 rustls，让 baseline 197 test 全过；用 `S: AsyncReadExt + AsyncWriteExt + Unpin + Send + 'static` |
| **R-2** | `forbid(unsafe_code)` 与 rustls 共存 — rustls 内部 unsafe 但 crate 外缘 safe，本 crate 不传递 | 编译 ok；只要本 crate 不写 unsafe 即可 | Cargo.toml 注释 "rustls 内部 unsafe 由 upstream audit"；CI `cargo deny check advisories` 周期审计 |
| **R-3** | `await_holding_lock` lint 在 TLS handshake 路径误伤 — TLS handshake 必跨 await | 强制 lock 作用域不跨 await，对 controller 现有 closure-only API 是好事 | TLS handshake 完全在 bin 端 accept 后做（持 `TcpStream` → wrap → 喂 `accept_and_handshake_async`）；session 内不持锁 await，与现状一致 |
| **R-4** | 教学 self-signed cert 假装 trust — 用户复制 README 命令在公网启动被 MITM | 同 V5d "无 TLS" 等价或更坏（假装有保护） | CLI flag `--tls-cert` 必须配 `--tls-i-trust-this-cert` 双 explicit consent；启动 warn + 红 stderr 标注 "教学 cert 不做 chain validation"；README "永远不要用于生产" 章节 |
| **R-5** | 配置文件 secret 泄漏 — cert/key 路径误 commit 进仓库 | 私钥外泄 | CLI 只接 `--tls-key /path/to/key.pem`（path，不接 inline content）；README 写 `.gitignore` 模板 |
| **R-6** | TLS 失败时 fallback 行为不明 — plaintext ICReq 到 TLS port → framing 错乱 | 用户调试体验差 | TLS port 强制 TLS（无 fallback）；plaintext port 仍开通；同进程可同时跑 TLS+plaintext listener（dual-listener pattern 同 V8f discovery）|
| **R-7** | mTLS host identity ↔ Connect.subnqn 校验未做 | 不符 spec 强 identity binding | 本 phase 写明 "暂不绑 identity"，留 V-followup-auth；CLI 不暴露 client cert 验证开关防误以为已 enforce |
| **R-8** | byte-identical gate 不覆盖 TLS-wrapped wire | 仅文档风险 | V-followup-tls-4 加新 gate `vt_tls_4_app_layer_bytes_identical`：解密后应用 bytes 与 plaintext 路径在同 input 下等价 |
| **R-9** | rustls / tokio-rustls 版本冲突 | 编译冲突 | crate-local Cargo.toml 直接钉死版本；选 rustls 0.23 + tokio-rustls 0.26（最新 stable）；rcgen 0.13 作 dev-dep |
| **R-10** | TLS slowloris：client send ClientHello 一半就不动 | DoS | bin 端 `tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT_SECS=30, tls_acceptor.accept(stream))` 包；与 V5d-fix-2 handshake_timeout 对等 |

---

## 3. 设计 Q&A

### Q1. stream 抽象边界放哪一层？泛型化 `AsyncSession` 还是 trait object？

**决策：泛型 + trait bound，不用 trait object。**

理由：
- stream 类型在 conn accept 时一次绑定，运行时不变
- 泛型零成本；trait object virtual call + 不能再加 trait method
- bin 端二分：if tls { `accept_and_handshake_async::<TlsStream<TcpStream>>` } else { `::<TcpStream>` }

trait bound 用 `S: AsyncRead + AsyncWrite + Unpin + Send + 'static`。

### Q2. cert 与 PSK 谁优先？

**决策：本 phase 只做 cert；PSK 拆 V-followup-tls-PSK。**

理由：
- cert wrap 100% 是 tokio-rustls 标准 API（`TlsAcceptor::accept`），代码 ~50 LOC
- PSK 需要 PSKI 格式解析 + HKDF-Expand-Label NVMe labels + rustls PSK callback ~400 LOC

### Q3. tokio-rustls vs 直接 rustls？

**决策：tokio-rustls 0.26（基于 rustls 0.23）。**

- 已 wrap 好 `TlsAcceptor::accept(TcpStream) -> Future<TlsStream<TcpStream>>`
- 直接 rustls 要手摇 `read_tls/write_tls + process_new_packets` ~150 LOC

### Q4. 配置 surface 怎么暴露？

**决策：纯 CLI flag，不入新 config file。**

```bash
nvme_of_tcp_target \
    --listen 127.0.0.1:4420 \                # plaintext port（保留）
    --tls-listen 127.0.0.1:8009 \            # TLS port（spec 推荐 8009）
    --tls-cert /etc/nvmeof/server.pem \      # X.509 cert PEM
    --tls-key  /etc/nvmeof/server.key \      # PKCS#8 key PEM
    --tls-i-trust-this-cert \                # explicit 双 consent
    --backing-file disk.img
```

### Q5. TLS 失败时 fallback 行为？

**决策：TLS port 不 fallback plaintext；handshake fail 直接 drop。**

理由：静默 fallback 是 protocol downgrade 漏洞模式。

### Q6. tokio-rustls 与 `await_holding_lock` lint 是否冲突？

**决策：不冲突。lint 在 session 层；TLS handshake 在 bin 层，与 controller 锁完全错开。**

### Q7. 测试矩阵怎么覆盖 cert lifecycle？

**决策：dev-dep rcgen 0.13 在测试启动时生成 self-signed cert 到 tempdir。**

---

## 4. 分阶段拆分

### V-followup-tls-1：stream 抽象泛化（**不引 rustls dep**）

- **目标**：把 `AsyncSession.stream`、`ic_handshake_async`、`accept_and_handshake_async`、内部 helper 从 `TokioStream` 改泛型；仍只跑 `tokio::net::TcpStream`，197 baseline 全过
- **改动文件**：
  - `src/async_session.rs`：`AsyncSession<S>` 加泛型；所有 `pub async fn` 加 `S` bound
  - `src/lib.rs`：bin 用 `AsyncSession<tokio::net::TcpStream>`
- **不改**：framing.rs（已泛型）、session.rs、所有 V8b/c/d/f sync test
- **测试**：`tests/vt_tls_1_stream_generic_smoke.rs`
  - `vt_tls_1_async_session_compiles_with_tokio_duplex`：tokio::io::duplex pair 跑 ICReq/ICResp
- **LOC 预算**：impl ≤ 80 / test ≤ 120

### V-followup-tls-2：rustls dep + 教学版 TlsAcceptor builder

- **目标**：Cargo.toml 加 tokio-rustls / rustls / rustls-pemfile / rcgen(dev)；新 module `src/tls.rs` 暴露 `build_acceptor_from_pem(cert_path, key_path) -> Result<TlsAcceptor>`
- **改动文件**：
  - `Cargo.toml`：加 rustls 0.23 / tokio-rustls 0.26 / rustls-pemfile 2 / dev-dep rcgen 0.13
  - `src/tls.rs`（新）：`build_acceptor_from_pem`
  - `src/lib.rs`：`pub mod tls;`
- **测试**：`tests/vt_tls_2_acceptor_build.rs`
  - happy path / missing key / malformed PEM 三测
- **LOC 预算**：impl ≤ 120 / test ≤ 150

### V-followup-tls-3：bin 接入 dual-listener (TLS + plaintext)

- **目标**：bin 加 CLI flag；启动时 build acceptor + spawn 第二条 `run_accept_loop_tls`
- **改动文件**：
  - `src/bin/nvme_of_tcp_target.rs`：
    - Cli 加 4 个 TLS field
    - 启动时 validate + load acceptor + spawn 第二 listener
    - `run_accept_loop_tls`：accept 后 `tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept)` + `handle_conn_async_tls`
    - `handle_conn_async_tls`：与 `handle_conn_async` 体一致，stream 类型不同（泛型 monomorphize）
- **测试**：
  - `tests/vt_tls_3_bin_dual_listener.rs`（bin smoke）：3 测（启 / 缺 cert fail / 缺 consent fail）
  - `tests/vt_tls_3_handshake_e2e.rs`（lib level）：`handshake_then_icreq_roundtrip`
- **LOC 预算**：impl ≤ 250 / test ≤ 200

### V-followup-tls-4：byte-identical gate + 文档

- **目标**：补 `vt_tls_4_app_layer_bytes_identical`；README + lib.rs doc 更新
- **改动文件**：
  - `tests/vt_tls_4_app_layer_bytes_identical.rs`：plaintext / TLS-wrapped 同 input PDU 序列下应用层 bytes 等价
  - `README.md`：加 "## TLS 教学开关" 章节
  - `src/lib.rs`：phase log 加 V-followup-tls；caveat 去掉 "无 TLS"
- **LOC 预算**：impl ≤ 30 / test ≤ 100

---

## 5. 测试矩阵

| 测试 ID | 类型 | 目的 | Phase |
|---------|------|------|-------|
| `vt_tls_1_async_session_compiles_with_tokio_duplex` | lib | 泛型 stream 跑 in-memory duplex | tls-1 |
| `vt_tls_2_acceptor_built_from_rcgen_self_signed` | lib | TlsAcceptor builder 正路径 | tls-2 |
| `vt_tls_2_acceptor_rejects_missing_key_file` | lib | bad input 路径 | tls-2 |
| `vt_tls_2_acceptor_rejects_malformed_pem` | lib | bad input 路径 | tls-2 |
| `vt_tls_3_handshake_then_icreq_roundtrip` | lib | TLS handshake → ICReq/ICResp e2e | tls-3 |
| `vt_tls_3_bin_starts_with_tls_listen` | bin smoke | CLI 正常启动 | tls-3 |
| `vt_tls_3_bin_fails_without_cert` | bin smoke | CLI bad input → exit != 0 | tls-3 |
| `vt_tls_3_bin_fails_without_consent` | bin smoke | 缺 explicit consent → exit != 0 | tls-3 |
| `vt_tls_4_app_layer_bytes_identical` | lib | TLS-wrapped wire 等价 plaintext 应用层 | tls-4 |
| 现有 197 active test | — | regression gate 全过 | 每 phase |

---

## 6. 推荐执行顺序 + LOC + 估时

| 顺序 | Phase | impl LOC | test LOC | 估时 |
|------|-------|----------|----------|------|
| 1 | V-followup-tls-1（stream 泛化） | ≤ 80 | ≤ 120 | 1.5 h |
| 2 | V-followup-tls-2（rustls dep + acceptor builder） | ≤ 120 | ≤ 150 | 2 h |
| 3 | V-followup-tls-3（bin dual-listener + e2e） | ≤ 250 | ≤ 200 | 3 h |
| 4 | V-followup-tls-4（byte-identical gate + 文档） | ≤ 30 | ≤ 100 | 1 h |

**合计：impl ≤ 480 / test ≤ 570 / 7.5 h 估时**

每 phase 末：`cargo fmt && cargo clippy && cargo test --all-targets` → rust-reviewer → security-reviewer → commit。

---

## 7. Definition of Done

- [ ] 4 个 sub-phase commit 全 push 上 `feat/pcie-remote-experimental`
- [ ] `cargo test --all-targets` 共 207 active test（197 baseline + 10 新）全过
- [ ] `cargo clippy --all-targets -- -D warnings` 0 warning
- [ ] `cargo fmt --check` clean
- [ ] `#![forbid(unsafe_code)]` 仍在；本 crate 0 unsafe block
- [ ] bin 启动 4 模式手测全过
- [ ] README 含完整 cert 生成步骤 + prod 警示 + PSK roadmap
- [ ] rust-reviewer + security-reviewer 对每个 commit 0 CRITICAL/HIGH
- [ ] 每个 commit message caveat 更新："TLS 1.3 server-auth (X.509) 已支持；
      in-band auth / mTLS / NQN identity binding / PSK 留 V-followup-auth"

---

## 8. 下次执行 hint

V-followup-TLS 完成后下一阶段优先级：

1. **V-followup-tls-PSK**（TP-8011 PSK + HKDF-Expand-Label NVMe labels）
2. **V-followup-auth**（NQN ↔ TLS identity binding；spec § 8.13 强制）
3. **V-followup-mtls**（client cert require）
4. **DH-HMAC-CHAP**（in-band auth，spec § 8.13.5）

执行起始命令：
```bash
git log --oneline -5  # 验 HEAD = afb90654
cd docs/superpowers/examples/nvme_of_tcp_target
cargo test --all-targets 2>&1 | tail -5  # 验 197 baseline
# 开始 V-followup-tls-1
```
