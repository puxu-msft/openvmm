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
