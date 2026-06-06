# NVMe-oF TCP Target — 后续开发 Roadmap (动态文档)

> **维护策略**: 本文件 phase 完成时即时更新；废弃 phase 标 ~~strikethrough~~；
> 新 phase 按 "what / why / acceptance / blockers / commit links" 模板加。

> **最后更新**: 2026-06-06 (V-followup-dhchap-4d + V-interop-8 + tls-psk-survey 完成后)

## 0. 当前坐标

```
V1 ──> V2 ──> V3 ──> V4abc ──> V5abcd ──> V5e/f-followup ──> V6abc ──> V7abc ──> V8a-f ──> V8e tokio ──>
V-followup-tls(1..4) ──> V-followup-mtls ──> V-followup-auth(1..2) ──> V-followup-dhchap(1..3,3-wire) ──>
V-followup-interop(1..7) ──> V-followup-prp-list ──> V-followup-dhchap-4 + 4d ──> V-interop-8 ──> [HERE]
```

**当前状态**：
- 306 lib + integration tests pass，clippy 0 warning，#![forbid(unsafe_code)] + #![deny(clippy::await_holding_lock)] 维持。
- Linux nvme-cli plaintext discover + connect + IO 互通已实证。
- DHCHAP simplified wire + spec § 8.13.5 4-message wire 都通，多 descriptor 兼容。
- TLS / mTLS / NQN<->cert binding 端到端 (server-auth 全栈)。
- TLS PSK TP-8011 deterministic crypto 已落，**rustls 注入待上游**。

## 1. 短期 (1-3 phase, 不依赖上游)

### V-followup-dhchap-4-real-host-interop (HIGH 优先, 预计 1 day)

**What**：让 V-interop-8 Python harness 通过即代表 Linux nvme-cli `--dhchap-secret` 也通；
现在还缺一步：跑真 nvme-cli 而非 Python harness。

**Why**：Python harness 是我们自家算法对自家算法 (compute_response 两边都用同实现)，
真互通必须找 third-party host 验。

**Acceptance**：
- `sudo nvme connect -t tcp -a 127.0.0.1 -s 4420 -n <subnqn> --hostnqn <hostnqn> --dhchap-secret DHHC-1:01:<base64key>:` 成功
- nvme-cli output 显示 "Connection established"
- 之后 `nvme list` 看得见 namespace

**Blockers**：
- Linux 6.6 WSL2 默认 kernel.modules 路径，nvme-cli 用户态 + nvme-tcp.ko 模块版本兼容
- DHHC-1 base64 key 格式 (kernel 用 ":sha256:" / ":sha384:" suffix)，文档化

**Links**：commit `714029df`, plans/2026-06-05-phase-v4-detailed.md §9 (entrypoint hint)。

### V-followup-tls-psk-kernel-vector (HIGH 优先, 预计 2 days, 独立于 rustls)

**What**：写 out-of-tree kernel module dump `nvme_auth_derive_tls_psk` 输出，固化
5 个 known-good (retained_psk, hostnqn, subsysnqn, hash, expected_tls_psk_hex)
五元组到 `src/tls_psk.rs::vt_tlspsk_kernel_ci_vectors_*` 测试。

**Why**：当前 13 tls_psk tests 全是 self-consistent，对自身 deterministic 但
*没有 anchor 到 Linux 真实输出*；任何 silent algorithm drift 都不会被发现。

**Acceptance**：
- `tls_psk.rs` 加 `vt_tlspsk_kernel_ci_vector_{1..5}_sha256` + `_{1,2}_sha384`
- 每个 test 用 hard-coded inputs 算出和 kernel 一致的 raw TLS PSK byte 串
- 之后任何 `tls_psk.rs` 代码变更必须 *先* 改 vector (CI red) *再* 改实现

**Blockers**：
- 需要 host root + 写小 kmod (~ 50 LOC)，用户须授权或代提
- WSL2 kernel 已开 `nvme-tcp` / `nvme-auth` 内置；EXPORT_SYMBOL_GPL OK

### V-followup-discovery-multi-portal-real (MEDIUM, 预计 1 day)

**What**：实测 V7 / V8a `--discovery-target-addr` 重复指定多 portal，nvme-cli
discover 真能拿到多 entry。

**Why**：V7c-fix 是 lib test pass + reviewer 改的 CNTRLTYPE，但当时只测单
portal；多 portal 走 [[nvme-of-tcp-real-linux-interop-milestone]] 验过。

**Acceptance**：起 target with `--discovery-target-addr A:p1 -p A:p2 -p A:p3`，
`nvme discover -t tcp -a A -s 4420` 输出 3 个 entry，每个 NQN/IP/Port 正确。

### V-followup-py-harness-spec-wire-conformance (MEDIUM, 预计半 day)

**What**：把 `chap4_spec_wire_e2e.py` 扩到覆盖 reviewer M-4 那 5 个 case
(REPLY before challenge / REPLY truncation / tid mismatch / SUCCESS2 before
auth / FAILURE2 from host)，对齐 lib test。

**Why**：lib test 覆盖了，但 Python harness 缺；跨进程实证 wire 错误路径才能
保 Linux nvme-cli 拿到正确 FAILURE1 diagnostic。

## 2. 中期 (3-6 phase, 部分依赖上游)

### V-followup-tls-psk-rustls-wire (HIGH, depends rustls upstream)

**What**：rustls 出 stable external-PSK API 后立刻接：
- `src/tls.rs::build_acceptor_with_psk(store: PskStore) -> TlsAcceptor`
- `PskStore = HashMap<(Hostnqn, Subsysnqn), (raw_psk, hash)>`
- rustls PSK callback 内调 `tls_psk::{generate_psk_digest, derive_tls_psk, build_psk_identity}`
- `tests/vt_tls_psk_5_*` 系列 e2e (类似 V-followup-tls-3 的 5 个 file)
- `scripts/interop_py/tls_psk_e2e.py` 用 nvme-cli `--tls`

**Blocker**：rustls upstream（详 [tls-psk-survey](2026-06-06-phase-v-followup-tls-psk-survey.md)）。

### V-followup-fused-cmd (MEDIUM)

**What**：实现 NVMe Fused Compare-and-Write (CW + W 双 cmd 原子)；spec § 6.2。

**Why**：[[nvme-userspace-production-ready]] 教学 controller 标 "Fused detect"
但没真 atomic；nvme-of target 现在只对 hexadecimal opcode 0x06+0x09 拒。

### V-followup-zoned-namespace (LOW, 大块工作)

**What**：ZNS namespace support — Zone Append (0x7d) + Get Zone Receive Status。
需 backing layer 支持 zone state machine。

### V-followup-fabric-disconnect-real-interop (MEDIUM)

**What**：V8c Disconnect 真 Linux 实测；当前只测了 Python e2e。

## 3. 长期 (6+ phase, 架构级)

### V9 — RDMA Transport (HUGE, 6+ months)

**What**：spec § 5.13 NVMe-oF RDMA Transport；新 transport module `src/rdma_*`，
不复用 tcp_transport。

**Why**：spec 三大 fabric (TCP / RDMA / FC)，TCP 已 done；RDMA 是性能 ceiling。

**Architectural notes**：
- 不能复用 `framing.rs` (RDMA 不走 PDU)
- 需引 `rdma-core` Rust binding 或 fork ibverbs-sys
- AsyncSession 抽象层要再泛化 (TCP PDU vs RDMA SendRecv)
- 测试基础设施重写 (soft-RoCE in WSL2)

### V10 — DMA Backend (HUGE)

**What**：替换 `pcie_remote_nvme_userspace` 教学 controller 的 file-backed
storage 到真 PCIe NVMe device，把 nvme-of target 变成 NVMe-oF JBOD gateway。

### V-spec-strict-mode (MEDIUM-LARGE)

**What**：增 `--strict-spec` flag 关掉所有 "教学版简化"，全跑 spec：
- DHCHAP DH ephemeral key exchange (DH-2048/4096/6144/8192)
- bidirectional CHAP auth (mutual auth + host-verify rval)
- secure channel concatenation (sc_c=1)
- spec-conformant Identify NS LBA Format full set
- ANA group state machine (ANAGRPID > 1)

**Why**：教学路径方便实验 + 简化，但 prod 部署必须 strict。

## 4. 当前 PRINCIPLES.md / LESSONS.md 入口

- 不可变约束 / coding policy → [PRINCIPLES.md](PRINCIPLES.md)
- 踩过的坑 + 经验 → [LESSONS.md](LESSONS.md)
- TLS PSK 调研 → [2026-06-06-phase-v-followup-tls-psk-survey.md](2026-06-06-phase-v-followup-tls-psk-survey.md)
- 各 phase 详 spec → `2026-06-0*-phase-*-detailed.md`

## 5. 流程提示 (本文件如何用)

- **下一个 phase 起手**：先 read 本 ROADMAP 找 HIGH 优先未 done 的；如果跨多
  phase 互相依赖，跑 `essentials:planner` 出 sprint plan
- **完成 phase 后**：本文件加 commit hash + acceptance evidence；MEMORY.md
  写 one-liner pointer；删 todo
- **新发现 phase**：写在合适优先段（短期/中期/长期），不堆"杂项"
- **废弃 phase**：strikethrough 不删，留 rationale 句
