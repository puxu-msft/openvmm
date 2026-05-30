# PCIe Remote 实施计划 v2

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **2026-05-30 STATUS：实施计划已全部完成。** Phase 1 ~ 10 落地完毕，
> 真 Linux KVM (OpenVMM 路径) + 真 Hyper-V (OpenHCL 路径) 端到端
> 全部验证通过。最新成果详见 [../SESSION_LOG.md](../SESSION_LOG.md)
> 的"2026-05-30 🎉🎉🎉 真 Hyper-V 端到端验证"段（emoji 标题，
> 用 `grep -n "真 Hyper-V 端到端验证" docs/superpowers/SESSION_LOG.md` 定位）；
> K-IDs 表见 [../specs/2026-05-29-pcie-remote-design.md](../specs/2026-05-29-pcie-remote-design.md) §10。

**Goal:** 实现 OpenHCL/OpenVMM 远程 PCIe 实验设备 v1 —— guest 看到通用 PCIe 设备，所有语义在 Windows host 用户态程序，OpenHCL 内薄壳；同时把 OpenVMM 路径的半成品补齐。

**Architecture:** 2 个 in-tree crate (`pcie_remote_protocol` + `pcie_remote_device`)；vsock (OpenHCL) / TCP loopback (OpenVMM) 双 transport；resolver 动态注册自持 `prepared_map`；CVM 静默过滤 + 兜底 `AbsentPcieDevice`。

**Tech Stack:** Rust 1.95、prost、mesh::MeshPayload、pal_async (futures::io)、support/vmsocket、pci_core (ConfigSpaceType0Emulator + MsixEmulator)、chipset_device (IoResult::Defer)。

**Reference Spec:** [docs/superpowers/specs/2026-05-29-pcie-remote-design.md](../specs/2026-05-29-pcie-remote-design.md)（v3.1）

**Execution mode:** 无人值守，phase-by-phase commit；reviewer 已完成 v1→v2 收口。

---

## v1 → v2 主要修订（吸收 3 路 reviewer 反馈）

| ID | 来源 | 修订要点 |
|----|------|----------|
| F-1 | rust | `DeferredRead/Write::complete_error`（不是 `complete_err`） |
| F-2 | rust | `ResolvedPciDevice::from(dev)` 走 `Into<ResolvedChipsetDevice>`；需要 ChangeDeviceState + ProtobufSaveRestore + InspectMut + ChipsetDevice 四 trait |
| F-3 | rust | 不要 impl `GenericPciBusDevice`，pci_bus 通过 ChipsetDevice::supports_pci() 自动适配 |
| F-4 | rust | `pal_async::timer::with_timeout` 不存在；用 `mesh::CancelContext::new().with_timeout(d).until_cancelled(fut)` |
| F-5 | rust | `vmsocket::VmListener::accept` 同步；async 路径用 `PolledSocket::new(driver, listener)` + `.accept().await` |
| F-6 | rust | `Guid::as_bytes()` (zerocopy)，没有 `into_inner` |
| F-7 | rust | handshake 必须把 `Receiver<ToHost>` 一起返回给 worker，否则 receiver 立刻 drop = 所有 send 报错 |
| F-8 | rust | DeferredRead::complete 长度必须等于 access size（1/2/4/8），不能写死 8 字节 |
| F-9 | arch | dispatch.rs 必须**不**放在 `#[cfg(target_os = "linux")]` 内（vfio 是 linux-only）；找 try_join_all(cfg.pcie_devices) 之前注册 |
| F-10 | arch | Phase 6 + 8.2 合并：避免有"半 commit"窗口让 OpenVMM `--pcie-remote` 设备回归为 absent |
| F-11 | arch | dead-man `Connecting` 阶段宽限：在 SharedState != Live 时不触发 |
| F-12 | sec | OpenVMM CLI 的 `0.0.0.0:48914` 示例改为 `127.0.0.1`；`check_tcp_loopback` 失败用 `eprintln!` 让用户可见，不要静默 |
| F-13 | arch | host_sdk example crate 在 Cargo workspace.exclude 中声明 |
| F-14 | rust | `_tasks: Arc<Vec<Task>>` 改为 resolver 不持 task，task 由 underhill_core 独立 Vec 持有 |
| F-15 | rust | `Arc<JoinSet>` 不适用；直接给 resolver 持 `Vec<Task>`（无 Clone），resolver 用 `Arc<dyn AsyncResolveResource>` 注入 |
| F-16 | rust | worker `select_biased!` future 不能跨迭代复用；每轮重建 |
| F-17 | rust | `bind` 失败要 `tracing::error!` 区别于 per-connection 错误；不一律 `.ok()?` |
| F-18 | sec | host SDK example main.rs 强制 `bind 127.0.0.1`，文档约束不接受任何 `--bind` 参数 |
| F-19 | rust | msix_count 校验 ≤ 2048 |

---

## Phase 编排（v2）

> 所有 Phase 已完成（2026-05-30）；详细完成状态见末尾"任务分级"表。

| Phase | 主题 | 状态 | 阻塞 | 验证 |
|-------|------|------|------|------|
| 0 | 落 spec + plan + log（已完成 commit 60a100f5） | ✅ | — | — |
| 1 | `pcie_remote_protocol` crate | ✅ | — | crate `cargo test` |
| 2 | `pcie_remote_resources` 扩 + `pcie_remote_device` 骨架 + `AbsentPcieDevice` + state/error | ✅ | P1 | crate `cargo test` + `cargo check -p openvmm_entry`（不破坏 build） |
| 3 | worker.rs + dead-man（mock transport） | ✅ | P2 | 单测 |
| 4 | handshake.rs + prepared.rs（mock transport） | ✅ | P3 | 单测 |
| 5 | transport.rs + dma.rs + device.rs | ✅ | P4 | 单测 |
| **6** | **OpenVMM 端真实接线（resolver + handshake spawn + CLI 迁移）** —— 一次性把 OpenVMM 路径走通，不留 absent-stub 窗口 | ✅ | P5 | `cargo build -p openvmm` + 启动 noop host stub 后 VM 能枚举设备 |
| 7 | OpenHCL 端 handle/CLI/takeover/resolver 注入 | ✅ | P5 | `cargo check --target x86_64-unknown-linux-musl -p underhill_core` |
| 8 | host SDK 示例（noop 设备）+ setup.ps1 + Guide 文档 | ✅ | P6 | host stub 独立 `cargo run` |
| 9 | OpenVMM + Linux guest 在本机 VM 实验 | ✅ | P6+P8 | guest dmesg 能看到设备（真 KVM e2e）|
| 10 | OpenHCL IGVM build（WSL 交叉编译）+ 启动 Linux guest 验证（可选） | ✅ | P7 | IGVM 启动 + 真 Hyper-V vsock handshake ok |

---

## Phase 1 ─ `pcie_remote_protocol` crate

**Files:**
- Create: `vm/devices/pcie_remote_protocol/Cargo.toml`
- Create: `vm/devices/pcie_remote_protocol/build.rs`
- Create: `vm/devices/pcie_remote_protocol/src/lib.rs`
- Create: `vm/devices/pcie_remote_protocol/src/codec.rs`
- Create: `vm/devices/pcie_remote_protocol/proto/pcie_remote.proto`
- Modify: `Cargo.toml` (workspace.dependencies + workspace.members)

### Step 1.1 — proto schema

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

### Step 1.3 — build.rs (mirror diag_proto verbatim)

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

### Step 1.4 — codec.rs

- [ ] Create `vm/devices/pcie_remote_protocol/src/codec.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Length-prefixed framing codec on top of `futures::io`. Each frame:
//! 4-byte little-endian length + protobuf payload.

use crate::MAX_FRAME_BYTES;
use futures::io::AsyncRead;
use futures::io::AsyncReadExt;
use futures::io::AsyncWrite;
use futures::io::AsyncWriteExt;
use prost::Message;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame length {0} exceeds limit")]
    FrameTooLarge(u32),
    #[error("protobuf decode failed: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("protobuf encode failed: {0}")]
    Encode(#[from] prost::EncodeError),
}

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
    Ok(M::decode(buf.as_slice())?)
}

pub async fn write_frame<W: AsyncWrite + Unpin, M: Message>(
    writer: &mut W,
    msg: &M,
) -> Result<(), CodecError> {
    let mut buf = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut buf)?;
    if buf.len() > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge(buf.len() as u32));
    }
    writer.write_all(&(buf.len() as u32).to_le_bytes()).await?;
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
        assert_eq!(got.magic, msg.magic);
        assert_eq!(got.version, msg.version);
        assert_eq!(got.instance_id, msg.instance_id);
    }

    #[async_test]
    async fn frame_too_large_rejected() {
        let oversized = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes();
        let mut cur = Cursor::new(oversized.to_vec());
        let r: Result<Hello, _> = read_frame(&mut cur).await;
        assert!(matches!(r, Err(CodecError::FrameTooLarge(_))));
    }

    #[async_test]
    async fn truncated_payload_returns_io_error() {
        let mut buf = 16u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 4]);
        let mut cur = Cursor::new(buf);
        let r: Result<Hello, _> = read_frame(&mut cur).await;
        assert!(matches!(r, Err(CodecError::Io(_))));
    }
}
```

### Step 1.5 — lib.rs

- [ ] Create `vm/devices/pcie_remote_protocol/src/lib.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wire protocol for the pcie_remote experimental device.
//! All multi-byte numeric fields are little-endian on the wire.

#![forbid(unsafe_code)]

use inspect as _;
use mesh as _;
use prost as _;

pub const MAX_FRAME_BYTES: usize = 1 << 20;
pub const MAX_DMA_BYTES: usize = 64 << 10;
pub const PROTOCOL_MAGIC: u32 = 0x52504345;
pub const PROTOCOL_VERSION: u32 = 1;

pub mod codec;

/// Generated protobuf types. Generated code does not conform to our lint
/// configuration; silence the relevant lints inside this submodule only.
#[expect(missing_docs)]
#[expect(clippy::allow_attributes)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/openhcl.pcie_remote.v1.rs"));
}

pub use proto::*;
```

### Step 1.6 — workspace 注册

- [ ] Modify `Cargo.toml`：
  - `workspace.members` 中 `"vm/devices/pcie_remote_resources",` 后追加：
    ```toml
        "vm/devices/pcie_remote_protocol",
        "vm/devices/pcie_remote_device",
    ```
  - `workspace.dependencies` 中 `pcie_remote_resources = ...` 行后追加：
    ```toml
    pcie_remote_protocol = { path = "vm/devices/pcie_remote_protocol" }
    pcie_remote_device = { path = "vm/devices/pcie_remote_device" }
    ```
  - 同时 `workspace.exclude`（若不存在则添加）加入：
    ```toml
    exclude = [
        "docs/superpowers/examples/pcie_remote_noop_host",
    ]
    ```
    （pcie_remote_device 在 Phase 2 创建；workspace 此时引用一个不存在的 path 会 cargo error。**所以本 step 把 device crate 路径与 members 一起写好，Phase 2 创建空壳让 workspace 能 resolve。** —— 在 Phase 2 Step 2.0 完成）

- [ ] **Step 1.6a (workaround)**：先临时把 `pcie_remote_device` 路径**注释掉**（前缀 `#`），等 Phase 2 创建 crate 后取消注释。或：现在不在 members 里加 device，等 Phase 2 再加。**选后者** —— 本 Step 仅添加 protocol。

### Step 1.7 — 构建 + 测试

- [ ] Run: `cargo build -p pcie_remote_protocol 2>&1 | tail -10`
  Expected: 成功；产物 `target/debug/build/pcie_remote_protocol-*/out/openhcl.pcie_remote.v1.rs`
  Verify: `ls target/debug/build/pcie_remote_protocol-*/out/*.rs`

- [ ] Run: `cargo test -p pcie_remote_protocol 2>&1 | tail -20`
  Expected: 3 passed

- [ ] Run: `cargo clippy -p pcie_remote_protocol -- -D warnings 2>&1 | tail -10`
  Expected: no warnings

### Step 1.8 — Commit

```bash
git add vm/devices/pcie_remote_protocol/ Cargo.toml
git commit -m "feat(pcie_remote): pcie_remote_protocol crate

Wire protocol via prost + length-prefixed framing on futures::io.
build.rs mirrors openhcl/diag_proto verbatim (MeshPayload + mesh(prost)).

- proto: Hello/HelloAck/DeviceDescribe/ToHost/ToOpenhcl
- codec.rs: read_frame/write_frame; 1 MiB upper bound
- 3 unit tests pass

Implements spec §3.4.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 2 ─ resources 扩 + device 骨架 + AbsentPcieDevice

**Files:**
- Create: `vm/devices/pcie_remote_device/Cargo.toml`
- Create: `vm/devices/pcie_remote_device/src/lib.rs`
- Create: `vm/devices/pcie_remote_device/src/state.rs`
- Create: `vm/devices/pcie_remote_device/src/error.rs`
- Create: `vm/devices/pcie_remote_device/src/absent.rs`
- Modify: `vm/devices/pcie_remote_resources/src/lib.rs`
- Modify: `vm/devices/pcie_remote_resources/Cargo.toml`
- Modify: `Cargo.toml`（添加 device crate 到 members）

### Step 2.0 — workspace 添加 device crate（占位）

- [ ] Create empty `vm/devices/pcie_remote_device/Cargo.toml`（仅最少内容）+ `src/lib.rs`（空文件），让 `Cargo.toml` 把 path 加进 members 后能 resolve。完整内容在后续 step 填充。

- [ ] Modify `Cargo.toml`：`workspace.members` 追加 `"vm/devices/pcie_remote_device",`

### Step 2.1 — pcie_remote_resources 改造

- [ ] Read `vm/devices/pcie_remote_resources/Cargo.toml`：确认 `guid = { workspace = true, features = ["mesh"] }` 已存在；若缺则补。

- [ ] Modify `vm/devices/pcie_remote_resources/Cargo.toml`：dependencies 段确保 `guid = { workspace = true, features = ["mesh"] }`。

- [ ] Modify `vm/devices/pcie_remote_resources/src/lib.rs`：完整替换为：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

//! Resource definitions for the PCIe remote experimental device.

use mesh::MeshPayload;
use vm_resource::ResourceId;
use vm_resource::kind::PciDeviceHandleKind;

pub const DEFAULT_SOCKET_ADDR: &str = "127.0.0.1:48914";

/// Legacy handle kept for backwards-compatibility with existing OpenVMM CLI
/// (`--pcie-remote ...,socket=...`). New code should use
/// `PcieRemoteTcpHandle` (OpenVMM) or `PcieRemoteVmbusHandle` (OpenHCL).
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
///
/// `socket_addr` 必须是 loopback（127.0.0.1 / ::1）；resolver 会校验。
#[derive(Debug, Clone, MeshPayload)]
pub struct PcieRemoteTcpHandle {
    pub instance_id: guid::Guid,
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

### Step 2.2 — device crate Cargo.toml（完整版替换 Step 2.0 占位）

- [ ] Overwrite `vm/devices/pcie_remote_device/Cargo.toml`：

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

# vm-* deps
pci_core.workspace = true
pci_resources.workspace = true
pci_bus.workspace = true
chipset_device.workspace = true
chipset_device_resources.workspace = true
vmcore.workspace = true
vm_resource.workspace = true
guestmem.workspace = true

# support-* deps
pal_async.workspace = true
mesh.workspace = true
guid = { workspace = true, features = ["mesh"] }
vmsocket.workspace = true
task_control.workspace = true
inspect.workspace = true

# crates.io
async-trait.workspace = true
thiserror.workspace = true
anyhow.workspace = true
tracing.workspace = true
tracelimit.workspace = true
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

use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

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
        s.try_transition(DeviceState::Connecting, DeviceState::Live).unwrap();
        assert_eq!(s.load(), DeviceState::Live);
        s.store(DeviceState::Lost);
        assert_eq!(s.load(), DeviceState::Lost);
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
    #[error("msix count {0} exceeds limit 2048")]
    MsixCountTooLarge(u32),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("codec error: {0}")]
    Codec(#[from] pcie_remote_protocol::codec::CodecError),
}
```

### Step 2.5 — absent.rs（F-2/F-3 修订；只实现 ChipsetDevice 全家四 trait）

- [ ] Create `vm/devices/pcie_remote_device/src/absent.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Local stub PCI device used to honor CVM bug-safety (spec §3.10) and as
//! a unit-test fixture. No host protocol surface.
//! cfg_read 全 1；cfg_write 静默丢弃。

use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use inspect::InspectMut;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::SaveRestore;

#[derive(InspectMut)]
pub struct AbsentPcieDevice {}

impl AbsentPcieDevice {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for AbsentPcieDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangeDeviceState for AbsentPcieDevice {
    fn start(&mut self) {}
    fn stop(&mut self) -> impl std::future::Future<Output = ()> + Send {
        std::future::ready(())
    }
    fn reset(&mut self) -> impl std::future::Future<Output = ()> + Send {
        std::future::ready(())
    }
}

impl SaveRestore for AbsentPcieDevice {
    type SavedState = NoSavedState;
    fn save(&mut self) -> Result<Self::SavedState, vmcore::save_restore::SaveError> {
        Ok(NoSavedState)
    }
    fn restore(&mut self, _: Self::SavedState) -> Result<(), vmcore::save_restore::RestoreError> {
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfg_read_all_ones() {
        let mut d = AbsentPcieDevice::new();
        let mut v = 0u32;
        assert!(matches!(d.pci_cfg_read(0, &mut v), IoResult::Ok));
        assert_eq!(v, !0u32);
    }

    #[test]
    fn cfg_write_silently_ok() {
        let mut d = AbsentPcieDevice::new();
        assert!(matches!(d.pci_cfg_write(0, 0xdead), IoResult::Ok));
    }

    #[test]
    fn supports_pci_returns_self() {
        let mut d = AbsentPcieDevice::new();
        assert!(d.supports_pci().is_some());
    }
}
```

> 验证 `ChangeDeviceState` / `SaveRestore` / `NoSavedState` / `PciConfigSpace::suggested_bdf` 签名时若发现仓库实际形态不同（如 `start/stop/reset` 是 `async fn` 而非 RPIT），按编译器报错调整。

### Step 2.6 — lib.rs

- [ ] Create `vm/devices/pcie_remote_device/src/lib.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Remote PCIe experimental device.
//!
//! Architecture notes (spec v3.1):
//!   - Resolver self-owns prepared_map; handle carries only instance_id
//!   - cfg 100% sync, MMIO IoResult::Defer
//!   - state machine: Connecting → Live → Lost (terminal in v1)
//!   - 不支持 CVM / save_restore / hotplug (v1)

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

- [ ] Run: `cargo build -p pcie_remote_device -p pcie_remote_resources 2>&1 | tail -15`
  Expected: 成功

- [ ] Run: `cargo test -p pcie_remote_device 2>&1 | tail -15`
  Expected: state + absent 测试通过

- [ ] Run: `cargo check -p openvmm_entry 2>&1 | tail -10`
  Expected: 成功（deprecated handle 仍兼容）

### Step 2.8 — Commit

```bash
git add vm/devices/pcie_remote_device/ vm/devices/pcie_remote_resources/ Cargo.toml
git commit -m "feat(pcie_remote): device crate skeleton + AbsentPcieDevice + new handles

- pcie_remote_resources: PcieRemoteTcpHandle (OpenVMM) and
  PcieRemoteVmbusHandle (OpenHCL); old PcieRemoteHandle deprecated;
  DEFAULT_SOCKET_ADDR changed to 127.0.0.1 (security hardening)
- pcie_remote_device: state.rs / error.rs / absent.rs
- AbsentPcieDevice impls ChipsetDevice + PciConfigSpace +
  ChangeDeviceState + SaveRestore (NoSavedState); cfg_read returns !0

Implements spec §3.10 (sentinel) and §3.8 (state).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 3 ─ worker + dead-man（mock transport）

**Files:**
- Create: `vm/devices/pcie_remote_device/src/worker.rs`
- Create: `vm/devices/pcie_remote_device/src/deadman.rs`
- Modify: `vm/devices/pcie_remote_device/src/lib.rs`

### Step 3.1 — deadman.rs（F-11：Connecting 不触发）

- [ ] Create `vm/devices/pcie_remote_device/src/deadman.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Dead-man switch (spec §3.5).
//!
//! 双条件并联：
//! 1. 连续 ≥ N_CONSEC 次 timeout（默认 4）
//! 2. 1s 滑窗内 ≥ N_WINDOW 次 timeout（默认 8）且占比 ≥ 50%
//!
//! Connecting 阶段的 timeout 不计数（boot 早期 host 可能未就绪）。

use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;

pub const N_CONSEC: u32 = 4;
pub const N_WINDOW: usize = 8;
pub const WINDOW: Duration = Duration::from_secs(1);

pub struct DeadMan {
    consec: u32,
    window: VecDeque<(Instant, bool /* is_timeout */)>,
}

impl DeadMan {
    pub fn new() -> Self {
        Self { consec: 0, window: VecDeque::with_capacity(16) }
    }

    pub fn record_timeout(&mut self, now: Instant) -> bool {
        self.consec += 1;
        self.window.push_back((now, true));
        self.trim(now);
        self.is_tripped()
    }

    pub fn record_success(&mut self, now: Instant) -> bool {
        self.consec = 0;
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
        if self.consec >= N_CONSEC {
            return true;
        }
        let timeouts = self.window.iter().filter(|(_, t)| *t).count();
        timeouts >= N_WINDOW && timeouts * 2 >= self.window.len()
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
        let t0 = Instant::now();
        for i in 0..3 {
            assert!(!d.record_timeout(t0 + Duration::from_millis(i)));
        }
        assert!(d.record_timeout(t0 + Duration::from_millis(4)));
    }

    #[test]
    fn one_success_resets_consec() {
        let mut d = DeadMan::new();
        let now = Instant::now();
        for _ in 0..3 {
            d.record_timeout(now);
        }
        d.record_success(now);
        for _ in 0..3 {
            assert!(!d.record_timeout(now));
        }
    }

    #[test]
    fn window_ratio_only_no_consec() {
        // 通过 7 timeout + 1 success 交替防 consec 触发，验证滑窗 ratio
        // 路径在 N_CONSEC < threshold 时也能触发：
        let mut d = DeadMan::new();
        let t0 = Instant::now();
        let mut tripped = false;
        for i in 0..16 {
            let when = t0 + Duration::from_millis(i * 50);
            let should_be_timeout = i % 2 == 0; // 50/50
            let res = if should_be_timeout {
                d.record_timeout(when)
            } else {
                d.record_success(when)
            };
            if res {
                tripped = true;
                break;
            }
        }
        // 50/50 占比 + 总数 16，timeouts=8 == N_WINDOW，ratio=50% → 应触发
        assert!(tripped, "ratio threshold should trip");
    }

    #[test]
    fn old_entries_trimmed() {
        let mut d = DeadMan::new();
        let t0 = Instant::now();
        for _ in 0..3 {
            d.record_timeout(t0);
        }
        d.record_success(t0);
        let t1 = t0 + Duration::from_secs(2);
        for _ in 0..3 {
            d.record_timeout(t1);
        }
        assert!(d.record_timeout(t1 + Duration::from_millis(1)));
    }
}
```

### Step 3.2 — worker.rs（F-1/F-7/F-8/F-16 修订）

- [ ] Create `vm/devices/pcie_remote_device/src/worker.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Background task processing messages over a Transport.
//!
//! 设计要点（spec §3.3 / §3.5 / §3.8）：
//! - worker 拥有 transport、in-flight map、dead-man、SharedState
//! - 进 Lost 时同步 drain in-flight (complete_error NoResponse)
//! - 关闭信号通过 mesh::Receiver<()> 接收

use crate::deadman::DeadMan;
use crate::state::DeviceState;
use crate::state::SharedState;
use chipset_device::io::IoError;
use chipset_device::io::deferred::DeferredRead;
use chipset_device::io::deferred::DeferredWrite;
use futures::FutureExt;
use futures::io::AsyncRead;
use futures::io::AsyncWrite;
use mesh::Receiver;
use pcie_remote_protocol::ToHost;
use pcie_remote_protocol::ToOpenhcl;
use pcie_remote_protocol::codec;
use std::collections::HashMap;

/// Pending request stored against a sequence number.
pub enum InFlight {
    Read { token: DeferredRead, access_size: usize },
    Write { token: DeferredWrite },
}

/// Message sent from device shim → worker.
pub struct DeviceRequest {
    pub seq: u64,
    pub frame: ToHost,
    pub pending: Option<InFlight>,
}

pub struct Worker<T> {
    transport: T,
    state: SharedState,
    in_flight: HashMap<u64, InFlight>,
    _deadman: DeadMan,
    from_device: Receiver<DeviceRequest>,
}

impl<T> Worker<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    pub fn new(
        transport: T,
        state: SharedState,
        from_device: Receiver<DeviceRequest>,
    ) -> Self {
        Self {
            transport,
            state,
            in_flight: HashMap::new(),
            _deadman: DeadMan::new(),
            from_device,
        }
    }

    pub async fn run(mut self, mut shutdown: Receiver<()>) {
        use futures::select_biased;
        loop {
            // 每轮重建 future（mesh Recv 不是 Unpin）
            select_biased! {
                _ = shutdown.recv().fuse() => {
                    tracing::info!("pcie_remote worker shutdown signal");
                    break;
                }
                req = self.from_device.recv().fuse() => {
                    let Ok(req) = req else { break };
                    if let Some(pending) = req.pending {
                        self.in_flight.insert(req.seq, pending);
                    }
                    if let Err(e) = codec::write_frame(&mut self.transport, &req.frame).await {
                        tracing::warn!(error = %e, "write_frame failed; going Lost");
                        break;
                    }
                }
                inbound = codec::read_frame::<_, ToOpenhcl>(&mut self.transport).fuse() => {
                    match inbound {
                        Ok(m) => self.dispatch_inbound(m),
                        Err(e) => {
                            tracing::warn!(error = %e, "read_frame failed; going Lost");
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
                if let Some(InFlight::Read { token, access_size }) = self.in_flight.remove(&seq) {
                    let bytes = r.value.to_le_bytes();
                    // F-8: 严格按 access_size 截断（1/2/4/8）
                    token.complete(&bytes[..access_size]);
                }
            }
            Some(Body::ReadGpa(_) | Body::WriteGpa(_)) => {
                tracing::warn!("DMA messages not yet implemented");
            }
            Some(Body::InterruptFire(_)) => {
                tracing::warn!("InterruptFire wiring lands when device.rs holds Vec<Interrupt>");
            }
            None => {
                tracing::warn!("ToOpenhcl message missing body");
            }
        }
    }

    fn drain_in_flight(&mut self) {
        for (_, inflight) in self.in_flight.drain() {
            match inflight {
                InFlight::Read { token, .. } => token.complete_error(IoError::NoResponse),
                InFlight::Write { token } => token.complete_error(IoError::NoResponse),
            }
        }
    }
}
```

### Step 3.3 — lib.rs

- [ ] Modify `vm/devices/pcie_remote_device/src/lib.rs`：追加：

```rust
pub mod deadman;
pub mod worker;
```

### Step 3.4 — 测试

- [ ] Run: `cargo build -p pcie_remote_device 2>&1 | tail -15`
  Expected: 成功；若 `chipset_device::io::deferred::*` 在 mod 路径不同，按报错调整 use。

- [ ] Run: `cargo test -p pcie_remote_device 2>&1 | tail -15`
  Expected: 全通过

### Step 3.5 — Commit

```bash
git add vm/devices/pcie_remote_device/
git commit -m "feat(pcie_remote): worker loop + dead-man switch

- worker.rs: futures::select_biased loop on shutdown/device/inbound;
  drain_in_flight on Lost; access_size-aware DeferredRead::complete
- deadman.rs: 连续 ≥4 + 滑窗 ratio 双触发；4 单测
- DeviceRequest carries seq + frame + optional InFlight

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 4 ─ handshake + prepared

**Files:**
- Create: `vm/devices/pcie_remote_device/src/handshake.rs`
- Create: `vm/devices/pcie_remote_device/src/prepared.rs`
- Modify: `vm/devices/pcie_remote_device/src/lib.rs`

### Step 4.1 — prepared.rs（F-7：包含 sender + receiver；Phase 6 调用方拆 receiver 给 worker）

- [ ] Create `vm/devices/pcie_remote_device/src/prepared.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::worker::DeviceRequest;
use mesh::Receiver;
use mesh::Sender;
use pcie_remote_protocol::DeviceDescribe;

/// Per-instance handshake 完成后的产物。
///
/// `to_worker` 给 device 薄壳用；`worker_inbox` 由调用方 take 给 Worker::new。
/// Transport 由调用方持有（不放进 Prepared，因为 trait object 类型擦除不便）。
pub struct PreparedPcieRemoteDevice {
    pub describe: DeviceDescribe,
    pub to_worker: Sender<DeviceRequest>,
    pub worker_inbox: Option<Receiver<DeviceRequest>>,
}

impl PreparedPcieRemoteDevice {
    pub fn take_worker_inbox(&mut self) -> Receiver<DeviceRequest> {
        self.worker_inbox.take().expect("worker_inbox already taken")
    }
}
```

### Step 4.2 — handshake.rs（F-4/F-6/F-19：超时、Guid::as_bytes、msix_count 校验）

- [ ] Create `vm/devices/pcie_remote_device/src/handshake.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Handshake (spec §3.3 Connecting).

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
use zerocopy::IntoBytes;

const EXT_CAP_TOTAL_LIMIT: usize = 0xFFC;
const MSIX_COUNT_LIMIT: u32 = 2048;

/// Run application-level handshake over a connected transport.
///
/// 调用方在外部用 `CancelContext::with_timeout(...)` 给整个调用加超时。
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
        instance_id: instance_id.as_bytes().to_vec(),
    };
    codec::write_frame(&mut transport, &hello).await?;
    let ack: HelloAck = codec::read_frame(&mut transport).await?;
    if !ack.ok {
        return Err(Error::BarLayout(format!("host rejected: {}", ack.reason)));
    }
    let describe = ack
        .device
        .ok_or_else(|| Error::BarLayout("HelloAck.device missing".into()))?;
    validate_describe(&describe)?;
    let (sender, receiver) = channel::<crate::worker::DeviceRequest>();
    let prepared = PreparedPcieRemoteDevice {
        describe,
        to_worker: sender,
        worker_inbox: Some(receiver),
    };
    Ok((prepared, transport))
}

fn validate_describe(d: &DeviceDescribe) -> Result<(), Error> {
    if d.msix_count > MSIX_COUNT_LIMIT {
        return Err(Error::MsixCountTooLarge(d.msix_count));
    }
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
    use pcie_remote_protocol::BarInfo;
    use pcie_remote_protocol::CapabilityBlob;
    use pcie_remote_protocol::bar_info::Kind;

    fn good_describe() -> DeviceDescribe {
        DeviceDescribe {
            vendor_id: 0x1414,
            device_id: 0xc000,
            class_code: 0x010802,
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

    #[test]
    fn rejects_bad_bar_size() {
        let mut d = good_describe();
        d.bars[0].size = 100;
        assert!(validate_describe(&d).is_err());
        let mut d = good_describe();
        d.bars[0].size = 0;
        assert!(validate_describe(&d).is_err());
        let mut d = good_describe();
        d.bars[0].size = 2048;
        assert!(validate_describe(&d).is_err());
    }

    #[test]
    fn rejects_unaligned_cap_raw() {
        let mut d = good_describe();
        d.capabilities.push(CapabilityBlob {
            cap_id: 0x10,
            raw: vec![0u8; 7],
        });
        assert!(validate_describe(&d).is_err());
    }

    #[test]
    fn rejects_msix_overflow() {
        let mut d = good_describe();
        d.msix_count = 4000;
        assert!(validate_describe(&d).is_err());
    }

    #[test]
    fn accepts_good_describe() {
        assert!(validate_describe(&good_describe()).is_ok());
    }
}
```

### Step 4.3 — lib.rs

- [ ] Modify `vm/devices/pcie_remote_device/src/lib.rs`：追加：

```rust
pub mod handshake;
pub mod prepared;
pub use prepared::PreparedPcieRemoteDevice;
```

### Step 4.4 — 测试 + Commit

- [ ] Run: `cargo test -p pcie_remote_device 2>&1 | tail -20`
  Expected: 全通过

```bash
git add vm/devices/pcie_remote_device/
git commit -m "feat(pcie_remote): handshake + Prepared

- handshake.rs: writes Hello (Guid::as_bytes), reads HelloAck, validates
  msix_count ≤2048, BAR power-of-2 ≥4096, cap raw 4-aligned ≤0xFFC
- prepared.rs: keeps receiver via take_worker_inbox() so caller can
  hand it to Worker::new (F-7 fix)
- 4 unit tests pass

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 5 ─ transport + dma + device

**Files:**
- Create: `vm/devices/pcie_remote_device/src/transport.rs`
- Create: `vm/devices/pcie_remote_device/src/dma.rs`
- Create: `vm/devices/pcie_remote_device/src/device.rs`
- Modify: `vm/devices/pcie_remote_device/src/lib.rs`

### Step 5.1 — transport.rs（F-12 vsock 端口黑名单 + TCP loopback enforce）

- [ ] Create `vm/devices/pcie_remote_device/src/transport.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Transport selection + port validation.

use anyhow::Context as _;
use anyhow::anyhow;

/// vsock 端口黑名单（spec §3.2，v3.1 修正数值）。
pub const PORT_BLACKLIST: &[u32] = &[
    1,      // VSOCK_CONTROL_PORT (openhcl/diag_proto)
    2,      // VSOCK_DATA_PORT
    3,      // VNC default
    4,      // gdbstub default
    0x1337, // PIPETTE_VSOCK_PORT
];

pub fn check_vsock_port(
    port: u32,
    vnc_port: Option<u32>,
    gdbstub_port: Option<u32>,
) -> anyhow::Result<()> {
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
            assert!(check_vsock_port(p, None, None).is_err());
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

### Step 5.2 — dma.rs

- [ ] Create `vm/devices/pcie_remote_device/src/dma.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Result;
use anyhow::anyhow;
use guestmem::GuestMemory;
use pcie_remote_protocol::MAX_DMA_BYTES;

pub fn read_gpa(gm: &GuestMemory, gpa: u64, len: u32, ram_end: u64) -> Result<Vec<u8>> {
    if len as usize > MAX_DMA_BYTES {
        return Err(anyhow!("DMA read len {len} > MAX_DMA_BYTES"));
    }
    let end = gpa.checked_add(len as u64).ok_or_else(|| anyhow!("gpa overflow"))?;
    if end > ram_end {
        return Err(anyhow!("gpa {gpa}+{len} exceeds ram_end {ram_end}"));
    }
    let mut buf = vec![0u8; len as usize];
    gm.read_at(gpa, &mut buf).map_err(anyhow::Error::from)?;
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
    gm.write_at(gpa, data).map_err(anyhow::Error::from)?;
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

    #[test]
    fn read_rejects_overflow() {
        let gm = GuestMemory::empty();
        let r = read_gpa(&gm, u64::MAX, 1, u64::MAX);
        assert!(r.is_err());
    }
}
```

### Step 5.3 — device.rs（F-2/F-3：完整 trait 套装 + cfg 100% sync + Lost InvalidRegister）

- [ ] Create `vm/devices/pcie_remote_device/src/device.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Thin device shim (spec §3.5).
//!
//! 因为 vpci `compute_config_writes` 在 RMW 时用 now_or_never，cfg_read
//! 必须 100% sync 返回；Lost 状态返回 Err(InvalidRegister) 让 vpci 走
//! fill(!0) 路径。

use crate::state::DeviceState;
use crate::state::SharedState;
use crate::worker::DeviceRequest;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pci::PciConfigSpace;
use inspect::InspectMut;
use mesh::Sender;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::SaveRestore;

#[derive(InspectMut)]
pub struct PcieRemoteDevice {
    #[inspect(skip)]
    state: SharedState,
    #[inspect(skip)]
    to_worker: Sender<DeviceRequest>,
    /// 本地 cfg 镜像（64 dwords = 256 字节标准 cfg）。
    #[inspect(skip)]
    cfg_local: [u32; 64],
    #[inspect(skip)]
    next_seq: u64,
}

impl PcieRemoteDevice {
    pub fn new(state: SharedState, to_worker: Sender<DeviceRequest>, cfg_local: [u32; 64]) -> Self {
        Self {
            state,
            to_worker,
            cfg_local,
            next_seq: 1,
        }
    }
}

impl ChangeDeviceState for PcieRemoteDevice {
    fn start(&mut self) {}
    fn stop(&mut self) -> impl std::future::Future<Output = ()> + Send {
        std::future::ready(())
    }
    fn reset(&mut self) -> impl std::future::Future<Output = ()> + Send {
        std::future::ready(())
    }
}

impl SaveRestore for PcieRemoteDevice {
    type SavedState = NoSavedState;
    fn save(&mut self) -> Result<Self::SavedState, vmcore::save_restore::SaveError> {
        Ok(NoSavedState)
    }
    fn restore(&mut self, _: Self::SavedState) -> Result<(), vmcore::save_restore::RestoreError> {
        Ok(())
    }
}

impl ChipsetDevice for PcieRemoteDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl PciConfigSpace for PcieRemoteDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        match self.state.load() {
            DeviceState::Lost => IoResult::Err(IoError::InvalidRegister),
            DeviceState::Connecting | DeviceState::Live => {
                let idx = (offset >> 2) as usize;
                if idx < self.cfg_local.len() {
                    *value = self.cfg_local[idx];
                } else {
                    *value = !0;
                }
                IoResult::Ok
            }
        }
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        match self.state.load() {
            DeviceState::Lost => IoResult::Ok,
            _ => {
                let idx = (offset >> 2) as usize;
                if idx < self.cfg_local.len() {
                    self.cfg_local[idx] = value;
                }
                // cfg_write_side_effect forward: 留给后续 step
                // 当前仅本地记录。
                self.next_seq = self.next_seq.wrapping_add(1);
                let _ = &self.to_worker; // silence unused warning
                IoResult::Ok
            }
        }
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh::channel;

    #[test]
    fn lost_returns_err_invalid_register() {
        let s = SharedState::new(DeviceState::Lost);
        let (tx, _rx) = channel::<DeviceRequest>();
        let mut dev = PcieRemoteDevice::new(s, tx, [0u32; 64]);
        let mut v = 0;
        let r = dev.pci_cfg_read(0, &mut v);
        assert!(matches!(r, IoResult::Err(IoError::InvalidRegister)));
    }

    #[test]
    fn live_cfg_read_returns_local_value() {
        let s = SharedState::new(DeviceState::Live);
        let (tx, _rx) = channel::<DeviceRequest>();
        let mut cfg = [0u32; 64];
        cfg[0] = 0x80861234;
        let mut dev = PcieRemoteDevice::new(s, tx, cfg);
        let mut v = 0;
        assert!(matches!(dev.pci_cfg_read(0, &mut v), IoResult::Ok));
        assert_eq!(v, 0x80861234);
    }
}
```

### Step 5.4 — lib.rs + 测试 + Commit

- [ ] Modify `vm/devices/pcie_remote_device/src/lib.rs`：追加：

```rust
pub mod device;
pub mod dma;
pub mod transport;

pub use device::PcieRemoteDevice;
```

- [ ] Run: `cargo test -p pcie_remote_device 2>&1 | tail -20`

- [ ] Run: `cargo clippy -p pcie_remote_device -- -D warnings 2>&1 | tail -10`

```bash
git add vm/devices/pcie_remote_device/
git commit -m "feat(pcie_remote): transport + dma + device shim

- transport.rs: port blacklist (1/2/3/4/0x1337); tcp loopback enforce
- dma.rs: GPA range + MAX_DMA_BYTES (no IOMMU equivalence)
- device.rs: ChipsetDevice + PciConfigSpace + ChangeDeviceState +
  SaveRestore; cfg 100% sync; Lost → Err(InvalidRegister) for vpci

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 6 ─ OpenVMM 端真实接线（F-10：一次性走通）

> v2 把"resolver 接线"+"真实 prepared_map 组装"+"CLI 改造"合并为一个 phase，避免 v1 留半 commit 窗口。

**Files:**
- Create: `vm/devices/pcie_remote_device/src/resolver.rs`
- Create: `vm/devices/pcie_remote_device/src/handshake_spawn.rs`
- Modify: `vm/devices/pcie_remote_device/src/lib.rs`
- Modify: `openvmm/openvmm_core/Cargo.toml`
- Modify: `openvmm/openvmm_core/src/worker/dispatch.rs`
- Modify: `openvmm/openvmm_entry/Cargo.toml`
- Modify: `openvmm/openvmm_entry/src/lib.rs`
- Modify: `openvmm/openvmm_entry/src/cli_args.rs`（注释把 0.0.0.0 → 127.0.0.1）

### Step 6.1 — resolver.rs（F-2/F-14：返回 ResolvedPciDevice via .into()；不持 task）

- [ ] Create `vm/devices/pcie_remote_device/src/resolver.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Dynamic resolvers. Prepared map is populated by the caller before VM
//!装配 PCI devices; task ownership remains with caller (workers Vec).

use crate::AbsentPcieDevice;
use crate::PcieRemoteDevice;
use crate::PreparedPcieRemoteDevice;
use crate::state::DeviceState;
use crate::state::SharedState;
use crate::worker::Worker;
use async_trait::async_trait;
use parking_lot::Mutex;
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;
use pcie_remote_protocol::DeviceDescribe;
use pcie_remote_resources::PcieRemoteTcpHandle;
use pcie_remote_resources::PcieRemoteVmbusHandle;
use std::collections::HashMap;
use std::sync::Arc;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::kind::PciDeviceHandleKind;

pub type PreparedMap = Arc<Mutex<HashMap<guid::Guid, PreparedPcieRemoteDevice>>>;

pub struct PcieRemoteTcpResolver {
    prepared: PreparedMap,
}

impl PcieRemoteTcpResolver {
    pub fn new(prepared: PreparedMap) -> Self {
        Self { prepared }
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
        _: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let Some(prep) = self.prepared.lock().remove(&handle.instance_id) else {
            tracing::error!(instance_id = %handle.instance_id,
                "pcie_remote handshake missing; serving AbsentPcieDevice");
            return Ok(AbsentPcieDevice::new().into());
        };
        let dev = assemble_device(prep);
        Ok(dev.into())
    }
}

pub struct PcieRemoteVmbusResolver {
    prepared: PreparedMap,
}

impl PcieRemoteVmbusResolver {
    pub fn new(prepared: PreparedMap) -> Self {
        Self { prepared }
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
        _: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let Some(prep) = self.prepared.lock().remove(&handle.instance_id) else {
            tracing::error!(instance_id = %handle.instance_id,
                "pcie_remote handshake missing; serving AbsentPcieDevice");
            return Ok(AbsentPcieDevice::new().into());
        };
        let dev = assemble_device(prep);
        Ok(dev.into())
    }
}

fn assemble_device(mut prep: PreparedPcieRemoteDevice) -> PcieRemoteDevice {
    let cfg_local = build_initial_cfg(&prep.describe);
    let state = SharedState::new(DeviceState::Live);
    // worker_inbox 由 spawner 在 spawn worker 前 take 走（见 handshake_spawn）。
    // 此处仅消费 to_worker；worker 已在 spawner 端持有 inbox。
    let _ = prep.worker_inbox.take();
    PcieRemoteDevice::new(state, prep.to_worker, cfg_local)
}

/// 用 DeviceDescribe 填本地 cfg 镜像的前 4 dwords（vendor/device/status/class）。
/// MMIO BAR 暂未走 RegisterMmioIntercept；v2 留给 Phase 8 后扩展。
fn build_initial_cfg(d: &DeviceDescribe) -> [u32; 64] {
    let mut cfg = [0u32; 64];
    cfg[0] = (d.vendor_id as u32) | ((d.device_id as u32) << 16);
    // command=0, status=0
    cfg[1] = 0;
    // revision (low) + class_code (high 24)
    cfg[2] = ((d.class_code & 0x00ff_ffff) << 8) | (d.revision & 0xff);
    cfg
}

#[allow(dead_code)]
fn spawn_worker_task<T>(
    spawner: impl pal_async::task::Spawn,
    transport: T,
    state: SharedState,
    inbox: mesh::Receiver<crate::worker::DeviceRequest>,
) -> pal_async::task::Task<()>
where
    T: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin + Send + 'static,
{
    let (_shutdown_tx, shutdown_rx) = mesh::channel::<()>();
    // shutdown_tx 不被任何人持有，意味着 shutdown 不会主动触发；
    // worker 通过 transport EOF 进 Lost 即可（v1 不支持 graceful shutdown）。
    spawner.spawn("pcie_remote_worker", async move {
        Worker::new(transport, state, inbox).run(shutdown_rx).await;
    })
}
```

### Step 6.2 — handshake_spawn.rs（F-4/F-5/F-17：CancelContext 超时；PolledSocket accept；bind 失败 error log）

- [ ] Create `vm/devices/pcie_remote_device/src/handshake_spawn.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Spawn TCP/vsock handshake at boot time. Per-instance timeout via
//! `mesh::CancelContext::with_timeout(...).until_cancelled(...)`.

use crate::handshake::handshake;
use crate::prepared::PreparedPcieRemoteDevice;
use crate::resolver::PreparedMap;
use mesh::CancelContext;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use std::time::Duration;
use std::time::Instant;

/// 启动期 handshake 抽象：listener 由调用方按 transport 类型创建后传入；
/// 此函数负责 accept-retry-on-failure (spec §3.8) + 总超时 + 调 handshake。
///
/// 返回 `(Option<PreparedPcieRemoteDevice>, Option<polled_stream)>` —— 调用方
/// 拿到 polled_stream 后自行 spawn worker（worker 由调用方持 Task）。
pub async fn accept_and_handshake<L>(
    driver: impl pal_async::driver::Driver + Clone,
    listener: L,
    instance_id: guid::Guid,
    handshake_timeout: Duration,
) -> Option<(PreparedPcieRemoteDevice, PolledSocket<L::Socket>)>
where
    L: pal_async::socket::Listener + 'static,
    L::Socket: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin + Send + 'static,
{
    let polled_listener = match PolledSocket::new(&driver, listener) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(instance_id = %instance_id, error = %e,
                "pcie_remote: PolledSocket::new failed");
            return None;
        }
    };

    let mut ctx = CancelContext::new().with_timeout(handshake_timeout);
    let deadline = Instant::now() + handshake_timeout;

    let res = ctx
        .until_cancelled(async {
            let mut polled_listener = polled_listener;
            loop {
                if Instant::now() >= deadline {
                    return None;
                }
                let (stream, _addr) = match polled_listener.accept().await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(instance_id = %instance_id, error = %e,
                            "pcie_remote: accept failed; backing off");
                        backoff(&driver).await;
                        continue;
                    }
                };
                let polled = match PolledSocket::new(&driver, stream) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(instance_id = %instance_id, error = %e,
                            "pcie_remote: PolledSocket::new(stream) failed");
                        continue;
                    }
                };
                match handshake(polled, instance_id).await {
                    Ok((prep, polled)) => return Some((prep, polled)),
                    Err(e) => {
                        tracing::warn!(instance_id = %instance_id, error = %e,
                            "pcie_remote: handshake failed; listener kept open");
                        backoff(&driver).await;
                        continue;
                    }
                }
            }
        })
        .await
        .ok()
        .flatten();
    res
}

async fn backoff(driver: &(impl pal_async::driver::Driver + Clone)) {
    pal_async::timer::PolledTimer::new(driver)
        .sleep(Duration::from_millis(100))
        .await;
}

/// Tcp loopback spawner — 把 PORT/instance_id 列表挨个 spawn 入 prepared_map。
///
/// 返回 Vec<Task<()>>（每个 instance 一个 task：listener+handshake+worker），
/// 调用方持有 Vec 让 task 不被 drop。
pub async fn spawn_tcp_handshakes(
    driver: impl pal_async::driver::Driver + Clone + 'static,
    spawner: impl Spawn + Clone,
    instances: Vec<(guid::Guid, String, Duration)>,
    prepared: PreparedMap,
) -> Vec<pal_async::task::Task<()>> {
    let mut tasks = Vec::new();
    for (id, addr, timeout) in instances {
        let driver = driver.clone();
        let prepared = prepared.clone();
        let spawner_inner = spawner.clone();
        let task = spawner.spawn(format!("pcie_remote_listen_{id}"), async move {
            // bind TCP listener (sync)
            let listener = match std::net::TcpListener::bind(&addr) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(instance_id = %id, addr = %addr, error = %e,
                        "pcie_remote: TCP bind failed");
                    return;
                }
            };
            if let Err(e) = listener.set_nonblocking(true) {
                tracing::error!(instance_id = %id, error = %e,
                    "pcie_remote: set_nonblocking failed");
                return;
            }
            let Some((mut prep, polled_stream)) =
                accept_and_handshake(driver.clone(), listener, id, timeout).await
            else {
                tracing::warn!(instance_id = %id, "pcie_remote: handshake timeout; absent");
                return;
            };
            let inbox = prep.take_worker_inbox();
            let state = crate::state::SharedState::new(crate::state::DeviceState::Live);
            prepared.lock().insert(id, prep);
            // worker task
            let (_shutdown_tx, shutdown_rx) = mesh::channel::<()>();
            let worker = crate::worker::Worker::new(polled_stream, state, inbox);
            spawner_inner
                .spawn(format!("pcie_remote_worker_{id}"), worker.run(shutdown_rx))
                .detach();
            // _shutdown_tx 立即 drop → recv 返回 Err，worker 通过 transport EOF
            // 自然进 Lost；v1 不支持 graceful。
        });
        tasks.push(task);
    }
    tasks
}
```

> 这一步内 mesh API、PolledSocket API 与实际仓库版本可能微调；编译错误按报错最小修正。

### Step 6.3 — lib.rs

- [ ] Modify `vm/devices/pcie_remote_device/src/lib.rs`：追加：

```rust
pub mod handshake_spawn;
pub mod resolver;

pub use resolver::PcieRemoteTcpResolver;
pub use resolver::PcieRemoteVmbusResolver;
pub use resolver::PreparedMap;
```

### Step 6.4 — openvmm_core/Cargo.toml + dispatch.rs

- [ ] Modify `openvmm/openvmm_core/Cargo.toml`：dependencies 段添加：

```toml
pcie_remote_device.workspace = true
pcie_remote_resources.workspace = true
```

- [ ] Modify `openvmm/openvmm_core/src/worker/dispatch.rs`：找到 `try_join_all(cfg.pcie_devices` 调用前（约 L2090 附近）、且**在** vfio cfg-block 外面，添加：

```rust
            // pcie_remote TCP resolver（不在 #[cfg(target_os = "linux")] 内，
            // Windows OpenVMM 也要走）
            let pcie_remote_prepared: pcie_remote_device::PreparedMap =
                std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
            // 启动 handshake listener tasks（detach 让 task 在 worker 期持续）。
            // 注意：cfg.pcie_remote_tcp 字段需要在 openvmm_entry 端把 CLI 解析结果
            // 塞进 worker config；当前 step 仅注册 resolver，spawn 在 openvmm_entry。
            resolver.add_async_resolver::<
                vm_resource::kind::PciDeviceHandleKind,
                _,
                pcie_remote_resources::PcieRemoteTcpHandle,
                _,
            >(pcie_remote_device::PcieRemoteTcpResolver::new(pcie_remote_prepared.clone()));
            // 把 prepared_map 通过 worker config 暴露给 openvmm_entry 启动期 spawn。
            // 实施期：在 WorkerCfg / RunParams 里加一个 Arc<PreparedMap> 字段。
```

> 由于 RunParams / WorkerCfg 结构暴露需要跟 openvmm_entry 一起改，**v2 plan 把 prepared_map 暴露**做简化：在 dispatch.rs 内即 spawn handshake（用 worker 现成的 spawner）。

- [ ] **Step 6.4a (Simplification)**：把 spawn handshake 移到 dispatch.rs 内。dispatch.rs 已经持有 `spawner`/`driver`；从 worker cfg 拿 `pcie_remote_tcp_instances: Vec<(Guid, String, Duration)>`（新加字段），调 `handshake_spawn::spawn_tcp_handshakes(...)`，把返回的 tasks 存进 worker 自己持有的 `Vec<Task<()>>`（或 detach）。

### Step 6.5 — openvmm_entry/Cargo.toml + cli_args.rs + lib.rs

- [ ] Modify `openvmm/openvmm_entry/Cargo.toml`：加入：

```toml
pcie_remote_device.workspace = true
```

- [ ] Modify `openvmm/openvmm_entry/src/cli_args.rs`：把 long_help 里的 `0.0.0.0:48914` 注释改为 `127.0.0.1:48914`，并在 long_help 末尾加：

```
WARNING: v3.1 起仅允许 loopback 地址（127.0.0.1 / ::1）。WSL2 用户须知：
  WSL2 默认 localhostForwarding 把 loopback 暴露给 Windows host 任意用户进程。
  生产 / 多用户 host 请使用 OpenHCL + vsock 后端。
```

- [ ] Modify `openvmm/openvmm_entry/src/lib.rs`：把现有 `pcie_remote_resources::PcieRemoteHandle { ... }` 替换为：

```rust
        let socket_addr = cli_cfg
            .socket_addr
            .clone()
            .unwrap_or_else(|| pcie_remote_resources::DEFAULT_SOCKET_ADDR.to_string());
        if let Err(e) = pcie_remote_device::transport::check_tcp_loopback(&socket_addr) {
            // F-12: user-visible
            eprintln!(
                "pcie_remote {}: refusing non-loopback TCP addr ({e}); skipping device.",
                cli_cfg.port_name
            );
            tracing::warn!(error = %e, "pcie_remote: skip non-loopback");
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

### Step 6.6 — 构建 + 测试

- [ ] Run: `cargo check -p openvmm_entry -p openvmm_core 2>&1 | tail -15`
  Expected: 成功

- [ ] Run: `cargo build -p openvmm 2>&1 | tail -10`
  Expected: 成功（耗时较长）

### Step 6.7 — Commit

```bash
git add vm/devices/pcie_remote_device/ openvmm/
git commit -m "feat(pcie_remote): wire OpenVMM path D — resolver + handshake spawn + CLI

- resolver.rs: dynamic Tcp/Vmbus resolvers; prepared_map self-owned;
  fallback returns AbsentPcieDevice via .into() (no bail!)
- handshake_spawn.rs: PolledSocket-based accept loop with backoff;
  CancelContext for per-instance timeout; listener kept open on
  handshake failure
- dispatch.rs: add_async_resolver outside #[cfg(linux)] gate so
  Windows OpenVMM works too; tasks detached
- openvmm_entry: switch to PcieRemoteTcpHandle; enforce loopback;
  eprintln! makes silent skip user-visible
- cli_args.rs: example addresses updated to 127.0.0.1 + WSL2 warning

Phase 6 closes OpenVMM wiring end-to-end; no absent-stub regression.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 7 ─ OpenHCL 端 handle + CLI + takeover + resolver 注入

> 与 v1 基本一致；本 v2 仅同步术语和 API 名字。

**Files:**
- Modify: `openhcl/underhill_core/Cargo.toml`
- Modify: `openhcl/underhill_core/src/options.rs`
- Modify: `openhcl/underhill_core/src/dispatch/vtl2_settings_worker.rs`
- Modify: `openhcl/underhill_core/src/worker.rs`

### Step 7.1 — Cargo.toml

- [ ] Modify `openhcl/underhill_core/Cargo.toml`：加 `pcie_remote_device.workspace = true` 与 `pcie_remote_resources.workspace = true`。

### Step 7.2 — options.rs CLI

- [ ] Modify `openhcl/underhill_core/src/options.rs`：参照 v1 plan Step 7.1 的 `PcieRemoteCliConfig` 与 Options 字段。**额外**在解析期调用 `pcie_remote_device::transport::check_vsock_port(...)`，失败 → `eprintln!` + 跳过。

### Step 7.3 — vtl2_settings_worker.rs

- [ ] 与 v1 plan Step 7.2 一致：
  - `InitialControllers::new` 入口检查 `isolation.is_hardware_isolated()` → 静默过滤 takeover/CLI（参照 [worker.rs:2042](../../../openhcl/underhill_core/src/worker.rs#L2042) 取 isolation）
  - `is_restoring` → 同上
  - `create_storage_controllers_from_vtl2_settings` 内的 NVMe 循环：匹配 takeover 白名单 → 改派构造 `UhVpciDeviceConfig { resource: PcieRemoteVmbusHandle.into_resource() }`

### Step 7.4 — worker.rs handshake spawn + resolver 注册

- [ ] 在 underhill `worker.rs` 找到 `resolver.add_async_resolver` 既有点（约 L2287/L2324），追加：

```rust
    // pcie_remote
    let pcie_remote_prepared: pcie_remote_device::PreparedMap =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let pcie_remote_instances: Vec<_> = opt
        .pcie_remote_instance
        .iter()
        .chain(opt.pcie_remote_takeover.iter())
        .map(|c| {
            (
                c.instance_id,
                c.vsock_port,
                std::time::Duration::from_millis(c.handshake_timeout_ms as u64),
            )
        })
        .collect();
    let pcie_remote_tasks = pcie_remote_device::handshake_spawn::spawn_vsock_handshakes(
        driver.clone(),
        spawner.clone(),
        pcie_remote_instances,
        pcie_remote_prepared.clone(),
    )
    .await;
    // detach 让 task 不被 drop
    for t in pcie_remote_tasks {
        t.detach();
    }
    resolver.add_async_resolver::<
        vm_resource::kind::PciDeviceHandleKind,
        _,
        pcie_remote_resources::PcieRemoteVmbusHandle,
        _,
    >(pcie_remote_device::PcieRemoteVmbusResolver::new(pcie_remote_prepared));
```

并在 `handshake_spawn.rs` 加一个 `spawn_vsock_handshakes`：

```rust
pub async fn spawn_vsock_handshakes(
    driver: impl pal_async::driver::Driver + Clone + 'static,
    spawner: impl Spawn + Clone,
    instances: Vec<(guid::Guid, u32, Duration)>,
    prepared: PreparedMap,
) -> Vec<pal_async::task::Task<()>> {
    let mut tasks = Vec::new();
    for (id, port, timeout) in instances {
        let driver = driver.clone();
        let prepared = prepared.clone();
        let spawner_inner = spawner.clone();
        let task = spawner.spawn(format!("pcie_remote_vsock_{id}"), async move {
            let listener = match vmsocket::VmListener::bind(
                vmsocket::VmAddress::vsock_any(port),
            ) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(instance_id = %id, port, error = %e,
                        "pcie_remote: vsock bind failed");
                    return;
                }
            };
            let Some((mut prep, polled_stream)) =
                accept_and_handshake(driver.clone(), listener, id, timeout).await
            else {
                tracing::warn!(instance_id = %id, "pcie_remote vsock handshake timeout");
                return;
            };
            let inbox = prep.take_worker_inbox();
            let state = crate::state::SharedState::new(crate::state::DeviceState::Live);
            prepared.lock().insert(id, prep);
            let (_shutdown_tx, shutdown_rx) = mesh::channel::<()>();
            let worker = crate::worker::Worker::new(polled_stream, state, inbox);
            spawner_inner
                .spawn(format!("pcie_remote_vsock_worker_{id}"), worker.run(shutdown_rx))
                .detach();
        });
        tasks.push(task);
    }
    tasks
}
```

### Step 7.5 — 构建

- [ ] Run: `cargo check --target x86_64-unknown-linux-musl -p underhill_core 2>&1 | tail -20`
  Expected: 成功（首次会拉 musl 工具链，长）

### Step 7.6 — Commit

```bash
git add openhcl/underhill_core/ vm/devices/pcie_remote_device/
git commit -m "feat(pcie_remote): OpenHCL CLI + vtl2_settings_worker + resolver

- options.rs: --pcie-remote-instance / --pcie-remote-takeover with
  port blacklist check
- vtl2_settings_worker: CVM + servicing filter; takeover GUID re-routing
  in NVMe loop
- worker.rs: spawn_vsock_handshakes + add_async_resolver

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 8 ─ host SDK 示例 + setup.ps1 + Guide

**Files:**
- Create: `docs/superpowers/examples/pcie_remote_noop_host/Cargo.toml`
- Create: `docs/superpowers/examples/pcie_remote_noop_host/src/main.rs`
- Create: `docs/superpowers/scripts/setup-pcie-remote.ps1`
- Create: `Guide/src/reference/openhcl/devices/pcie_remote.md`
- Modify: `Cargo.toml`（workspace.exclude 已在 Phase 1 加）

### Step 8.1 — host noop stub（F-18：仅 bind 127.0.0.1）

- [ ] Create `docs/superpowers/examples/pcie_remote_noop_host/Cargo.toml`：

```toml
[package]
name = "pcie_remote_noop_host"
version = "0.1.0"
edition = "2024"

[dependencies]
pcie_remote_protocol = { path = "../../../../vm/devices/pcie_remote_protocol" }
prost = "0.13"
anyhow = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "io-util", "net"] }
futures = "0.3"
tracing = "0.1"
tracing-subscriber = "0.3"
```

- [ ] Create `docs/superpowers/examples/pcie_remote_noop_host/src/main.rs`：

```rust
//! Minimal host stub. Listens 127.0.0.1:48914, replies HelloAck with a
//! noop DeviceDescribe; any later MMIO read returns 0.

use anyhow::Result;
use futures::io::AsyncReadExt;
use futures::io::AsyncWriteExt;
use pcie_remote_protocol::BarInfo;
use pcie_remote_protocol::DeviceDescribe;
use pcie_remote_protocol::HelloAck;
use pcie_remote_protocol::MmioReadResult;
use pcie_remote_protocol::ToHost;
use pcie_remote_protocol::ToOpenhcl;
use pcie_remote_protocol::bar_info::Kind;
use pcie_remote_protocol::codec;
use pcie_remote_protocol::to_host::Body as HostBody;
use pcie_remote_protocol::to_openhcl::Body as OpenhclBody;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::compat::TokioAsyncWriteCompatExt;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let addr: SocketAddr = "127.0.0.1:48914".parse()?;
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("listening on {addr}");
    loop {
        let (stream, peer) = listener.accept().await?;
        tracing::info!("connected: {peer}");
        tokio::spawn(async move {
            if let Err(e) = serve(stream).await {
                tracing::warn!(error = %e, "session ended");
            }
        });
    }
}

async fn serve(stream: tokio::net::TcpStream) -> Result<()> {
    let (rd, wr) = stream.into_split();
    let mut rd = rd.compat();
    let mut wr = wr.compat_write();
    let hello: pcie_remote_protocol::Hello = codec::read_frame(&mut rd).await?;
    tracing::info!(magic = format_args!("{:#x}", hello.magic), "received Hello");
    let ack = HelloAck {
        ok: true,
        reason: String::new(),
        device: Some(DeviceDescribe {
            vendor_id: 0x1414,
            device_id: 0xc0de,
            class_code: 0x010802,
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
        }),
    };
    codec::write_frame(&mut wr, &ack).await?;
    loop {
        let req: ToHost = codec::read_frame(&mut rd).await?;
        match req.body {
            Some(HostBody::MmioRead(_)) => {
                let resp = ToOpenhcl {
                    seq: req.seq,
                    body: Some(OpenhclBody::MmioReadResult(MmioReadResult { value: 0 })),
                };
                codec::write_frame(&mut wr, &resp).await?;
            }
            _ => {}
        }
    }
}
```

> tokio + tokio_util::compat 是 host SDK 唯一可接受的"非仓库依赖"。

### Step 8.2 — setup.ps1

- [ ] Create `docs/superpowers/scripts/setup-pcie-remote.ps1`：spec §4.3 setup.ps1 模板的完整版（已在 spec 中给出最终代码）。

### Step 8.3 — Guide 文档

- [ ] Create `Guide/src/reference/openhcl/devices/pcie_remote.md`：

```markdown
# pcie_remote 实验设备

> 注：实验/调试用途，不上生产。CVM 下硬禁。

## 4 种部署形态

| 形态 | 谁是 host | 通道 | CLI |
|------|---------|------|-----|
| A. OpenVMM 启 OpenHCL（开发） | OpenVMM 进程 | TCP loopback | OpenVMM `--pcie-remote ...,socket=127.0.0.1:48914` |
| B. OpenHCL + 自建 IGVM | Hyper-V vmwp | vsock | OpenHCL cmdline `--pcie-remote-instance <guid>:<port>` |
| C. OpenHCL + 生产 Hyper-V（占位 NVMe） | Hyper-V vmwp | vsock | `Add-VMNvmeController` + `--pcie-remote-takeover <nvme_guid>:<port>` |
| D. OpenVMM-only | OpenVMM 进程 | TCP loopback | 同 A |

## 安全

- service GUID ACL 默认仅 Admin/SYSTEM；运行 `scripts/setup-pcie-remote.ps1` 完成注册。
- **不提供 IOMMU 等价隔离**；host 进程必须可信。
- **CVM 硬禁**：isolation=SNP/TDX/VBS 时设备被静默过滤。
- WSL2 + OpenVMM TCP 形态：loopback 会通过 localhostForwarding 暴露给 Windows host 任意用户进程。生产请用 vsock。
```

### Step 8.4 — Commit

```bash
git add docs/superpowers/examples/ docs/superpowers/scripts/ Guide/src/reference/openhcl/devices/
git commit -m "docs(pcie_remote): host SDK example + setup.ps1 + Guide

- examples/pcie_remote_noop_host: minimal tokio-based stub (bind 127.0.0.1 only)
- setup-pcie-remote.ps1: service GUID + Admin/SYSTEM ACL (SDDL applied)
- Guide doc: 4 deployment forms + security caveats

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase 9 ─ 本机 VM 实验（OpenVMM + Linux guest）

> 这是用户的"本机起虚拟机做实验"诉求。OpenVMM 路径不需要 IGVM 构建，验证最快。

### Step 9.1 — 准备 minimal Linux 镜像

> **2026-05-30 实际结果**：未走 wget alpine 路径；最终用 `--linux-direct` +
> 仓库 OpenVMM-friendly kernel（来自 cargo xflowey 拉取的 fixture）跑通
> 真 KVM end-to-end。详见 [../SESSION_LOG.md](../SESSION_LOG.md) 真 KVM
> 端到端证据段。下面的探索性占位代码保留作历史。

- [x] Run: 检查仓库 fixtures。`ls petri/test_artifacts/ 2>/dev/null` 或下载一个 ~20MB 的 alpine kernel + initrd。
  - 如果 fixture 缺，从公网取一个 alpine-mini-rootfs（无人值守可接受）：

```bash
mkdir -p /tmp/pcie_remote_exp
cd /tmp/pcie_remote_exp
wget -nv https://github.com/alpinelinux/aports/raw/master/main/linux-virt/...   # 视具体可下载源 — 占位，未实际使用
# 或使用 OpenVMM 仓库 README 推荐的 OpenVMM-friendly linux image
```

> 实施期若发现 fixture 难取，转向用 OpenVMM `--linux-direct` 走仓库 docs 推荐的 kernel；或退回到只起 host stub + 用 unit test 模拟设备 enumerate（接受 Phase 9 退化）。

### Step 9.2 — 启动 host stub

- [ ] Run: `cargo run -p pcie_remote_noop_host 2>&1 &`
- [ ] Verify: `ss -tnl | grep 48914`，确认仅监听 127.0.0.1

### Step 9.3 — 启动 OpenVMM + Linux guest

- [ ] Run: 具体命令按 Step 9.1 决定。最小骨架：

```bash
target/debug/openvmm \
  --processors 2 --memory 1G \
  --linux-direct \
  --kernel /tmp/pcie_remote_exp/vmlinuz \
  --initrd /tmp/pcie_remote_exp/initrd \
  --pcie-remote rc0rp0,socket=127.0.0.1:48914 \
  > /tmp/pcie_remote_exp/vm.log 2>&1
```

- [ ] Verify: guest dmesg / vm.log 中含 `pci 0000:00:..` 行；host stub log 含 "connected" 与 "received Hello"。

### Step 9.4 — Commit 实验结果

- [ ] Append results to `docs/superpowers/SESSION_LOG.md`，commit：

```bash
git add docs/superpowers/SESSION_LOG.md
git commit -m "docs: Phase 9 — OpenVMM + linux guest experiment results

- noop host stub serves HelloAck
- guest enumerates a PCIe device exposing vendor=1414 device=c0de
- $N MMIO read requests handled by host stub during boot
"
```

---

## Phase 10 ─ OpenHCL IGVM build + Windows guest 验证（可选）

> 仅在 Phase 9 已成功 + 时间充裕时执行。否则把 IGVM build 推到后续。

### Step 10.1 — IGVM build

- [ ] Run: `cargo xflowey build-igvm x64 --release 2>&1 | tail -20`
  Expected: 输出 IGVM 文件路径

### Step 10.2 — 启动 OpenHCL + Linux guest + takeover

按 spec §4.3 setup.ps1 注册一个 service GUID，然后启动 OpenVMM 加 `--hv --vtl2 --igvm <path> --pcie-remote-takeover <guid>:50000`。验证 vsock handshake 完成。

### Step 10.3 — Commit

```bash
git add docs/superpowers/SESSION_LOG.md
git commit -m "docs: Phase 10 — OpenHCL IGVM + vsock handshake validated"
```

---

## 任务分级（用于无人值守模式）

| 级别 | Phase | 必须无人值守完成 | 状态 |
|------|-------|------------------|------|
| **MUST** | 1, 2, 3, 4, 5 | crate 全部单测通过 | ✅ 完成 (28+7+10 = 45 单测，3 集成测试通过) |
| **MUST** | 6 | OpenVMM 能编 + 启动（不强求成功 enumerate guest） | ✅ 完成（真 KVM e2e 通过）|
| **SHOULD** | 7 | OpenHCL `cargo check` 通过 | ✅ 完成 |
| **SHOULD** | 8 | host SDK 能 `cargo build`；setup.ps1 + Guide 文档落盘 | ✅ 完成（TCP + vsock 两个 bin）|
| **NICE** | 9 | OpenVMM + linux guest 真实跑通设备枚举 | ✅ 完成（真 KVM e2e）|
| **NICE** | 10 | IGVM build + vsock 真实跑通 | ✅ **完成（真 Hyper-V vsock handshake ok, worker spawned）** |

无人值守模式：MUST 卡住即停止并写 SESSION_LOG；SHOULD/NICE 任何一步失败也写日志，但继续尝试下一 phase。

**实际执行额外达成（计划外）：**
- K-1 到 K-19 全部 spec gap 已实现并通过测试
- Path C Hyper-V 创建陷阱（`-GuestStateIsolationType OpenHCL`）已定位 + 文档化（[../HYPERV_RUNBOOK.md](../HYPERV_RUNBOOK.md)）
- 诊断工具 `vmrs_log_scanner` + `vmrs_log_scanner_win` 已实现并 commit
- Hyper-V 自定义 VM 重建脚本 `docs/superpowers/scripts/hyperv/create_openhcl_vm_correct.ps1`
