# pcie_device_sdk — 决策日志 (crate-local ADR)

> 本 crate（用户态 PCIe 设备 SDK / core 抽象）自有的 Architecture Decision
> Records。core 抽象边界的决策归属本 crate；跨切面 / 架构级决策见
> [项目 DECISIONS.md](/usnvmemu/docs/DECISIONS.md)（含全 ADR 索引表）。时间倒序。

---

## ADR-010 — Phase W：pcie_device_sdk 补全 hexagonal 结构 (2026-06-08)

**Context**：ADR-002 (Phase T) 只抽了**出站**半边——`trait Transport` 三原语
(`dma_read/write` + `fire_interrupt`) 让 `NvmeController` 主体 transport-agnostic，
这部分成功且经 vfio-user/nvme-of 两 backend 验证。但经本轮深度复核 (architect
subagent 对照代码 confirm)，抽象是**非对称且泄漏**的，5 条结构债：

1. **入站未抽象**：host→device (MMIO/cfg/reset/dma-completion) 不在任何 trait，
   `run.rs::dispatch_inbound` 对 `pcie_remote_protocol::to_host::Body` 硬编码。
2. **`run()` 单态**：内部硬构造 `OpenhclVsockTransport::new()`，非 generic；
   主循环/握手/派发无法被其他 backend 复用 (全仓零外部调用 `sdk::run()`)。
3. **描述模型分叉**（复核新挖，比"泄漏"更根本）：`PcieDevice::describe()` 返回
   `pcie_remote_protocol::DeviceDescribe`，而 vfio-user 另起 `Regions` trait——
   **同一份 BAR/MSI-X/cfg 信息两套不兼容真相源**，`NvmeController` 被迫各实现一遍。
4. **核心 API 泄漏 wire 类型**：`lib.rs` `pub use pcie_remote_protocol`；device 层
   `nvme_firmware/Cargo.toml` 也直连 `pcie_remote_protocol` + 两个 transport crate。
5. **Cargo 无条件耦合**：core 无条件依赖 `vmsocket/guid/mesh/pcie_remote_protocol`
   (OpenHCL 专属)，无 feature/cfg gate。

**Options**：
- A: 维持现状 (Phase T 半成品) — 否。教学=严谨全面 (见 PRINCIPLES)，半 hexagonal 是债。
- B: 做**全** hexagonal：core 中立 + 每 transport 平级 adapter + wire 只活 adapter 内。
- C: 追加"对称全双工入站 trait" + 单一 `generic run<T>()` 把三 transport 统一。

**Decision**：选 **B**，并**显式否决 C 的两个过度设计**：
- **不做对称入站 trait**：PCI 读写本就不对称 (read 需同步返回值、write 无)；nvme-of
  零入站 variant；vfio-user 的 dma-completion 是同步合成非真 wire 帧。保留
  `PcieDevice` 现有非对称入站方法，**入站派发是 adapter 私事**，不进 core trait。
- **不做 generic `run<T: Transport>()`**：三 transport async 模型不同 (pal_async /
  tokio / 同步 blocking pump)，强统一逼出 hack。正确形状 = runtime-agnostic **同步**
  device 核心 + 每 transport **薄 async driver**。

落地为 **Phase W** 四子阶段 (详
[2026-06-08-phase-w-hexagonal-restructure.md](/usnvmemu/crates/pcie_device_sdk/docs/plans/2026-06-08-phase-w-hexagonal-restructure.md))：
- **W1** 中立设备描述模型 (最高优先)：core-native `DeviceDescribe` 同时满足
  pcie_remote cfg-space 生成 + vfio-user GET_REGION_INFO/GET_IRQ_INFO；干掉 `Regions`
  分叉。**顺带修 vfio-user cfg-space identity 源缺失** (复核硬 gap 之一)。
- **W2** crate 拆分：`pcie_device_sdk` → `pcie_device_core` (零 wire/零 runtime 依赖) +
  `pcie_transport_openhcl` (vsock+pcie_remote+run loop+WireStream)。撤 `pub use
  pcie_remote_protocol`。vmsocket 进 `[target.'cfg(windows)']`。
- **W3** device 层解耦：`nvme_firmware` 拆 lib (只依赖 core) + 选 transport 的 bin/feature
  (兑现 Phase T 早标的 deferred)。
- **W4** dma-completion 语义对齐文档：明确是"transport 内部 token 完成通知"，
  pcie_remote 真异步帧 / vfio-user 同步合成；core 只定 `on_dma_complete` 契约。

**Consequences**：
- core 之上"用不用 `vfio_user` crate"降级为**局部可逆的 adapter 私事** (见下 ADR 关于
  vfio-user 主动掌控的说明)；好 seam 让库选择不再是架构决策。
- vfio-user 仍有**两个 spec-complete 硬 gap** (DMA 同步读 head-of-line 阻塞 `dma.rs:260`
  自承 / mmap 共享内存 DMA / capability list cap_offset=0)——**不属 Phase W 结构范围**，
  另立 vfio-user spec-complete track，ROADMAP 跟踪。cfg-space identity 源由 W1 顺带补。
- 与 [项目 ADR-008/009](/usnvmemu/docs/DECISIONS.md) 协同：W2 的 core 零 pal_async 边界正好
  为将来仓库外部化 (路径 C) 铺路；但 Phase W **现在做的是结构正确性**，不是外部化 (那仍推迟)。
- 机械成本：新 crate 必须加进主 `/Cargo.toml` exclude 列表 (line 65-74)。

**vfio-user 库决策 (本 ADR 附带确认)**：当前 vfio-user adapter **由我们主动完全掌控 +
做好结构 + 推向 spec-complete**，**不**外包给 `vfio_user` crate (rust-vmm 0.1.3
"minimal maintenance" 达不到可托付正确性的标准)；它最多当 differential oracle 校验我们
手写 wire。替代品成熟前不改此立场。

**Status**：active；**W1–W4 已 SHIPPED**（commits `3100a102` / `e9139833` /
`249483c3` / `96539f25` / `baa73fe7` / `27b4159d`）。core 抽出 + 撤 wire 泄漏 +
描述模型分叉消除 + vfio-user cfg-space 修复 + device 层 feature-gate 解耦全部落地。
**crate 重命名 `pcie_device_sdk`→`pcie_transport_openhcl` 按 ADR-009 推迟到
Phase X 仓库拆分**（避免双轨改名）。vfio-user spec-complete（DMA head-of-line /
mmap）另立 track。

---

## ADR-002 — Phase T 抽 `trait Transport` (2026-06-04)

**Context**：vsock/protobuf 强耦合让 NvmeController 不能复用到 vfio-user /
NVMe-oF TCP。

**Decision**：抽 5 个原语 `dma_read/write/fire_*/fire_interrupt` 成
`pcie_device_sdk` crate，3 backend 实现。

**Consequences**：Phase U / V 都基于本抽象，不动 controller 主体。**仅做了出站
半边**——入站 / 描述模型 / crate 边界仍泄漏 OpenHCL，由 ADR-010 (Phase W) 补全。

**Status**：active；出站抽象成立，结构债由 ADR-010 接手。
