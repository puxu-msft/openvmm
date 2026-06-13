# usnvmemu — 重大决策日志 (ADR, 项目级)

> Architecture Decision Records, 时间倒序。每条捕获 "我们曾在两条路径间纠
> 结，最终选 X 而不选 Y，因为 ..." 类决策，未来回看不必重新算 cost-benefit。
>
> **两层制 (ADR-011)**：本文件只留**跨切面 / 架构级**决策；有明确归属 crate 的
> ADR 下放到 `crates/<X>/docs/DECISIONS.md`。下表是全 ADR 索引，保证跨切面可发现性。
>
> 模板：每条决策 → 日期 / Context / Options / Decision / Consequences /
> Status (active / superseded / revisited)。

## ADR 索引 (全项目)

| ADR | 决策 | 归属 / 位置 |
|-----|------|------------|
| 001 | PCIe Remote Path C (OpenHCL VTL2 + vsock) | [pcie-remote-phase](/usnvmemu/docs/pcie-remote-phase/DECISIONS.md) |
| 002 | Phase T 抽 `trait Transport` | [pcie_device_sdk](/usnvmemu/crates/pcie_device_sdk/docs/DECISIONS.md) |
| 003 | V8e tokio runtime + AsyncSession | [nvme_of_tcp_target](/usnvmemu/crates/nvme_of_tcp_target/docs/DECISIONS.md) |
| 004 | Python interop 不走 sudo nvme-cli | [nvme_of_tcp_target](/usnvmemu/crates/nvme_of_tcp_target/docs/DECISIONS.md) |
| 005 | DHCHAP wire 双模式共存 | [nvme_of_tcp_target](/usnvmemu/crates/nvme_of_tcp_target/docs/DECISIONS.md) |
| 006 | prp-list 走 session-level chunking | [nvme_of_tcp_target](/usnvmemu/crates/nvme_of_tcp_target/docs/DECISIONS.md) |
| 007 | TP-8011 PSK 不 fork rustls，等上游 | [nvme_of_tcp_target](/usnvmemu/crates/nvme_of_tcp_target/docs/DECISIONS.md) |
| 008 | Phase X 外部化路径 C，预设但不立即执行 | **本文件** ↓ |
| 009 | Firmware-as-core 愿景确认 + 命名重构 | **本文件** ↓ |
| 010 | Phase W：pcie_device_sdk 补全 hexagonal | [pcie_device_sdk](/usnvmemu/crates/pcie_device_sdk/docs/DECISIONS.md) |
| 011 | 文档架构改两层制 | **本文件** ↓ |
| 012 | Transport 三层模型 + control/data plane + 命名裁定 | **本文件** ↓ |
| 013 | Per-transport 独立-oracle 覆盖矩阵 + 结构律 | **本文件** ↓ |

---

## ADR-013 — Per-transport 独立-oracle 覆盖矩阵 + 结构律 (锐化 ADR-009 Tier-1) (2026-06-13)

**Context**：[LESSONS](/usnvmemu/docs/LESSONS.md) §2/§20/§26/§28/§29 同一根——**便宜 /
happy-path / self-consistent harness 绿 = 结构性假安心,真 bug 全藏在没被走的路径上**。
五次都靠"另一条代码路径"(真 QEMU / libvfio-user / 真内核 nvme-cli / 真 guest /
deterministic interleaving 单测)才 catch。最承重的 lesson 是 [§20](/usnvmemu/docs/LESSONS.md)
**"self-consistent 假设当 wire 判据是反模式——自写两端共享错误假设两边都不报错"**;成因:
测试金字塔中下层(lib / proptest / Python harness)多是自写两端。(§27 只是借其判断启发——
"同一现象在 N 处重复,先问是不是一个结构缺口";但 §27 的修法是把决策移进**数据/类型**,
与本 ADR 的"补独立-oracle 覆盖 + CI gate"机制**不同构**,不引为机制依据。)

[ADR-009](#adr-009) Tier-1 已把"3 接入各 1 真 host e2e"提为目标,但留两处缺陷:
①三件事(CHAP real-host / vfio-user QEMU / kernel-CI vector)散成三个零散 backlog 项,
掩盖了"它们是**同一个缺口的三实例**"这一本质;②把"真 host e2e"当**单一形态**,忽略独立
oracle 是一个**按『可否每-commit 无人值守 standing』分档的家族**——特权/真机 oracle(真
Windows guest / `sudo nvme connect` / kmod dump)、乃至非特权但过重的真 QEMU 全-e2e,都进
不了 standing gate(本仓 [RUNBOOK_HOST_ROOT](/usnvmemu/docs/RUNBOOK_HOST_ROOT.md) 与各项
Blocker 已写明),不能与轻量 deterministic oracle 齐头并进。

**Options**：
- A: 维持三个零散 HIGH,各自推进 — 否。掩盖"一个缺口"本质;且把不可无人值守的 oracle
  错当可 gate,排期上反复撞同一堵墙。
- B: 把"每 transport 真 guest / 真机 e2e"统一提为硬 standing CI gate — 否。重/特权 oracle
  无法无人值守,硬提只会变成永红或永跳的**假门**,比没有更坏。
- C: **一条结构律 + 按『可否 standing』分三档的 oracle 矩阵**(判别轴是可 CI 性,不是
  userspace-vs-特权)。

**Decision**：选 **C**。

**结构律**：任何 transport 的**数据/wire 路径**,不得在缺少**至少一个『另一条代码路径』
独立 oracle 作为 standing CI gate** 的情况下视为"完成";oracle 取该路径**可 standing 化的
最强档**(下)。"独立"= 判据来自另一条代码路径 / 第三方实现 / 内核真相,**不是**自家两端
共享的约定(§20)。
- **"数据/wire 路径"定义**(消歧,挂 [ADR-012](#adr-012) 的 control/data plane 划分)：指该
  transport 上**任何承载语义的 wire 交互**——**含 control plane**(握手 / 寄存器 MMIO / 中断
  setup / queue 创建),不只 IO payload。理由:§28 的 4 bug 多藏 control 边角(posted-write
  失步 / MSI-X cap / DBBUF doorbell),窄解成"只 IO data"会把律自己引用的 bug 类漏在覆盖外。
- **横切子条款(不绑某 transport)**:任何自写 harness 的 **guest-memory 模型须能生成 ≥4 GiB /
  高地址**(§26),否则 flat 小内存结构性排除"64-bit 高半截断"整类 bug。此为对**所有** transport
  自写 harness 的承重条款,因 §26 根因在 firmware-core(`controller/mmio.rs`)、是跨 transport
  共性盲区,只是碰巧由某 transport 真 guest 先撞到。

**oracle 三档(判别轴 = 可否每-commit 无人值守 standing)**：
1. **standing gate**(deterministic + 快 + 无外部进程/root 依赖)：firmware-core 共享的
   interleaving 单测(§29)/ mmio qword size-aware(§26)/ prp 单测、以及 userspace 轻量差分或
   wire harness(libvfio-user differential、Python CHAP / 纯-4K wire、conformance vectors)。
   → **必须进 `cargo test` 或 CI standing gate**。注:firmware-core 单测对**所有** transport
   一次性生效,记在矩阵的 core 行,不重复挂各 transport 行。
2. **frozen golden vector**(重/特权 oracle 的 standing 形态)：真内核 / 真机 / 重 e2e 跑**一次**
   dump → 冻成 hard-coded vector → 代码改必须**先改 vector(CI red)再改实现**。`tls-psk-kernel-
   vector` 即此档范本——它不是"三症状之一",是本 ADR 对**不可无人值守捕获的 oracle**的标准解。
   → 冻结值进 CI gate。**承重款(防 frozen vector 腐烂成 §20 式假安心)**:每个 vector 必带
   **provenance 注释**(source kernel/QEMU 版本 + 生成命令 + dump 日期);定 **refresh 触发器**
   (目标内核/QEMU major bump 时 + 一个 cadence 兜底);**refresh owner = 谁持特权环境谁负责**,
   挂进 RUNBOOK。仅"CI red 先改 vector"只防己方实现 drift,**防不住上游真相漂移**——那一半正是
   §20/§26 反复栽的坑,必须由这三款补上。
3. **cadence live oracle**(不计 standing gate)：跑真重家伙——真 Windows Hyper-V guest boot
   (§26 >4 GiB / §28 真 driver 4-bug)、`sudo nvme connect`(dhchap-4)、真 QEMU 全-guest-boot
   e2e。按"为何当不了 standing"再分:**3a 特权**(需 root/Windows/kmod)/ **3b 重但非特权**
   (真 QEMU e2e,纯 userspace 但起整机有时长/flakiness)。两者皆:以 **cadence + RUNBOOK 兜底**,
   且**必须产出一个档-2 frozen vector 作为 CI 代理**,否则该路径在 CI 里裸奔。

**Consequences**：
- ROADMAP Tier-1 加「Oracle 覆盖矩阵」元项(含 firmware-core 行 + 三 transport 行);三个现有
  V-followup(dhchap-4 / tls-psk-vector / vfio real-guest-boot)**降为该元项的执行实例**(交叉
  引用保留 runbook / blocker 执行细节,不删)。
- **立即可做(从一次性手跑升成 standing gate)**:libvfio-user differential CI job(需
  `libjson-c-dev`,无 root)、Python CHAP / 纯-4K wire conformance 入 CI。这些档-1 oracle 早已
  存在却只一次性跑过——升 gate 是本 ADR 最廉价的兑现。
- **本 ADR 自身推出的两个缺口**(矩阵据律标 ⛔):vfio 已有档-3 live(`run_qemu_vfio_guest.py`,
  §28)却**欠档-2 frozen 代理**;OpenHCL L3 真 guest 同样欠代理。"凡有档3 必配档2"是律,这两格
  的空白是 violation,不是"不适用"。
- **新 transport 落地 checklist**:列三档各自 oracle;档-1 必 gate;凡有档-3 路径必配档-2 frozen
  代理;自写 harness 满足 ≥4 GiB 子条款。并入 [HOW_TO_ADD_TRANSPORT](/usnvmemu/docs/HOW_TO_ADD_TRANSPORT.md)。
- 相对 ADR-009 的核心增量:从"枚举三个目标"到"结构律 + 路径定义 + 分档执行机制 + 自动暴露缺口"。
- **revisit**:当 real-guest-boot CI(如 Windows runner)或 WSL2 `CONFIG_NVME_AUTH` 可得时,把
  对应档-3 升为档-1/2,矩阵相应收紧。

> **⚠ 承重发现(2026-06-13,执行首个 vfio oracle 的 POC 时坐实,纠正本 ADR 的一个隐含错误前提)**：
> **usnvmemu 全 9 crate 在根 `Cargo.toml` 的 `[workspace.exclude]`**,故 openvmm CI 的
> `cargo test --workspace`(flowey `TestPackages::Workspace{exclude}`)**不含任何 usnvmemu crate**,
> `.github/` 0 处提及 usnvmemu —— 即 **usnvmemu 当前无任何 standing CI gate**,现有所有测试(含
> firmware-core §29/§26 单测、openhcl/vfio 跨进程 e2e)都只是 `cargo test` 手动 cadence 可跑。
> 这意味着本 ADR 的"档1 必须进 cargo test 或 **CI standing gate**"中的后半截对 usnvmemu **尚无载体**。
> 反讽:此前起草本 ADR 时假设"usnvmemu 测试已骑 workspace nextest 门"而未核验 workspace 成员——
> **正是本 ADR 要打击的「假设 gate 覆盖你却没验证」(§20/§28 元层面)**。**新增前置条款**:把任一已写
> 独立 oracle 变成真正的 standing gate,先决条件 = **建一个 usnvmemu 专属 CI workflow**(`cd usnvmemu/
> crates/<X> && cargo test`,toolchain 钉 1.95;exclude 是 [ADR-008](#adr-008) 仓库外置意图,不宜并入
> members)。在该 CI gate 落地前,矩阵的"standing"维度对全 usnvmemu **记为未实现**(独立性维度照常推进)。

**Status**：active;**锐化** [ADR-009](#adr-009) Tier-1(不取代,补其执行机制与分层;ADR-009
Tier-1 执行细则回指本 ADR)。

---

## ADR-011 — 文档架构改两层制 (项目级 + crate 级) (2026-06-08)

**Context**：`DECISIONS.md` / `PRINCIPLES.md` / `LESSONS.md` 三份顶层 dev-doc 出身为
NVMe-oF TCP target 的开发文档（标题仍挂 "NVMe-oF TCP Target —"），随项目长成
"用户态 NVMe firmware + 3 transport" 后吸收了全项目内容，造成杂糅：
- `DECISIONS.md` 混着 5 条 NVMe-oF **本地** ADR (003-007) + 5 条**跨切面** ADR
  (001/002/008/009/010)。
- `PRINCIPLES.md` / `LESSONS.md` 标题挂 "NVMe-oF" 却装着**通用工程**内容
  (不手算 offset / reviewer 必跑 / async 借用 …)，适用所有 crate。

各 crate 的 `plans/` `specs/` 早已分好（2026-06-08 重组），问题只在顶层 4 份。

**Options**：
- A: 全部按 crate 拆 — 否。会把通用工程教训散落各 crate，普适知识反而找不到、会重复。
- B: 维持单层项目级 — 否。crate 本地决策 (tokio / CHAP / chunking) 混在项目日志里是噪音。
- C: **两层制**，按 scope 分：项目级只留不可再归属的跨切面；有明确归属 crate 的下放。

**Decision**：选 **C**。
- **项目级** `docs/`：PROJECT_VISION / ROADMAP / PRINCIPLES / LESSONS /
  DECISIONS(跨切面 ADR + 索引表)。
- **crate 级** `crates/<X>/docs/DECISIONS.md`：该 crate 自有 ADR。
- **ADR 归属规则**：决策主表面归哪个 crate 就放哪；只有架构级 (愿景 / 外部化 /
  文档架构本身) 留项目级。
- **PRINCIPLES / LESSONS 不拆**（通用工程知识），仅退标题为项目级。

迁移结果：001 → pcie-remote-phase；002/010 → pcie_device_sdk；003-007 →
nvme_of_tcp_target；008/009/011 留项目级。

**Consequences**：
- 新 ADR 起草先判 scope：跨 ≥2 crate 或架构级 → 项目级；否则 crate 级。
- 跨文档 link 一律用 `/usnvmemu/` repo-root-absolute（见
  [LESSONS §18](/usnvmemu/docs/LESSONS.md)：大规模 move 用脚本 + 死链扫描，别手算深度）。
- PRINCIPLES / LESSONS 退标题，内容不动。

**Status**：active。

---

## ADR-009 — Firmware-as-core 愿景确认 + 命名重构 (2026-06-06)

**Context**：用户 explicit "未来希望以用户态 NVMe firmware 为核心，提供支持 openvmm/openhcl/qemu(vfio-user) 的方式"。

调研 (见 [PROJECT_VISION.md](PROJECT_VISION.md)) 发现当前架构已对齐：
- NVMe controller core (`controller/*.rs`) runtime-agnostic
- `trait Transport` (5 原语) 已是 firmware ↔ transport 边界
- 3 个 transport 实现已落地 (PCIe Remote vsock+TCP / vfio-user / NVMe-oF TCP)

**Decision**：**接受愿景**。优先级重排:
- Tier 1 (本季): 3 条接入各 1 个真 host e2e harness (CHAP real-host / vfio-user QEMU / kernel-CI vector)
  —— **执行机制与分档见 [ADR-013](#adr-013)**(把这三件锐化为 per-transport 独立-oracle 覆盖矩阵 + 结构律)
- Tier 2 (下季): firmware crate 命名重构 (`nvme_firmware` / `pcie_device_sdk` / `pcie_protocol`)
- Tier 3 (半年): Phase X 仓库拆分 (按 ADR-008 路径 C)

**Consequences**：
- ROADMAP §1 加 firmware-as-core Tier 标签
- 新 crate 命名先在 PROJECT_VISION 提议，落地走单独 phase
- "教学版简化" / "spec-strict-mode" 边界对每个 firmware feature 都明标
- 长期: 新仓 `userspace-nvme-firmware`，主仓只留 VTL2 device

**Status**：active；命名重构等仓库拆分一起走，避免双轨。

---

## ADR-008 — Phase X 外部化路径 C (换 tokio + vendor)，预设但不立即执行 (2026-06-06)

**Context**：用户问"未来希望把 nvme 这些挪出本仓库"是否可行/方便。完成调研
[2026-06-06-phase-x-extract-from-openvmm-survey.md](2026-06-06-phase-x-extract-from-openvmm-survey.md)：
7 个 example crate 早是 workspace `exclude`，真正"借自 openvmm"只 4 个 crate
(pal_async / vmsocket / nvme_spec / storage_string)。

**Options**：
- A: 全 git dep — workspace inheritance 让单 crate git dep fail，得 git 整 openvmm 64 K 行
- B: vendor 小依赖 + fork pal_async minimal subset — 中等代价 + 持续追上游
- C: 换 tokio + vendor 小依赖 — 大代价 (4-6 day) 但完全独立 + 长期低维护

**Decision**：**预设路径 C，但不立即执行**。下个月跑 X1 试水 (`nvme_of_tcp_target` 单 crate)
看 git dep 真实痛点，再定 B vs C。

**Consequences**：
- **现在不动**: 让用户专注 ROADMAP §1 HIGH 待办 (real-host CHAP interop / kernel-CI vector)
- **新工作按"将来要搬"预设写**: 新 dep 优先 crates.io；避免新增 `mesh::*` use；
  `pal_async` use 集中到少数文件 (易将来 swap)
- **VTL2 path 必须保留 pal_async** (VTL2 paravisor 不能跑 tokio)；用户态路径可换
- **新 repo 命名候选**: `pcie-userspace-toolkit` / `nvme-of-tcp-toolkit` 之类

**Status**：调研完成，落地推迟。X1 试水时 revisit。

---

## 加新 ADR 模板

> 先判 scope：跨 ≥2 crate 或架构级 → 写本文件 (项目级)；单 crate 内部 →
> 写 `crates/<X>/docs/DECISIONS.md`，并在上方索引表加一行。

```markdown
## ADR-NNN — <一行决策>  (YYYY-MM-DD)

**Context**：背景一两段。

**Options**：
- A: ...
- B: ...
- C: ...

**Decision**：选 X。

**Consequences**：
- 落地代价
- 副作用
- 未来如何 revisit

**Status**：active / superseded by ADR-MMM / revisited (YYYY-MM-DD)
```
