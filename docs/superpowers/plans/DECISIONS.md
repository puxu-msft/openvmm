# NVMe-oF TCP Target — 重大决策日志 (ADR)

> Architecture Decision Records, 时间倒序。每条捕获 "我们曾在两条路径间纠
> 结，最终选 X 而不选 Y，因为 ..." 类决策，未来回看不必重新算 cost-benefit。
>
> 模板：每条决策 → 日期 / Context / Options / Decision / Consequences /
> Status (active / superseded / revisited)。

---

## ADR-007 — TP-8011 PSK 不 fork rustls，等上游 (2026-06-06)

**Context**：实现 NVMe TP-8011 需 TLS 1.3 external PSK；rustls 0.23 没公开
API；SpiderOak fork (PR #2424) closed without merge。

**Options**：
- A: 等 rustls upstream — 时间不可控
- B: 自 fork rustls — 高维护成本，安全债
- C: SpiderOak fork 当 git dep — 教学 OK，prod 不可
- D: 换 TLS 栈 (s2n-tls / openssl) — 800-1500 LOC 重写

**Decision**：路径 A + C 并行（短期不动主线，等上游；C 作为可选实验脚本）。

**Consequences**：
- `src/tls_psk.rs` 留 deterministic crypto only，标 "rustls 注入待上游"
- 出 detailed survey 文档 [2026-06-06-phase-v-followup-tls-psk-survey.md]
- 优先级降为"等 + monitor"；HIGH 优先 swap 给 *kernel CI vector* (独立)

**Status**：active；rustls 出 external PSK API 后 revisit。

---

## ADR-006 — V-followup-prp-list 走 session-level chunking 不动 controller (2026-06-06)

**Context**：V5_NLB_MAX=16 LBA (8 KiB) 卡死 IO 大小；MDTS=5 (128 KiB) 名义
support 但实际 reject。

**Options**：
- A: 改 controller PRP-list path (扩 sentinel decode + multi-page)
  ~ 500 LOC + 跨 lib 改动 + 新 reviewer round
- B: session 端透明 chunking (16 → 256 LBA, 内部分 16 个 sub-cmd)
  ~ 200 LOC，0 controller 改动

**Decision**：B。

**Consequences**：
- 单 IO 上限提到 256 LBA / 128 KiB，符合 MDTS
- host 不感知 chunking；wire 仍走每 sub-cmd 一组 C2HData
- *Future production* 路径仍是 A，本文档保留作为起点
- plan 文档标 SUPERSEDED + 留 rationale

**Status**：active；real-Linux interop 实测过后可考虑回 A。

---

## ADR-005 — DHCHAP wire 同时支持 simplified + spec 4-msg (2026-06-06)

**Context**：V-followup-dhchap-3 已落地教学版 simplified wire (2-msg)，
Linux nvme-cli 实发 spec § 8.13.5 4-msg (NEGOTIATE/CHALLENGE/REPLY/SUCCESS1)。
新加 4-msg 是否破坏老 simplified？

**Options**：
- A: 删 simplified，只留 spec — 删 V-dhchap-3 测试 + 改 Python harness
- B: 双 wire 共存，host 首发包自动识别

**Decision**：B（`ChapWireMode::Unknown → Spec4Msg/Simplified` 三态锁定）。

**Consequences**：
- 兼容 V-dhchap-3 全部测试 (105 个) + V-dhchap-4 新增 10 e2e
- 自动检测的 collision 概率 ≈ 2^-32 (4 条 condition 同时满足，见 reviewer L-3)
- 文档承载额外复杂度 (两条路径)
- *设计模式*：wire 自动识别 + 锁定，是后向兼容的可复用模板 (见 LESSONS §11
  decision-then-IO 模式同段落)

**Status**：active；若未来 simplified 路径再没人用可推 ADR-008 deprecate it。

---

## ADR-004 — Python interop 不走 sudo nvme-cli (2026-05-31..2026-06-06)

**Context**：Python harness 想真验 wire，最自然是 `subprocess` 调 nvme-cli。
但 nvme-cli `connect` 需要 root + `nvme-tcp.ko` 模块 + WSL2 kernel 兼容。

**Options**：
- A: sudo nvme-cli 包装（高真实度，前置门槛高）
- B: 纯 Python raw socket 发 NVMe-oF wire（自构造 PDU 全部字段）
- C: libnvme C library Python binding（依赖编译）

**Decision**：B（uv + stdlib only，0 sudo）。

**Consequences**：
- 7+1 Python scenarios 跑通，每个 < 200 LOC，纯 stdlib
- 不能验 Linux kernel nvme-tcp.ko 的 *实际* wire (只验自家算法)
- 真 kernel interop 留另一类 task (ROADMAP §1 real-host CHAP interop)
- *硬限*: Python harness 是 "我们自家算法对自家算法的高效 e2e"，不是 spec
  conformance test

**Status**：active；real-host interop task 是补足，不是替代。

---

## ADR-003 — V8e tokio runtime + AsyncSession (2026-06-06)

**Context**：V8 之前 session 是 sync thread-per-conn；扩到多 conn + AER +
KATO timer 需要 select。

**Options**：
- A: 继续 sync + 加 mpsc 桥 — KATO / AER 跨 thread 难
- B: tokio runtime + AsyncSession + select! — 重构成本高

**Decision**：B（V8e-1..V8e-7 7 sub-phase）。

**Consequences**：
- 引入 `#![deny(clippy::await_holding_lock)]` 防 lock-await-stall
- 落实 decision-then-IO 借用模式 (见 LESSONS §11)
- 性能：AER Notify < 10ms，KATO timer 精度 < 100ms
- bin 全 async，spawn_blocking 退役

**Status**：active。

---

## ADR-002 — Phase T 抽 `trait Transport` (2026-06-04)

**Context**：vsock/protobuf 强耦合让 NvmeController 不能复用到 vfio-user /
NVMe-oF TCP。

**Decision**：抽 5 个原语 `dma_read/write/fire_*/fire_interrupt` 成
`pcie_device_sdk` crate，3 backend 实现。

**Consequences**：Phase U / V 都基于本抽象，不动 controller 主体。

**Status**：active。

---

## ADR-001 — PCIe Remote Path C (OpenHCL VTL2 + vsock) (2026-05-29..30)

**Context**：早期纠结 Path A (OpenVMM Linux) / Path B (mshv) / Path C
(OpenHCL VTL2)。

**Decision**：Path C，理由见 [../SESSION_LOG.md](../SESSION_LOG.md) 早期段。

**Status**：active；Path A 仍可跑，Path B (mshv) [MSHV_DIAGNOSIS.md](../MSHV_DIAGNOSIS.md)
搁置。

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

## 加新 ADR 模板

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
