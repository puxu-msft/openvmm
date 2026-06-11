# W1 — `vfio_user_device` client wire + AF_UNIX + 握手

> **执行方式：inline（控制者本人执行，非 subagent-driven）。** 与 W0.5 同——单 commit 与 subagent 默认每-task-commit 冲突；inline 让控制者掌握每条 git 命令，无裸 commit 吞别会话 DECISIONS.md 之险。
>
> **Plan type: 新功能（TDD）。** client 握手是真新代码。wire crate 纯决策**先写测试后实现**（RED→GREEN）；client framing/connect 用 socketpair loopback 集成测验证。
>
> **⚠️ 单 commit 纪律（用户 2026-06-11）**：整个 W1 合成**一个 commit**。见 [[commit-granularity-coarse-not-per-step]]。

**Goal:** 建 underhill 侧 `vfio_user_device` client crate 的第一刀——client 能 connect AF_UNIX socket、发 VERSION command、收 server VERSION reply、协商验证（major 相等 + minor ≤ 提议），loopback 单测（Linux 可跑，无需 VTL/underhill）对接真 `vfio_user_transport::server_handshake`。

**Architecture:** client 握手的**纯决策**（构造 VERSION command payload、验证 server reply）加到 sans-IO `vfio_user_wire`（server/client 共享）；client 的 **IO**（connect + 收发）放新 crate `usnvmemu/crates/vfio_user_device/`。W1 framing 用**纯 `UnixStream` read/write**（握手无 fd → 无 recvmsg/SCM_RIGHTS/unsafe，保持 `#![deny(unsafe_code)]` 干净）；fd-capable framing 留 W3。同步阻塞（镜像 transport 单测范式），async 留 W6。

**Tech Stack:** Rust 2024，rustc 1.95；`vfio_user_wire`(path) + zerocopy 0.8 + anyhow 1.0 + thiserror 1.0 + tracing 0.1；dev-dep `vfio_user_transport`(path) 跑 loopback 真 server。**无 nix/libc/unsafe**（W1 无 fd 路径）。

**Spec 来源：** `docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md` §6 W1 + §4 组件。

**前置 commit：** `48a4fd17`（W0.5）。

**回退指引：** W1 全程未 commit；任一 Stage 失败 `git checkout -- <file>`（已 rm 的用 git checkout 恢复）；新建 crate 目录乱了 `rm -rf usnvmemu/crates/vfio_user_device`。**不碰** `usnvmemu/docs/DECISIONS.md`（别会话）。

---

## Baseline（起手必跑）

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire && cargo test --lib 2>&1 | tail -2
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && cargo test --lib 2>&1 | tail -2
```
预期：wire **39**、transport **68**（实测记真值，后续不退化）。

---

## File Structure

新建：
- `usnvmemu/crates/vfio_user_device/Cargo.toml`
- `usnvmemu/crates/vfio_user_device/src/lib.rs`
- `usnvmemu/crates/vfio_user_device/src/framing.rs` —— client 纯 read/write（无 fd，无 unsafe）
- `usnvmemu/crates/vfio_user_device/src/client.rs` —— `VfioUserClient::connect` + `handshake`
- `usnvmemu/crates/vfio_user_device/tests/loopback_handshake.rs` —— 真 server ⇄ client 集成测

修改：
- `usnvmemu/crates/vfio_user_wire/src/proto.rs` —— `ProtoError` 加 `VersionMismatch` 变体
- `usnvmemu/crates/vfio_user_wire/src/handshake.rs` —— 加 `build_version_command_payload` + `verify_server_version_reply` + `NegotiatedClient` + `CLIENT_CAPS_JSON`
- `Cargo.toml`（workspace 根）—— exclude 加 `vfio_user_device`

---

## Stage 1 — wire crate client 纯决策（RED→GREEN + 单测）

**Files:** `vfio_user_wire/src/proto.rs`、`vfio_user_wire/src/handshake.rs`

### Step 1.1: ProtoError 加 VersionMismatch 变体

在 `usnvmemu/crates/vfio_user_wire/src/proto.rs` 的 `pub enum ProtoError` 末尾（`BadJson(String),` 之后、`}` 之前）加：

```rust
    /// VERSION 协商失败：server reply 的 (major, minor) 与 client 提议不兼容
    /// （major 必须相等；server.minor 不得高于 client 提议）。
    #[error(
        "version mismatch: server replied (major={server_major}, minor={server_minor}), \
         client proposed (major={proposed_major}, minor={proposed_minor})"
    )]
    VersionMismatch {
        /// client 提议的 major。
        proposed_major: u16,
        /// client 提议的 minor。
        proposed_minor: u16,
        /// server reply 的 major。
        server_major: u16,
        /// server reply 的 minor。
        server_minor: u16,
    },
```

### Step 1.2: handshake.rs 加 client 纯决策（先写测试）

在 `usnvmemu/crates/vfio_user_wire/src/handshake.rs` 的 `#[cfg(test)] mod tests` **之前**加生产代码，在 tests 内加测试。

生产代码（加在 `build_version_reply_payload` 之后）：

```rust
/// client advertise 的最简 capabilities JSON。
///
/// client 不发 server-语义字段（`max_dma_maps` / `pgsizes` 是接收方=server 的
/// 能力）；`max_data_xfer_size` / `max_msg_fds` 只对 client→server DMA 方向
/// （W3+）有意义，W1 握手省略。空 capabilities 对端用 `parse_caps_blob` 接受。
pub const CLIENT_CAPS_JSON: &str = "{\"capabilities\":{}}";

/// client 侧握手结果摘要（镜像 [`Negotiated`]，但字段语义是**对端 = server**）。
#[derive(Debug, Clone)]
pub struct NegotiatedClient {
    /// server reply 的 protocol major（与 client 提议必须相等）。
    pub server_major: u16,
    /// server reply 的 protocol minor（spec：≤ client 提议）。
    pub server_minor: u16,
    /// server reply 的 capabilities JSON 原文（已 UTF-8 解码 / NUL 去尾）。
    pub server_caps_json: String,
}

/// 构造 client→server 的 VERSION **command** payload：
/// `VersionPayload(4) + caps_json + NUL`。
///
/// 对称于 [`build_version_reply_payload`]，但 client **自填** `(major, minor)`
/// 而非 echo/negotiate（client 是发起协商的一方）。caller 负责包 Header
/// （`Command::Version` + `Header::command`）并写到 socket。
pub fn build_version_command_payload(major: u16, minor: u16, client_caps: &[u8]) -> Vec<u8> {
    let payload_len = 4 + client_caps.len() + 1;
    let mut payload = Vec::with_capacity(payload_len);
    let ver = VersionPayload { major, minor };
    payload.extend_from_slice(ver.as_bytes());
    payload.extend_from_slice(client_caps);
    payload.push(0); // NUL
    payload
}

/// client 验证 server 的 VERSION reply：
/// - `reply.major == proposed_major`（major 必须相等）
/// - `reply.minor <= proposed_minor`（spec：server.minor ≤ client 提议；
///   server 回更高 minor 非法）
///
/// **注意方向**：这不是 [`negotiate_minor`]（那是 server 决定回什么）。client
/// 是**校验对端没回过头**——`negotiate_minor` 当验证会漏掉"server 回更高 minor"
/// 的非法情形。
pub fn verify_server_version_reply(
    proposed_major: u16,
    proposed_minor: u16,
    reply: &VersionPayload,
) -> Result<(), ProtoError> {
    if reply.major != proposed_major || reply.minor > proposed_minor {
        return Err(ProtoError::VersionMismatch {
            proposed_major,
            proposed_minor,
            server_major: reply.major,
            server_minor: reply.minor,
        });
    }
    Ok(())
}
```

测试（加在 `mod tests` 内，`use super::*;` 已在）：

```rust
    /// client VERSION command payload 布局：4B VersionPayload + caps + NUL。
    #[test]
    fn build_version_command_payload_has_expected_layout() {
        let caps = b"{\"capabilities\":{}}";
        let payload = build_version_command_payload(PROTOCOL_MAJOR, PROTOCOL_MINOR, caps);
        assert_eq!(payload.len(), 4 + caps.len() + 1);
        assert_eq!(*payload.last().unwrap(), 0);
        assert_eq!(&payload[0..2], PROTOCOL_MAJOR.to_le_bytes());
        assert_eq!(&payload[2..4], PROTOCOL_MINOR.to_le_bytes());
        assert_eq!(&payload[4..4 + caps.len()], caps);
    }

    /// server reply 与提议完全一致 → 通过。
    #[test]
    fn verify_server_reply_exact_match_ok() {
        let reply = VersionPayload {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
        };
        assert!(verify_server_version_reply(PROTOCOL_MAJOR, PROTOCOL_MINOR, &reply).is_ok());
    }

    /// server 回更低 minor（合法协商降级）→ 通过。
    #[test]
    fn verify_server_reply_lower_minor_ok() {
        let reply = VersionPayload {
            major: PROTOCOL_MAJOR,
            minor: 0,
        };
        assert!(verify_server_version_reply(PROTOCOL_MAJOR, PROTOCOL_MINOR, &reply).is_ok());
    }

    /// server 回更高 minor（非法：spec 要求 server.minor ≤ client 提议）→ 拒。
    #[test]
    fn verify_server_reply_higher_minor_rejected() {
        let reply = VersionPayload {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR + 1,
        };
        assert!(matches!(
            verify_server_version_reply(PROTOCOL_MAJOR, PROTOCOL_MINOR, &reply),
            Err(ProtoError::VersionMismatch { .. })
        ));
    }

    /// server 回不同 major → 拒。
    #[test]
    fn verify_server_reply_major_mismatch_rejected() {
        let reply = VersionPayload {
            major: PROTOCOL_MAJOR + 99,
            minor: PROTOCOL_MINOR,
        };
        assert!(matches!(
            verify_server_version_reply(PROTOCOL_MAJOR, PROTOCOL_MINOR, &reply),
            Err(ProtoError::VersionMismatch { .. })
        ));
    }

    /// CLIENT_CAPS_JSON 是合法 UTF-8 且不含 server-语义字段。
    #[test]
    fn client_caps_json_is_minimal() {
        assert!(CLIENT_CAPS_JSON.is_ascii());
        assert!(!CLIENT_CAPS_JSON.contains("max_dma_maps"));
        assert!(!CLIENT_CAPS_JSON.contains("pgsizes"));
    }
```

注意：`VersionPayload` / `PROTOCOL_MAJOR` / `PROTOCOL_MINOR` 已在 handshake.rs 顶部 `use crate::proto::...` 导入（W0-5 抽 pure 决策时已引）；若 `VersionPayload` 未导入则补 `use crate::proto::VersionPayload;`。

### Step 1.3: 验证 wire crate

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire && \
  cargo test --lib 2>&1 | tail -3 && \
  cargo clippy --lib --tests -- -D warnings 2>&1 | tail -2
```
预期：39 + 6 = **45 passed** + clippy clean。

---

## Stage 2 — 新建 `vfio_user_device` crate 骨架

**Files:** `vfio_user_device/Cargo.toml`、`vfio_user_device/src/lib.rs`、root `Cargo.toml`

### Step 2.1: Cargo.toml + .gitignore

创建 `usnvmemu/crates/vfio_user_device/Cargo.toml`：

```toml
[package]
name = "vfio_user_device"
version = "0.1.0"
edition = "2024"
license = "MIT"
description = "vfio-user client (underhill 侧)：connect AF_UNIX + VERSION 握手 + (后续) PCIe 呈现 / DMA_MAP / MSI-X。与 vfio_user_transport (firmware server) 对接，复用 vfio_user_wire sans-IO 协议层。"

[dependencies]
# 与其它 usnvmemu crate 对齐版本；本 crate 在 workspace exclude（不用 workspace.dependencies）。
vfio_user_wire = { path = "../vfio_user_wire" }
zerocopy = { version = "0.8", features = ["derive"] }
anyhow = "1.0"
thiserror = "1.0"
tracing = "0.1"

[dev-dependencies]
# loopback 单测对接真 server_handshake。
vfio_user_transport = { path = "../vfio_user_transport" }
```

**[architect D-1 BLOCKING] 同步创建 `.gitignore`**（镜像所有其它 usnvmemu crate；exclude crate 构建产物落自己 target/，不加 .gitignore 则 `git add 目录` 吞数百 MB target/ + Cargo.lock，W0-1 踩过）：

创建 `usnvmemu/crates/vfio_user_device/.gitignore`：
```
target/
Cargo.lock
```

### Step 2.2: lib.rs

创建 `usnvmemu/crates/vfio_user_device/src/lib.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `vfio_user_device` —— vfio-user **client**（underhill 侧）。
//!
//! 与 `vfio_user_transport`（firmware server）对接：connect AF_UNIX socket、
//! VERSION 握手协商，后续（W2+）PCIe 呈现 / REGION_RW / DMA_MAP / MSI-X。
//! 复用 `vfio_user_wire` sans-IO 协议层（编解码 + 握手纯决策）。
//!
//! **W1 范围**：client wire + AF_UNIX + 握手（loopback 单测）。同步阻塞收发
//! （握手无 fd → 纯 `UnixStream` read/write，无 recvmsg/SCM_RIGHTS/unsafe）；
//! fd-capable framing 留 W3，async 化留 W6。

#![deny(unsafe_code)]

mod client;
mod framing;

pub use client::VfioUserClient;
pub use vfio_user_wire::handshake::NegotiatedClient;
```

### Step 2.3: 注册 workspace exclude + stub 编译

root `Cargo.toml` 的 `exclude` 列表（`vfio_user_transport` 行附近）加：
```toml
  "usnvmemu/crates/vfio_user_device",
```

先建 `framing.rs` 空 stub + `client.rs` 占位（`pub struct VfioUserClient;`，Stage 4.1 覆盖）让骨架编译（**[architect G-1] 确定性指令，非"若如此"**）：
```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_device/src && \
  : > framing.rs && \
  printf '// Copyright (c) Microsoft Corporation.\n// Licensed under the MIT License.\n\n/// W1 占位，Stage 4.1 覆盖为真实现。\npub struct VfioUserClient;\n' > client.rs
```
**Stage 2 验 Cargo.toml 解析**：
```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_device && cargo metadata --no-deps --format-version 1 2>&1 | tail -2
```
预期：输出 JSON（无 manifest 错误）。framing.rs 空 + lib.rs `mod framing;` 暂时无内容 OK（mod 空文件合法）。

---

## Stage 3 — client framing（纯 read/write，无 fd，无 unsafe）

**Files:** `vfio_user_device/src/framing.rs`

W1 握手无 fd → 用 `std::os::unix::net::UnixStream` 的 `Read`/`Write`（`read_exact`/`write_all`）收发 header+payload，无 recvmsg/SCM_RIGHTS/unsafe。

### Step 3.1: 写 framing.rs

创建 `usnvmemu/crates/vfio_user_device/src/framing.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! client 端线消息收发（W1：纯 `UnixStream` read/write，无 fd）。
//!
//! 握手不携带 fd，故 W1 用 `Read::read_exact` / `Write::write_all` 即可，无需
//! recvmsg/SCM_RIGHTS（那是 W3 DMA_MAP fd 传递才需要）。保持本 crate
//! `#![deny(unsafe_code)]` 干净。
//!
//! 字节布局与 server 端 `vfio_user_transport::framing` 一致（同一份
//! `vfio_user_wire::proto` 编解码），仅收发原语不同（client connect / 无 fd）。

use anyhow::Context as _;
use std::io::Read;
use std::io::Write;
use std::os::unix::net::UnixStream;
use vfio_user_wire::framing::WireMessage;
use vfio_user_wire::proto::HEADER_LEN;
use vfio_user_wire::proto::Header;
use vfio_user_wire::proto::decode_header;
use zerocopy::IntoBytes; // Header::as_bytes() 需要（写死，非"若报错再加"）

/// 写一帧（header + payload，无 fd）到 stream。
pub(crate) fn write_message(
    stream: &mut UnixStream,
    header: &Header,
    payload: &[u8],
) -> anyhow::Result<()> {
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.extend_from_slice(header.as_bytes());
    buf.extend_from_slice(payload);
    stream.write_all(&buf).context("write_message: write_all")?;
    stream.flush().context("write_message: flush")?;
    Ok(())
}

/// 读一帧（header + payload，无 fd）。先读定长 header，据 `msg_size` 读 payload。
pub(crate) fn read_message(stream: &mut UnixStream) -> anyhow::Result<WireMessage> {
    let mut hdr_buf = [0u8; HEADER_LEN];
    stream
        .read_exact(&mut hdr_buf)
        .context("read_message: read header")?;
    let header = decode_header(&hdr_buf).context("read_message: decode_header")?;
    // **packed 字段先 copy 到本地**（Header 是 #[repr(C,packed)]，deny(unsafe_code)
    // 下不能直接借用/读字段，E0793）。decode_header 已校验 msg_size ∈ [HEADER_LEN,
    // MAX_MSG_SIZE]，checked_sub 是双保险。
    let msg_size = header.msg_size as usize;
    let payload_len = msg_size
        .checked_sub(HEADER_LEN)
        .context("read_message: msg_size < HEADER_LEN")?;
    let mut payload = vec![0u8; payload_len];
    stream
        .read_exact(&mut payload)
        .context("read_message: read payload")?;
    Ok(WireMessage { header, payload })
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfio_user_wire::proto::Command;

    /// header + payload roundtrip（socketpair，无 fd）。
    #[test]
    fn roundtrip_header_and_payload() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let payload = b"hello-vfio-user".to_vec();
        let hdr = Header::command(7, Command::Version, payload.len() as u32);
        write_message(&mut a, &hdr, &payload).unwrap();
        let got = read_message(&mut b).unwrap();
        let msg_id = got.header.msg_id; // packed 字段先 copy
        assert_eq!(msg_id, 7);
        assert_eq!(got.payload, payload);
    }

    /// 空 payload roundtrip。
    #[test]
    fn roundtrip_header_only() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let hdr = Header::command(1, Command::Version, 0);
        write_message(&mut a, &hdr, &[]).unwrap();
        let got = read_message(&mut b).unwrap();
        let msg_id = got.header.msg_id; // packed 字段先 copy
        assert_eq!(msg_id, 1);
        assert!(got.payload.is_empty());
    }
}
```

注意：
- `Header::as_bytes()` 需 `zerocopy::IntoBytes`——已在顶部写死 `use zerocopy::IntoBytes;`。
- `Header` 是 `#[repr(C, packed)]`——读 `header.msg_size` / `got.header.msg_id` 等字段**必须先 copy 到本地 `let`**（`deny(unsafe_code)` 下直接读 packed 字段是 E0793 编译错；transport/proto 自家测试全程这么做）。
- `decode_header` 返回 `Result<Header, ProtoError>`；`Header::command(msg_id, cmd, payload_len)` 现成构造器。`Header.msg_size` 字段名已确认（proto.rs struct Header）。

### Step 3.2: 验证 framing

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_device && cargo test framing 2>&1 | tail -4
```
预期：2 passed（roundtrip 两个）。**注意**：此时 client.rs 仍空，lib.rs `pub use client::VfioUserClient` 会编译失败 → cargo test 整体可能因 client.rs 报错。若如此，先在 client.rs 放最小 stub（`pub struct VfioUserClient;`）让编译过，Stage 4 再填实现。

---

## Stage 4 — client connect + handshake + loopback 集成测

**Files:** `vfio_user_device/src/client.rs`、`vfio_user_device/tests/loopback_handshake.rs`

### Step 4.1: 写 client.rs

创建 `usnvmemu/crates/vfio_user_device/src/client.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! vfio-user client：connect AF_UNIX + VERSION 握手。

use crate::framing::read_message;
use crate::framing::write_message;
use anyhow::Context as _;
use anyhow::anyhow;
use std::os::unix::net::UnixStream;
use std::path::Path;
use vfio_user_wire::handshake::CLIENT_CAPS_JSON;
use vfio_user_wire::handshake::NegotiatedClient;
use vfio_user_wire::handshake::build_version_command_payload;
use vfio_user_wire::handshake::parse_caps_blob;
use vfio_user_wire::handshake::verify_server_version_reply;
use vfio_user_wire::proto::Command;
use vfio_user_wire::proto::Header;
use vfio_user_wire::proto::PROTOCOL_MAJOR;
use vfio_user_wire::proto::PROTOCOL_MINOR;
use vfio_user_wire::proto::VersionPayload;
use vfio_user_wire::proto::decode_payload;

/// vfio-user client 连接。W1 持一条同步 `UnixStream`，做 VERSION 握手。
pub struct VfioUserClient {
    stream: UnixStream,
}

impl VfioUserClient {
    /// connect 到 server 的 AF_UNIX socket 路径。
    pub fn connect(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(path.as_ref())
            .with_context(|| format!("connect vfio-user socket {:?}", path.as_ref()))?;
        Ok(Self { stream })
    }

    /// 从已建立的 `UnixStream` 构造（loopback 单测 / 已 connect 的场景）。
    pub fn from_stream(stream: UnixStream) -> Self {
        Self { stream }
    }

    /// 执行 VERSION 握手：发 client VERSION command → 收 server reply → 验证协商。
    ///
    /// client 提议 `(PROTOCOL_MAJOR, PROTOCOL_MINOR)`；server 回
    /// `(PROTOCOL_MAJOR, min(client_minor, server_minor))`；client 验
    /// `reply.major == 提议` 且 `reply.minor <= 提议`（见
    /// [`verify_server_version_reply`]）。
    pub fn handshake(&mut self) -> anyhow::Result<NegotiatedClient> {
        // 1. 发 VERSION command。
        let payload =
            build_version_command_payload(PROTOCOL_MAJOR, PROTOCOL_MINOR, CLIENT_CAPS_JSON.as_bytes());
        let msg_id = 1u16;
        let hdr = Header::command(msg_id, Command::Version, payload.len() as u32);
        write_message(&mut self.stream, &hdr, &payload).context("send VERSION command")?;

        // 2. 收 server reply。
        let reply = read_message(&mut self.stream).context("recv VERSION reply")?;

        // 3. reply 帧校验。**packed 字段先 copy 到本地**（Header #[repr(C,packed)]，
        //    deny(unsafe_code) 下不能直接读字段，E0793）。flags() 返回 by-value，OK。
        let flags = reply.header.flags();
        let reply_cmd = reply.header.cmd;
        let reply_error_no = reply.header.error_no;
        let reply_msg_id = reply.header.msg_id;
        if !flags.is_reply() {
            return Err(anyhow!("server VERSION reply 不是 REPLY 帧"));
        }
        if flags.is_error() {
            return Err(anyhow!(
                "server VERSION reply 是 error（error_no={reply_error_no}）"
            ));
        }
        if reply_cmd != Command::Version as u16 {
            return Err(anyhow!("server VERSION reply cmd 不匹配：got {reply_cmd}"));
        }
        if reply_msg_id != msg_id {
            return Err(anyhow!(
                "server VERSION reply msg_id 不匹配：sent {msg_id}, got {reply_msg_id}"
            ));
        }

        // 4. 解 VersionPayload + 验证协商。
        if reply.payload.len() < core::mem::size_of::<VersionPayload>() {
            return Err(anyhow!("server VERSION reply payload 过短"));
        }
        let ver: VersionPayload =
            decode_payload(&reply.payload[..core::mem::size_of::<VersionPayload>()])
                .context("decode server VersionPayload")?;
        verify_server_version_reply(PROTOCOL_MAJOR, PROTOCOL_MINOR, &ver)
            .context("verify server VERSION reply")?;

        // 5. 解 server caps（NUL 截断 + UTF-8）。
        let server_caps_json =
            parse_caps_blob(&reply.payload[core::mem::size_of::<VersionPayload>()..])
                .context("parse server caps")?;

        Ok(NegotiatedClient {
            server_major: ver.major,
            server_minor: ver.minor,
            server_caps_json,
        })
    }
}
```

注意：
- `Header` 是 `#[repr(C, packed)]`——读 `cmd`/`error_no`/`msg_id`/`msg_size` 字段**必须先 copy 到本地 `let`**（`deny(unsafe_code)` 下直接读 E0793）。`flags()` 返回 `HeaderFlags` by-value，可直接用。已在代码里 copy。
- `flags()` / `error_no` / `cmd` / `msg_id` 字段/方法名已确认（proto.rs struct Header + impl）；`HeaderFlags::is_reply()` / `is_error()` 确认存在（proto.rs L159/L167）。
- `decode_payload::<VersionPayload>` 现成（server handshake.rs 在用）。

### Step 4.1b: client.rs 加 error-path 单测（architect E-1）

handshake 的 4 条帧校验分支（非 REPLY / is_error / cmd / msg_id）零集成覆盖。在 client.rs 末尾 `#[cfg(test)] mod tests` 加（socketpair 对端手写 error reply 帧，不需真 server）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::write_message;
    use std::thread;
    use vfio_user_wire::proto::HeaderFlags;

    /// server 回 error reply（flags F_ERROR）→ client handshake 报错。
    #[test]
    fn handshake_rejects_error_reply() {
        let (server_end, client_end) = UnixStream::pair().unwrap();
        let srv = thread::spawn(move || {
            let mut s = server_end;
            // 先读 client 的 VERSION command（丢弃），再回 error reply。
            let _ = read_message(&mut s).unwrap();
            let hdr = Header::reply_err(1, Command::Version, libc_eproto());
            write_message(&mut s, &hdr, &[]).unwrap();
        });
        let mut client = VfioUserClient::from_stream(client_end);
        let err = client.handshake().unwrap_err();
        assert!(
            format!("{err:#}").contains("error"),
            "应报 error reply：{err:#}"
        );
        srv.join().unwrap();
    }

    /// helper：EPROTO errno（不引 libc，硬编码 71 = Linux EPROTO）。
    fn libc_eproto() -> u32 {
        71
    }

    // 防 unused import（HeaderFlags 仅文档/未来用，避免 warning）
    #[allow(unused_imports)]
    use HeaderFlags as _UnusedHeaderFlags;
}
```

注意：
- `Header::reply_err(msg_id, cmd, errno)` 确认存在（server handshake.rs / irq.rs 用 `Header::reply_err`）。若签名不同按实调整。
- 若 `HeaderFlags` 未实际用到则删该 import（避免 unused）；上面的 `_UnusedHeaderFlags` 兜底可删——执行时按编译器提示清理。
- EPROTO 硬编码 71 避免引 libc dep（W1 不需要 libc）。

### Step 4.2: loopback 集成测

创建 `usnvmemu/crates/vfio_user_device/tests/loopback_handshake.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W1 loopback 集成测：真 `vfio_user_transport::server_handshake` ⇄ client
//! `VfioUserClient::handshake`，验证协商一致。socketpair，Linux 直跑，无需 VTL。

use std::os::unix::net::UnixStream;
use std::thread;
use vfio_user_device::VfioUserClient;

/// happy path：client 握手成功，拿到 server major/minor/caps。
#[test]
fn loopback_handshake_succeeds() {
    let (server_end, client_end) = UnixStream::pair().unwrap();

    // server 半段在线程跑真 server_handshake。
    let server = thread::spawn(move || -> anyhow::Result<()> {
        let mut s = server_end;
        vfio_user_transport::server_handshake(&mut s)?;
        Ok(())
    });

    let mut client = VfioUserClient::from_stream(client_end);
    let negotiated = client.handshake().expect("client handshake should succeed");

    // server 回 PROTOCOL_MAJOR + min(client_minor, server_minor)。
    assert_eq!(negotiated.server_major, 0, "PROTOCOL_MAJOR");
    assert!(
        negotiated.server_minor <= 1,
        "server_minor 应 ≤ client 提议（PROTOCOL_MINOR=1）"
    );
    assert!(
        negotiated.server_caps_json.contains("capabilities"),
        "server caps JSON 应含 capabilities：{}",
        negotiated.server_caps_json
    );

    server.join().unwrap().expect("server_handshake should succeed");
}
```

### Step 4.3: 验证全 crate + clippy

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_device && \
  cargo test 2>&1 | tail -8 && \
  cargo clippy --all-targets -- -D warnings 2>&1 | tail -3
```
预期：framing 2 + client error-path 1 + loopback 1 = **4 passed**（lib 3 + integration 1）+ clippy clean。

---

## Final — 全套 oracle + 单 commit

### Step F.1: 全相关 crate 不退化

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire && echo "wire:" && cargo test --lib 2>&1 | grep "test result"
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && echo "transport:" && cargo test --lib 2>&1 | grep "test result"
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_device && echo "device:" && cargo test 2>&1 | grep "test result"
```
预期：wire **45**（39+6）、transport **68** 不退化、device **4**（framing 2 + client error 1 + loopback 1）。

### Step F.2: clippy 全 clean

```bash
for c in vfio_user_wire vfio_user_device; do
  cd /home/xp/refs/openvmm/usnvmemu/crates/$c && echo "--- $c ---" && cargo clippy --all-targets -- -D warnings 2>&1 | tail -1
done
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && echo "--- transport ---" && cargo clippy --lib --tests -- -D warnings 2>&1 | tail -1
```
预期：3 个 clean。

### Step F.3: nvme_firmware bin + POC 不退化（wire 改动波及验证）

wire crate 加了 ProtoError 变体 + handshake 函数（纯增量），不应影响 server。验证：
```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/nvme_firmware && cargo build --bin nvme_firmware 2>&1 | tail -2
POC_SKIP_BUILD=1 python3 /home/xp/refs/openvmm/usnvmemu/experiments/2026-06-11-openhcl-vfio-user-client-poc/poc1_dma_zerocopy.py 2>&1 | tail -3
```
预期：bin clean + POC-1 PASS。

### Step F.4: 单 commit

```bash
cd /home/xp/refs/openvmm && git status --short
```
确认 W1 范围；DECISIONS.md / poc6 Cargo.lock 不碰。精确 add：

```bash
cd /home/xp/refs/openvmm
git add \
  usnvmemu/crates/vfio_user_wire/src/proto.rs \
  usnvmemu/crates/vfio_user_wire/src/handshake.rs \
  usnvmemu/crates/vfio_user_device/ \
  docs/superpowers/plans/2026-06-11-w1-vfio-user-device-client.md \
  Cargo.toml
# 确认不含 DECISIONS.md
git diff --cached --name-only | grep -i decisions && echo "ERROR DECISIONS staged" || echo "OK no DECISIONS"
git diff --cached --name-only | grep -i 'Cargo.lock' && echo "CHECK: Cargo.lock — 确认属本次" || echo "OK no Cargo.lock"
# [architect D-1] 确认不吞 vfio_user_device 构建产物
git diff --cached --name-only | grep -E 'vfio_user_device/(target|Cargo\.lock)' && echo "ERROR build artifact staged" || echo "OK no artifact"
```

commit：
```bash
git commit -m "feat(vfio_user_device): W1 client wire + AF_UNIX + VERSION 握手

新建 underhill 侧 vfio-user client crate（与 vfio_user_transport firmware
server 对接，复用 vfio_user_wire sans-IO 协议层）。W1 范围 = client wire +
AF_UNIX + 握手（loopback 单测），PCIe/DMA/IRQ 留 W2-W4。

wire crate 加 client 纯决策（sans-IO，server/client 共享）：
- build_version_command_payload —— 对称 build_version_reply_payload，client
  自填 (major,minor) 而非 echo/negotiate
- verify_server_version_reply —— client 侧校验（major 相等 + minor ≤ 提议；
  方向与 negotiate_minor 相反，防 server 回更高 minor 的非法情形）
- NegotiatedClient（镜像 Negotiated 但字段语义=对端 server）+ CLIENT_CAPS_JSON
- ProtoError 加 VersionMismatch 变体

vfio_user_device crate（workspace exclude，无 nix/unsafe）：
- framing.rs —— 纯 UnixStream read/write（握手无 fd → 无 recvmsg/SCM_RIGHTS/
  unsafe，保持 deny(unsafe_code)；fd-capable framing 留 W3）
- client.rs —— VfioUserClient::connect + handshake（发 command → 收 reply →
  帧校验 REPLY/非 error/cmd/msg_id echo → 解 VersionPayload + verify + 解 caps）
- 同步阻塞（镜像 transport 单测范式），async 留 W6

oracle：
- vfio_user_wire 45（39+6 client 决策测）/ vfio_user_transport 68 不退化 /
  vfio_user_device 3（framing 2 + loopback 1：真 server_handshake ⇄ client）
- nvme_firmware bin clean + POC-1 PASS（wire 纯增量不影响 server）
- clippy --all-targets -D warnings clean（三 crate）

经 spec §6 W1 + explore + architect review + inline 执行（单 commit）。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
git log --oneline -1
```

---

## Self-Review

**1. Spec 覆盖**：spec §6 W1 "client wire + AF_UNIX + 握手（loopback 单测）" → Stage 1（wire client 决策）+ Stage 3（framing）+ Stage 4（connect/handshake/loopback）。PCIe/DMA/IRQ 明确留 W2-W4，不越界。

**2. Placeholder 扫描**：无 TBD。所有代码完整给出。"grep 确认字段名"是真验证步骤（Header 字段名 msg_size/cmd/error_no/flags() 需执行时确认，非占位）。

**3. 类型一致性**：
- `build_version_command_payload(major, minor, caps: &[u8]) -> Vec<u8>` —— Stage 1 定义 / Stage 4 client.rs 调用一致
- `verify_server_version_reply(proposed_major, proposed_minor, reply: &VersionPayload) -> Result<(), ProtoError>` —— Stage 1 定义 / Stage 4 调用一致
- `NegotiatedClient { server_major, server_minor, server_caps_json }` —— Stage 1 定义 / lib.rs re-export / Stage 4 构造 / loopback 测断言一致
- `ProtoError::VersionMismatch { proposed_major, proposed_minor, server_major, server_minor }` —— Step 1.1 定义 / Step 1.2 verify 构造 / 测试 matches! 一致
- `VfioUserClient::{connect, from_stream, handshake}` —— client.rs 定义 / loopback 测用 from_stream + handshake
- framing `read_message -> WireMessage` / `write_message(stream, header, payload)` —— framing.rs 定义 / client.rs 调用一致

**4. 单 commit 纪律**：Final F.4 一次性精确 pathspec + DECISIONS/Cargo.lock 防吞检查。

**5. 风险缓解（explore 标的）**：
- client/server 不对称：新建 NegotiatedClient（不复用 Negotiated）；verify 用 `≤` 校验非 negotiate_minor（注释显式标方向）→ Stage 1 已落实
- caps 语义：CLIENT_CAPS_JSON 最简（测试断言不含 server 字段）→ Step 1.2
- 同步 vs async：W1 同步，handshake 决策（wire 纯函数）与 framing IO 解耦，W6 换 framing 不动 handshake → 架构已分层
- fd 路径：W1 framing 无 fd、无 unsafe（deny(unsafe_code)）→ Stage 3 纯 read/write
- workspace exclude：Step 2.3 显式加

---

**Plan 完。** 4 Stage + Final = 5 段，单 commit 收口。
