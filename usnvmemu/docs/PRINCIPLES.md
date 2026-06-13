# usnvmemu — 工程原则 (PRINCIPLES.md, 项目级)

> 不可变约束。代码 review、commit、subagent 调用都按这些办；偏离需 explicit
> rationale。本文件为底层契约，**不随单 phase 变**；如需变更先在 PR 描述里
> 标 "principle change"，加入 MEMORY.md 重新让 reviewer pass。
>
> **两类寄存器**：§1–§8 是**可机械二值核对的硬契约**（有没有 unsafe / anchor /
> 格式对不对，reviewer 直接卡）；§9·§11 是 **review 工程与设计追问（指导，非
> commit-gate 闸门）**——需工程判断、不二值核对、不作 pass/fail 卡点。别拿 §11
> 当硬闸门，也别把硬契约写成"启发"。

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
   `usnvmemu/crates/nvme_of_tcp_target/`。
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

**最该问的设计追问见 §11「设计 review 追问清单」**——那 4 条是 specific question 里
反复证明能抓 H 级的设计判断（cap 界定 / 抽象前数实现 / 结构-简化公因子 / 独立 oracle 判据）。

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

## 11. 设计 review 追问清单 (判断项，非二值闸门)

> 本节与 §1–§8 硬契约不同：这 4 条是 review（尤其对抗 review）时必问的**设计判断**，
> 不可机械二值判定、不作 commit-gate，只作"思考清单"——是 §9 reviewer prompt 工程里
> 最该问的 specific question。每条把承重判别句留这里，事故全文锚回 LESSONS（按**标题/
> 命名**引，不按易随插条重排而悬挂的 §号，见 [LESSONS §18]）。

1. **防御性界限：界定"对的量" + 必证能触发**
   任何 cap / timeout / 熔断 / 重试上限，先问"**要挡的坏情况与正常负载，在哪个量上分得开**"，
   界定那个量——挡错量必误伤真负载（反例：挡吞吐推进→真高吞吐 driver 稳态合法推进无界、永远撞；
   挡环距离→振荡 wrap-back 也算前向距离，cap 单调不可触发 = dead）。写完必须证它**真能 fire**：
   dead cap = 装了个永不响的保险，且没人发现。三个量的试错全程见 LESSONS『自馈 async poll 循环』。

2. **抽象前数实现 / 对称性常是伪需求**
   抽 trait / enum variant 前问"**现有实现里几个真用得上这个 variant**"；不足 2 个用得上不证成
   抽象（trait 版 YAGNI）。对称性多是伪需求——read 必须同步返值、write 不返，硬塞一个 enum 等于用
   `Option` 把编译期保证换成运行期 match。见 LESSONS『对称性是伪需求』。

3. **结构修 vs 简化的公因子：让错误状态无从表达**
   两条看似一个要简化、一个要加结构，公因子是同一句：**让正确性内建到数据 / 控制流的形状里，
   使错误状态无从表达**。
   - 对抗 review BLOCK 时 → 默认怀疑"设计太绕"，优先 **hoist / 换更简单设计消掉对抗态**（减代码），
     不是再加一层处理。见 LESSONS『review BLOCK 的对抗-case bug，常是"换更简单的设计"』。
   - 同一易错决策在 **N 处重复**（手填同参数 / 同盲点跨调用点）→ 是**一个**结构缺口的 N 个实例，
     把决策**移进数据 / 类型**让调用点无从填错。见 LESSONS『API footgun（每个调用点手填同一参数）→ 结构性修』。
     - **判别线（仅约束上一条"N 处重复→结构化"，防与第 2 条打架）**：**有 ≥N 处真实重复才结构化；
       两家以下是投机抽象，按第 2 条拒**。
   - 注：[ADR-013](DECISIONS.md) 是"补 oracle + CI gate"的**平行**结构修，与本条"决策移进数据/类型"
     **不同构**（ADR-013 自身显式切割），不是本条子例。

4. **判据来自独立 oracle，还是自家两端约定自证？**
   写任何 wire 匹配 / SAFETY 不变量 / 测试判据，问"这判据**来自独立 oracle**（协议字段如
   `is_reply()` / `fstat` 真相 / 第三方实现 / 另一条代码路径），**还是不可信输入 / 我自己两端
   共享的编号范围·顶位·默契自证**？"——后者一律换前者。自写 server+client / 自写两端测试共享的
   错误假设两边都不报错、单测全绿，catch 它的只有独立第二实现。
   - *"独立第二实现才能 catch、按可 CI 性分档做 standing gate"那半已升为结构律——见
     [ADR-013](DECISIONS.md)；本条保留的是"写码判据用协议字段不用自家默契"这半（ADR-013 未覆盖）。
     案例见 LESSONS『self-consistent 假设当 wire 判据是反模式』『unsafe 的 SAFETY 判据必须用独立 oracle』。*

> **增长触发**：本节超 ~8 条即拆出独立 `HEURISTICS.md`；当前 4 条 + 未来候选（§13 多分支
> dispatch 每臂 anchor / §23 跨锁读的值必在终用它的锁内复读）本目录已是合适 host，暂不另开顶层
> 文件（[§8]）。新增条目走 §11 末『LESSONS → 蒸馏槽位』机制，门槛 = ≥2 独立场景复现。
