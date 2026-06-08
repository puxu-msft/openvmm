# Phase W — pcie_device_sdk 补全 hexagonal 结构

> **Status:** planning（已 architect-review，准备执行 W1）
> **Date:** 2026-06-08
> **Prereqः** Phase T（ADR-002，出站 `trait Transport` 已抽）/ Phase U（vfio-user）/ Phase V（NVMe-oF TCP）均已 shipped
> **决策依据:** [ADR-010](/usnvmemu/crates/pcie_device_sdk/docs/DECISIONS.md)
> **Successor:** vfio-user spec-complete track（独立，见 §8）

---

## 0. 一句话

Phase T 只做了**出站**半边 hexagonal。Phase W 补**入站方向的类型卫生 + 描述模型统一 +
crate 物理边界**，让 `pcie_device_core` 成为零 wire / 零 runtime 依赖的中立 domain，
pcie_remote / vfio-user / nvme-of 三 transport 退化为**平级 adapter**。

**显式不做**（ADR-010 否决的过度设计）：
- ❌ 对称全双工入站 trait —— PCI 读写本就不对称，保留 `PcieDevice` 现有非对称方法。
- ❌ 单一 `generic run<T: Transport>()` —— 三 transport async 模型不同，各自留薄 driver。

## 1. 复核确认的结构债（architect subagent，对照代码）

| # | 债 | 证据 (file:line) | Phase W 处理 |
|---|----|------|------|
| 1 | 入站未抽象，硬编码 pcie_remote `HostBody` | `run.rs:131-180` | 保持 adapter 私有（不进 core），W2 |
| 2 | `run()` 单态硬构造 `OpenhclVsockTransport` | `run.rs:60,93` | 随 crate 拆分进 openhcl adapter，W2 |
| 3 | **描述模型分叉** `DeviceDescribe` vs `Regions` | `device.rs:61` / `vfio_user_transport/src/session.rs:47-56` | **W1（最高优先）** |
| 4 | core API 泄漏 wire 类型 | `lib.rs:71-76` `pub use`；`nvme_firmware/Cargo.toml:15-16` | W2（撤 pub use）+ W3（device 层） |
| 5 | Cargo 无条件耦合 vmsocket/guid/mesh/protocol | `pcie_device_sdk/Cargo.toml:8-12` | W2 |

复核额外纠正：
- 论断 1 修正：`dma-completion` 在 vfio-user **不是真入站帧**——同步 DMA（`session.rs:458-495`）
  当场拿 data，再 `pending_completions` 合成投递（`session.rs:159-167`）。→ W4 文档对齐语义。
- 论断 3 修正：vfio-user **生产路径不调 `describe()`**，只 test fixture 被迫 import
  （`session.rs:543`）；但生产用 `Regions` trait（`session.rs:210-316`）绕开 `DeviceDescribe`。
  真问题是**分叉**而非泄漏：`NvmeController` 同时实现 `describe()`（`controller/mod.rs:2341`）
  和 `Regions`，信息冗余可能不一致。

## 2. 目标 crate 布局

```
usnvmemu/crates/
├── pcie_device_core/                 ── NEW（从 pcie_device_sdk 抽出）
│   Cargo.toml  deps: anyhow + tracing only（零 wire / 零 pal_async / 零 tokio / 零 vmsocket）
│   src/
│     device.rs    trait PcieDevice（入站非对称方法，原样）+ trait Transport（出站三原语）
│     describe.rs  中立 DeviceDescribe / BarLayout / MsixConfig / CfgIdentity / Capability
│     ctx.rs       DeviceCtx<'a>（持 &mut dyn Transport）
├── pcie_transport_openhcl/           ── 由旧 pcie_device_sdk 改名/瘦身
│   Cargo.toml  deps: pcie_device_core + pcie_remote_protocol + pal_async
│               [target.'cfg(windows)'.dependencies] vmsocket + guid
│   src/
│     transport.rs       OpenhclVsockTransport: core::Transport + 中立→protobuf 转换
│     run.rs             pcie_remote 主循环 + dispatch_inbound（adapter 私有）
│     wire_stream.rs     WireStream（AsyncRead+AsyncWrite，byte-stream，pcie_remote 专属）
│     connect.rs         connect_vsock(#[cfg(windows)]) / connect_tcp
├── vfio_user_transport/              ── 改：依赖 core 而非旧 sdk；Regions 由 DeviceDescribe 派生
├── nvme_of_tcp_target/               ── 改：依赖 core
└── nvme_firmware/                    ── W3 拆 lib + bin（见 §5）
```

**不新建空 crate 凑对称**（复核警告）：vfio_user_transport / nvme_of 已独立，只需把
`pcie_device_sdk` 一拆为二。

## 3. core 中立类型设计（W1 核心）

唯一真相源 `DeviceDescribe`，**同时**供 pcie_remote cfg-space 生成 + vfio-user
GET_REGION_INFO/GET_IRQ_INFO 派生：

```rust
// pcie_device_core::describe
pub struct DeviceDescribe {
    pub identity: CfgIdentity,          // vendor/device/subsys/class/rev —— 修 vfio-user cfg gap
    pub bars: Vec<BarLayout>,           // index/size/kind(mmio64|io|prefetch)
    pub msix: MsixConfig,               // count + table/pba BAR+offset
    pub caps: Vec<Capability>,          // MSI-X capability 等 —— 修 cap_offset=0 gap
    pub cfg_write_side_effect_offsets: Vec<u32>,
}
```

adapter 各自转换（中立 → wire，转换在 adapter 内）：
- openhcl: `DeviceDescribe` → `pcie_remote_protocol::DeviceDescribe`（含 cfg-space blob 组装）。
- vfio-user: `DeviceDescribe` → `Regions`（bar→region）+ IRQ info + **真 cfg-space region 路由**
  （现 `session.rs:256-259` CONFIG region 读写误当 MMIO，W1 补 cfg identity 来源后接真路由）。
- nvme-of: 无 PCI 层，不用 `DeviceDescribe`（NVMe-oF 命令直达 controller 方法）。

`Regions` trait **删除**；`NvmeController` 只实现一次 `describe()`。

## 4. 四子阶段实施

### W1 — 中立设备描述模型（最高优先，先做）
1. core crate 暂以**模块**形态在现 crate 内引入 `describe.rs` 中立类型（降低一次性爆破）。
2. `PcieDevice::describe()` 改返中立 `DeviceDescribe`。
3. openhcl `run.rs` handshake 处加 中立→protobuf 转换。
4. vfio-user：`Regions` 改由 `DeviceDescribe` 派生；补 `CfgIdentity` → 真 cfg-space region
   读写路由（vendor/device ID 可被 guest enumerate）；capability list 填 `cap_offset`。
5. `NvmeController` 删 `Regions` impl，`describe()` 补 identity/caps 字段。
6. reviewer + 全测试。**验收：vfio-user 下 guest 能读到正确 vendor/device ID。**

### W2 — crate 物理拆分
1. 新建 `pcie_device_core`（device.rs + describe.rs + ctx.rs），deps = anyhow + tracing。
   1 个 object-safety 单测 + 中立类型 round-trip 单测。
2. 旧 `pcie_device_sdk` 改名 `pcie_transport_openhcl`：留 transport/run/wire_stream/connect；
   依赖 core；撤 `pub use pcie_remote_protocol`（改为 adapter 内部 use）。
3. vmsocket/guid → `[target.'cfg(windows)'.dependencies]`；Linux 构建不再链 vmsocket。
4. vfio_user_transport / nvme_of_tcp_target 改依赖 `pcie_device_core`。
5. 主仓 `/Cargo.toml` exclude 列表（line 65-74）加新 crate 名。
6. 全 crate fmt + clippy + test。

### W3 — device 层解耦（兑现 Phase T deferred）
1. `nvme_firmware` 拆 `lib.rs`（controller，仅依赖 `pcie_device_core`）+ bin。
2. transport 选择（openhcl/vfio-user/nvme-of）移到 bin，或 `[features]` gate：
   `nvme_firmware/Cargo.toml` 不再无条件直连 `pcie_remote_protocol` + 三 transport crate。
3. `main.rs` 按 feature 选 transport 入口。
4. reviewer + test + 三 transport 各一次 smoke。

### W4 — dma-completion 语义对齐
1. core `on_dma_complete` doc 明确："transport 内部 token 完成通知；pcie_remote 为真异步
   入站帧，vfio-user 为同步 DMA 结果合成事件。**非**跨 transport 对称入站事件。"
2. 在 LESSONS.md 记一条：对称性是伪需求，read/write/completion 三者语义本就不齐。

## 5. 关键风险

| 风险 | 严重 | 缓解 |
|---|---|---|
| `PcieDevice::describe()` 签名变击穿所有 impl（NvmeController + 2 MockDev） | HIGH | W1 一个 PR 内改完，grep 旧 `Regions` → 0 |
| vfio-user cfg-space 真路由引入回归（现误当 MMIO） | HIGH | W1 加 cfg read/write 专测；guest enumerate smoke |
| crate 改名碰 path dep（5 处 import 路径） | M | W2 先全仓 grep `pcie_device_sdk::` 列清单再改 |
| vmsocket cfg gate 漏 → Linux 仍链 | M | W2 后 `cargo build` Linux + Windows 双跑 |
| wire-byte regression（中立→protobuf 转换错） | M | 复用 Phase T 的 8 个 `dispatch_inbound_*`/`*_body` wire 契约单测 |
| 主 workspace exclude 漏加 → 主仓编译炸 | M | W2 末 `cargo metadata` 确认新 crate 不在主 workspace member |

## 6. Acceptance（Phase W 整体）
- [ ] `pcie_device_core/Cargo.toml` deps = `anyhow + tracing`（零 wire/runtime/vmsocket）
- [ ] `grep -r "pub use pcie_remote_protocol"` → 0；`grep "Regions" ` → 0（已删分叉）
- [ ] `nvme_firmware/Cargo.toml` 不再无条件列 `pcie_remote_protocol` + transport crate
- [ ] vfio-user 下 guest 读到正确 vendor/device ID（cfg-space 真路由）
- [ ] 三 crate fmt + clippy `-D warnings` 全绿；现有全部 test pass（无净减）
- [ ] 三 transport 各一次 smoke（OpenHCL vsock / vfio-user QEMU 或 mock / nvme-of loopback）
- [ ] 每子阶段过 rust-reviewer（见 mem:subagent-review-required）

## 7. 受影响文件（绝对路径）
- `usnvmemu/crates/pcie_device_sdk/src/{lib,device,run,transport,openhcl_transport}.rs`
- `usnvmemu/crates/pcie_device_sdk/Cargo.toml`
- `usnvmemu/crates/vfio_user_transport/src/{session,proto}.rs`（Regions→DeviceDescribe 派生 + cfg 路由）
- `usnvmemu/crates/nvme_of_tcp_target/Cargo.toml`（依赖改 core）
- `usnvmemu/crates/nvme_firmware/Cargo.toml` + `src/{main,lib}.rs` + `src/controller/mod.rs`（describe）
- `/Cargo.toml`（exclude 列表 line 65-74）

## 8. Out of scope —— vfio-user spec-complete track（独立追踪，ROADMAP）

Phase W 是**结构正确性**，不含 vfio-user 协议完整性。复核挖出的 spec gap 另立 track：

| gap | 证据 | 严重 |
|---|---|---|
| DMA 同步读 head-of-line 阻塞（等 reply 时不能处理插入 cmd，真 QEMU 并发会断） | `dma.rs:260-264` 自承 | **高** |
| mmap 共享内存 DMA 完全没做（强制 message-mediated） | `dma.rs:8-26` | 高（性能） |
| DEVICE_GET_REGION_IO_FDS 不实现 | `session.rs:197-200` ENOTSUP | 中 |
| migration / dirty-page tracking 无 | `handshake.rs:20-21` | 低（教学可省） |

cfg-space identity 源 + capability list 由 **W1 顺带补**（与描述模型统一同源）。

> 注：本 Phase 不外部化仓库（ADR-008 路径 C 仍推迟）；W2 的 core 零 pal_async 边界为将来外部化铺路，但不是本 Phase 目标。
