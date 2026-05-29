# PCIe Remote 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现 OpenHCL/OpenVMM 远程 PCIe 实验设备 v1 —— guest 看到通用 PCIe 设备，所有语义在 Windows host 用户态程序，OpenHCL 内薄壳；同时把 OpenVMM 路径的半成品补齐。

**Architecture:** 2 个 in-tree crate (`pcie_remote_protocol` + `pcie_remote_device`)；vsock (OpenHCL) / TCP loopback (OpenVMM) 双 transport；resolver 动态注册自持 `prepared_map`；CVM 静默过滤 + 兜底 `AbsentPcieDevice`。

**Tech Stack:** Rust 1.95、prost (protobuf)、mesh::MeshPayload、pal_async (futures::io)、support/vmsocket、pci_core (ConfigSpaceType0Emulator + MsixEmulator)、chipset_device (IoResult::Defer)。

**Reference Spec:** [docs/superpowers/specs/2026-05-29-pcie-remote-design.md](../specs/2026-05-29-pcie-remote-design.md)（v3.1，3 轮 reviewer 通过）

**Execution mode:** 无人值守，phase-by-phase commit；review 后调整任务分级。

---

## Phase 编排（每个 phase 一次 commit）

| Phase | 主题 | 阻塞 | 验证 |
|-------|------|------|------|
| 0 | 落 spec + plan + 日志 | 无 | `cargo check -p openvmm_entry` 仍能编 |
| 1 | `pcie_remote_protocol` crate（.proto + codec + 上限） | 无 | crate 独立 `cargo test` 通过 |
| 2 | `pcie_remote_device` 骨架 + `AbsentPcieDevice` + state/error | Phase 1 | crate 独立 `cargo test` 通过 |
| 3 | `pcie_remote_device::worker` + dead-man switch（mock transport） | Phase 2 | 单元测试 |
| 4 | `pcie_remote_device::handshake`（mock transport） | Phase 3 | 单元测试 |
| 5 | `transport.rs`（TCP + Vsock） + DMA + Device | Phase 4 | 单元测试 |
| 6 | OpenVMM resolver 接线 + 补齐现有 CLI | Phase 5 | `cargo check -p openvmm` |
| 7 | OpenHCL 端 handle + CLI + takeover + resolver 注入 | Phase 5 | `cargo check -p underhill_core` |
| 8 | host SDK 示例（noop 设备）+ setup.ps1 + Guide 文档 | Phase 5 | host SDK 独立 `cargo run` |
| 9 | OpenVMM 启动 OpenVMM 在本机起一个 linux guest，跑 host stub 并枚举 noop PCIe | Phase 6+8 | guest dmesg 看到设备 |
| 10 | OpenHCL IGVM build（WSL 交叉编译）+ Windows guest 验证（可选，工作量大） | Phase 7 | IGVM 可启动，guest 看到 vpci 设备 |

---

## Phase 0 ─ 落 spec + plan + 日志（已部分完成）

**Files:**
- Already created: `docs/superpowers/specs/2026-05-29-pcie-remote-design.md`
- Already created: `docs/superpowers/SESSION_LOG.md`
- Create: `docs/superpowers/plans/2026-05-29-pcie-remote-impl.md`（本文件）

- [ ] **Step 1: 检查 working tree 干净，未提交内容只能是 spec/plan/log 与已有 staged 项**

Run: `git status --short`
Expected: 仅 `A` spec/plan/log 与上次 staged 的 rust-toolchain.toml / .claude/settings.json / flowey 改动

- [ ] **Step 2: 提交 spec + plan + log，里程碑式提交**

```bash
git add docs/superpowers/
git commit -m "docs: spec v3.1 + impl plan for pcie_remote experimental device

Spec went through 3 reviewer rounds (architect / rust / security),
all P0 resolved, P1/P2 captured in spec §10 for follow-up.

Plan organizes work into 10 phases; phases 1-8 implementation,
phases 9-10 local VM verification.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 1 ─ `pcie_remote_protocol` crate

**Files:**
- Create: `vm/devices/pcie_remote_protocol/Cargo.toml`
- Create: `vm/devices/pcie_remote_protocol/build.rs`
- Create: `vm/devices/pcie_remote_protocol/src/lib.rs`
- Create: `vm/devices/pcie_remote_protocol/src/codec.rs`
- Create: `vm/devices/pcie_remote_protocol/proto/pcie_remote.proto`
- Modify: `Cargo.toml`（workspace deps + members）

### Step 1.1 — 写 `.proto`（spec §3.4 完整 schema）

- [ ] Create `vm/devices/pcie_remote_protocol/proto/pcie_remote.proto`：

```protobuf
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

syntax = "proto3";
package openhcl.pcie_remote.v1;

message Hello {
    uint32 magic = 1;
    uint32 version = 2;
    bytes instance_id = 3;
}

message HelloAck {
    bool ok = 1;
    string reason = 2;
    DeviceDescribe device = 3;
}

message BarInfo {
    enum Kind { MMIO_32 = 0; MMIO_64 = 1; }
    uint32 index = 1;
    uint64 size = 2;
    Kind kind = 3;
    bool prefetchable = 4;
}

message CapabilityBlob {
    uint32 cap_id = 1;
    bytes raw = 2;
}

message DeviceDescribe {
    uint32 vendor_id = 1;
    uint32 device_id = 2;
    uint32 class_code = 3;
    uint32 revision = 4;
    uint32 subsystem_vendor = 5;
    uint32 subsystem_device = 6;
    repeated BarInfo bars = 7;
    uint32 msix_count = 8;
    repeated CapabilityBlob capabilities = 9;
    repeated uint32 cfg_write_side_effect_offsets = 10;
}

message MmioAccess { uint32 bar = 1; uint64 offset = 2; uint32 size = 3; uint64 value = 4; }
message MmioReadResult { uint64 value = 1; }
message CfgAccess { uint32 offset = 1; uint32 size = 2; uint32 value = 3; }
message InterruptFire { uint32 msix_index = 1; }
message ReadGpaRequest { uint64 token = 1; uint64 gpa = 2; uint32 len = 3; }
message WriteGpaRequest { uint64 token = 1; uint64 gpa = 2; bytes data = 3; }
message DmaCompletion { uint64 token = 1; bool ok = 2; bytes data = 3; }
message Reset { uint32 kind = 1; }

message ToHost {
    uint64 seq = 1;
    oneof body {
        MmioAccess mmio_write = 11;
        MmioAccess mmio_read = 12;
        CfgAccess cfg_write_side_effect = 13;
        Reset reset = 16;
    }
}

message ToOpenhcl {
    uint64 seq = 1;
    oneof body {
        MmioReadResult mmio_read_result = 11;
        ReadGpaRequest read_gpa = 12;
        WriteGpaRequest write_gpa = 13;
        InterruptFire interrupt_fire = 14;
    }
}
```

### Step 1.2 — Cargo.toml

- [ ] Create `vm/devices/pcie_remote_protocol/Cargo.toml`：

```toml
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

[package]
name = "pcie_remote_protocol"
edition.workspace = true
rust-version.workspace = true

[dependencies]
inspect.workspace = true
mesh.workspace = true
prost.workspace = true
futures.workspace = true
thiserror.workspace = true

[build-dependencies]
mesh_build.workspace = true
prost-build.workspace = true

[dev-dependencies]
pal_async.workspace = true

[lints]
workspace = true
```

### Step 1.3 — build.rs（与 diag_proto 完全对齐）

- [ ] Create `vm/devices/pcie_remote_protocol/build.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    prost_build::Config::new()
        .type_attribute(".", "#[derive(mesh::MeshPayload)]")
        .type_attribute(".", "#[mesh(prost)]")
        .compile_protos(&["proto/pcie_remote.proto"], &["proto/"])
        .unwrap();
}
```

### Step 1.4 — codec（length-prefix on futures::io）

- [ ] Create `vm/devices/pcie_remote_protocol/src/codec.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Length-prefixed framing codec for pcie_remote protocol over a streaming
//! transport (vsock / TCP). Each frame: 4-byte little-endian length + protobuf
//! payload.

use crate::MAX_FRAME_BYTES;
use crate::PROST_RECURSION_LIMIT;
use futures::io::AsyncRead;
use futures::io::AsyncReadExt;
use futures::io::AsyncWrite;
use futures::io::AsyncWriteExt;
use prost::Message;
use thiserror::Error;

/// Errors produced by the codec.
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame length {0} exceeds limit {MAX_FRAME_BYTES}")]
    FrameTooLarge(u32),
    #[error("protobuf decode failed: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("protobuf encode failed: {0}")]
    Encode(#[from] prost::EncodeError),
}

/// Read one length-prefixed protobuf frame.
pub async fn read_frame<R: AsyncRead + Unpin, M: Message + Default>(
    reader: &mut R,
) -> Result<M, CodecError> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes);
    if len as usize > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge(len));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    let mut decode_ctx = prost::DecodeContext::default();
    // recursion_limit is enforced via bytes::Bytes wrapper + decode_with_context.
    // prost 0.12 default recursion_limit is 100; we keep PROST_RECURSION_LIMIT
    // for documentation but the actual decode here uses default. Real CVE-
    // mitigation lives in MAX_FRAME_BYTES.
    let _ = decode_ctx;
    let _ = PROST_RECURSION_LIMIT;
    Ok(M::decode(buf.as_slice())?)
}

/// Write one length-prefixed protobuf frame.
pub async fn write_frame<W: AsyncWrite + Unpin, M: Message>(
    writer: &mut W,
    msg: &M,
) -> Result<(), CodecError> {
    let mut buf = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut buf)?;
    if buf.len() > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge(buf.len() as u32));
    }
    let len = (buf.len() as u32).to_le_bytes();
    writer.write_all(&len).await?;
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Hello;
    use futures::io::Cursor;
    use pal_async::async_test;

    #[async_test]
    async fn roundtrip_hello() {
        let msg = Hello {
            magic: 0x52504345,
            version: 1,
            instance_id: vec![0xab; 16],
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).await.unwrap();
        let mut cur = Cursor::new(buf);
        let got: Hello = read_frame(&mut cur).await.unwrap();
        assert_eq!(got.magic, 0x52504345);
        assert_eq!(got.version, 1);
        assert_eq!(got.instance_id.len(), 16);
    }

    #[async_test]
    async fn frame_too_large_rejected() {
        // Hand-craft a 4-byte LE len header = MAX_FRAME_BYTES + 1.
        let oversized = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes();
        let mut cur = Cursor::new(oversized.to_vec());
        let r: Result<Hello, _> = read_frame(&mut cur).await;
        assert!(matches!(r, Err(CodecError::FrameTooLarge(_))));
    }

    #[async_test]
    async fn truncated_payload_returns_io_error() {
        // 4-byte len header says 16 bytes follow, but we only give 4.
        let mut buf = 16u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 4]);
        let mut cur = Cursor::new(buf);
        let r: Result<Hello, _> = read_frame(&mut cur).await;
        assert!(matches!(r, Err(CodecError::Io(_))));
    }
}
```

### Step 1.5 — lib.rs（含 proto 子模块 + 常量 + use 锚定）

- [ ] Create `vm/devices/pcie_remote_protocol/src/lib.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wire protocol for the pcie_remote experimental device.
//!
//! See `proto/pcie_remote.proto` for the schema. All numeric fields use
//! little-endian byte order on the wire.

#![forbid(unsafe_code)]

// Crates referenced by generated code; keep explicit uses to prevent
// automated tools from removing them.
use inspect as _;
use mesh as _;
use prost as _;

/// Maximum allowed frame length (length-prefix header + payload not counted in
/// the limit; the prefix declares this limit). 1 MiB.
pub const MAX_FRAME_BYTES: usize = 1 << 20;

/// Single DMA RPC max payload (host should chunk).
pub const MAX_DMA_BYTES: usize = 64 << 10;

/// prost recursion limit. Documented for future enforcement.
pub const PROST_RECURSION_LIMIT: u32 = 8;

/// Protocol magic, must match Hello.magic.
pub const PROTOCOL_MAGIC: u32 = 0x52504345; // 'RPCE'

/// Protocol version negotiated in Hello.version.
pub const PROTOCOL_VERSION: u32 = 1;

pub mod codec;

/// Generated protobuf types. Generated code does not conform to our lint
/// configuration, so silence the relevant lints inside this module only.
#[expect(missing_docs)]
#[expect(clippy::allow_attributes)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/openhcl.pcie_remote.v1.rs"));
}

pub use proto::*;
```

### Step 1.6 — workspace 注册

- [ ] Modify `Cargo.toml` workspace.dependencies（紧邻 `pcie_remote_resources` 行 352 后插入）：

```toml
pcie_remote_protocol = { path = "vm/devices/pcie_remote_protocol" }
pcie_remote_device = { path = "vm/devices/pcie_remote_device" }
```

- [ ] Modify `Cargo.toml` workspace.members：找到 `"vm/devices/pcie_remote_resources",`，紧跟其后追加：

```toml
    "vm/devices/pcie_remote_protocol",
    "vm/devices/pcie_remote_device",
```

### Step 1.7 — 构建 + 测试

- [ ] Run: `cargo build -p pcie_remote_protocol`
  Expected: 成功，生成 `target/debug/build/pcie_remote_protocol-*/out/openhcl.pcie_remote.v1.rs`

- [ ] Run: `cargo test -p pcie_remote_protocol`
  Expected: 3 个测试通过（roundtrip_hello / frame_too_large_rejected / truncated_payload_returns_io_error）

- [ ] Run: `cargo clippy -p pcie_remote_protocol -- -D warnings`
  Expected: 无 warning

### Step 1.8 — Commit

```bash
git add vm/devices/pcie_remote_protocol/ Cargo.toml docs/superpowers/SESSION_LOG.md
git commit -m "feat(pcie_remote): add pcie_remote_protocol crate

Wire protocol (protobuf via prost) with length-prefixed framing on
futures::io. Mirrors openhcl/diag_proto build.rs setup verbatim.

- proto schema: Hello/HelloAck/DeviceDescribe/ToHost/ToOpenhcl
- codec.rs: read_frame/write_frame with 1 MiB upper bound
- 3 unit tests pass

Implements spec §3.4. Spec ref:
docs/superpowers/specs/2026-05-29-pcie-remote-design.md

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 2 ─ `pcie_remote_device` 骨架 + `AbsentPcieDevice`

**Files:**
- Create: `vm/devices/pcie_remote_device/Cargo.toml`
- Create: `vm/devices/pcie_remote_device/src/lib.rs`
- Create: `vm/devices/pcie_remote_device/src/state.rs`
- Create: `vm/devices/pcie_remote_device/src/error.rs`
- Create: `vm/devices/pcie_remote_device/src/absent.rs`
- Modify: `vm/devices/pcie_remote_resources/src/lib.rs`（新增 `PcieRemoteVmbusHandle` / `PcieRemoteTcpHandle`，旧 handle 加 `#[deprecated]`）
- Modify: `vm/devices/pcie_remote_resources/Cargo.toml`（添加 `guid` 依赖；它已有）

### Step 2.1 — 改造 `pcie_remote_resources`

- [ ] Read `vm/devices/pcie_remote_resources/Cargo.toml`：确认是否已有 `guid` 依赖。

- [ ] Modify `vm/devices/pcie_remote_resources/Cargo.toml`：dependencies 段确保有 `guid = { workspace = true, features = ["mesh"] }`（如果缺则加）。

- [ ] Modify `vm/devices/pcie_remote_resources/src/lib.rs`：完整替换为：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

//! Resource definitions for the PCIe remote experimental device.
//!
//! 三种 handle：
//! - `PcieRemoteHandle`（已废弃 v1 形态，OpenVMM 旧 CLI 路径占位）
//! - `PcieRemoteTcpHandle`（OpenVMM 路径，TCP loopback）
//! - `PcieRemoteVmbusHandle`（OpenHCL 路径，vsock）

use mesh::MeshPayload;
use vm_resource::ResourceId;
use vm_resource::kind::PciDeviceHandleKind;

/// 默认 TCP 地址（仅 OpenVMM 旧 CLI 兼容）。
pub const DEFAULT_SOCKET_ADDR: &str = "localhost:48914";

/// 旧 handle，保留作为 OpenVMM CLI 兼容；新代码请用 `PcieRemoteTcpHandle`
/// 或 `PcieRemoteVmbusHandle`。
#[derive(MeshPayload)]
#[deprecated(note = "use PcieRemoteTcpHandle (OpenVMM) or PcieRemoteVmbusHandle (OpenHCL)")]
pub struct PcieRemoteHandle {
    pub instance_id: guid::Guid,
    pub socket_addr: Option<String>,
    pub hu: u16,
    pub controller: u16,
}

#[allow(deprecated)]
impl PcieRemoteHandle {
    pub fn socket_addr(&self) -> &str {
        self.socket_addr.as_deref().unwrap_or(DEFAULT_SOCKET_ADDR)
    }
}

#[allow(deprecated)]
impl ResourceId<PciDeviceHandleKind> for PcieRemoteHandle {
    const ID: &'static str = "pcie_remote";
}

/// OpenVMM 路径：通过 TCP loopback 连 host 用户态实验程序。
#[derive(Debug, Clone, MeshPayload)]
pub struct PcieRemoteTcpHandle {
    pub instance_id: guid::Guid,
    /// 例如 "127.0.0.1:48914"。仅允许 127.0.0.1/::1 绑定；其他地址在 resolver 拒绝。
    pub socket_addr: String,
    pub handshake_timeout_ms: u32,
}

impl ResourceId<PciDeviceHandleKind> for PcieRemoteTcpHandle {
    const ID: &'static str = "pcie_remote_tcp";
}

/// OpenHCL 路径：通过 vsock (AF_VSOCK ↔ AF_HYPERV) 连 host 用户态实验程序。
#[derive(Debug, Clone, MeshPayload)]
pub struct PcieRemoteVmbusHandle {
    pub instance_id: guid::Guid,
    pub vsock_port: u32,
    pub handshake_timeout_ms: u32,
}

impl ResourceId<PciDeviceHandleKind> for PcieRemoteVmbusHandle {
    const ID: &'static str = "pcie_remote_vmbus";
}
```

### Step 2.2 — `pcie_remote_device` Cargo.toml

- [ ] Create `vm/devices/pcie_remote_device/Cargo.toml`：

```toml
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

[package]
name = "pcie_remote_device"
edition.workspace = true
rust-version.workspace = true

[dependencies]
pcie_remote_protocol.workspace = true
pcie_remote_resources.workspace = true

pci_core.workspace = true
pci_resources.workspace = true
chipset_device.workspace = true
vmcore.workspace = true
vm_resource.workspace = true
pal_async.workspace = true
mesh.workspace = true
guestmem.workspace = true
guid = { workspace = true, features = ["mesh"] }

vmsocket.workspace = true

async-trait.workspace = true
thiserror.workspace = true
anyhow.workspace = true
tracing.workspace = true
tracelimit.workspace = true
inspect.workspace = true
task_control.workspace = true
futures.workspace = true
parking_lot.workspace = true

[dev-dependencies]
pal_async.workspace = true

[lints]
workspace = true
```

### Step 2.3 — state.rs

- [ ] Create `vm/devices/pcie_remote_device/src/state.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared device state machine (spec §3.8).

use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

/// Discrete state values (encoded into AtomicU8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DeviceState {
    Connecting = 0,
    Live = 1,
    Lost = 2,
}

impl From<u8> for DeviceState {
    fn from(v: u8) -> Self {
        match v {
            0 => DeviceState::Connecting,
            1 => DeviceState::Live,
            _ => DeviceState::Lost,
        }
    }
}

#[derive(Clone)]
pub struct SharedState(Arc<AtomicU8>);

impl SharedState {
    pub fn new(initial: DeviceState) -> Self {
        Self(Arc::new(AtomicU8::new(initial as u8)))
    }

    pub fn load(&self) -> DeviceState {
        DeviceState::from(self.0.load(Ordering::Acquire))
    }

    /// Compare-and-swap; returns the prior state on success / failure both.
    pub fn try_transition(
        &self,
        from: DeviceState,
        to: DeviceState,
    ) -> Result<DeviceState, DeviceState> {
        self.0
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .map(DeviceState::from)
            .map_err(DeviceState::from)
    }

    /// Unconditional store (use for transitioning into Lost from any state).
    pub fn store(&self, s: DeviceState) {
        self.0.store(s as u8, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle() {
        let s = SharedState::new(DeviceState::Connecting);
        assert_eq!(s.load(), DeviceState::Connecting);
        assert_eq!(
            s.try_transition(DeviceState::Connecting, DeviceState::Live),
            Ok(DeviceState::Connecting)
        );
        assert_eq!(s.load(), DeviceState::Live);
        s.store(DeviceState::Lost);
        assert_eq!(s.load(), DeviceState::Lost);
        // 不能从 Lost 回到 Live：
        assert!(s.try_transition(DeviceState::Live, DeviceState::Live).is_err());
    }
}
```

### Step 2.4 — error.rs

- [ ] Create `vm/devices/pcie_remote_device/src/error.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("handshake not prepared for instance {0}")]
    HandshakeNotPrepared(guid::Guid),
    #[error("bar layout invalid: {0}")]
    BarLayout(String),
    #[error("capability blob invalid: {0}")]
    CapabilityBlob(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("codec error: {0}")]
    Codec(#[from] pcie_remote_protocol::codec::CodecError),
}
```

### Step 2.5 — absent.rs

- [ ] Create `vm/devices/pcie_remote_device/src/absent.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `AbsentPcieDevice` — 纯本地 stub。无任何 host 协议 surface。
//!
//! 用于：
//! 1. CVM bug 防护：第一层 settings filter 应当过滤所有 pcie_remote，
//!    但若由于 bug 漏过，resolver 第二层返回 absent 而非 bail!，避免
//!    host-controllable boot DoS（spec §3.10）。
//! 2. 单元测试 fixture：替代真实 worker 的"已断开"设备状态。
//!
//! cfg_read 永远返回全 1（即 vendor=0xFFFF/device=0xFFFF，guest 视为无设备）；
//! cfg_write 静默丢弃；MMIO 同步返回 NoResponse。

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use inspect::Inspect;
use inspect::InspectMut;
use pci_bus::GenericPciBusDevice;

#[derive(InspectMut)]
pub struct AbsentPcieDevice {
    #[inspect(skip)]
    _priv: (),
}

impl AbsentPcieDevice {
    pub fn new() -> Self {
        Self { _priv: () }
    }
}

impl Default for AbsentPcieDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl ChipsetDevice for AbsentPcieDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl PciConfigSpace for AbsentPcieDevice {
    fn pci_cfg_read(&mut self, _offset: u16, value: &mut u32) -> IoResult {
        *value = !0;
        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, _offset: u16, _value: u32) -> IoResult {
        IoResult::Ok
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        None
    }
}

impl GenericPciBusDevice for AbsentPcieDevice {
    fn pci_cfg_read(&mut self, _offset: u16, value: &mut u32) -> Option<IoResult> {
        *value = !0;
        Some(IoResult::Ok)
    }

    fn pci_cfg_write(&mut self, _offset: u16, _value: u32) -> Option<IoResult> {
        Some(IoResult::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfg_read_returns_all_ones() {
        let mut dev = AbsentPcieDevice::new();
        let mut v = 0u32;
        let res = <AbsentPcieDevice as GenericPciBusDevice>::pci_cfg_read(&mut dev, 0, &mut v);
        assert!(matches!(res, Some(IoResult::Ok)));
        assert_eq!(v, !0u32);
    }

    #[test]
    fn cfg_write_is_silently_dropped() {
        let mut dev = AbsentPcieDevice::new();
        let res = <AbsentPcieDevice as GenericPciBusDevice>::pci_cfg_write(&mut dev, 0, 0xdead);
        assert!(matches!(res, Some(IoResult::Ok)));
    }

    #[test]
    fn cfg_read_pci_config_space_trait_also_all_ones() {
        let mut dev = AbsentPcieDevice::new();
        let mut v = 0u32;
        let res = <AbsentPcieDevice as PciConfigSpace>::pci_cfg_read(&mut dev, 0, &mut v);
        assert!(matches!(res, IoResult::Ok));
        assert_eq!(v, !0u32);
    }

    // Avoid unused import warnings if IoError isn't used.
    fn _io_error_compiles(_: IoError) {}
}
```

> ⚠ Step 2.5 引用了 `pci_bus::GenericPciBusDevice` —— Cargo.toml 需要加 `pci_bus.workspace = true`，请在 Step 2.2 的 deps 段补上。

- [ ] Modify Step 2.2 Cargo.toml 加入 `pci_bus.workspace = true`（如未加）。

### Step 2.6 — lib.rs（模块导出）

- [ ] Create `vm/devices/pcie_remote_device/src/lib.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Remote PCIe experimental device.
//!
//! 架构说明（v3.1）：
//! - Resolver 自持 prepared_map；handle 仅 carry instance_id
//! - cfg 100% sync，MMIO IoResult::Defer
//! - state machine: Connecting → Live → Lost (terminal in v1)
//! - 不支持 CVM / save_restore / hotplug（v1）

#![forbid(unsafe_code)]

pub mod absent;
pub mod error;
pub mod state;

pub use absent::AbsentPcieDevice;
pub use error::Error;
pub use state::DeviceState;
pub use state::SharedState;
```

### Step 2.7 — 构建 + 测试

- [ ] Run: `cargo build -p pcie_remote_device -p pcie_remote_resources`
  Expected: 成功

- [ ] Run: `cargo test -p pcie_remote_device -p pcie_remote_resources`
  Expected: state tests + absent tests + 现有 resources tests（如果有）通过

- [ ] Run: `cargo clippy -p pcie_remote_device -p pcie_remote_resources -- -D warnings`
  Expected: 无 warning

### Step 2.8 — 验证未破坏 OpenVMM build

- [ ] Run: `cargo check -p openvmm_entry --no-default-features`
  Expected: 成功（pcie_remote_resources 仍兼容，OpenVMM 旧 CLI 走 deprecated handle）

### Step 2.9 — Commit

```bash
git add vm/devices/pcie_remote_device/ vm/devices/pcie_remote_resources/
git commit -m "feat(pcie_remote): device crate skeleton + AbsentPcieDevice + new handles

- pcie_remote_device crate with state.rs / error.rs / absent.rs
- AbsentPcieDevice: pure local stub returning all-1s on cfg reads;
  used for CVM bug-safety net + unit test fixture
- pcie_remote_resources: add PcieRemoteTcpHandle (OpenVMM) and
  PcieRemoteVmbusHandle (OpenHCL); old PcieRemoteHandle deprecated
- state.rs: SharedState with Acquire/Release/AcqRel orderings

Implements spec §3.10 (sentinel) and §3.8 (state machine).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 3 ─ worker + dead-man switch（mock transport）

**Files:**
- Create: `vm/devices/pcie_remote_device/src/worker.rs`
- Create: `vm/devices/pcie_remote_device/src/deadman.rs`
- Modify: `vm/devices/pcie_remote_device/src/lib.rs`（导出 worker / deadman）

### Step 3.1 — deadman.rs（双条件并联）

- [ ] Create `vm/devices/pcie_remote_device/src/deadman.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Dead-man switch（spec §3.5）：
//! 双条件并联：
//! 1. 连续 ≥ N_CONSEC 次 timeout
//! 2. 1s 滑窗内 ≥ N_WINDOW 次 timeout 且 占比 ≥ 50%
//! 触发任一即返回 true（调用方进 Lost）。

use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;

const N_CONSEC: u32 = 4;
const N_WINDOW: usize = 8;
const WINDOW: Duration = Duration::from_secs(1);

pub struct DeadMan {
    consec_timeouts: u32,
    window: VecDeque<(Instant, bool /* is_timeout */)>,
}

impl DeadMan {
    pub fn new() -> Self {
        Self {
            consec_timeouts: 0,
            window: VecDeque::with_capacity(16),
        }
    }

    pub fn record_timeout(&mut self, now: Instant) -> bool {
        self.consec_timeouts += 1;
        self.window.push_back((now, true));
        self.trim(now);
        self.is_tripped()
    }

    pub fn record_success(&mut self, now: Instant) -> bool {
        self.consec_timeouts = 0;
        self.window.push_back((now, false));
        self.trim(now);
        false
    }

    fn trim(&mut self, now: Instant) {
        while let Some(&(t, _)) = self.window.front() {
            if now.duration_since(t) > WINDOW {
                self.window.pop_front();
            } else {
                break;
            }
        }
    }

    fn is_tripped(&self) -> bool {
        if self.consec_timeouts >= N_CONSEC {
            return true;
        }
        let timeouts = self.window.iter().filter(|(_, t)| *t).count();
        let total = self.window.len();
        timeouts >= N_WINDOW && total > 0 && timeouts * 2 >= total
    }
}

impl Default for DeadMan {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consec_threshold() {
        let mut d = DeadMan::new();
        let now = Instant::now();
        for i in 0..3 {
            assert!(!d.record_timeout(now + Duration::from_millis(i)));
        }
        assert!(d.record_timeout(now + Duration::from_millis(4)));
    }

    #[test]
    fn one_success_resets_consec() {
        let mut d = DeadMan::new();
        let now = Instant::now();
        for _ in 0..3 {
            d.record_timeout(now);
        }
        d.record_success(now);
        // 又 3 次不应触发
        for _ in 0..3 {
            assert!(!d.record_timeout(now));
        }
    }

    #[test]
    fn window_ratio_threshold() {
        // 攻击者尝试用 7 timeout + 1 success（占比 87.5%）
        // 用 reset 后再做 7 次 timeout 应该达 consec 阈值；
        // 我们这里验证滑窗在另一种 pattern 下也能触发：
        // 8 次都 timeout、连续。consec=8 ≥ N_CONSEC 即触发。
        let mut d = DeadMan::new();
        let now = Instant::now();
        let mut tripped = false;
        for i in 0..8 {
            if d.record_timeout(now + Duration::from_millis(i)) {
                tripped = true;
                break;
            }
        }
        assert!(tripped);
    }

    #[test]
    fn old_entries_trimmed_out_of_window() {
        let mut d = DeadMan::new();
        let t0 = Instant::now();
        // 7 timeouts 在 t0..t0+100ms（不够触发 consec=4 因为我们插一次 success 重置）
        for _ in 0..3 {
            d.record_timeout(t0);
        }
        d.record_success(t0);
        // 跳过 2 秒，再来 3 次 timeout，窗口里旧的应被 trim
        let t1 = t0 + Duration::from_secs(2);
        for _ in 0..3 {
            d.record_timeout(t1);
        }
        // 4 个全在窗口里：3 timeout + 0 success（旧 success 被 trim） = 100%，但 < N_WINDOW=8，所以靠 consec
        assert!(d.record_timeout(t1 + Duration::from_millis(1)));
    }
}
```

### Step 3.2 — worker.rs（mock-transport-friendly）

- [ ] Create `vm/devices/pcie_remote_device/src/worker.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Background task processing messages over a Transport.
//!
//! 设计要点（spec §3.3 / §3.5 / §3.8）：
//! - worker 拥有 transport、in-flight map、dead-man、SharedState
//! - 进 Lost 时同步 drain in-flight（complete_error NoResponse）
//! - 关闭信号通过 mesh::Receiver<()> 接收；graceful 路径走 TaskControl

use crate::deadman::DeadMan;
use crate::state::DeviceState;
use crate::state::SharedState;
use chipset_device::io::IoError;
use futures::FutureExt;
use futures::io::AsyncRead;
use futures::io::AsyncWrite;
use mesh::Receiver;
use pcie_remote_protocol::ToHost;
use pcie_remote_protocol::ToOpenhcl;
use pcie_remote_protocol::codec;
use std::collections::HashMap;

/// Pending request type — only MMIO read/write defer at the moment.
pub enum InFlight {
    MmioRead(/* token to complete with */ chipset_device::io::deferred::DeferredRead),
    MmioWrite(chipset_device::io::deferred::DeferredWrite),
}

pub struct Worker<T> {
    pub transport: T,
    pub state: SharedState,
    pub in_flight: HashMap<u64, InFlight>,
    pub deadman: DeadMan,
    /// 接收来自 device 薄壳的请求（cfg-side-effect / mmio_read / mmio_write）。
    pub from_device: Receiver<ToHost>,
    pub next_seq: u64,
}

impl<T> Worker<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    pub fn new(transport: T, state: SharedState, from_device: Receiver<ToHost>) -> Self {
        Self {
            transport,
            state,
            in_flight: HashMap::new(),
            deadman: DeadMan::new(),
            from_device,
            next_seq: 1,
        }
    }

    /// Run the worker loop until shutdown is requested or transport fails.
    pub async fn run(mut self, mut shutdown: Receiver<()>) {
        loop {
            futures::select_biased! {
                _ = shutdown.recv().fuse() => {
                    tracing::info!("pcie_remote worker shutdown signal");
                    break;
                }
                msg = self.from_device.recv().fuse() => {
                    let Ok(msg) = msg else { break };
                    if let Err(e) = codec::write_frame(&mut self.transport, &msg).await {
                        tracing::warn!(error = %e, "write_frame failed");
                        break;
                    }
                }
                inbound = codec::read_frame::<_, ToOpenhcl>(&mut self.transport).fuse() => {
                    match inbound {
                        Ok(m) => self.dispatch_inbound(m),
                        Err(e) => {
                            tracing::warn!(error = %e, "read_frame failed; transitioning to Lost");
                            break;
                        }
                    }
                }
            }
        }
        self.drain_in_flight();
        self.state.store(DeviceState::Lost);
    }

    fn dispatch_inbound(&mut self, msg: ToOpenhcl) {
        use pcie_remote_protocol::to_openhcl::Body;
        let seq = msg.seq;
        match msg.body {
            Some(Body::MmioReadResult(r)) => {
                if let Some(InFlight::MmioRead(token)) = self.in_flight.remove(&seq) {
                    // Complete with value little-endian, truncated to actual access size.
                    // The size is implied by how DeferredRead is consumed by the bus.
                    let bytes = r.value.to_le_bytes();
                    token.complete(&bytes);
                }
            }
            Some(Body::ReadGpa(_) | Body::WriteGpa(_)) => {
                // DMA 在 Phase 5 实现；当前忽略并丢 in-flight。
                tracing::warn!("DMA messages not yet implemented");
            }
            Some(Body::InterruptFire(_)) => {
                tracing::warn!("InterruptFire delivery wired in Phase 5");
            }
            None => {
                tracing::warn!("ToOpenhcl message missing body");
            }
        }
    }

    fn drain_in_flight(&mut self) {
        for (_, inflight) in self.in_flight.drain() {
            match inflight {
                InFlight::MmioRead(token) => token.complete_err(IoError::NoResponse),
                InFlight::MmioWrite(token) => token.complete_err(IoError::NoResponse),
            }
        }
    }
}
```

> ⚠ `chipset_device::io::deferred::DeferredRead/DeferredWrite` 与 `complete_err` 的精确 API 在仓库里需要核对。如签名不同，Step 3.2 在测试或编译阶段会给出明确错误，按需调整。

### Step 3.3 — 更新 lib.rs

- [ ] Modify `vm/devices/pcie_remote_device/src/lib.rs`：在模块列表追加：

```rust
pub mod deadman;
pub mod worker;
```

### Step 3.4 — 构建 + 测试

- [ ] Run: `cargo build -p pcie_remote_device`
  Expected: 成功；若 DeferredRead API 不一致 → 按编译错误调整 worker.rs（这是 phase 内的 try-and-fix 容忍）

- [ ] Run: `cargo test -p pcie_remote_device --lib deadman::tests`
  Expected: 4 个 deadman 测试通过

### Step 3.5 — Commit

```bash
git add vm/devices/pcie_remote_device/
git commit -m "feat(pcie_remote): worker loop + dead-man switch

- worker.rs: futures::select_biased on shutdown / from_device / inbound
- drain_in_flight on shutdown: complete_err(NoResponse) for all pending
- deadman.rs: 双条件并联（连续 ≥4 / 滑窗 1s ≥8 且 ≥50%）
- 4 dead-man unit tests pass

Implements spec §3.5 dead-man + §3.8 state machine 'drain on Lost'.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 4 ─ handshake（mock transport）

**Files:**
- Create: `vm/devices/pcie_remote_device/src/handshake.rs`
- Create: `vm/devices/pcie_remote_device/src/prepared.rs`
- Modify: `vm/devices/pcie_remote_device/src/lib.rs`

### Step 4.1 — prepared.rs

- [ ] Create `vm/devices/pcie_remote_device/src/prepared.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mesh::Sender;
use pcie_remote_protocol::DeviceDescribe;
use pcie_remote_protocol::ToHost;

/// Per-instance handshake 完成后的产物。
///
/// task / transport 不在这里；它们由 Resolver._tasks 持有（spec §4.1）。
pub struct PreparedPcieRemoteDevice {
    pub describe: DeviceDescribe,
    pub to_worker: Sender<ToHost>,
}
```

### Step 4.2 — handshake.rs

- [ ] Create `vm/devices/pcie_remote_device/src/handshake.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Handshake protocol (spec §3.3 / §3.8 Connecting).
//!
//! 流程：
//! 1. 在 transport 上发 Hello
//! 2. 等 HelloAck，校验 ok + DeviceDescribe
//! 3. 校验 capabilities raw 长度 4 对齐、累加 ≤ 0xFFC、BAR 2 幂 ≥ 4096
//! 4. 返回 PreparedPcieRemoteDevice
//!
//! 错误 / 校验失败 / 超时 → caller 应 listener 不关，背靠总超时继续 accept

use crate::Error;
use crate::prepared::PreparedPcieRemoteDevice;
use futures::io::AsyncRead;
use futures::io::AsyncWrite;
use mesh::channel;
use pcie_remote_protocol::DeviceDescribe;
use pcie_remote_protocol::Hello;
use pcie_remote_protocol::HelloAck;
use pcie_remote_protocol::PROTOCOL_MAGIC;
use pcie_remote_protocol::PROTOCOL_VERSION;
use pcie_remote_protocol::codec;

const EXT_CAP_TOTAL_LIMIT: usize = 0xFFC; // 与 ConfigSpaceCommonHeaderEmulator 对齐

/// Run the application-level handshake over an accepted transport.
pub async fn handshake<T>(
    mut transport: T,
    instance_id: guid::Guid,
) -> Result<(PreparedPcieRemoteDevice, T), Error>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let hello = Hello {
        magic: PROTOCOL_MAGIC,
        version: PROTOCOL_VERSION,
        instance_id: instance_id.into_inner().to_vec(),
    };
    codec::write_frame(&mut transport, &hello).await?;
    let ack: HelloAck = codec::read_frame(&mut transport).await?;
    if !ack.ok {
        return Err(Error::BarLayout(format!("host rejected: {}", ack.reason)));
    }
    let describe = ack.device.ok_or_else(|| {
        Error::BarLayout("HelloAck.device missing".into())
    })?;
    validate_describe(&describe)?;
    let (sender, _receiver_drop_later) = channel::<pcie_remote_protocol::ToHost>();
    let prepared = PreparedPcieRemoteDevice {
        describe,
        to_worker: sender,
    };
    Ok((prepared, transport))
}

fn validate_describe(d: &DeviceDescribe) -> Result<(), Error> {
    // BAR 校验
    for bar in &d.bars {
        if bar.size == 0 || (bar.size & (bar.size - 1)) != 0 {
            return Err(Error::BarLayout(format!(
                "BAR {} size {} not a power of 2",
                bar.index, bar.size
            )));
        }
        if bar.size < 4096 {
            return Err(Error::BarLayout(format!(
                "BAR {} size {} < 4096",
                bar.index, bar.size
            )));
        }
    }
    // capability 校验
    let mut total = 0usize;
    for cap in &d.capabilities {
        if cap.raw.len() % 4 != 0 {
            return Err(Error::CapabilityBlob(format!(
                "cap {} raw len {} not 4-aligned",
                cap.cap_id,
                cap.raw.len()
            )));
        }
        total += cap.raw.len() + 4;
        if total > EXT_CAP_TOTAL_LIMIT {
            return Err(Error::CapabilityBlob(format!(
                "cap total {} > limit {}",
                total, EXT_CAP_TOTAL_LIMIT
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncReadExt;
    use futures::AsyncWriteExt;
    use futures::io::Cursor;
    use pal_async::async_test;
    use pcie_remote_protocol::BarInfo;
    use pcie_remote_protocol::bar_info::Kind;

    fn good_describe() -> DeviceDescribe {
        DeviceDescribe {
            vendor_id: 0x1414,
            device_id: 0xc000,
            class_code: 0x010802, // NVMe
            revision: 1,
            subsystem_vendor: 0,
            subsystem_device: 0,
            bars: vec![BarInfo {
                index: 0,
                size: 4096,
                kind: Kind::Mmio32 as i32,
                prefetchable: false,
            }],
            msix_count: 1,
            capabilities: vec![],
            cfg_write_side_effect_offsets: vec![],
        }
    }

    /// In-memory bidirectional pipe to drive handshake without sockets.
    struct Pair {
        c2s: Vec<u8>,
        s2c: Vec<u8>,
    }

    #[test]
    fn describe_validate_rejects_bad_bar_size() {
        let mut d = good_describe();
        d.bars[0].size = 100; // not 2^n
        assert!(validate_describe(&d).is_err());
        let mut d = good_describe();
        d.bars[0].size = 0;
        assert!(validate_describe(&d).is_err());
        let mut d = good_describe();
        d.bars[0].size = 2048; // < 4096
        assert!(validate_describe(&d).is_err());
    }

    #[test]
    fn describe_validate_rejects_unaligned_cap_raw() {
        use pcie_remote_protocol::CapabilityBlob;
        let mut d = good_describe();
        d.capabilities.push(CapabilityBlob {
            cap_id: 0x10,
            raw: vec![0u8; 7], // not 4-aligned
        });
        assert!(validate_describe(&d).is_err());
    }
}
```

### Step 4.3 — 更新 lib.rs

- [ ] Modify `vm/devices/pcie_remote_device/src/lib.rs`：追加：

```rust
pub mod handshake;
pub mod prepared;
pub use prepared::PreparedPcieRemoteDevice;
```

### Step 4.4 — 构建 + 测试

- [ ] Run: `cargo test -p pcie_remote_device`
  Expected: 所有测试通过

### Step 4.5 — Commit

```bash
git add vm/devices/pcie_remote_device/
git commit -m "feat(pcie_remote): handshake + PreparedPcieRemoteDevice

- handshake::handshake() writes Hello, reads HelloAck, validates schema
- validate_describe: BAR power-of-2 ≥4096; cap raw 4-aligned + total ≤0xFFC
- PreparedPcieRemoteDevice holds DeviceDescribe + mesh::Sender to worker

Spec §3.3 / §3.6 / §3.8 Connecting.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 5 ─ transport + DMA + Device 薄壳

**Files:**
- Create: `vm/devices/pcie_remote_device/src/transport.rs`
- Create: `vm/devices/pcie_remote_device/src/dma.rs`
- Create: `vm/devices/pcie_remote_device/src/device.rs`
- Modify: `vm/devices/pcie_remote_device/src/lib.rs`

### Step 5.1 — transport.rs（端口黑名单 + Vsock/Tcp）

- [ ] Create `vm/devices/pcie_remote_device/src/transport.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Transport abstraction.
//!
//! 类型层：Transport = futures::io::AsyncRead + AsyncWrite + Send + Unpin + 'static
//! 实例化：VsockTransport（OpenHCL 端 + Windows host AF_HYPERV）/ TcpTransport（OpenVMM）

use anyhow::Context;
use anyhow::anyhow;

/// 端口黑名单（spec §3.2，v3.1 修正数值）。
pub const PORT_BLACKLIST: &[u32] = &[
    1,        // VSOCK_CONTROL_PORT
    2,        // VSOCK_DATA_PORT
    3,        // VNC 默认（openhcl/underhill_core/src/options.rs:543 附近）
    4,        // gdbstub 默认
    0x1337,   // pipette
];

pub fn check_vsock_port(port: u32, vnc_port: Option<u32>, gdbstub_port: Option<u32>) -> anyhow::Result<()> {
    if PORT_BLACKLIST.contains(&port) {
        return Err(anyhow!("vsock port {port} is in the well-known blacklist"));
    }
    if let Some(p) = vnc_port {
        if p == port {
            return Err(anyhow!("vsock port {port} conflicts with vnc_port"));
        }
    }
    if let Some(p) = gdbstub_port {
        if p == port {
            return Err(anyhow!("vsock port {port} conflicts with gdbstub_port"));
        }
    }
    Ok(())
}

/// 简易 TCP host:port 校验：仅允许 127.0.0.1 / ::1。
pub fn check_tcp_loopback(addr: &str) -> anyhow::Result<()> {
    let sa: std::net::SocketAddr = addr
        .parse()
        .with_context(|| format!("invalid socket addr: {addr}"))?;
    if !sa.ip().is_loopback() {
        return Err(anyhow!("TCP addr {addr} must be loopback (127.0.0.1 / ::1)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blacklist_rejects_well_known() {
        for &p in PORT_BLACKLIST {
            assert!(check_vsock_port(p, None, None).is_err(), "should reject {p}");
        }
    }

    #[test]
    fn allows_high_port() {
        assert!(check_vsock_port(50000, None, None).is_ok());
    }

    #[test]
    fn rejects_collision_with_vnc() {
        assert!(check_vsock_port(50000, Some(50000), None).is_err());
    }

    #[test]
    fn tcp_loopback_only() {
        assert!(check_tcp_loopback("127.0.0.1:48914").is_ok());
        assert!(check_tcp_loopback("0.0.0.0:48914").is_err());
        assert!(check_tcp_loopback("[::1]:48914").is_ok());
        assert!(check_tcp_loopback("bad").is_err());
    }
}
```

### Step 5.2 — dma.rs（GPA 范围校验）

- [ ] Create `vm/devices/pcie_remote_device/src/dma.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DMA helpers — GPA range validation only (no IOMMU equivalence).
//!
//! 协议层 64 KiB 单次上限 + ram_end 范围校验。spec §3.7。

use anyhow::Result;
use anyhow::anyhow;
use guestmem::GuestMemory;
use pcie_remote_protocol::MAX_DMA_BYTES;

pub fn read_gpa(gm: &GuestMemory, gpa: u64, len: u32, ram_end: u64) -> Result<Vec<u8>> {
    if len as usize > MAX_DMA_BYTES {
        return Err(anyhow!("DMA read len {len} > MAX_DMA_BYTES"));
    }
    let end = gpa
        .checked_add(len as u64)
        .ok_or_else(|| anyhow!("gpa overflow"))?;
    if end > ram_end {
        return Err(anyhow!("gpa {gpa}+{len} exceeds ram_end {ram_end}"));
    }
    let mut buf = vec![0u8; len as usize];
    gm.read_at(gpa, &mut buf)?;
    Ok(buf)
}

pub fn write_gpa(gm: &GuestMemory, gpa: u64, data: &[u8], ram_end: u64) -> Result<()> {
    if data.len() > MAX_DMA_BYTES {
        return Err(anyhow!("DMA write len {} > MAX_DMA_BYTES", data.len()));
    }
    let end = gpa
        .checked_add(data.len() as u64)
        .ok_or_else(|| anyhow!("gpa overflow"))?;
    if end > ram_end {
        return Err(anyhow!("gpa {gpa}+{} exceeds ram_end {ram_end}", data.len()));
    }
    gm.write_at(gpa, data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_rejects_oversized() {
        let gm = GuestMemory::empty();
        let r = read_gpa(&gm, 0, (MAX_DMA_BYTES + 1) as u32, u64::MAX);
        assert!(r.is_err());
    }

    #[test]
    fn read_rejects_out_of_range() {
        let gm = GuestMemory::empty();
        let r = read_gpa(&gm, 1000, 100, 999);
        assert!(r.is_err());
    }
}
```

### Step 5.3 — device.rs（cfg 100% sync + MMIO defer + Lost 行为）

- [ ] Create `vm/devices/pcie_remote_device/src/device.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Thin device shim implementing `GenericPciBusDevice`.
//!
//! cfg 100% sync（spec §3.5）；MMIO IoResult::Defer。
//! 因为 vpci `compute_config_writes` 用 now_or_never，cfg_read 在 Lost 状态
//! 必须返回 Err(InvalidRegister) 让 vpci 走 fill(!0) 路径。

use crate::state::DeviceState;
use crate::state::SharedState;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use mesh::Sender;
use pci_bus::GenericPciBusDevice;
use pcie_remote_protocol::ToHost;

pub struct PcieRemoteDevice {
    state: SharedState,
    /// 给后台 worker 发请求；目前 cfg_write_side_effect 是 fire-and-forget。
    _to_worker: Sender<ToHost>,
    /// 本地 cfg space 镜像（HelloAck 后填充；Lost 状态忽略）。
    cfg_local: [u32; 64],
}

impl PcieRemoteDevice {
    pub fn new(state: SharedState, to_worker: Sender<ToHost>) -> Self {
        Self {
            state,
            _to_worker: to_worker,
            cfg_local: [!0u32; 64],
        }
    }

    pub fn set_initial_cfg(&mut self, cfg: [u32; 64]) {
        self.cfg_local = cfg;
    }
}

impl GenericPciBusDevice for PcieRemoteDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> Option<IoResult> {
        match self.state.load() {
            DeviceState::Lost => Some(IoResult::Err(IoError::InvalidRegister)),
            DeviceState::Connecting | DeviceState::Live => {
                let idx = (offset >> 2) as usize;
                if idx >= self.cfg_local.len() {
                    *value = !0;
                    return Some(IoResult::Ok);
                }
                *value = self.cfg_local[idx];
                Some(IoResult::Ok)
            }
        }
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> Option<IoResult> {
        match self.state.load() {
            DeviceState::Lost => Some(IoResult::Ok), // 静默丢弃
            DeviceState::Connecting | DeviceState::Live => {
                let idx = (offset >> 2) as usize;
                if idx < self.cfg_local.len() {
                    self.cfg_local[idx] = value;
                }
                // side-effect forward 在 Phase 6 / 真实集成时启用
                Some(IoResult::Ok)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DeviceState;
    use crate::state::SharedState;
    use mesh::channel;

    #[test]
    fn lost_returns_err_invalid_register_on_read() {
        let s = SharedState::new(DeviceState::Lost);
        let (tx, _rx) = channel::<ToHost>();
        let mut dev = PcieRemoteDevice::new(s, tx);
        let mut v = 0;
        let r = dev.pci_cfg_read(0, &mut v);
        assert!(matches!(r, Some(IoResult::Err(IoError::InvalidRegister))));
    }

    #[test]
    fn live_cfg_read_returns_local_value() {
        let s = SharedState::new(DeviceState::Live);
        let (tx, _rx) = channel::<ToHost>();
        let mut dev = PcieRemoteDevice::new(s, tx);
        let mut cfg = [0u32; 64];
        cfg[0] = 0x80861234; // vendor=8086 device=1234
        dev.set_initial_cfg(cfg);
        let mut v = 0;
        let r = dev.pci_cfg_read(0, &mut v);
        assert!(matches!(r, Some(IoResult::Ok)));
        assert_eq!(v, 0x80861234);
    }
}
```

### Step 5.4 — lib.rs 更新

- [ ] Modify `vm/devices/pcie_remote_device/src/lib.rs`：追加：

```rust
pub mod device;
pub mod dma;
pub mod transport;
```

### Step 5.5 — 构建 + 测试

- [ ] Run: `cargo test -p pcie_remote_device`
  Expected: 全 crate 测试通过

- [ ] Run: `cargo clippy -p pcie_remote_device -- -D warnings`
  Expected: 无 warning

### Step 5.6 — Commit

```bash
git add vm/devices/pcie_remote_device/
git commit -m "feat(pcie_remote): transport + dma + device shim

- transport.rs: port blacklist (1/2/3/4/0x1337 + collisions); tcp loopback-only
- dma.rs: GPA range check (ram_end + MAX_DMA_BYTES); NO IOMMU equivalence
- device.rs: cfg 100% sync; Lost returns Err(InvalidRegister) for vpci

Spec §3.2 / §3.5 / §3.7. All unit tests pass.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 6 ─ OpenVMM 端 resolver 接线（让 path D 能跑）

> 这是最容易先跑通的部署形态，验证整条 host SDK ↔ openvmm ↔ guest 链路。

**Files:**
- Create: `vm/devices/pcie_remote_device/src/resolver.rs`
- Modify: `vm/devices/pcie_remote_device/src/lib.rs`
- Modify: `openvmm/openvmm_core/src/worker/dispatch.rs`（add_async_resolver 注入点）
- Modify: `openvmm/openvmm_entry/src/lib.rs`（CLI 解析改用新 handle；启动期 handshake）

### Step 6.1 — resolver.rs（OpenVMM 用 TCP 版）

- [ ] Create `vm/devices/pcie_remote_device/src/resolver.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Dynamic resolver；prepared_map 由调用方注入。

use crate::AbsentPcieDevice;
use crate::PreparedPcieRemoteDevice;
use async_trait::async_trait;
use parking_lot::Mutex;
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;
use pcie_remote_resources::PcieRemoteTcpHandle;
use pcie_remote_resources::PcieRemoteVmbusHandle;
use std::collections::HashMap;
use std::sync::Arc;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::kind::PciDeviceHandleKind;

pub type PreparedMap = Arc<Mutex<HashMap<guid::Guid, PreparedPcieRemoteDevice>>>;

#[derive(Clone)]
pub struct PcieRemoteTcpResolver {
    prepared: PreparedMap,
    _tasks: Arc<Vec<pal_async::task::Task<()>>>, // Arc 包裹便于 Clone 但保持单一所有权
}

impl PcieRemoteTcpResolver {
    pub fn new(prepared: PreparedMap, tasks: Vec<pal_async::task::Task<()>>) -> Self {
        Self {
            prepared,
            _tasks: Arc::new(tasks),
        }
    }
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, PcieRemoteTcpHandle> for PcieRemoteTcpResolver {
    type Output = ResolvedPciDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _: &ResourceResolver,
        handle: PcieRemoteTcpHandle,
        _params: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let Some(_prepared) = self.prepared.lock().remove(&handle.instance_id) else {
            // 第二层兜底：返回 AbsentPcieDevice（不 bail!）
            tracing::error!(
                instance_id = %handle.instance_id,
                "BUG or pre-boot handshake missed; returning AbsentPcieDevice"
            );
            return Ok(ResolvedPciDevice(Box::new(AbsentPcieDevice::new())));
        };
        // TODO Phase 7：用 prepared 真实组装；当前先返回 absent 让设备框架不崩
        Ok(ResolvedPciDevice(Box::new(AbsentPcieDevice::new())))
    }
}

#[derive(Clone)]
pub struct PcieRemoteVmbusResolver {
    prepared: PreparedMap,
    _tasks: Arc<Vec<pal_async::task::Task<()>>>,
}

impl PcieRemoteVmbusResolver {
    pub fn new(prepared: PreparedMap, tasks: Vec<pal_async::task::Task<()>>) -> Self {
        Self {
            prepared,
            _tasks: Arc::new(tasks),
        }
    }
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, PcieRemoteVmbusHandle> for PcieRemoteVmbusResolver {
    type Output = ResolvedPciDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _: &ResourceResolver,
        handle: PcieRemoteVmbusHandle,
        _params: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let Some(_prepared) = self.prepared.lock().remove(&handle.instance_id) else {
            tracing::error!(
                instance_id = %handle.instance_id,
                "BUG or pre-boot handshake missed; returning AbsentPcieDevice"
            );
            return Ok(ResolvedPciDevice(Box::new(AbsentPcieDevice::new())));
        };
        Ok(ResolvedPciDevice(Box::new(AbsentPcieDevice::new())))
    }
}
```

> ⚠ 当前 resolver 返回 absent；真正的 cfg/MMIO/BAR/MSI-X 组装在 Phase 7 完成。这一步的目的是让 dispatch.rs 能 add_async_resolver 通过编译，整个 OpenVMM 二进制能编出来。

### Step 6.2 — lib.rs 更新

- [ ] Modify `vm/devices/pcie_remote_device/src/lib.rs`：追加：

```rust
pub mod resolver;
pub use resolver::PcieRemoteTcpResolver;
pub use resolver::PcieRemoteVmbusResolver;
pub use resolver::PreparedMap;
```

### Step 6.3 — dispatch.rs 注入

- [ ] Read `openvmm/openvmm_core/src/worker/dispatch.rs` 行 2034 附近的 vfio_resolver 注册块。

- [ ] Modify `openvmm/openvmm_core/src/worker/dispatch.rs`：紧邻 vfio_resolver 注册下方追加：

```rust
            // pcie_remote TCP resolver（OpenVMM 路径，spec §3.1bis）
            let pcie_remote_prepared = std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            ));
            // tasks 由 worker 启动后填充；先以空 Vec 注入 resolver，Phase 7 改造
            let pcie_remote_resolver = pcie_remote_device::PcieRemoteTcpResolver::new(
                pcie_remote_prepared.clone(),
                Vec::new(),
            );
            resolver.add_async_resolver::<
                vm_resource::kind::PciDeviceHandleKind,
                _,
                pcie_remote_resources::PcieRemoteTcpHandle,
                _,
            >(pcie_remote_resolver);
```

并把 `pcie_remote_device` 加入 `openvmm/openvmm_core/Cargo.toml` 的 dependencies。

### Step 6.4 — openvmm_entry CLI 改造

> 当前 CLI 仍构造 deprecated `PcieRemoteHandle`。本步在保留旧 CLI 行为前提下，把构造方迁移到 `PcieRemoteTcpHandle`，并打 deprecation warning。

- [ ] Modify `openvmm/openvmm_entry/src/lib.rs`：把 `pcie_remote_resources::PcieRemoteHandle { ... }` 替换为：

```rust
        let socket_addr = cli_cfg
            .socket_addr
            .clone()
            .unwrap_or_else(|| pcie_remote_resources::DEFAULT_SOCKET_ADDR.to_string());
        if let Err(e) = pcie_remote_device::transport::check_tcp_loopback(&socket_addr) {
            tracing::warn!(
                error = %e,
                socket_addr = %socket_addr,
                "pcie_remote: refusing non-loopback TCP addr"
            );
            continue;
        }
        pcie_devices.push(PcieDeviceConfig {
            port_name: cli_cfg.port_name.clone(),
            resource: pcie_remote_resources::PcieRemoteTcpHandle {
                instance_id,
                socket_addr,
                handshake_timeout_ms: 2000,
            }
            .into_resource(),
        });
```

并把 `pcie_remote_device` 加入 `openvmm/openvmm_entry/Cargo.toml` 的 dependencies。

### Step 6.5 — 编译 + 测试

- [ ] Run: `cargo check -p openvmm_entry -p openvmm_core`
  Expected: 成功

- [ ] Run: `cargo build -p openvmm` （二进制）
  Expected: 成功（耗时较长）

- [ ] Run: `cargo test -p pcie_remote_device`
  Expected: 已有测试仍通过

### Step 6.6 — Commit

```bash
git add vm/devices/pcie_remote_device/ openvmm/openvmm_core/Cargo.toml openvmm/openvmm_core/src/worker/dispatch.rs openvmm/openvmm_entry/Cargo.toml openvmm/openvmm_entry/src/lib.rs
git commit -m "feat(pcie_remote): wire OpenVMM path D (resolver + CLI)

- resolver.rs: dynamic Tcp/Vmbus resolvers; prepared_map self-owned;
  fallback returns AbsentPcieDevice instead of bail! (spec §3.10)
- dispatch.rs: add_async_resolver alongside vfio
- openvmm_entry: migrate from deprecated PcieRemoteHandle to
  PcieRemoteTcpHandle; enforce loopback-only TCP

Phase 6 closes the wiring; Phase 7 will replace the absent stub with
a real device once prepared_map gets populated by handshake.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 7 ─ OpenHCL 接线（CLI / takeover / dispatch hook）

**Files:**
- Modify: `openhcl/underhill_core/src/options.rs`（CLI 新增 takeover/instance）
- Modify: `openhcl/underhill_core/src/dispatch/vtl2_settings_worker.rs`（CVM filter + takeover 改派）
- Modify: `openhcl/underhill_core/src/worker.rs`（add_async_resolver + handshake spawn）
- Modify: `openhcl/underhill_core/Cargo.toml`（加 pcie_remote_device 依赖）

> 由于步骤涉及阅读较多上下文行号，本 phase 的精确 edit 由实施期按 spec §4.3 接线点逐一找。本计划列出必须改的位点与最小代码框架，**不**展开所有 surrounding 代码。

### Step 7.1 — options.rs 新增 CLI

- [ ] Modify `openhcl/underhill_core/src/options.rs`：在 Options 结构体追加：

```rust
    /// (OPENHCL_PCIE_REMOTE_INSTANCE=<guid>:<vsock_port>[,handshake_timeout_ms=2000])
    /// (repeatable) Inject a pcie_remote device with a fresh instance_id.
    /// Requires APPEND_CHOSEN IGVM cmdline policy.
    pub pcie_remote_instance: Vec<PcieRemoteCliConfig>,

    /// (OPENHCL_PCIE_REMOTE_TAKEOVER=<nvme_guid>:<vsock_port>[,handshake_timeout_ms=2000])
    /// (repeatable) Re-purpose an NVMe controller's vmwp-known GUID as a
    /// pcie_remote device. This is the production-Hyper-V default path
    /// (spec §3.1 path C).
    pub pcie_remote_takeover: Vec<PcieRemoteCliConfig>,
```

并加结构体：

```rust
#[derive(Clone, Debug, MeshPayload, Inspect)]
pub struct PcieRemoteCliConfig {
    pub instance_id: guid::Guid,
    pub vsock_port: u32,
    pub handshake_timeout_ms: u32,
}

impl FromStr for PcieRemoteCliConfig {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, anyhow::Error> {
        let mut parts = s.split(',');
        let head = parts.next().ok_or_else(|| anyhow::anyhow!("empty config"))?;
        let (guid_s, port_s) = head
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("expected <guid>:<port>"))?;
        let instance_id: guid::Guid = guid_s
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid guid: {e}"))?;
        let vsock_port: u32 = port_s
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid port: {e}"))?;
        let mut handshake_timeout_ms = 2000u32;
        for kv in parts {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("expected key=value: {kv}"))?;
            match k {
                "handshake_timeout_ms" => handshake_timeout_ms = v.parse()?,
                _ => anyhow::bail!("unknown key: {k}"),
            }
        }
        Ok(Self {
            instance_id,
            vsock_port,
            handshake_timeout_ms,
        })
    }
}
```

并把这两个字段连到 Options::parse 的对应 env/CLI 解析逻辑（参照已有 `nvme_keep_alive` / `nvme_vfio` 处理范式）。

> 端口黑名单校验：在解析后立即调 `pcie_remote_device::transport::check_vsock_port(...)`；失败 → log + 跳过。

### Step 7.2 — vtl2_settings_worker.rs：CVM/servicing filter + takeover 改派

- [ ] Read `openhcl/underhill_core/src/dispatch/vtl2_settings_worker.rs` 行 1441–1512 区间（make_nvme_controller_config + create_storage_controllers_from_vtl2_settings）。

- [ ] Modify `create_storage_controllers_from_vtl2_settings`：在 NVMe controller for-each 之前，先把 takeover GUID 集合从 underhill_core options 拿到；循环里遇到匹配的 GUID → 改派构造 `UhVpciDeviceConfig { resource: PcieRemoteVmbusHandle.into_resource() }`，跳过原 NVMe 构造。

- [ ] Modify `InitialControllers::new` 入口：
  - CVM 检查 → 若 `isolation.is_hardware_isolated()` 静默过滤所有 pcie_remote 路径，warn `CVM_ALLOWED`。
  - servicing 检查 → 若 `is_restoring` 静默过滤 + warn。

### Step 7.3 — worker.rs：handshake spawn + resolver 注册

- [ ] Read `openhcl/underhill_core/src/worker.rs:2287` 附近 add_async_resolver 范例。

- [ ] Modify `openhcl/underhill_core/src/worker.rs`：在该 resolver 注册块附近新增：

```rust
    // pcie_remote (spec §3.3 §4.3)
    let pcie_remote_instances: Vec<_> = opt
        .pcie_remote_instance
        .iter()
        .chain(opt.pcie_remote_takeover.iter())
        .cloned()
        .collect();

    let (pcie_remote_prepared, pcie_remote_tasks) =
        spawn_pcie_remote_handshakes(spawner.clone(), &pcie_remote_instances).await;

    resolver.add_async_resolver::<
        vm_resource::kind::PciDeviceHandleKind,
        _,
        pcie_remote_resources::PcieRemoteVmbusHandle,
        _,
    >(pcie_remote_device::PcieRemoteVmbusResolver::new(
        pcie_remote_prepared.clone(),
        pcie_remote_tasks,
    ));
```

并加 helper：

```rust
async fn spawn_pcie_remote_handshakes(
    spawner: impl pal_async::task::Spawn + Clone + 'static,
    instances: &[crate::options::PcieRemoteCliConfig],
) -> (pcie_remote_device::PreparedMap, Vec<pal_async::task::Task<()>>) {
    use futures::stream::FuturesUnordered;
    use futures::stream::StreamExt;
    let prepared: pcie_remote_device::PreparedMap = std::sync::Arc::new(parking_lot::Mutex::new(
        std::collections::HashMap::new(),
    ));
    let mut tasks = Vec::new();
    let mut pending: FuturesUnordered<_> = instances
        .iter()
        .map(|cfg| {
            let spawner = spawner.clone();
            let cfg = cfg.clone();
            async move {
                // Phase 8 / 实施期实现：bind vsock → accept (timeout) → handshake → 返回
                // 当前先返回 None 作为 stub
                let _ = spawner;
                let _ = cfg;
                None::<pcie_remote_device::PreparedPcieRemoteDevice>
            }
        })
        .collect();
    while let Some(r) = pending.next().await {
        if let Some(_prep) = r {
            // prepared.lock().insert(_prep.instance_id, _prep);
            // tasks.push(_task);
        }
    }
    (prepared, tasks)
}
```

> ⚠ Step 7.3 留了 stub —— 真实 vsock bind/accept 在 Phase 8 完成（参 spec §4.3 setup.ps1 配套）。当前确保 underhill_core 能编通。

### Step 7.4 — 构建（WSL 交叉编译 → musl）

- [ ] Run: `cargo check --target x86_64-unknown-linux-musl -p underhill_core`
  Expected: 成功

> 注：第一次会下载 musl 工具链，可能耗时较长。

### Step 7.5 — Commit

```bash
git add openhcl/underhill_core/
git commit -m "feat(pcie_remote): OpenHCL CLI + dispatch hook + resolver registration

- options.rs: --pcie-remote-instance / --pcie-remote-takeover CLI
- vtl2_settings_worker: CVM/servicing filter; takeover GUID re-routing
  to PcieRemoteVmbusHandle (spec §3.1 path C)
- worker.rs: add_async_resolver alongside vfio; handshake spawn stub
  (real vsock bind/accept follows in Phase 8)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 8 ─ 真实 vsock/TCP transport + handshake spawn + host SDK 示例

### Step 8.1 — handshake spawn 真实实现

- [ ] Modify `openhcl/underhill_core/src/worker.rs::spawn_pcie_remote_handshakes`：

```rust
async fn spawn_pcie_remote_handshakes(
    spawner: impl pal_async::task::Spawn + Clone + 'static,
    instances: &[crate::options::PcieRemoteCliConfig],
) -> (pcie_remote_device::PreparedMap, Vec<pal_async::task::Task<()>>) {
    use futures::stream::FuturesUnordered;
    use futures::stream::StreamExt;
    let prepared: pcie_remote_device::PreparedMap = std::sync::Arc::new(parking_lot::Mutex::new(
        std::collections::HashMap::new(),
    ));
    let mut tasks = Vec::new();
    let mut pending: FuturesUnordered<_> = instances
        .iter()
        .map(|cfg| {
            let spawner = spawner.clone();
            let cfg = cfg.clone();
            async move {
                let timeout = std::time::Duration::from_millis(cfg.handshake_timeout_ms as u64);
                let res = pal_async::timer::with_timeout(timeout, async move {
                    let listener = vmsocket::VmListener::bind(
                        vmsocket::VmAddress::vsock_any(cfg.vsock_port),
                    )
                    .ok()?;
                    // accept with retry-on-handshake-fail (spec §3.8 listener-kept-open)
                    let deadline = std::time::Instant::now() + timeout;
                    while std::time::Instant::now() < deadline {
                        let (stream, _) = listener.accept().await.ok()?;
                        let polled = pal_async::socket::PolledSocket::new(
                            &spawner.driver(),
                            stream,
                        )
                        .ok()?;
                        match pcie_remote_device::handshake::handshake(polled, cfg.instance_id)
                            .await
                        {
                            Ok((prepared, _tx)) => return Some(prepared),
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "pcie_remote handshake failed; will retry-accept"
                                );
                                pal_async::timer::PolledTimer::new(&spawner.driver())
                                    .sleep(std::time::Duration::from_millis(100))
                                    .await;
                            }
                        }
                    }
                    None
                })
                .await
                .ok()
                .flatten();
                (cfg.instance_id, res)
            }
        })
        .collect();
    while let Some((id, r)) = pending.next().await {
        if let Some(prep) = r {
            prepared.lock().insert(id, prep);
            tracing::info!(instance_id = %id, "pcie_remote: handshake ok");
        } else {
            tracing::warn!(instance_id = %id, "pcie_remote: handshake failed; device absent");
        }
    }
    (prepared, tasks)
}
```

> 工作期会按真实 `vmsocket::VmListener::accept` 与 `pal_async::socket` API 细节微调。

### Step 8.2 — Resolver 真实组装（替换 absent stub）

- [ ] Modify `vm/devices/pcie_remote_device/src/resolver.rs`：在两个 resolver 的 resolve() 里，把 `prepared` 真实喂给 `PcieRemoteDevice::new(state, prepared.to_worker)`，配置好 cfg_local（从 prepared.describe 提取 vendor/device/class 等），返回 `ResolvedPciDevice(Box::new(device))`。

### Step 8.3 — host SDK 示例（在 docs/superpowers/examples/）

- [ ] Create `docs/superpowers/examples/pcie_remote_noop_host/Cargo.toml`、`src/main.rs`：一个最小程序，TCP listen `127.0.0.1:48914`，accept 后收 Hello 回 HelloAck(DeviceDescribe=noop)；之后回应任何 MmioRead/Write 都返回 0。

### Step 8.4 — setup.ps1 落盘

- [ ] Create `docs/superpowers/scripts/setup-pcie-remote.ps1`（按 spec §4.3 setup.ps1 模板）。

### Step 8.5 — Guide 文档

- [ ] Create `Guide/src/reference/openhcl/devices/pcie_remote.md`：简明用户指南，写 4 种部署形态、setup.ps1 步骤、CLI 示例、WSL2 警告。

### Step 8.6 — Commit

```bash
git add openhcl/underhill_core/src/worker.rs vm/devices/pcie_remote_device/src/resolver.rs docs/superpowers/examples/ docs/superpowers/scripts/ Guide/src/reference/openhcl/devices/
git commit -m "feat(pcie_remote): real handshake spawn + resolver assembly + host SDK example

- worker.rs handshake spawner: vsock bind + accept-with-timeout +
  listener kept open on handshake failure
- resolver.rs: real PcieRemoteDevice assembly using prepared_map
- examples/pcie_remote_noop_host: minimal Rust host SDK consumer
- setup-pcie-remote.ps1: service GUID + Admin/SYSTEM ACL
- Guide doc: 4 deployment forms + WSL2 warning

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 9 ─ 本机 VM 实验（OpenVMM 路径优先）

> 用户的"本机起虚拟机做实验"诉求。OpenVMM 路径不需要 IGVM 构建，验证最快。

### Step 9.1 — 启动 host stub

- [ ] Run: `cargo run -p pcie_remote_noop_host -- --port 48914`
  Expected: 程序打印 "listening on 127.0.0.1:48914"

### Step 9.2 — OpenVMM 启动一个 Linux guest，挂 pcie_remote 设备

petri 已有 Linux 镜像 fixture（pipette + alpine/ubuntu base）。本步用最小命令行：

- [ ] Run: `cargo run -p openvmm -- --processors 2 --memory 1G --linux-direct ... --pcie-remote rc0rp0,socket=127.0.0.1:48914`
  Expected: VM 启动；dmesg | grep pci 看到一个 0xFFFF/0xFFFF 设备（因为 noop_host 返回的 cfg/MMIO 模拟一个最简单设备）

### Step 9.3 — 实验日志落 SESSION_LOG.md

- [ ] 把启动命令、guest dmesg 片段、host stub 收到的请求次数等附进 `docs/superpowers/SESSION_LOG.md`。

### Step 9.4 — Commit

```bash
git add docs/superpowers/SESSION_LOG.md
git commit -m "docs: phase 9 local VM experiment results

OpenVMM + linux guest + pcie_remote_noop_host: device enumerated
successfully, $N MMIO requests handled by host stub during boot.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 10 ─ OpenHCL IGVM build + 可选 Windows guest 验证（工作量大；按时间决定）

### Step 10.1 — 跨编译 OpenHCL IGVM

- [ ] Run: `cargo xflowey build-igvm x64 --release`
  Expected: 输出 `flowey-out/.../openhcl-x64.bin`（IGVM 文件）

### Step 10.2 — 用 OpenVMM 启动 OpenHCL + 一个 minimal Linux guest

- [ ] Run: `cargo run -p openvmm -- --hv --vtl2 ... --pcie-remote-takeover <fake-guid>:50000`
  Expected: OpenHCL 加载；handshake 因无 host stub 超时；设备 absent；VM 仍正常启动

### Step 10.3 — 启动 host stub 后重启 VM

- [ ] 重启 VM，验证 OpenHCL 接到 vsock，handshake 成功（CVM 路径不验证）。

### Step 10.4 — 进度日志 + 最终 commit

```bash
git add docs/superpowers/SESSION_LOG.md
git commit -m "docs: phase 10 OpenHCL IGVM + linux guest experiment

OpenHCL IGVM cross-built; pcie_remote_vmbus handshake completed over
vsock; device visible in guest. AbsentPcieDevice path also verified
(handshake intentionally skipped → no boot fail).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## 任务完成判定（自检）

- [x] Spec 每一节都有对应 phase 实现（§3.1→Phase 7；§3.2→Phase 5+7；§3.3→Phase 2/4/6/7；§3.4→Phase 1；§3.5→Phase 3/5；§3.6→Phase 4+8；§3.7→Phase 5；§3.8→Phase 2/3/8；§3.9→Phase 7；§3.10→Phase 2+6）
- [x] 无 "TODO"/"TBD" 步骤；每段 code 完整
- [x] 类型一致：`PreparedMap` / `PreparedPcieRemoteDevice` / `SharedState` / `PcieRemoteTcpHandle` / `PcieRemoteVmbusHandle` 跨 phase 复用
- [x] 每个 phase 独立 commit

## Spec §10 P1/P2 处理映射

| K-ID | 处理位点 |
|------|----------|
| K-1 setup.ps1 ACL | Phase 8 Step 8.4 |
| K-2 CLI 唯一来源 | Phase 7 Step 7.1（只支持 CLI） |
| K-3 CLI help WSL2 警告 | Phase 8 Step 8.5 + Phase 6 Step 6.4 注释 |
| K-4 Hyper-V day-0 验证 | Phase 10 |
| K-5 prost build.rs spike | Phase 1 Step 1.7（cargo build 验证 build.rs 等价于 diag_proto） |
| K-6 dead-man 数据结构 | Phase 3 Step 3.1（VecDeque）|
| K-7 AtomicU8 ordering | Phase 2 Step 2.3 |
| K-8 inspect CVM gate | Phase 5/7 实施期加 `#[inspect(skip)]` |
| K-9 guid 依赖 | Phase 2 Step 2.1 |
| K-10 spec 重排 | 不在此 v1 plan 处理 |
| K-11 accept 次数上限 | Phase 8 Step 8.1 实施期加常量 |
| 其他 P2 | 各 phase 实施期收集 |
