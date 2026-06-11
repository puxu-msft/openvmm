# W0 — 抽 sans-IO `vfio_user_wire` crate 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Plan type: refactor-with-existing-tests。** 不强制 RED-first（搬运/抽取，已有测试是 GREEN 守门）；oracle = 原 96 个 test 不退化（搬运后总和 ≥ 96）+ POC-1/2 真跑 PASS。新增 pure-fn 子集补 6 个新 unit test，对应步骤显式标 "新加 RED test"。

**Goal:** 把 `usnvmemu/crates/vfio_user_transport` 里的 vfio-user wire **协议层**（编解码、消息类型、握手协商决策、access 切分、PCIe config space 状态机）抽到一个独立的 sans-IO crate `vfio_user_wire`，server 那侧的 IO（AF_UNIX、SCM_RIGHTS、reactor、mmap）**留在原 crate**，server 现有 96 个 `#[test]` 与 POC-1/2 不退化。

**Architecture:** 新 crate `usnvmemu/crates/vfio_user_wire/` 只依赖 `zerocopy + thiserror + pcie_device_core`，不依赖 nix/tokio/memmap2/libc。原 `vfio_user_transport` 通过 `pub use vfio_user_wire::*;` 兼容性 re-export 保住所有现有外部 import 路径（`nvme_firmware/src/main.rs:198` `vfio_user_transport::serve_unix` 等），下游 crate 一行不改。

**Tech Stack:** Rust 2024 edition，rustc 1.95（仓库已钉），zerocopy 0.8 + thiserror 1.0（与 `vfio_user_transport` 对齐，**不**用 `workspace.dependencies`）；测试用现有 `cargo test`；oracle 用现有 POC-1/2 Python harness。

**Spec 来源：** `docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md` §6 W0。

**Workspace 关系（重要）：** `vfio_user_transport` 与 `nvme_firmware`、`nvme_of_tcp_target` 一样**在 workspace 根 Cargo.toml 的 `exclude = [...]` 列表里**（root Cargo.toml L65-76），**不**是 workspace member。新 crate `vfio_user_wire` 同样加进 `exclude`，**不**加进 `members`——保持与其它 usnvmemu crate 一致的隔离设计。后续 cargo 命令都从 crate 自己的目录跑，不靠 workspace `-p` 选择。

**回退指引：** 任一 Task 完成后 cargo test 退化即 `git reset --hard HEAD~N`（N = 当前 Task 已 commit 数）；Task 6 POC 失败而 cargo test pass 则用 `git bisect run` 在 5 个 W0-* commit 中二分。

---

## File Structure

新建：
- `usnvmemu/crates/vfio_user_wire/Cargo.toml`
- `usnvmemu/crates/vfio_user_wire/src/lib.rs`
- `usnvmemu/crates/vfio_user_wire/src/proto.rs` —— **整文件搬自**原 `proto.rs`（825 行，16 tests）
- `usnvmemu/crates/vfio_user_wire/src/access.rs` —— **整文件搬自**原 `access.rs`（111 行，5 tests）—— 含 3 个 `pub(crate)` 项（见 Task 3 处理方案）
- `usnvmemu/crates/vfio_user_wire/src/config.rs` —— **整文件搬自**原 `config.rs`（224 行，4 tests）
- `usnvmemu/crates/vfio_user_wire/src/handshake.rs` —— **部分抽自**原 `handshake.rs`（pure 决策 + 常量；UnixStream IO 留原 crate）

修改：
- `Cargo.toml`（workspace 根）—— 在 `exclude` 列表加 `"usnvmemu/crates/vfio_user_wire"`
- `usnvmemu/crates/vfio_user_transport/Cargo.toml` —— 加 dep `vfio_user_wire = { path = "../vfio_user_wire" }`
- `usnvmemu/crates/vfio_user_transport/src/proto.rs` —— 全文替换为 `pub use vfio_user_wire::proto::*;`
- `usnvmemu/crates/vfio_user_transport/src/access.rs` —— pub re-export + 额外 `pub(crate) use` 把 3 个 `pub(crate)` 项映射回原 crate-private 可见性
- `usnvmemu/crates/vfio_user_transport/src/config.rs` —— `pub use vfio_user_wire::config::*;`
- `usnvmemu/crates/vfio_user_transport/src/handshake.rs` —— 删 pure helper、改 `server_handshake` 调 wire crate 的 pure helper；保留 UnixStream-based 函数与测试

**不动**：
- `framing.rs` / `server.rs` / `session.rs` / `dma.rs` / `irq.rs` / `transport.rs`（spec §9 audit 提的 `Message` 拆 `WireMessage + fds` 留 W0.5/W1）
- `nvme_firmware/src/main.rs` / `nvme_of_tcp_target/*`（兼容 re-export 保住 import 路径）

---

## Scope（W0 起手 vs 留 W0.5/W1）

**在 W0**：proto / access / config / handshake pure 决策 5 个文件搬运 + 25 个 wire-only test 迁移 + 6 个新 pure-fn test。
**留 W0.5/W1（不入本 plan）**：`Message` 拆 `WireMessage` + fds、`DmaRegion`/`alloc_server_msg_id`/`DmaError`/`validate_dma_reply` 提取（依赖 WireMessage 拆分）、`decide_set_irqs_action` pure 化、`NoopTransport` 迁出。

---

## Task 0：实测 baseline（在改动前定锚）

**Files:** 不改代码

理由：后续每 Task 的 oracle 数字（80/75/71/69）都建在 "原 crate baseline = 96 tests" 之上。先实测确认基线（每 fresh subagent 都该从这步起手）。

- [ ] **Step 1: 实测 baseline test 数**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && \
  cargo test --lib 2>&1 | tail -3
```

预期：末行 `test result: ok. 96 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out`。

**如果实际数字不是 96**，记真值 `N`，并把后续 Task 的预期数字按 plan 末尾"测试数推导表"换算（不要硬套 plan 内写死的数字）。预期 N=96，若 ≠96 在 commit message 里显式声明 baseline。

- [ ] **Step 2: 记下基线 commit**

```bash
cd /home/xp/refs/openvmm && git log --oneline -1
```

记最新 commit hash 为 `BASELINE`（口袋记，回退用）。

---

## Task 1：新建空 crate 骨架 + workspace 注册

**Files:**
- Create: `usnvmemu/crates/vfio_user_wire/Cargo.toml`
- Create: `usnvmemu/crates/vfio_user_wire/src/lib.rs` + 4 个空 stub 文件
- Modify: `Cargo.toml`（workspace 根 `exclude` 列表）

- [ ] **Step 1: 写新 crate Cargo.toml**

创建 `/home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire/Cargo.toml`：

```toml
[package]
name = "vfio_user_wire"
version = "0.1.0"
edition = "2024"
license = "MIT"
description = "Sans-IO vfio-user wire protocol (codec, version handshake decisions, region access chunking, PCI config space state machine). Used by vfio_user_transport (server IO) and forthcoming vfio_user_device (underhill client)."

[dependencies]
# 与 vfio_user_transport/Cargo.toml L10/L14 对齐版本; 不用 workspace.dependencies
# 因为本 crate 与 vfio_user_transport 一样在 workspace exclude 列表 (workspace =
# true 仅 member 可用).
zerocopy = { version = "0.8", features = ["derive"] }
thiserror = "1.0"
pcie_device_core = { path = "../pcie_device_core" }
```

不加 `[lints] workspace = true`（同因：不是 member）。

- [ ] **Step 2: 写 lib.rs 占位 + 4 个空 stub 模块**

创建 `/home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire/src/lib.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `vfio_user_wire` —— sans-IO vfio-user 线协议层。
//!
//! 抽自 `vfio_user_transport`（W0，2026-06-11）。本 crate 只含**无 IO**
//! 的协议事实：
//!
//! - [`proto`] —— Header / Command / Payload 编解码（zerocopy 字节布局）
//! - [`access`] —— REGION 访问的 sans-IO 对齐切分
//! - [`config`] —— PCIe Config Space 状态机（无 IO，输入 `DeviceDescribe`）
//! - [`handshake`] —— VERSION 协商纯决策 + server caps 常量
//!
//! IO（AF_UNIX 收发、SCM_RIGHTS、mmap、reactor）由调用方负责：
//! - server 端：`vfio_user_transport`（同仓）
//! - client 端：`vfio_user_device`（W1 起做，underhill 内）

#![deny(unsafe_op_in_unsafe_fn)]

pub mod access;
pub mod config;
pub mod handshake;
pub mod proto;
```

然后建 4 个空 stub 文件（让 `cargo check` 不抱怨 missing module）：

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire/src && \
  touch proto.rs access.rs config.rs handshake.rs
```

- [ ] **Step 3: 注册到 workspace 的 `exclude`**

修改 `/home/xp/refs/openvmm/Cargo.toml`。找到（约 L65）：

```toml
exclude = [
  "xsync",
  ...
  "usnvmemu/crates/vfio_user_transport",
  ...
]
```

在 `vfio_user_transport` 那行**前**插入：

```toml
  "usnvmemu/crates/vfio_user_wire",
```

**不要**把它加到 `members`（会破坏与其它 usnvmemu crate 的隔离一致性，且会让 workspace 编 deps 膨胀）。

- [ ] **Step 4: 验证编译**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire && cargo check 2>&1 | tail -5
```

预期：`Finished ... in N.NNs`，0 errors。warnings 允许（4 个空 mod 会有 "missing documentation" 风险——不强制 `missing_docs` lint，已删）。

- [ ] **Step 5: 提交**

```bash
cd /home/xp/refs/openvmm
git add Cargo.toml usnvmemu/crates/vfio_user_wire/
git commit -m "feat(vfio_user_wire): W0-1 新建 sans-IO crate 骨架

抽自 vfio_user_transport 的承载 crate, 准备搬 proto/access/config/
handshake 4 个 sans-IO 模块. deps: zerocopy + thiserror + pcie_device_core
(无 nix/tokio/memmap2/libc).

注: 加进 workspace exclude (与 vfio_user_transport/nvme_firmware/
nvme_of_tcp_target 一致), 不进 members. cargo 命令从 crate 目录跑.

只是骨架, 4 个 pub mod 文件为空; 下一步搬 proto.rs 整文件.

Spec: docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md §6 W0"
```

---

## Task 2：搬 `proto.rs` 整文件 + 兼容 re-export

**Files:**
- Create-via-copy: `usnvmemu/crates/vfio_user_wire/src/proto.rs`（来自 `usnvmemu/crates/vfio_user_transport/src/proto.rs`）
- Modify: `usnvmemu/crates/vfio_user_transport/src/proto.rs`（改成 re-export）
- Modify: `usnvmemu/crates/vfio_user_transport/Cargo.toml`（加 `vfio_user_wire` dep）

- [ ] **Step 1: 拷贝整文件**

```bash
cd /home/xp/refs/openvmm
cp usnvmemu/crates/vfio_user_transport/src/proto.rs \
   usnvmemu/crates/vfio_user_wire/src/proto.rs
```

确认无 `use crate::framing` / `use crate::session` 类跨模块 IO 依赖：

```bash
grep -n '^use ' usnvmemu/crates/vfio_user_wire/src/proto.rs | grep -v '^use zerocopy' | grep -v '^use thiserror' | head -10
```

预期：无输出（或仅 `use std::...` 标准库）。若有 `use crate::framing::*` 等，**停步**先报告——意味着原始评估漏看，需要解耦再继续。

- [ ] **Step 2: 验证新 crate 单独编译 + 16 个 test 跑通**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire && cargo test --lib 2>&1 | tail -5
```

预期：`test result: ok. 16 passed; 0 failed`。

测试名应见：`header_layout_is_16_bytes_le`, `command_numeric_values_match_header`, `command_from_u16_roundtrip`, `header_flags_semantics`, `header_command_builder_sets_msg_size`, `header_reply_err_carries_errno`, `payload_struct_sizes`, `encode_decode_header_only_roundtrip`, `encode_decode_with_payload_roundtrip`, `decode_header_rejects_short_buf`, `decode_header_rejects_bad_msg_size`, `decode_payload_strict_length`, `decode_header_rejects_oversized_msg`, `validate_msg_in_buffer_rejects_short_buffer`, `command_tryfrom_error_path`, `dma_flag_constants`。

- [ ] **Step 3: 给 vfio_user_transport 加 dep**

在 `/home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport/Cargo.toml` 的 `[dependencies]` 段（与 `pcie_device_core` 同级附近）插入：

```toml
vfio_user_wire = { path = "../vfio_user_wire" }
```

- [ ] **Step 4: 把 vfio_user_transport 的 proto.rs 改成 re-export**

把 `/home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport/src/proto.rs` 全文替换为：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W0-2: proto 模块已迁移到 sans-IO crate `vfio_user_wire`. 本文件保留为
//! 兼容性 re-export, 保住所有现有 `crate::proto::*` / `vfio_user_transport::proto::*`
//! 引用路径不破.
//!
//! 新代码应直接 `use vfio_user_wire::proto::*;`.

pub use vfio_user_wire::proto::*;
```

- [ ] **Step 5: 验证原 crate 所有测试不退化**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && cargo test --lib 2>&1 | tail -5
```

预期：`test result: ok. 80 passed; 0 failed`（96 - 16 = 80，16 已迁到 wire crate）。

若编译错误，多半是 `pub(crate)` 项被 server 端用了通配 re-export 漏掉。grep proto.rs：

```bash
grep -n 'pub(crate)' /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire/src/proto.rs
```

若有命中（预期：无；proto.rs 全 `pub`），在 transport-crate 的 proto.rs 补 `pub(crate) use vfio_user_wire::proto::<那些项名>;`，再跑 Step 5。

- [ ] **Step 6: 验证 nvme_firmware bin 仍能编**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/nvme_firmware && cargo build --bin nvme_firmware 2>&1 | tail -5
```

`nvme_firmware` `default-features = ["openhcl", "vfio-user"]`（Cargo.toml L39），所以裸 `cargo build` 已含 `--features vfio-user`，无需显式加。

预期：`Finished` clean。

- [ ] **Step 7: 提交**

```bash
cd /home/xp/refs/openvmm
git add usnvmemu/crates/vfio_user_wire/src/proto.rs \
        usnvmemu/crates/vfio_user_transport/Cargo.toml \
        usnvmemu/crates/vfio_user_transport/src/proto.rs
git commit -m "feat(vfio_user_wire): W0-2 搬 proto.rs 整文件 (16 tests)

proto.rs 是完全 sans-IO 的 wire 编解码 (Header/Command/Payload struct
+ encode/decode helper + ProtoError + flag 子模块), 0 个 use crate::,
0 个 IO 依赖, 16 个 #[test] 全是字节布局/编解码断言.

整文件搬到 vfio_user_wire crate, 在 vfio_user_transport/src/proto.rs
保留 pub use vfio_user_wire::proto::* re-export, 保住:
- nvme_firmware/src/main.rs:198 vfio_user_transport::serve_unix 路径
- vfio_user_transport 内部 use crate::proto::* 路径
- 16 个 wire-only test 迁到 vfio_user_wire (test count 96 → 80+16)

Verify: cargo test (in vfio_user_wire) 16 passed,
        cargo test (in vfio_user_transport) 80 passed,
        cargo build --bin nvme_firmware (in nvme_firmware, default
        features 含 vfio-user) clean"
```

---

## Task 3：搬 `access.rs` 整文件 + 处理 `pub(crate)` 项

**Files:**
- Create-via-copy: `usnvmemu/crates/vfio_user_wire/src/access.rs`
- Modify: `usnvmemu/crates/vfio_user_transport/src/access.rs`

**`pub(crate)` 处理策略**（重要）：

原 `vfio_user_transport/src/access.rs` 含 3 个 `pub(crate)` 项被 `config.rs:134` 和 `session.rs:468/487` 调用：
- `pub(crate) const MAX_CHUNK_MMIO: u32 = 8;` (L29)
- `pub(crate) const MAX_CHUNK_CONFIG: u32 = 4;` (L32)
- `pub(crate) fn register_chunks(offset: u64, count: usize, max: u32) -> Vec<(u64, u32)>` (L38)

通配 `pub use vfio_user_wire::access::*;` **不会**搬运 `pub(crate)` 项。方案：**搬到 wire crate 时全部提为 `pub`**（理由：本来就是要给同 workspace 内的 server/client 用，`pub(crate)` 在 wire crate 内反而是误约束；wire crate 整个就是个内部协议层，`pub` 不破封装）。`vfio_user_transport` 通过 `pub use` 通配吸收。

- [ ] **Step 1: 拷贝整文件 + 把 3 个 `pub(crate)` 改 `pub`**

```bash
cd /home/xp/refs/openvmm
cp usnvmemu/crates/vfio_user_transport/src/access.rs \
   usnvmemu/crates/vfio_user_wire/src/access.rs
```

修改 `/home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire/src/access.rs`，把 3 处 `pub(crate)` 改为 `pub`：

```bash
sed -i 's/pub(crate) const MAX_CHUNK_MMIO/pub const MAX_CHUNK_MMIO/;
        s/pub(crate) const MAX_CHUNK_CONFIG/pub const MAX_CHUNK_CONFIG/;
        s/pub(crate) fn register_chunks/pub fn register_chunks/' \
   /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire/src/access.rs
```

验证：

```bash
grep -n '^pub' /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire/src/access.rs
```

预期 3 行：`pub const MAX_CHUNK_MMIO`、`pub const MAX_CHUNK_CONFIG`、`pub fn register_chunks`，**无** `pub(crate)`。

- [ ] **Step 2: 验证 wire crate 测试**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire && cargo test --lib 2>&1 | tail -5
```

预期：`test result: ok. 21 passed`（16 proto + 5 access）。测试名：`empty_when_zero_count`, `aligned_whole_access_is_single_chunk`, `non_power_of_two_count_splits_largest_first`, `unaligned_offset_degrades_until_aligned`, `config_max_caps_at_dword`。

- [ ] **Step 3: vfio_user_transport access.rs 改 re-export**

`/home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport/src/access.rs` 全文替换为：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W0-3: access 模块已迁移到 `vfio_user_wire`. re-export 兼容.
//!
//! 注: 原 `pub(crate)` 3 项 (MAX_CHUNK_MMIO / MAX_CHUNK_CONFIG /
//! register_chunks) 在 wire crate 提为 pub (因为 wire crate 整体就是
//! 内部协议层, pub(crate) 反而是误约束), 这里通过 `pub use` 通配吸收;
//! 原 crate 内对 `crate::access::register_chunks` 等调用一行不改.
pub use vfio_user_wire::access::*;
```

- [ ] **Step 4: 验证原 crate 不退化**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && cargo test --lib 2>&1 | tail -5
```

预期：`test result: ok. 75 passed`（80 - 5 access 已迁）。

若编译错误（如 `register_chunks` 不可见），grep `crate::access` 的实际调用点排查：

```bash
grep -rn 'crate::access' /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport/src/
```

正常应是 `crate::access::register_chunks(...)` 和 `crate::access::MAX_CHUNK_MMIO` 类——`pub use` 通配后这些会找到 wire crate 的 pub 版本。

- [ ] **Step 5: 提交**

```bash
cd /home/xp/refs/openvmm
git add usnvmemu/crates/vfio_user_wire/src/access.rs \
        usnvmemu/crates/vfio_user_transport/src/access.rs
git commit -m "feat(vfio_user_wire): W0-3 搬 access.rs 整文件 (5 tests)

access.rs 是纯算法 (REGION 访问对齐切分), 0 依赖, 5 个 unit test.

3 个 pub(crate) 项 (MAX_CHUNK_MMIO/MAX_CHUNK_CONFIG/register_chunks)
在 wire crate 提为 pub (本 crate 即内部协议层, pub(crate) 是误约束);
vfio_user_transport 通过 pub use 通配吸收, crate::access::* 调用点
一行不改.

Verify: cargo test (in vfio_user_wire) 21 passed,
        cargo test (in vfio_user_transport) 75 passed"
```

---

## Task 4：搬 `config.rs` 整文件

**Files:**
- Create-via-copy: `usnvmemu/crates/vfio_user_wire/src/config.rs`
- Modify: `usnvmemu/crates/vfio_user_transport/src/config.rs`

`config.rs` 已 `use pcie_device_core::{BarKind, DeviceDescribe, describe::cfg_offset}`，新 crate 的 Cargo.toml 已在 Task 1 加好这个 dep，故搬运无摩擦。原文件**无** `pub(crate)` 项（grep 验过），通配 re-export 干净。

- [ ] **Step 1: 拷贝整文件**

```bash
cd /home/xp/refs/openvmm
cp usnvmemu/crates/vfio_user_transport/src/config.rs \
   usnvmemu/crates/vfio_user_wire/src/config.rs
```

确认无 `pub(crate)` + 依赖项可见：

```bash
grep -n 'pub(crate)\|^use ' /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire/src/config.rs | head -10
```

预期：无 `pub(crate)`；`use pcie_device_core::*` / `use crate::proto::*` 等是同 wire crate 内或已有 dep，OK。

- [ ] **Step 2: 验证 wire crate 测试**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire && cargo test --lib 2>&1 | tail -5
```

预期：`test result: ok. 25 passed`（21 + 4）。测试名：`read_identity`, `command_rw_identity_ro`, `bar0_size_probe`, `bar_64bit_high_dword_probe`。

- [ ] **Step 3: vfio_user_transport config.rs 改 re-export**

`/home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport/src/config.rs` 全文替换为：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W0-4: config 模块已迁移到 `vfio_user_wire`. re-export 兼容.
pub use vfio_user_wire::config::*;
```

- [ ] **Step 4: 验证原 crate 不退化**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && cargo test --lib 2>&1 | tail -5
```

预期：`test result: ok. 71 passed`（75 - 4 config 已迁）。

- [ ] **Step 5: 提交**

```bash
cd /home/xp/refs/openvmm
git add usnvmemu/crates/vfio_user_wire/src/config.rs \
        usnvmemu/crates/vfio_user_transport/src/config.rs
git commit -m "feat(vfio_user_wire): W0-4 搬 config.rs 整文件 (4 tests)

PCIe Config Space 状态机, sans-IO (输入 DeviceDescribe / 输出字节).
依赖 pcie_device_core (wire crate 已 dep), 4 个 unit test, 无 pub(crate)
项. 通配 re-export 干净.

Verify: cargo test (in vfio_user_wire) 25 passed,
        cargo test (in vfio_user_transport) 71 passed"
```

---

## Task 5：从 `handshake.rs` 抽 pure 决策子集

**Files:**
- Create: `usnvmemu/crates/vfio_user_wire/src/handshake.rs`
- Modify: `usnvmemu/crates/vfio_user_transport/src/handshake.rs`

handshake.rs 含 IO（`UnixStream` + `framing::{read_message,write_message}`）+ pure 决策（minor 协商、caps 解析、reply payload 构造）。**只搬 pure 部分**，IO 留原 crate。

**新加 6 个 unit test（RED-first 用 wire crate 的 `cargo test` 验证）**：覆盖 `negotiate_minor` / `parse_caps_blob` / `build_version_reply_payload` 行为锚。

- [ ] **Step 1: 写 wire crate 端 sans-IO handshake.rs（包含 6 个 RED test）**

创建 `/home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire/src/handshake.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VERSION 握手的 sans-IO 决策与常量.
//!
//! 抽自 `vfio_user_transport::handshake` (W0-5, 2026-06-11). 本模块只含
//! **无 IO** 的协商决策:
//!
//! - [`SERVER_MAX_DATA_XFER_SIZE`] / [`SERVER_CAPS_JSON`] —— server 端常量
//! - [`Negotiated`] —— 协商结果数据结构
//! - [`negotiate_minor`] —— pure fn: `min(client_minor, PROTOCOL_MINOR)`
//! - [`parse_caps_blob`] —— NUL-terminated bytes → UTF-8 String
//! - [`build_version_reply_payload`] —— 构造 VERSION reply 字节 (不含 IO)
//!
//! UnixStream 收发 (`server_handshake`) 留在 `vfio_user_transport`.

use crate::proto::PROTOCOL_MAJOR;
use crate::proto::PROTOCOL_MINOR;
use crate::proto::ProtoError;
use crate::proto::VersionPayload;
use zerocopy::IntoBytes;

/// 本 server 广告的 `max_data_xfer_size` (字节).
///
/// **vfio-spec 语义**: `max_data_xfer_size` 是 per-receiver 的——发送方不得超过
/// 接收方广告的值. `REGION_READ/WRITE` 是 client→server (接收方 = 本 server),
/// 故服务端用本常量作为 REGION 访问 `count` 的上限. client 广告的值只对反方向
/// (server→client 的 `DMA_READ/WRITE`) 有意义.
///
/// 1 MiB = spec 默认, 且 ≤ QEMU 64 MiB 上限. 必须与 [`SERVER_CAPS_JSON`] 里
/// 广告的数值一致 (`server_caps_advertises_declared_max_xfer` drift gate 测试钉死).
pub const SERVER_MAX_DATA_XFER_SIZE: usize = 1_048_576;

/// 本 server 默认 advertise 的 capabilities JSON.
///
/// **review (真 QEMU 11 oracle)** — `max_msg_fds` 须 ≤ 客户端可接受上限. QEMU
/// v11.0.1 的 `VFIO_USER_MAX_MAX_FDS = 16`, server 广告 > 16 会被判 "malformed
/// max_msg_fds" 握手失败. 广告 8 (= QEMU 默认 `VFIO_USER_DEF_MAX_FDS`) 对所有
/// client 安全; NVMe 单 `SET_IRQS` 只需 msix_count(≤4) 个 fd / `DMA_MAP` 1 个,
/// 8 足够.
pub const SERVER_CAPS_JSON: &str = concat!(
    "{",
    "\"capabilities\":{",
    "\"max_msg_fds\":8,",
    "\"max_data_xfer_size\":1048576,",
    "\"max_dma_maps\":65535,",
    "\"pgsizes\":4096",
    "}",
    "}"
);

/// 握手后协商出的双方 capabilities 摘要.
#[derive(Debug, Clone)]
pub struct Negotiated {
    /// client 上报的 protocol major (与 [`PROTOCOL_MAJOR`] 必须相等).
    pub client_major: u16,
    /// client 上报的 protocol minor. spec 允许 server.minor ≤ client.minor.
    pub client_minor: u16,
    /// client 上报的 capabilities JSON 原文 (已 UTF-8 解码 / NUL 去尾).
    pub client_caps_json: String,
}

/// **vfio-spec 协商规则**: server reply 的 version 须 **≤** client 提议.
///
/// 回比 client 高的 minor 会被 QEMU 判 "incompatible server version" 断开.
/// 此前硬编码 `PROTOCOL_MINOR(=1)`, QEMU 11 提议 minor=0 → 回 1 > 0 → 握手失败.
#[inline]
pub fn negotiate_minor(client_minor: u16) -> u16 {
    client_minor.min(PROTOCOL_MINOR)
}

/// 把 VERSION JSON 字节段 (可能带 trailing NUL + padding) 转 String.
///
/// 找首个 NUL; spec 说 NUL-terminated.
pub fn parse_caps_blob(raw: &[u8]) -> Result<String, ProtoError> {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let s =
        std::str::from_utf8(&raw[..end]).map_err(|e| ProtoError::BadJson(format!("UTF-8: {e}")))?;
    Ok(s.to_string())
}

/// 构造 VERSION reply 的 payload 字节: `VersionPayload(4) + caps_json + NUL`.
///
/// 返回 `Vec<u8>`, caller 负责包 Header 并写到 UnixStream.
pub fn build_version_reply_payload(client_minor: u16, server_caps: &[u8]) -> Vec<u8> {
    let payload_len = 4 + server_caps.len() + 1;
    let mut reply_payload = Vec::with_capacity(payload_len);
    let ver_reply = VersionPayload {
        major: PROTOCOL_MAJOR,
        minor: negotiate_minor(client_minor),
    };
    reply_payload.extend_from_slice(ver_reply.as_bytes());
    reply_payload.extend_from_slice(server_caps);
    reply_payload.push(0); // NUL
    reply_payload
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **drift gate** —— `SERVER_MAX_DATA_XFER_SIZE` 须与 `SERVER_CAPS_JSON`
    /// 里广告的数值一致; 任一改了忘改另一个, REGION 上限就和广告脱节.
    #[test]
    fn server_caps_advertises_declared_max_xfer() {
        let needle = format!("\"max_data_xfer_size\":{SERVER_MAX_DATA_XFER_SIZE}");
        assert!(
            SERVER_CAPS_JSON.contains(&needle),
            "SERVER_CAPS_JSON 须广告与 SERVER_MAX_DATA_XFER_SIZE 一致的值: {needle}"
        );
        assert_eq!(
            SERVER_CAPS_JSON.matches("max_data_xfer_size").count(),
            1,
            "max_data_xfer_size 字段须恰好出现一次"
        );
    }

    /// **协商规则锚**: `negotiate_minor` 严格 ≤ client_minor 且 ≤ PROTOCOL_MINOR.
    #[test]
    fn negotiate_minor_returns_min_of_client_and_server() {
        // client < server
        assert_eq!(negotiate_minor(0), 0u16.min(PROTOCOL_MINOR));
        // client == server
        assert_eq!(negotiate_minor(PROTOCOL_MINOR), PROTOCOL_MINOR);
        // client > server
        assert_eq!(
            negotiate_minor(PROTOCOL_MINOR.saturating_add(5)),
            PROTOCOL_MINOR
        );
    }

    /// reply payload 字节布局锚: 4 bytes VersionPayload + caps bytes + 1 NUL.
    #[test]
    fn build_version_reply_payload_has_expected_layout() {
        let caps = b"{\"capabilities\":{}}";
        let payload = build_version_reply_payload(0, caps);
        assert_eq!(payload.len(), 4 + caps.len() + 1);
        // 末字节是 NUL
        assert_eq!(*payload.last().unwrap(), 0);
        // 头 4 字节是 VersionPayload: major (u16 le) + minor (u16 le)
        assert_eq!(&payload[0..2], PROTOCOL_MAJOR.to_le_bytes());
        // minor = min(0, PROTOCOL_MINOR) = 0
        assert_eq!(&payload[2..4], 0u16.to_le_bytes());
        // 中间是 caps bytes
        assert_eq!(&payload[4..4 + caps.len()], caps);
    }

    /// parse_caps_blob 切首个 NUL 并 UTF-8 解码.
    #[test]
    fn parse_caps_blob_strips_nul_and_padding() {
        let raw = b"{\"x\":1}\0\0\0";
        assert_eq!(parse_caps_blob(raw).unwrap(), "{\"x\":1}");
    }

    /// parse_caps_blob 无 NUL 视全段为 JSON.
    #[test]
    fn parse_caps_blob_no_nul_uses_full_slice() {
        let raw = b"{\"x\":1}";
        assert_eq!(parse_caps_blob(raw).unwrap(), "{\"x\":1}");
    }

    /// parse_caps_blob 拒绝非 UTF-8.
    #[test]
    fn parse_caps_blob_rejects_invalid_utf8() {
        let raw = &[0xff, 0xfe, 0xfd, 0x00];
        assert!(matches!(parse_caps_blob(raw), Err(ProtoError::BadJson(_))));
    }
}
```

- [ ] **Step 2: 验证 wire crate 新测试通过**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire && cargo test --lib 2>&1 | tail -5
```

预期：`test result: ok. 31 passed`（25 + 6 新 handshake test）。

- [ ] **Step 3: 改 vfio_user_transport handshake.rs**

修改 `/home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport/src/handshake.rs`（**用 find-replace 文本块策略，不靠行号**，因为前几步行号已漂）：

(a) **加 use 行**（在 `use crate::framing::...` 附近添加）：

```rust
use vfio_user_wire::handshake::{build_version_reply_payload, parse_caps_blob};
```

注：`negotiate_minor` 由 `build_version_reply_payload` 内部调用，无需在 transport-crate 直接 use。

(b) **删本地 pure helpers + 加 re-export**。找到这块文本：

```rust
pub const SERVER_MAX_DATA_XFER_SIZE: usize = 1_048_576;
```

向下连续删除到（含）`pub const SERVER_CAPS_JSON: &str = concat!(...);`（多行），以及紧随的 `pub struct Negotiated { ... }`（含 doc comment 与 derive，找它结束的 `}`），共 3 个 pub 项。在删除位置插入：

```rust
pub use vfio_user_wire::handshake::{
    Negotiated, SERVER_CAPS_JSON, SERVER_MAX_DATA_XFER_SIZE,
};
```

(c) **删本地 `parse_caps_blob` fn**。找到（约 L180）：

```rust
fn parse_caps_blob(raw: &[u8]) -> Result<String, ProtoError> {
```

删整个 fn 体（到对应 `}`）。`use vfio_user_wire::handshake::parse_caps_blob;`（Step 3-a 已加）将让原 `server_handshake` 内对 `parse_caps_blob(...)` 的调用绑到 wire crate 的 pub fn。

(d) **替换 `server_handshake` 内 reply 构造段**。找到（约 L155-170）这块：

```rust
    let payload_len = 4 + server_caps.len() + 1;
    let mut reply_payload = Vec::with_capacity(payload_len);
    let ver_reply = VersionPayload {
        major: PROTOCOL_MAJOR,
        minor: client_minor.min(PROTOCOL_MINOR),
    };
    reply_payload.extend_from_slice(ver_reply.as_bytes());
    reply_payload.extend_from_slice(server_caps);
    reply_payload.push(0); // NUL
```

整段替换为：

```rust
    let reply_payload = build_version_reply_payload(client_minor, server_caps);
    let payload_len = reply_payload.len();
```

(e) **删过时 imports**。`server_handshake` 不再直接用 `PROTOCOL_MAJOR` / `VersionPayload` / `zerocopy::IntoBytes`（已由 `build_version_reply_payload` 内部消化），grep 确认无残留引用后删：

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && \
  grep -n 'PROTOCOL_MAJOR\|VersionPayload\|zerocopy::IntoBytes' src/handshake.rs
```

若仅在已删行/已替换段命中，安全删 use 行。**`PROTOCOL_MINOR` 仍可能被 `client_minor.min(PROTOCOL_MINOR)` 之外的代码用**——若 grep 还命中其它地方就保留 use。

(f) **删迁出的本地测试**。在 `handshake.rs` 的 `#[cfg(test)] mod tests` 段找：
- `fn server_caps_advertises_declared_max_xfer()` 整段（约 L218-230）—— 已搬到 wire crate
- `fn handshake_replies_min_minor_not_higher_than_client()` 整段（约 L272 附近）—— wire crate 的 `negotiate_minor_returns_min_of_client_and_server` 覆盖同一性质

整 fn 删除（含 `#[test]` 属性 + body + 闭合 `}`）。

**保留** 5 个 UnixStream-based 测试：`handshake_happy_path`, `handshake_caps_minor_at_server_supported_max`, `handshake_rejects_non_version_first_message`, `handshake_rejects_major_mismatch`, `handshake_rejects_bad_utf8_caps`。

- [ ] **Step 4: 验证 vfio_user_transport 编译 + 全部测试**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && cargo test --lib 2>&1 | tail -5
```

预期：`test result: ok. 69 passed`（71 - 2 个迁出）。

若编译错误（多半是 use 路径漏改 / `Negotiated` re-export 顺序问题），按 rustc 报错调整。**回退策略**：本 Task 涉及较多 hand-edit，若失败定位窄不到：

```bash
cd /home/xp/refs/openvmm && git reset --hard HEAD  # 回滚未 commit 的修改
# 重做 Step 3
```

- [ ] **Step 5: 跑 wire crate + nvme_firmware 全量验证**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire && \
  cargo test --lib 2>&1 | tail -3 && \
cd /home/xp/refs/openvmm/usnvmemu/crates/nvme_firmware && \
  cargo build --bin nvme_firmware 2>&1 | tail -3
```

预期：wire 31 passed + nvme_firmware clean build。

- [ ] **Step 6: 提交**

```bash
cd /home/xp/refs/openvmm
git add usnvmemu/crates/vfio_user_wire/src/handshake.rs \
        usnvmemu/crates/vfio_user_transport/src/handshake.rs
git commit -m "feat(vfio_user_wire): W0-5 抽 handshake pure 决策子集 (6 tests)

从 vfio_user_transport::handshake 抽出无 IO 部分到 wire crate:
- SERVER_MAX_DATA_XFER_SIZE / SERVER_CAPS_JSON 常量
- Negotiated 结构
- negotiate_minor() pure fn — min(client_minor, PROTOCOL_MINOR) 协商规则
- parse_caps_blob() — NUL-terminated bytes → UTF-8 String
- build_version_reply_payload() — 构造 VERSION reply 字节

server_handshake (UnixStream IO) 留 vfio_user_transport, 改成调用 wire
crate 的 pure helper. wire crate 给 6 个新单测覆盖协商规则 + drift gate
+ 字节布局 + NUL 处理 + invalid UTF-8 拒绝.

测试分布: vfio_user_wire 31 passed (25 搬运 + 6 新),
        vfio_user_transport 69 passed (5 socketpair 集成 handshake +
        64 其他, 2 个 pure test 迁到 wire crate)."
```

---

## Task 6：POC 烟测 + W0 完工 oracle

W0 完工 oracle：原 96 个 test 不退化（实际拆为 wire 31 + transport 69 = 100 ≥ 96）+ POC-1/2 真跑 + clippy clean。

**Files:** 不改代码，只跑 oracle

- [ ] **Step 1: 各 crate 单独 check 不退化**

不跑 `cargo check --workspace`（B1 教训：会编 workspace member 全集而新 crate 在 exclude，且 vfio_user_transport/nvme_firmware 也在 exclude）。

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire && cargo check 2>&1 | tail -3 && \
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && cargo check 2>&1 | tail -3 && \
cd /home/xp/refs/openvmm/usnvmemu/crates/nvme_firmware && cargo check 2>&1 | tail -3
```

预期：3 个 `Finished`，0 errors。

- [ ] **Step 2: wire + transport 全测**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire && \
  cargo test --lib 2>&1 | tail -3 && \
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && \
  cargo test --lib 2>&1 | tail -3
```

预期：wire **31 passed** + transport **69 passed** = 100 total，**≥** baseline 96。

- [ ] **Step 3: nvme_firmware bin 编 + clippy**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/nvme_firmware && \
  cargo build --bin nvme_firmware 2>&1 | tail -3 && \
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire && \
  cargo clippy --lib -- -D warnings 2>&1 | tail -3 && \
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && \
  cargo clippy --lib -- -D warnings 2>&1 | tail -3
```

预期：3 个都 clean（0 warning，clippy `-D warnings` 强约束）。

- [ ] **Step 4: 预先 build firmware bin（用 POC_SKIP_BUILD 复用）**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/nvme_firmware && \
  cargo build --bin nvme_firmware 2>&1 | tail -3
```

`nvme_firmware` `default-features = ["openhcl", "vfio-user"]`（Cargo.toml L39），裸 build 已含 vfio-user transport。验证 binary 存在：

```bash
ls -lh /home/xp/refs/openvmm/usnvmemu/crates/nvme_firmware/target/debug/nvme_firmware
```

- [ ] **Step 5: POC-1 烟测（DMA 零拷贝端到端）**

```bash
POC_SKIP_BUILD=1 python3 /home/xp/refs/openvmm/usnvmemu/experiments/2026-06-11-openhcl-vfio-user-client-poc/poc1_dma_zerocopy.py 2>&1 | tail -15
```

`POC_SKIP_BUILD=1` 复用 Step 4 build 的 binary，避免 POC 内部用 `cargo build --bin nvme_firmware`（不含 features 参数，但默认 features 已含 vfio-user，所以即便 POC 自 build 也 OK——`POC_SKIP_BUILD=1` 只是省 build 时间 + 确定性）。

预期：脚本最后输出 `PASS` / `PASSED` / `OK`（具体格式查 `poc1_dma_zerocopy.py` 末尾 print）。

若 POC 协议层失败（说明 wire crate 抽取有 byte-level drift），**立刻回退 W0-5**：

```bash
cd /home/xp/refs/openvmm && git log --oneline -6
# 找到 W0-5 commit hash, 例如 abc1234
git reset --hard abc1234^   # 回到 W0-5 之前
# 单独调查 handshake.rs 差异
```

- [ ] **Step 6: POC-2 烟测（MSI-X eventfd）**

```bash
POC_SKIP_BUILD=1 python3 /home/xp/refs/openvmm/usnvmemu/experiments/2026-06-11-openhcl-vfio-user-client-poc/poc2_msix_eventfd.py 2>&1 | tail -15
```

预期：`PASS`。

- [ ] **Step 7: nvme_of_tcp_target NoopTransport 路径不破**

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/nvme_of_tcp_target && \
  cargo test --lib 2>&1 | tail -3
```

预期：原 250+ tests `ok`，全 passed。`nvme_of_tcp_target` 仍 `use vfio_user_transport::NoopTransport`（W0 不动 transport.rs），路径未变。

- [ ] **Step 8: 更新 spec 状态 + 提交 W0 完工标记**

取本会话 5 个 W0 commit 的首尾 hash：

```bash
cd /home/xp/refs/openvmm
W0_FIRST=$(git log --oneline --author="$(git config user.email)" --since='2026-06-11' --grep='W0-1' -1 --format='%h')
W0_LAST=$(git log --oneline --author="$(git config user.email)" --since='2026-06-11' --grep='W0-5' -1 --format='%h')
echo "W0_FIRST=$W0_FIRST W0_LAST=$W0_LAST"
```

用真实 hash 替换 spec 行 87 占位。打开 `/home/xp/refs/openvmm/docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md`，找到 L87：

```
- **W0**：抽 `vfio_user_wire`（主仓 sans-IO crate；framing 留 server；server 96 测试不破）。
```

替换为（把 `<W0_FIRST>` `<W0_LAST>` 换成上面拿到的真值）：

```
- **W0**：✅ 完成（2026-06-11，commit <W0_FIRST>→<W0_LAST>）。
```

提交：

```bash
cd /home/xp/refs/openvmm
git add docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md
git commit -m "spec(W0): 标 W0 完成 — sans-IO vfio_user_wire crate 抽出

5 个 commit 链 (W0-1..W0-5) 完成 W0:
- 新建 vfio_user_wire crate (zerocopy + thiserror + pcie_device_core,
  workspace exclude 而非 member, 与其它 usnvmemu crate 一致)
- 搬 proto.rs / access.rs / config.rs 整文件 + 兼容 re-export
  (access.rs 3 个 pub(crate) 在 wire crate 提为 pub, 因 wire 即内部
  协议层 pub(crate) 是误约束; transport 通过 pub use 通配吸收)
- 抽 handshake pure 决策子集 (negotiate_minor / parse_caps_blob /
  build_version_reply_payload), UnixStream IO 留 vfio_user_transport

oracle:
- cargo test (in vfio_user_wire) 31 passed (25 搬运 + 6 新)
- cargo test (in vfio_user_transport) 69 passed (5 socketpair 集成
  handshake + 64 其他)
- POC-1 (DMA 零拷贝端到端) PASS
- POC-2 (MSI-X eventfd) PASS
- nvme_of_tcp_target 全测不退化 (NoopTransport 路径未动)
- clippy --lib -D warnings clean (wire / transport)

W0 不动 framing/server/session/dma/irq/transport — Message vs WireMessage
拆分留 W0.5/W1 (依赖 fd-bearing 类型重构, 量大不入 W0 scope).

spec §7 提的 '真 QEMU 11 e2e 不退' 不在本 plan 强制 (Task 6 验跨进程
协议层 POC-1/2 已是真 wire), 用户后续可单跑 vfio_user_transport/scripts/
qemu_interop/ 真 QEMU harness."
```

---

## 测试数推导表（baseline 校准用）

baseline 设为 N（Task 0 实测，预期 96）。Task 间分布：

| Task | wire crate test 数 | transport crate test 数 | sum |
|---|---|---|---|
| baseline | 0 | N (96) | N |
| 2 (proto) | 16 | N − 16 (80) | N |
| 3 (access) | 16 + 5 = 21 | N − 16 − 5 (75) | N |
| 4 (config) | 21 + 4 = 25 | N − 16 − 5 − 4 (71) | N |
| 5 (handshake) | 25 + 6 = 31 | N − 16 − 5 − 4 − 2 (69) | N + 4 |

handshake 步骤多 4 个 net test（迁出 2 个、新加 6 个）。最终 sum **应 > baseline**，不是 = baseline。

如 Task 0 实测 N ≠ 96，用 plan-doc 推导逻辑（不是写死的数字）核对 oracle：

- Task N 后 wire crate test 数应增量 = 当 Task 该迁移的 #test
- Task N 后 transport crate test 数应减量 = 同上
- 总和直到 Task 5 才偏离 N（净 +4）

---

## Open risks / not-W0 followups

W0 完工后留这些项给 W0.5/W1 plan：

- `framing::Message` 拆 `WireMessage { header, payload } + Message { wire, fds }`（spec §9 audit 提的，影响 server 端 session/dma/irq 多处 `msg.fds` 用法 → 大量小改动）
- `dma::{DmaRegion, alloc_server_msg_id, DmaError, validate_dma_reply}` 提到 wire crate（依赖 WireMessage 拆分）
- `irq::decide_set_irqs_action` pure 化（独立小任务，可入 W0.5）
- `vfio_user_transport::NoopTransport` 从该 crate 移出（nvme_of_tcp_target 不该因 `NoopTransport` 拉整 vfio crate；可放 `pcie_device_core` 当 test helper，独立任务）
- 真 QEMU 11 e2e（spec §7 要求 W0 后不退化，本 plan 不强制；用户单跑 `usnvmemu/crates/vfio_user_transport/scripts/qemu_interop/`）

---

## Self-Review

**1. Spec 覆盖：** Spec §6 行 87 `W0：抽 vfio_user_wire（主仓 sans-IO crate；framing 留 server；server 96 测试不破）` → Task 2/3/4/5 实现搬运，Task 6 oracle 验证 "server 96 测试不破"（实测 wire 31 + transport 69 = 100 > 96）。spec §7 提的 "W0 后 真 QEMU e2e 不退" 在 Open risks 标了"本 plan 不强制，单跑 harness"。覆盖完整。

**2. Placeholder 扫描：** 已无 "TBD" / "implement later" / "Similar to Task N" / 模糊行号（Task 5 已改成 find-replace 文本块策略）。回退指引在 plan 顶部 + Task 5 Step 4 + Task 6 Step 5 都有。所有 cargo 命令都从 crate 自己的 dir 跑（workspace exclude 约束）。

**3. 类型一致性：**
- `negotiate_minor(u16) -> u16` / `parse_caps_blob(&[u8]) -> Result<String, ProtoError>` / `build_version_reply_payload(u16, &[u8]) -> Vec<u8>` —— Task 5 Step 1 定义 + Step 3 调用，签名一致。
- `Negotiated` 三字段 `client_major / client_minor / client_caps_json` —— wire crate 定义、transport-crate `server_handshake` 沿用、re-export 透明。
- `SERVER_CAPS_JSON: &str` / `SERVER_MAX_DATA_XFER_SIZE: usize` —— 与原 crate 同类型，re-export 透明。
- access 3 个 `pub(crate)` 项搬到 wire crate 提为 `pub`，transport `pub use *` 吸收，调用点 `crate::access::*` 一行不改。

**4. Review-fix 覆盖（对应 v1 plan 的 architect review）：**
- B1 (workspace 注册到 exclude 而非 members) —— Task 1 Step 3 + Task 6 Step 1 已修
- H1 (handshake `Negotiated` 删除指令模糊) —— Task 5 Step 3 (b) 已改文本块 find-replace
- H2 (access 3 个 pub(crate)) —— Task 3 显式策略 + sed 命令
- H3 (Cargo.toml workspace.dependencies 不可用) —— Task 1 Step 1 改 version 形式 + 注释说明
- H4 (baseline 未实测) —— Task 0 新增 + 推导表
- H5 (POC build feature flag) —— 验过 nvme_firmware default-features 含 vfio-user，裸 build 已 OK；Task 6 Step 4 + 5 + 6 显式用 `POC_SKIP_BUILD=1` 复用
- H6 (commit grep 漂移) —— Task 6 Step 8 加 `--author` + `--since` 限定
- M1 (Task 1 Step 4 stub 主路径) —— Step 2 已直接给 touch 命令为主路径
- M2 (refactor RED-first) —— plan 顶部已声明 "refactor-with-existing-tests"
- M3 (nvme_of_tcp_target cd 进 crate) —— Task 6 Step 7 已 cd
- M4 (Task 5 行号漂移) —— Step 3 全改 find-replace
- M5 (回退指引) —— plan 顶部 + Task 5 Step 4 + Task 6 Step 5

---

**Plan 完。** 7 个任务（含 Task 0 baseline）+ 35 个步骤，单步 < 5 分钟，所有命令真可跑，自洽到 fresh subagent 不需要回看上下文。
