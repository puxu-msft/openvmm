# 如何新增一条 Transport（接入方式）

> 教学文档。读者：想让**同一份用户态 PCIe/NVMe 设备**通过一种新的"接入方式"被
> host/guest 驱动的贡献者。本文以 `pcie_device_core` 的 `Transport` trait 为中心，
> 用仓库里已有的 3 条接入做范例，并给出加第 4/5 条的分步清单。

## 0. 一句话心智模型

```
        ┌─────────────────────────────────────────────┐
        │  pcie_device_core （零依赖 domain core）       │
        │                                               │
        │   trait PcieDevice  ── 设备实现这个（NVMe…）   │
        │        ▲                                       │
        │        │ 入站：mmio_read/write / cfg / reset   │
        │        │ 出站：ctx.dma_read/write / fire_irq    │
        │        ▼                                       │
        │   trait Transport   ── 接入方式实现这个         │
        └────────────▲──────────────────────────────────┘
                     │ object-safe，DeviceCtx 持 &mut dyn Transport
     ┌───────────────┼────────────────┬───────────────────┐
     │               │                │                   │
 OpenhclVsock    vfio_user        （你的新接入）       NVMe-oF TCP*
 (VTL2 paravisor) (QEMU 接管)      (stdio-pipe…)       *出站同 Transport，
                                                       入站非 PCIe，见 §6
```

**核心解耦**（ADR-010 hexagonal）：设备（`PcieDevice`，如 NVMe controller）**只**
认 `pcie_device_core` 的中立类型，**不知道**自己是被 OpenHCL vsock、vfio-user 还是
别的什么驱动。接入方式是一个实现了 `Transport`（出站 3 原语）的 adapter，外加一个
把"线缆/协议入站事件"翻译成 `PcieDevice` 入站方法调用的 driver 主循环。

新增一条接入 = **写一个 adapter crate**，不动 `pcie_device_core`、不动设备实现。

## 1. 两个 trait 的契约（必读）

源：[`pcie_device_core/src/device.rs`](/usnvmemu/crates/pcie_device_core/src/device.rs)。

### `trait Transport`（你要实现的出站后端，object-safe）

```rust
pub trait Transport {
    fn fire_interrupt(&mut self, msix_index: u32);          // 给 guest 触发 MSI-X
    fn dma_read(&mut self, gpa: u64, len: u32) -> u64;      // 读 guest 内存，返 token
    fn dma_write(&mut self, gpa: u64, data: Vec<u8>) -> u64; // 写 guest 内存，返 token
}
```

- **object-safe 是硬约束**：无 `Self` 返回、无泛型方法。`DeviceCtx<'_>` 持
  `&mut dyn Transport`，让 controller 代码不必全加 `<T: Transport>` 泛型。新接入
  若想加方法，先确认不破坏 object-safety。
- **token 模型**：`dma_read/write` **同步返回一个 `u64` token**，真正的完成**异步**
  由 driver 主循环通过 `PcieDevice::on_dma_complete(token, ok, data, ctx)` 回调。
  **token 分配权在 Transport 实现手里**——因为 vsock / vfio-user / 你的新接入各有
  不同的 msg_id ↔ token 映射需求。

### `trait PcieDevice`（设备侧，你**不**实现，但要理解它怎么被驱动）

设备实现 `describe()` / `mmio_read` / `mmio_write(ctx, …)` / `cfg_write_side_effect`
/ `reset` / `tick(ctx)` / `on_dma_complete(ctx, token, ok, data)`。你的 driver 主
循环负责：把入站线缆事件**翻译**成这些调用，并在每次回调里传一个
`DeviceCtx::new(&mut your_transport)`，让设备能反向 `ctx.dma_read/write/fire_interrupt`。

**时序契约**（别违反）：
- `describe()` 在握手阶段调一次，**静态**（adapter 可缓存，见 vfio-user 的
  `describe` 懒缓存）。运行期改 BAR/MSI-X 布局不支持；FLR 只重置 config base/Command。
- `mmio_*` / `cfg_*` 由 guest 触发，**可乱序**；同步返回值即设备响应。
- `on_dma_complete` 的契约是"**token 终会被回调一次**"。设备**不应假设**底层是同步
  还是异步——这正是 W4/ADR-010 的关键抽象（见 §3）。

## 2. on_dma_complete：同步 vs 异步，别让它泄漏（W4 / ADR-010）

这是新接入最容易做错的地方。`on_dma_complete` **不是**一个跨接入对称的"入站事件"，
不同 backend 产生它的方式根本不同：

| 接入 | DMA 是怎么完成的 | 怎么产生 on_dma_complete |
|------|------------------|--------------------------|
| OpenHCL vsock | host 收到一帧**真异步** `DmaCompletion` wire 消息 | 主循环收到该帧 → 回调 |
| vfio-user | DMA_READ/WRITE 是**同步** wire 往返（或 mmap 本地 memcpy） | adapter 在往返完成后**合成**一条完成事件投递 |
| 你的新接入 | ？（取决于你的协议） | 你负责保证 token 终被回调一次 |

**设计准则**：设备只依赖 token 契约。你的 adapter 内部是 block-then-synth（像
vfio-user）还是真异步收帧（像 vsock）是你的自由，但**对设备不可见**。vfio-user 的
做法（同步 `dma_*_sync` 完成后推 `pending_completions` 队列，主循环 drain 时回调）
是教科书参考：见 [`vfio_user_transport/src/session.rs`](/usnvmemu/crates/vfio_user_transport/src/session.rs)
的 `drain_dma_completions` + `dma_read/dma_write`。注意 vfio-user **无论** wire 往返
**还是** mmap 本地 memcpy（零拷贝命中），**都统一**走 `pending_completions` + drain
合成完成事件——对设备完全一致，设备不知道也不关心这次 DMA 是否走了零拷贝。

## 3. 三条参考接入（照着抄）

### A. OpenHCL vsock —— `pcie_device_sdk`（VTL2 paravisor 路径）

- 形态：OpenHCL paravisor 里，设备通过 `pcie_remote` 私有 wire 协议跨 vsock 跟
  host 通信。`OpenhclVsockTransport` 实现 `Transport`，把 3 原语转成 protobuf wire。
- 入口：[`pcie_device_sdk/src/run.rs`](/usnvmemu/crates/pcie_device_sdk/src/run.rs) 主循环。
- 特点：`pal_async` runtime（VTL2 必须）；token = 专用 `next_dma_token` 分配器
  （`1<<40` 起，独立于 seq；见 `openhcl_transport.rs`）；真异步 `DmaCompletion` 帧。

### B. vfio-user —— `vfio_user_transport`（QEMU `-device vfio-user-pci` 接管）

- 形态：QEMU（≥10.1）或 libvfio-user client 通过 vfio-user 协议（Nutanix spec）
  跨 UNIX socket 接管设备。`VfioUserSession` 同时是 driver（`pump_one` 主循环把
  REGION_READ/WRITE/DMA_MAP/SET_IRQS 等入站 cmd 翻译成 `PcieDevice` 调用）**和**
  `Transport`（`fire_interrupt`→MSI-X eventfd、`dma_read/write`→DMA_READ/WRITE wire
  或 mmap 零拷贝）。
- 入口：[`vfio_user_transport/src/session.rs`](/usnvmemu/crates/vfio_user_transport/src/session.rs)。
- 特点：tokio 可选；token = server-initiated msg_id（顶位 0x8000）；DMA 同步往返
  或 mmap 本地 memcpy（见 [`dma.rs`](/usnvmemu/crates/vfio_user_transport/src/dma.rs)）。
- **真 host e2e**：[`scripts/qemu_interop/`](/usnvmemu/crates/vfio_user_transport/scripts/qemu_interop/)
  用真 QEMU 11 做差分 oracle（见 §5）。

### C. NVMe-oF TCP —— `nvme_of_tcp_target`（**入站面不同**，见 §6）

NVMe-oF TCP 是 **fabric** transport：远端 host 用 `nvme connect -t tcp` 直接对
NVMe controller 说 NVMe-oF 协议。**关键区分**：它**不经过 `PcieDevice` 的 PCIe
入站呈现**（没有 BAR / MMIO / config / MSI-X 那套 guest 面），但它**仍然复用同一个
`Transport` 出站 trait**——`TcpAdminTransport impl Transport`
（[`tcp_transport.rs`](/usnvmemu/crates/nvme_of_tcp_target/src/tcp_transport.rs)）把
controller 的出站原语翻成 NVMe-oF wire：
- `dma_write(gpa, data)` → C2HData PDU（host 收数据）；
- `dma_read(gpa, len)` → R2T + H2CData 闭环（拉回 host 数据后 `on_dma_complete`）；
- `fire_interrupt(idx)` → **no-op + 计数**（NVMe-oF 不需要 MSI-X；C2HData / CapsuleResp
  PDU 本身就是"完成通知/中断"）。
- 入口：[`nvme_of_tcp_target`](/usnvmemu/crates/nvme_of_tcp_target/) 的 `AsyncSession`。
- 差异全在**入站面**：driver 主循环翻译的是 NVMe-oF capsule（Connect / admin / IO cmd），
  而非 PCIe MMIO；设备侧也不是 `PcieDevice` 的 mmio/config，而是 controller 的命令处理。

## 4. 加一条新 PCIe 接入：分步清单

目标示例：一条 **toy stdio-pipe** 接入（把设备 MMIO/DMA 走一条简单的 stdin/stdout
帧协议——教学用，便于无内核/无 QEMU 跑通完整设备生命周期）。

1. **建 adapter crate** `usnvmemu/crates/<your>_transport/`，`Cargo.toml` 依赖
   **只**加 `pcie_device_core` + 你的线缆所需（绝不反向依赖设备 crate）。
2. **定义 wire**（若是新协议）：framing + 命令枚举 + payload 结构。**用 `zerocopy`
   做零拷贝解码**，offset 用 `offset_of!` anchored（教训见 LESSONS）。
3. **实现 `Transport`**：
   - `fire_interrupt(idx)`：把"触发向量 idx"映射到你的线缆（eventfd / 一帧消息 /
     fabric 下可 no-op，见 §3-C）。
   - `dma_read(gpa, len) -> token`：分配 token，发起读（同步往返或异步发帧均可）。
     `len ≤ 64 KiB`（`MAX_DMA_BYTES`）—— **backend 自己负责**拒绝/截断超限请求
     （core 不强制；如 OpenHCL 对超限返 `ok=false`）。
   - `dma_write(gpa, data) -> token`：同理。
   - 维护 token ↔ 操作 的映射；完成时**保证回调一次** `on_dma_complete`。
4. **写 driver 主循环**：阻塞/异步读入站帧 → 按类型翻译成 `PcieDevice` 调用：
   - 读设备身份 → `describe()`（建议缓存）。
   - guest MMIO 读/写 → `mmio_read` / `mmio_write(&mut DeviceCtx::new(&mut self), …)`。
   - config 写 → `cfg_write_side_effect`（带 side-effect 的 offset；vfio-user 范例把
     config 收敛进 host-side `ConfigSpace::write_bytes`，side-effect 钩子按需调）；
     复位 → `reset(kind)`。
   - 周期性 → `tick(ctx)`。
   - 每轮 dispatch 后 **drain** 你的 DMA 完成队列 → `on_dma_complete`。
5. **bin entrypoint**：`main.rs` 起线缆 + 实例化设备（如 `NvmeController::open`）+ 跑
   driver loop。feature-gate（参考 `nvme_firmware` 的 `--features <transport>`）。
6. **测试三层**（见 §5）。
7. **文档**：crate README + 在本文件 §3 加一行；ROADMAP 标 SHIPPED。

**借鉴整段结构**：vfio_user_transport 是最完整的单 crate 范例（driver + Transport
合一 + 三层 oracle）。从它的 `lib.rs` 模块布局起手 copy，再换 wire。

## 5. 验证：三层 oracle（独立性递增，必读 LESSONS §20/§22）

**绝不**只用"自家 client 验自家 server"——共享的错误假设两边都不报错、单测全绿，
真 bug 照样 ship（本仓库 3 次血的教训：DMA head-of-line / version 协商 / mmap SIGBUS）。

1. **lib 单测**：协议解码 + 各命令 happy path + **adversarial 用例**（不可信输入的
   极端值：超长 / 越界 / size>真实大小 / flag 不匹配）。self-consistent 的测试数据
   碰不到真 bug 路径。
2. **跨进程 Python harness**（stdlib，无 sudo）：独立第二实现验 wire。参考
   [`vfio_user_transport/scripts/interop_py/`](/usnvmemu/crates/vfio_user_transport/scripts/interop_py/)
   / [`nvme_of_tcp_target/scripts/interop_py/`](/usnvmemu/crates/nvme_of_tcp_target/scripts/interop_py/)。
3. **真第三方实现做差分 oracle**：真 QEMU（vfio-user）/ 真 Linux nvme-cli（NVMe-oF）。
   这是最有价值的一层——第三方独立 C/kernel 实现能 catch 前两层抓不到的 bug。

**unsafe 必过 subagent review**，且 SAFETY 每条不变量都问"判据来自独立 oracle 还是
不可信输入自证？"（LESSONS §22：mmap 长度的 oracle 是 `fstat` 不是 client 声明）。

## 6. 加 fabric transport（NVMe-oF 类）：入站面是另一回事，出站仍是同一 `Transport`

若你要加的是**远端直接说 NVMe-oF**（RDMA / FC / 别的 fabric），它**仍然实现同一个
`Transport`**（出站 3 原语，参考 `TcpAdminTransport`），把 controller 的
`ctx.dma_read/write/fire_interrupt` 截获后翻成 fabric wire（如 R2T/H2CData/C2HData）。
**不同的是入站面**：fabric 没有 PCIe presentation（无 BAR/MMIO/config/MSI-X），
driver 主循环翻译的是 NVMe-oF capsule 而非 guest MMIO，设备侧走 controller 的命令
处理而非 `PcieDevice` 的 mmio/config 钩子。两类接入**共享同一个 controller core 和
同一个 `Transport` 出站 trait**，只是入站呈现不同。ROADMAP V9（RDMA）即此类。

## 7. 清单速查

- [ ] adapter crate 只依赖 `pcie_device_core` + 线缆库，**零**反向依赖设备
- [ ] `Transport` 三原语实现 + object-safety 不破
- [ ] token 模型：分配权在 adapter，完成保证回调一次，同步/异步对设备不可见
- [ ] driver 主循环翻译入站事件 → `PcieDevice` 方法 + 每轮 drain DMA 完成
- [ ] `describe()` 当静态缓存；FLR 失效重建
- [ ] 三层 oracle 验证 + adversarial 测试用例
- [ ] unsafe（如有）过 review + SAFETY 用独立 oracle 判据
- [ ] crate README + 本文件 §3 一行 + ROADMAP 标记

---
**相关**：[ADR-010 hexagonal](/usnvmemu/crates/pcie_device_sdk/docs/DECISIONS.md) ·
[PROJECT_VISION §3](/usnvmemu/docs/PROJECT_VISION.md) ·
[LESSONS §20/§22](/usnvmemu/docs/LESSONS.md)（self-consistent 反模式 / unsafe oracle）·
[PRINCIPLES](/usnvmemu/docs/PRINCIPLES.md)
