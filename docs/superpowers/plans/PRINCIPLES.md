# NVMe-oF TCP Target — 工程原则 (PRINCIPLES.md)

> 不可变约束。代码 review、commit、subagent 调用都按这些办；偏离需 explicit
> rationale。本文件为底层契约，**不随单 phase 变**；如需变更先在 PR 描述里
> 标 "principle change"，加入 MEMORY.md 重新让 reviewer pass。

## 1. 安全 / 安全 / 安全

1. **`#![forbid(unsafe_code)]`** crate-wide 永久维持。例外只能在引入新 dep 时
   出现（如 rustls 内部 unsafe）；本 crate 边界 0 unsafe block。
2. **`#![deny(clippy::await_holding_lock)]`** crate-wide。async 路径任何
   `std::sync::Mutex` / `parking_lot::Mutex` 跨 `.await` 必须借 `tokio::sync`
   或先 scope drop。V8e tokio refactor 教训。
3. **不绕 git hook**。`--no-verify` 永禁。reviewer signoff 才能 commit。
4. **`unwrap()` / `expect()`**：测试可用；prod 路径用 `?` + `.context()`；
   "infallible 但理论可能"用 `.expect("人话解释")`。
5. **HMAC / crypto key**：永远不放 source；用 `--host-secret` CLI flag 或文件
   挂载。不能截短 print（tracing log 抹 secret 至少前 8 byte）。

## 2. 可演化 / 可向后兼容

1. **wire format anchor 必用 `core::mem::offset_of!`**（不手算 packed struct 偏移）。
   教训：[[nvme-of-tcp-real-linux-interop-milestone]] 9 个 wire blocker 几乎全
   是手算 offset 错。任何 spec wire struct 至少配一个 anchor test。
2. **添 wire 字段**：增量加，不破已有 wire；用 `ChapWireMode::Unknown/Simplified/Spec4Msg`
   pattern 自动识别 + 锁定。
3. **删 / rename 公开 API**：分两 phase — 先加 `#[deprecated]` 别名（如
   `TlsPskHash::len` → `output_len`），下个 phase 再删。
4. **测试 anchor 不删**：reviewer L-2/L-3 加的 boundary test 视为 lock，未来
   改实现先红了 anchor test 才能改源。

## 3. 简化 vs 教学边界 (诚实标注)

1. **每个 "教学版简化" 必须在 doc 顶 / function doc 内**明写：
   - 简化掉的功能（如 "DHCHAP 不支持 DH ephemeral; HMAC-only"）
   - prod 路径如何接上（如 "若 host 实际发 DH-2048，本实现拒；spec-full 在
     V-spec-strict-mode"）
2. **TODO 必带定语**：`TODO(spec-full)` / `TODO(perf)` / `TODO(security)` /
   `TODO(reviewer L-N)`。裸 `TODO` 不接受。
3. **永远不假装 prod ready**：reviewer 标 "self-consistent only" 的就别说
   "interop ready"。

## 4. 测试纪律

1. **TDD-first** 用于 spec wire / crypto / 状态机；其他可后写 test。
2. **3 类 test**：
   - lib unit (in #[cfg(test)] mod)
   - integration (tests/vt_*.rs)
   - python interop (scripts/interop_py/*.py，跨进程实证)
3. **跨进程 e2e**：每个 spec wire 至少有一个 Python harness scenario，否则
   "lib test 通了" ≠ "wire 真对"。
4. **覆盖率 ≥ 80%** by `cargo llvm-cov` (待加 CI)；当前 manual count。
5. **prod 改动后 reviewer signoff**：每个 phase commit 前调 `ecc:rust-reviewer`
   + 至少修 H/M；L 可下一 phase。

## 5. Commit / PR / sub-agent

1. **commit 格式**：`feat(nvme-of-tcp): <Phase-NAME> — <subject>`，body 用
   中文 (因为 [[language-chinese-for-changes]])。
2. **trailer**：`Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
   （settings.json 全局 attribution disabled 但 nvme-of phase 显式带）。
3. **subagent reviewer 必跑**：`ecc:rust-reviewer` 改 Rust，`ecc:python-reviewer`
   改 Python，`ecc:security-reviewer` touch crypto / auth / TLS。详
   [[subagent-review-required]]。
4. **新 phase 前** 起 `essentials:planner` 或自写 `plans/YYYY-MM-DD-phase-N-detailed.md`
   先；不写 plan 直接 code 只允许 trivial 1-file fix。

## 6. 文件 / 模块

1. **单文件 ≤ 800 行**软上限；超 1000 行必须 split (`session.rs` 已超，待
   refactor — 当前默认接受)。
2. **新功能优先扩 existing module，不开新 crate**；nvme-of 教学课程统一在
   `docs/superpowers/examples/nvme_of_tcp_target/`。
3. **测试文件**：`tests/vt_<phase>_<topic>.rs`，便于 grep + 报告。
4. **公开 API doc**：`///` 必含 一句话 summary + 调用 example (若有) + 失败
   case。

## 7. 性能 / 资源

1. **不在 lib test pre-mature optimize**；spec 正确性 > 速度。
2. **零拷贝优先**：`bytes::Bytes` / `zerocopy::IntoBytes` 用，不 `.to_vec()`
   除非反序列化。
3. **MDTS** = 5（128 KiB），与 V_HOST_IO_NLB_MAX=256 + V5_NLB_MAX=16 + 透明
   chunking 一致。改任一都先改 anchor test (`v_interop_3_mdts_*`,
   `v_prp_list_anchor_constants`)。

## 8. 文档 (本目录)

- 本文 (PRINCIPLES) — 不变约束
- `ROADMAP.md` — 动态短中长期 phase 列表
- `LESSONS.md` — 教训 + 经验 (踩过坑后)
- `DECISIONS.md` — 重大决策日志 (ADR 风格)
- `2026-XX-YY-phase-*-detailed.md` — 各 phase 实施细节
- `2026-XX-YY-phase-*-survey.md` — 调研类（如 tls-psk-survey）

进新文档前先看本目录有没有合适 host；不为单次工作开新顶层文件夹。

## 9. Subagent reviewer prompt 工程

reviewer 抓 H 级 bug 的概率，正比于 prompt 给的 context 量。**最低 prompt
模板**：

```
Review the <Phase-NAME> change for <module>.

Working directory: <abs path>

Files changed:
- <path1> (+N/-M)
- <path2> (...)
- <path3> (new, ~N LOC)

Context:
- <一句话说本 phase 做什么>
- 后向兼容策略 (如自动检测 / sentinel / 后向 wire mode)
- 跨语言/进程 anchor (如 Linux kernel struct nvmf_*)
- 测试 / clippy 状态 (\"N tests pass + clippy 0 warning\")
- Crate policies (forbid_unsafe / await_holding_lock / 中文注释)

Run `git diff HEAD -- <paths>` to see the full diff.

Review specifically for:
1. <wire/算法 correctness specific question>
2. <state machine / borrow / lifetime specific question>
3. <error path coverage>
4. <edge case worth thinking about, e.g. 1/65536 collision>
5. <test gaps>
6. <doc accuracy>

Output findings by severity (CRITICAL/HIGH/MEDIUM/LOW). Be terse.
Skip nitpicks; teaching module status is acceptable for <X>.
```

**关键 gotcha**：
- 不要只说 "review the diff"；明列 5-6 个 specific question
- 把 *你已经知道的边缘 case* (如 wire mode 自动识别 collision 概率) 给
  reviewer，否则他要重新算
- 教学 vs prod 边界 explicitly 标，否则 reviewer 把 "教学版简化" 当
  CRITICAL

**来源**：V-dhchap-4 + V-tls-psk reviewer 两次都抓到 H 级 issue，对照
prompt 内的 specific question；之前同模板 reviewer 只抓 L。

## 10. 测试命名约定

| 前缀 | 含义 | 例 |
|------|------|------|
| `vt_<phase>_<topic>` | unit test in lib (#[cfg(test)] mod) | `vt_dhchap4_negotiate_parse_happy` |
| `v_<topic>_anchor_<what>` | spec layout / 不变量 anchor test | `v_prp_list_anchor_constants` |
| `<phase>_<topic>_dispatch_arm_<N>` | dispatch table 第 N 臂覆盖 | `vt_tlspsk_e2e_self_consistent_sha384` |
| `<phase>_kernel_ci_vector_<N>` | 真 kernel 输出 anchor (待补) | `vt_tlspsk_kernel_ci_vector_1_sha256` |
| `<phase>_regression_<bug>` | 修过的 bug regression | (TODO 用) |

集成 test 文件 `tests/vt_<phase>_<topic>.rs`：每个 phase / 子 phase 一个
文件便于 grep + 报告。Python harness `scripts/interop_py/<topic>.py`，
名字描述 *验什么 wire*，不描述实现。
