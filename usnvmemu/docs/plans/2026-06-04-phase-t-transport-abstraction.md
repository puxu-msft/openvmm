# Phase T — Transport Abstraction

> **✅ SHIPPED (2026-06-06 audit)** — `trait Transport` 已抽出并由 vsock/vfio-user/in-process 三 backend 实现；Phase U（vfio-user）+ Phase V (NVMe-oF TCP) 都基于本 abstraction 落地。本文档保留为历史 design 记录。
>
> 后续开发请参考：[ROADMAP.md](ROADMAP.md) / [PRINCIPLES.md](PRINCIPLES.md) / [LESSONS.md](LESSONS.md)。

> **Status:** planning（已 review，准备执行）
> **Date:** 2026-06-04
> **Prereq:** Phases A–S 已完成（67 NVMe tests / 8 SDK tests baseline）
> **Successor:** Phase U（vfio-user backend）/ Phase V（NVMe-oF TCP target）

## 1. 目标

把当前 vsock/protobuf 强耦合的 `pcie_remote_userspace_sdk::DeviceCtx` 抽成"传输无关"的
`pcie_device_sdk` crate，使 `NvmeController` 主体只依赖 `trait Transport` 五原语
（`dma_read`、`dma_write`、`dma_read_fire_and_forget`、`dma_write_fire_and_forget`、`fire_interrupt`）。
Phase U/V 只需新增 backend，不动 controller。

## 2. 改动后 workspace 布局

```
docs/superpowers/examples/
├── pcie_device_sdk/                   ── NEW
│   Cargo.toml   deps: anyhow + tracing + parking_lot only
│   src/lib.rs   trait Transport + trait PcieDevice + DeviceCtx<'a, T>
│                + InMemoryTransport + TransportEvent
├── pcie_remote_userspace_sdk/         ── 改：depends on pcie_device_sdk
│   提供 OpenhclVsockTransport: pcie_device_sdk::Transport
│   pub type DeviceCtx<'a> = pcie_device_sdk::DeviceCtx<'a, OpenhclVsockTransport>;
│   原 byte-stream `type Transport = …` 改名 `WireStream`
├── pcie_remote_nvme_userspace/        ── 改：controller body 改 use pcie_device_sdk::*
│   main.rs 仍用 pcie_remote_userspace_sdk runner
│   Cargo.toml 删除 pcie_remote_protocol 依赖
```

## 3. 核心抉择：generic vs dyn

**采用 generic** `DeviceCtx<'a, T: Transport>`：NVMe IO 是热路径（每 SQE ≥ 1 DMA），
monomorphization 成本可接受；同时 `trait Transport` 保持 object-safe
（无 `Self` 返回、无 generic method），未来需要 `&mut dyn Transport` 也不破坏。

## 4. 五步实施

### Step 1（独立 commit，低风险）
新建 `pcie_device_sdk/{Cargo.toml, src/{lib,transport,device,describe}.rs}`，
workspace `Cargo.toml` 加 member。1 个 object-safety 单测。下游无影响。

### Step 2 + 3（同 PR — `trait PcieDevice` 签名变会击穿所有 impl）

**Step 2** — `pcie_remote_userspace_sdk` 切内部：
- 加 `pcie_device_sdk` 依赖
- 从 `pcie_device_sdk` re-export `PcieDevice`/`DeviceCtx`/`Transport`
- 旧 byte-stream `Transport` 别名 → `WireStream`
- 实现 `OpenhclVsockTransport`（持有 `outbound: Vec<ToOpenhcl>`、`next_seq=1<<32`、`next_dma_token=1<<40`）
- `run.rs` 改：`DeviceCtx::new(&mut backend)`，然后 drain `backend.outbound` 进 wire
- `DeviceCtx::for_testing` 留 `#[deprecated]` shim

**Step 3** — NVMe controller：
- `use pcie_remote_userspace_sdk::*;` → `use pcie_device_sdk::*;`（controller body）
- 给约 50 个方法/helper 加 `<T: Transport>`
- `main.rs` 与 `tests.rs` 不动

### Step 4 — Tests + Cargo 清理
- `pcie_device_sdk` 加 `InMemoryTransport` + `TransportEvent` 枚举
- 重写 13 处 `DeviceCtx::for_testing` 测试调用 → `InMemoryTransport`
- `Body::ReadGpa { gpa, len, .. }` 匹配 → `TransportEvent::DmaRead { gpa, len, .. }`
- 删 `pcie_remote_nvme_userspace/Cargo.toml` 里 `pcie_remote_protocol`
- 删 `DeviceCtx::for_testing` shim

### Step 5 — Fmt + clippy + 全测试

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test  # nvme: 67 / userspace_sdk: 7-8 / device_sdk: 2
```

## 5. 关键发现 / 风险

| 风险 | 严重 | 缓解 |
|---|---|---|
| 名字冲突：旧 `pub type Transport = Box<dyn TransportTrait>` (byte-stream) 与新 device-primitive `trait Transport` | **HIGH** | Step 2 重命名旧别名 → `WireStream` |
| `trait PcieDevice` 签名变击穿所有 impl | HIGH | Step 2+3 必须同 PR |
| Wire-byte regression | M | 保持 `next_seq=1<<32`、`next_dma_token=1<<40`、alloc 顺序 (token, seq)；现有 8 个 `dispatch_inbound_*` / `device_ctx_*_body` 单测就是 wire 契约 |
| 13 处 `for_testing` 机械重写遗漏 | M | grep `for_testing` → 0 hits 才提交 Step 4 |
| `main.rs` 仍要 `pal_async`/`guid`（prompt 说要删，但 main 是 runner） | L | 任务范围内 main.rs 不动；进一步 lib/bin 拆分留 Phase U |

## 6. 测试快照表

| Step | nvme_userspace | userspace_sdk | device_sdk |
|------|---:|---:|---:|
| baseline | 67 | 8 | — |
| after 1 | 67 | 8 | 1 |
| after 2 | 67 | 8 | 1 |
| after 3 | 67 | 8 | 1 |
| after 4 | 67 | 7（for_testing test 删） | 2 |
| after 5 | 67 | 7 | 2 |

## 7. Acceptance

- [ ] 67 NVMe tests + 7 SDK tests + ≥2 新 `pcie_device_sdk` tests pass
- [ ] `pcie_remote_nvme_userspace/Cargo.toml` 不再列 `pcie_remote_protocol`
- [ ] `pcie_device_sdk/Cargo.toml` deps = `anyhow + tracing + parking_lot`
- [ ] `cargo fmt --check` + `cargo clippy -- -D warnings` 三 crate 全绿
- [ ] Manual OpenHCL smoke：`pcie_remote_nvme_userspace --vm-id <id> --port 50000` 仍能驱动 guest NVMe

## 8. Out of scope（留 U/V）

- Phase U：`VfioUserTransport` impl
- Phase V：`NvmeOfTcpTransport` impl
- 将 `pcie_remote_nvme_userspace` 拆 lib + bin，让 `pal_async`/`guid` 仅 OpenHCL bin 引入

## 附录 A：受影响文件清单

绝对路径：
- `usnvmemu/crates/pcie_remote_userspace_sdk/src/device.rs`（PcieDevice + DeviceCtx）
- `usnvmemu/crates/pcie_remote_userspace_sdk/src/run.rs`（主循环 + 8 wire 单测）
- `usnvmemu/crates/pcie_remote_userspace_sdk/src/transport.rs`（旧 `Transport` 别名重命名点）
- `usnvmemu/crates/pcie_remote_userspace_sdk/src/lib.rs`（re-export）
- `usnvmemu/crates/pcie_remote_userspace_sdk/Cargo.toml`
- `usnvmemu/crates/pcie_remote_nvme_userspace/Cargo.toml`（删 protocol 依赖）
- `usnvmemu/crates/pcie_remote_nvme_userspace/src/controller/{mod,admin,io,completion,mmio,reservation}.rs`（~50 ctx 调用点）
- `usnvmemu/crates/pcie_remote_nvme_userspace/src/controller/tests.rs`（13 `for_testing` 改写）
- `usnvmemu/crates/pcie_remote_nvme_userspace/src/main.rs`（不动）
- workspace `/Cargo.toml` 加 `pcie_device_sdk` member
