# W2 — `vfio_user_device` client REGION wire（GET_INFO / REGION_RW / RESET）

> **执行方式：inline（控制者本人执行，非 subagent-driven）。** 与 W0.5/W1 同——单 commit。inline 掌握每条 git 命令，无裸 commit 吞别会话 DECISIONS.md 之险。
>
> **Plan type: 新功能（TDD）。** client region wire 是真新代码。loopback 对接真 `VfioUserSession` + `MockDev` 验证（socketpair，Linux standalone）。
>
> **⚠️ 单 commit 纪律（用户 2026-06-11）**：整个 W2 合成**一个 commit**。见 [[commit-granularity-coarse-not-per-step]]。
>
> **⚠️ scope ADR（用户 2026-06-11 拍板，选项3）**：spec §6 W2 原写"PCIe 呈现（复用 pcie_remote 层）+ REGION_RW"，但 explore 证实：① PCIe 呈现器 `ConfigSpaceType0Emulator` 在重量级 `pci_core`（workspace member，拖 chipset_device/guestmem/vmcore/mesh/inspect 全家桶），path-dep 进 exclude crate 会**打破 W1 的 standalone Linux 可测**；② "向 guest 呈现 PCIe 设备" 本质需 openvmm/underhill partition + MMIO/config intercept，**任何方案都无法 standalone 测**；③ server 已是 config/BAR 真值源，client 透传 REGION_RW 即可，无需本地 pci_core 模拟。**故 W2 = client REGION wire only（保持 standalone）；guest-facing PCIe 呈现拆独立 adapter crate（依赖 pci_core）推迟 W6**。本 plan 落地后更新 spec §6 W2 记此 ADR。

**Goal:** 在 `VfioUserClient` 上加 client 侧 vfio-user region wire：`get_device_info` / `get_region_info` / `get_irq_info` / `region_read` / `region_write` / `reset`，loopback 对接真 `VfioUserSession`（+ `MockDev` backing）按 `enumerate_smoke` 剧本验证（GET_INFO num_regions / CONFIG identity / BAR0 size-probe / FLR）。

**Architecture:** 复用 `vfio_user_wire::proto`（`DeviceInfoPayload`/`RegionInfoPayload`/`IrqInfoPayload`/`RegionAccessPayload` + `encode_msg`/`encode_msg_bytes`/`decode_payload`）+ W1 的 `framing::{write_message,read_message}`。`VfioUserClient` 加 `next_msg_id: u16` 计数器（W1 握手写死 1，W2 起递增）+ `expect_reply` 校验 helper（packed 字段先 copy）。production deps 不变（wire+zerocopy+anyhow）；loopback 测加 dev-dep `pcie_device_core`（造 MockDev，零依赖 exclude crate，不破 standalone）。

**Tech Stack:** Rust 2024，rustc 1.95；deps 不变；dev-dep 增 `pcie_device_core`。无 nix/unsafe（region wire 无 fd）。

**Spec 来源：** `docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md` §6 W2（本 plan 落地后修正记 ADR）。

**前置 commit：** `920795a2`（W1）。

**回退指引：** W2 全程未 commit；失败 `git checkout -- <file>`。不碰 `usnvmemu/docs/DECISIONS.md`（别会话）。

---

## Baseline（起手必跑）

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_device && cargo test 2>&1 | grep "test result"
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && cargo test --lib 2>&1 | grep "test result"
```
预期：device **4**（W1）、transport **68**。实测记真值。

---

## File Structure

修改：
- `usnvmemu/crates/vfio_user_device/src/client.rs` —— `VfioUserClient` 加 `next_msg_id` + region wire 方法 + `expect_reply` helper
- `usnvmemu/crates/vfio_user_device/Cargo.toml` —— dev-dep 加 `pcie_device_core`

新建：
- `usnvmemu/crates/vfio_user_device/tests/loopback_region.rs` —— region wire loopback 集成测

落地后改：
- `docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md` §6 W2 —— 记 scope ADR

---

## Stage 1 — client 查询命令 + msg_id 计数器 + expect_reply helper

**Files:** `vfio_user_device/src/client.rs`

### Step 1.1: VfioUserClient 加 next_msg_id 字段

把 `client.rs` 的 struct + 构造器改为带 msg_id 计数器：

```rust
/// vfio-user client 连接。持一条同步 `UnixStream` + msg_id 计数器。
pub struct VfioUserClient {
    stream: UnixStream,
    /// 下一个请求的 msg_id（W1 握手用 1，W2 起每请求递增；reply 须 echo 同 id）。
    next_msg_id: u16,
}

impl VfioUserClient {
    /// connect 到 server 的 AF_UNIX socket 路径。
    pub fn connect(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(path.as_ref())
            .with_context(|| format!("connect vfio-user socket {:?}", path.as_ref()))?;
        Ok(Self {
            stream,
            next_msg_id: 1,
        })
    }

    /// 从已建立的 `UnixStream` 构造（loopback 单测 / 已 connect 的场景）。
    pub fn from_stream(stream: UnixStream) -> Self {
        Self {
            stream,
            next_msg_id: 1,
        }
    }

    /// 取下一个 msg_id 并自增（wrapping，避免溢出 panic）。
    fn alloc_msg_id(&mut self) -> u16 {
        let id = self.next_msg_id;
        self.next_msg_id = self.next_msg_id.wrapping_add(1);
        id
    }
```

`handshake()` 内把写死的 `let msg_id = 1u16;` 改为 `let msg_id = self.alloc_msg_id();`（保持先发后收逻辑不变）。

### Step 1.2: expect_reply 校验 helper

在 `impl VfioUserClient` 内加（统一 reply 帧校验，packed 字段先 copy）：

```rust
    /// 校验一个 reply 帧：必须是 REPLY（非 command）、非 error、cmd 匹配、msg_id echo。
    ///
    /// **packed 字段先 copy 到本地**（Header `#[repr(C,packed)]`，`deny(unsafe_code)`
    /// 下直接读字段是 E0793）。
    fn expect_reply(reply: &WireMessage, msg_id: u16, cmd: Command) -> anyhow::Result<()> {
        let flags = reply.header.flags();
        let reply_cmd = reply.header.cmd;
        let reply_error_no = reply.header.error_no;
        let reply_msg_id = reply.header.msg_id;
        if !flags.is_reply() {
            return Err(anyhow!("{cmd:?} reply 不是 REPLY 帧"));
        }
        if flags.is_error() {
            return Err(anyhow!("{cmd:?} reply 是 error（error_no={reply_error_no}）"));
        }
        if reply_cmd != cmd as u16 {
            return Err(anyhow!("{cmd:?} reply cmd 不匹配：got {reply_cmd}"));
        }
        if reply_msg_id != msg_id {
            return Err(anyhow!(
                "{cmd:?} reply msg_id 不匹配：sent {msg_id}, got {reply_msg_id}"
            ));
        }
        Ok(())
    }
```

W1 的 `handshake()` 内那段手写帧校验（is_reply/is_error/cmd/msg_id）可**保留不动**（避免 W2 改 W1 已 review 通过的代码），或顺手换成 `Self::expect_reply(&reply, msg_id, Command::Version)?` 再单独解 VersionPayload——二选一，推荐换成 expect_reply 复用（少重复），但若换则 handshake loopback 测须复跑确认不退化。**保守起见 W2 不动 handshake，只新增 region 方法用 expect_reply。**

### Step 1.3: get_device_info / get_region_info / get_irq_info

在 `impl VfioUserClient` 内加（用 `encode_msg` 编请求 + `decode_payload` 解 reply）：

```rust
    /// DEVICE_GET_INFO：查 region 数 / IRQ 数 / flags。
    pub fn get_device_info(&mut self) -> anyhow::Result<DeviceInfoPayload> {
        let req = DeviceInfoPayload {
            argsz: core::mem::size_of::<DeviceInfoPayload>() as u32,
            ..Default::default()
        };
        let reply = self.request(Command::DeviceGetInfo, &req)?;
        Self::decode_reply_payload(&reply, Command::DeviceGetInfo)
    }

    /// DEVICE_GET_REGION_INFO：查某 region 的 size / flags。
    pub fn get_region_info(&mut self, index: u32) -> anyhow::Result<RegionInfoPayload> {
        let req = RegionInfoPayload {
            argsz: core::mem::size_of::<RegionInfoPayload>() as u32,
            index,
            ..Default::default()
        };
        let reply = self.request(Command::DeviceGetRegionInfo, &req)?;
        Self::decode_reply_payload(&reply, Command::DeviceGetRegionInfo)
    }

    /// DEVICE_GET_IRQ_INFO：查某 IRQ type 的向量数 / flags。
    pub fn get_irq_info(&mut self, index: u32) -> anyhow::Result<IrqInfoPayload> {
        let req = IrqInfoPayload {
            argsz: core::mem::size_of::<IrqInfoPayload>() as u32,
            index,
            ..Default::default()
        };
        let reply = self.request(Command::DeviceGetIrqInfo, &req)?;
        Self::decode_reply_payload(&reply, Command::DeviceGetIrqInfo)
    }
```

配套两个私有 helper（也在 `impl VfioUserClient`）：

```rust
    /// 发一个"payload 是单个 zerocopy struct、reply 校验后返回整帧"的请求。
    fn request<T: zerocopy::IntoBytes + zerocopy::Immutable>(
        &mut self,
        cmd: Command,
        req: &T,
    ) -> anyhow::Result<WireMessage> {
        let msg_id = self.alloc_msg_id();
        let mut buf = Vec::new();
        let hdr = Header::command(msg_id, cmd, core::mem::size_of::<T>() as u32);
        encode_msg(&hdr, req, &mut buf);
        // encode_msg 把 header+payload 一起写进 buf；framing::write_message 期待
        // (header, payload) 分开 → 改用 write_raw（见下）一次性发整 buf。
        self.write_raw(&buf)?;
        let reply = read_message(&mut self.stream).context("recv reply")?;
        Self::expect_reply(&reply, msg_id, cmd)?;
        Ok(reply)
    }

    /// 解 reply 的前 size_of::<T>() 字节为 struct T。
    fn decode_reply_payload<T: zerocopy::FromBytes + zerocopy::KnownLayout + zerocopy::Immutable + Copy>(
        reply: &WireMessage,
        cmd: Command,
    ) -> anyhow::Result<T> {
        let want = core::mem::size_of::<T>();
        if reply.payload.len() < want {
            return Err(anyhow!("{cmd:?} reply payload 过短：{} < {want}", reply.payload.len()));
        }
        decode_payload(&reply.payload[..want]).with_context(|| format!("decode {cmd:?} reply payload"))
    }
```

**注意 framing 接口适配**：W1 的 `framing::write_message(stream, header, payload)` 把 header 和 payload 分开传。`encode_msg(hdr, payload_struct, buf)` 把两者合进一个 buf。两种风格混用会重复编码 header。**选一种**：
- **推荐**：region 方法不用 `encode_msg`，直接用 W1 的 `write_message(stream, &hdr, payload_bytes)`——payload_bytes = `req.as_bytes()`（struct）或 `req.as_bytes() + data`（REGION_WRITE）。这样复用 W1 framing，无需新 `write_raw`。
- 重写上面 `request`：
  ```rust
  fn request<T: IntoBytes + Immutable>(&mut self, cmd: Command, req: &T) -> anyhow::Result<WireMessage> {
      let msg_id = self.alloc_msg_id();
      let payload = req.as_bytes();
      let hdr = Header::command(msg_id, cmd, payload.len() as u32);
      write_message(&mut self.stream, &hdr, payload).with_context(|| format!("send {cmd:?}"))?;
      let reply = read_message(&mut self.stream).context("recv reply")?;
      Self::expect_reply(&reply, msg_id, cmd)?;
      Ok(reply)
  }
  ```
  删掉 `write_raw` / `encode_msg` 的使用（不引入）。`req.as_bytes()` 需 `use zerocopy::IntoBytes;`。**采用此版本**，更简洁且复用 W1 framing。

### Step 1.4: 验证编译（方法暂无测，下一 Stage 加 region_read/write + 测）

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_device && cargo build 2>&1 | tail -3
```
预期：clean（新方法未被调用会有 dead_code warning，但 cargo build 不报错；clippy 留 Final 一起验，那时 loopback 测会调用它们）。

---

## Stage 2 — region_read / region_write

**Files:** `vfio_user_device/src/client.rs`

### Step 2.1: region_read / region_write

在 `impl VfioUserClient` 内加：

```rust
    /// REGION_READ：读 region[index] 的 [offset, offset+count) 字节。
    ///
    /// reply = `RegionAccessPayload echo(16B) + count 字节数据`；校验 echo 的
    /// region/offset/count 与请求一致，返回数据段。
    pub fn region_read(&mut self, region: u32, offset: u64, count: u32) -> anyhow::Result<Vec<u8>> {
        let req = RegionAccessPayload {
            offset,
            region,
            count,
        };
        let reply = self.request(Command::RegionRead, &req)?;
        // 解 echo header（前 16B）并校验。
        let hdr_size = core::mem::size_of::<RegionAccessPayload>();
        let echo: RegionAccessPayload = Self::decode_reply_payload(&reply, Command::RegionRead)?;
        // packed 字段先 copy。
        let echo_region = echo.region;
        let echo_offset = echo.offset;
        let echo_count = echo.count;
        if echo_region != region || echo_offset != offset || echo_count != count {
            return Err(anyhow!(
                "REGION_READ echo 不匹配：req(region={region}, offset={offset}, count={count}) \
                 vs echo(region={echo_region}, offset={echo_offset}, count={echo_count})"
            ));
        }
        // 数据段 = echo 之后的 count 字节。
        let data = reply
            .payload
            .get(hdr_size..hdr_size + count as usize)
            .ok_or_else(|| {
                anyhow!(
                    "REGION_READ reply 数据段过短：payload {} < {}+{count}",
                    reply.payload.len(),
                    hdr_size
                )
            })?;
        Ok(data.to_vec())
    }

    /// REGION_WRITE：把 `data` 写到 region[index] 的 offset 处。
    ///
    /// 请求 = `RegionAccessPayload(16B) + data`；reply = echo only（无数据）。
    pub fn region_write(&mut self, region: u32, offset: u64, data: &[u8]) -> anyhow::Result<()> {
        let req = RegionAccessPayload {
            offset,
            region,
            count: data.len() as u32,
        };
        // payload = RegionAccessPayload bytes + data。
        let mut payload = Vec::with_capacity(core::mem::size_of::<RegionAccessPayload>() + data.len());
        payload.extend_from_slice(req.as_bytes());
        payload.extend_from_slice(data);
        let msg_id = self.alloc_msg_id();
        let hdr = Header::command(msg_id, Command::RegionWrite, payload.len() as u32);
        write_message(&mut self.stream, &hdr, &payload).context("send REGION_WRITE")?;
        let reply = read_message(&mut self.stream).context("recv REGION_WRITE reply")?;
        Self::expect_reply(&reply, msg_id, Command::RegionWrite)?;
        Ok(())
    }
```

注意：`req.as_bytes()` 需 `use zerocopy::IntoBytes;`（Stage 1.3 已加）。`RegionAccessPayload` 字段 `offset: u64, region: u32, count: u32`（已确认 proto.rs:329）。

### Step 2.2: reset()

```rust
    /// DEVICE_RESET：空 payload，触发 server 端 device reset（FLR）。
    pub fn reset(&mut self) -> anyhow::Result<()> {
        let msg_id = self.alloc_msg_id();
        let hdr = Header::command(msg_id, Command::DeviceReset, 0);
        write_message(&mut self.stream, &hdr, &[]).context("send DEVICE_RESET")?;
        let reply = read_message(&mut self.stream).context("recv DEVICE_RESET reply")?;
        Self::expect_reply(&reply, msg_id, Command::DeviceReset)?;
        Ok(())
    }
```

### Step 2.3: 补全 client.rs imports

确认 `client.rs` 顶部 use 含（W1 已有部分 + W2 新增）：
```rust
use crate::framing::read_message;
use crate::framing::write_message;
use anyhow::Context as _;
use anyhow::anyhow;
use std::os::unix::net::UnixStream;
use std::path::Path;
use vfio_user_wire::framing::WireMessage;            // W2 新增（request/expect_reply 用）
use vfio_user_wire::handshake::CLIENT_CAPS_JSON;
use vfio_user_wire::handshake::NegotiatedClient;
use vfio_user_wire::handshake::build_version_command_payload;
use vfio_user_wire::handshake::parse_caps_blob;
use vfio_user_wire::handshake::verify_server_version_reply;
use vfio_user_wire::proto::Command;
use vfio_user_wire::proto::DeviceInfoPayload;        // W2 新增
use vfio_user_wire::proto::Header;
use vfio_user_wire::proto::IrqInfoPayload;           // W2 新增
use vfio_user_wire::proto::PROTOCOL_MAJOR;
use vfio_user_wire::proto::PROTOCOL_MINOR;
use vfio_user_wire::proto::RegionAccessPayload;      // W2 新增
use vfio_user_wire::proto::RegionInfoPayload;        // W2 新增
use vfio_user_wire::proto::VersionPayload;
use vfio_user_wire::proto::decode_payload;
use zerocopy::IntoBytes;                             // W2 新增（req.as_bytes()）
```

### Step 2.4: 验证编译

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_device && cargo build 2>&1 | tail -3
```
预期：clean（dead_code warning 容忍，Stage 4 测会用到）。

---

## Stage 3 — loopback dev-dep

**Files:** `vfio_user_device/Cargo.toml`

### Step 3.1: dev-dep 加 pcie_device_core

`vfio_user_device/Cargo.toml` 的 `[dev-dependencies]` 加（造 MockDev 用；pcie_device_core 是零依赖 exclude crate，不破 standalone）：

```toml
[dev-dependencies]
# loopback 单测对接真 server_handshake + VfioUserSession。
vfio_user_transport = { path = "../vfio_user_transport" }
# loopback 测的 MockDev（PcieDevice 实现）。
pcie_device_core = { path = "../pcie_device_core" }
```

---

## Stage 4 — loopback region 集成测

**Files:** `vfio_user_device/tests/loopback_region.rs`

### Step 4.1: 写 loopback_region.rs

server 线程：`server_handshake` → `VfioUserSession::new` → 循环 `pump_one(&mut MockDev)`；client 线程跑 handshake + region 查询/读写，按 `enumerate_smoke` 剧本断言。

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W2 loopback 集成测：真 `vfio_user_transport::VfioUserSession` + `MockDev` ⇄
//! client region wire（GET_INFO / GET_REGION_INFO / REGION_RW / RESET）。
//! socketpair，Linux 直跑，无需 VTL。剧本对标 interop_py `enumerate_smoke`。

use pcie_device_core::BarKind;
use pcie_device_core::BarLayout;
use pcie_device_core::DeviceCtx;
use pcie_device_core::DeviceDescribe;
use pcie_device_core::PcieDevice;
use std::os::unix::net::UnixStream;
use std::thread;
use vfio_user_device::VfioUserClient;

/// 复制自 vfio_user_transport session.rs 的测试 MockDev：BAR0 8 KiB + 8 MSI-X +
/// identity（vendor 0x1234 / device 0x5678 / class 0x010802）。
struct MockDev {
    bar0: Vec<u64>,
    last_reset: u32,
}
impl MockDev {
    fn new() -> Self {
        Self {
            bar0: vec![0u64; 1024],
            last_reset: 0xFFFF_FFFF,
        }
    }
}
impl PcieDevice for MockDev {
    fn describe(&self) -> DeviceDescribe {
        DeviceDescribe {
            vendor_id: 0x1234,
            device_id: 0x5678,
            class_code: 0x01_08_02,
            revision: 1,
            subsystem_vendor: 0,
            subsystem_device: 0,
            bars: vec![BarLayout {
                index: 0,
                size: 8192,
                kind: BarKind::Mmio32,
                prefetchable: false,
            }],
            msix_count: 8,
            capabilities: vec![],
            cfg_write_side_effect_offsets: vec![],
        }
    }
    fn mmio_read(&mut self, _bar: u32, offset: u64, _size: u32) -> u64 {
        *self.bar0.get((offset / 8) as usize).unwrap_or(&0)
    }
    fn mmio_write(&mut self, _ctx: &mut DeviceCtx<'_>, _bar: u32, offset: u64, _size: u32, value: u64) {
        let idx = (offset / 8) as usize;
        if idx < self.bar0.len() {
            self.bar0[idx] = value;
        }
    }
    fn reset(&mut self, kind: u32) {
        self.last_reset = kind;
    }
}

/// 起 server 线程：handshake → session 循环 pump_one(MockDev) 直到 peer close。
fn spawn_server(server_end: UnixStream) -> thread::JoinHandle<anyhow::Result<()>> {
    thread::spawn(move || -> anyhow::Result<()> {
        let mut s = server_end;
        let neg = vfio_user_transport::server_handshake(&mut s)?;
        let mut sess = vfio_user_transport::VfioUserSession::new(s, neg);
        let mut dev = MockDev::new();
        // pump 到 peer 关闭（client drop）→ pump_one 返 Ok(false)。
        while sess.pump_one(&mut dev)? {}
        Ok(())
    })
}

#[test]
fn loopback_region_enumerate_and_rw() {
    let (server_end, client_end) = UnixStream::pair().unwrap();
    let server = spawn_server(server_end);

    let mut client = VfioUserClient::from_stream(client_end);
    client.handshake().expect("handshake");

    // 1. DEVICE_GET_INFO：MockDev → num_regions=9（PCI 标准）、num_irqs=5。
    let info = client.get_device_info().expect("get_device_info");
    let num_regions = info.num_regions; // packed copy
    let num_irqs = info.num_irqs;
    assert_eq!(num_regions, 9, "PCI num_regions");
    assert_eq!(num_irqs, 5, "PCI num_irqs");

    // 2. GET_REGION_INFO(BAR0=0)：size=8192，READ|WRITE。
    let bar0 = client.get_region_info(0).expect("get_region_info BAR0");
    let bar0_size = bar0.size; // packed copy
    assert_eq!(bar0_size, 8192, "BAR0 size");

    // 3. GET_REGION_INFO(CONFIG=7)：size=4096。
    let cfg = client.get_region_info(7).expect("get_region_info CONFIG");
    let cfg_size = cfg.size; // packed copy
    assert_eq!(cfg_size, 4096, "CONFIG size");

    // 4. CONFIG region READ @ offset 0：identity（vendor 0x1234 / device 0x5678）。
    //    config dword 0 = (device_id << 16) | vendor_id = 0x5678_1234，LE 字节。
    let id = client.region_read(7, 0, 4).expect("region_read CONFIG id");
    assert_eq!(id, vec![0x34, 0x12, 0x78, 0x56], "config dword0 identity LE");

    // 5. BAR0 REGION_WRITE then READ roundtrip @ offset 0。
    client
        .region_write(0, 0, &0xDEAD_BEEF_u32.to_le_bytes())
        .expect("region_write BAR0");
    let got = client.region_read(0, 0, 8).expect("region_read BAR0");
    // MockDev bar0[0] 存 u64；写 4 字节 0xDEADBEEF 后读 8 字节低位应含之。
    assert_eq!(&got[..4], &0xDEAD_BEEF_u32.to_le_bytes(), "BAR0 write/read roundtrip");

    // 6. DEVICE_RESET（FLR）：不报错即可（MockDev.reset 记 kind）。
    client.reset().expect("reset");

    drop(client); // 关 socket → server pump_one 返 false 退出循环。
    server.join().unwrap().expect("server loop");
}
```

注意：
- `num_regions=9`/`num_irqs=5`：server `handle_get_info` 回的 PCI 标准值（explore B 段确认）。若实测不同按真值调断言。
- config dword0 identity：`MockDev` vendor=0x1234 device=0x5678 → config[0..4] = vendor(LE) + device(LE) = `34 12 78 56`。这是 server `ConfigSpace` 从 `DeviceDescribe` 合成的（vfio_user_wire::config）。若 server config 布局不同（如先 device 后 vendor）按实调整——以 server `config_region_read_returns_identity` 测试的真值为准。
- BAR0 size-probe（写全 1 读回 mask）剧本可选，W2 先验基础 read/write roundtrip；size-probe 是 config space 的 BAR 寄存器语义，走 CONFIG region 不是 BAR0 region，复杂度高，W2 可不做（留注释 TODO）。
- `mmio_write` 的 `_ctx: &mut DeviceCtx` —— server 调它时传的 ctx 是 `VfioUserSession` 自身（impl Transport）。MockDev 不用 ctx，OK。

### Step 4.2: 验证 device 全测 + clippy

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_device && \
  cargo test 2>&1 | tail -10 && \
  cargo clippy --all-targets -- -D warnings 2>&1 | tail -3
```
预期：W1 的 4（framing 2 + client error 1 + loopback_handshake 1）+ W2 loopback_region 1 = **5 passed** + clippy clean。

若 identity/size 断言失败：以 server 端 `session.rs` 的 `config_region_read_returns_identity` / `get_info_reply_shape` / region_info 测试的真值为准调整（这些是 server 行为的权威 oracle）。

---

## Final — 全套 oracle + spec ADR + 单 commit

### Step F.1: 全相关 crate 不退化

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_wire && echo "wire:" && cargo test --lib 2>&1 | grep "test result"
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && echo "transport:" && cargo test --lib 2>&1 | grep "test result"
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_device && echo "device:" && cargo test 2>&1 | grep "test result"
```
预期：wire **45** 不退化、transport **68** 不退化、device **5**（4+1）。

### Step F.2: clippy + nvme_firmware bin

```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_device && cargo clippy --all-targets -- -D warnings 2>&1 | tail -1
cd /home/xp/refs/openvmm/usnvmemu/crates/nvme_firmware && cargo build --bin nvme_firmware 2>&1 | tail -1
```
预期：clean（W2 不碰 wire/transport/firmware 生产代码，纯 client 增量 + dev-dep）。

### Step F.3: 更新 spec §6 W2 记 ADR

`docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md` 找到 §6 W2 行：
```
- **W2**：PCIe 呈现（复用 pcie_remote 层）+ REGION_RW。
```
替换为：
```
- **W2**：✅ client REGION wire（GET_INFO/GET_REGION_INFO/REGION_RW/RESET，loopback 对接真 server）。**scope ADR（2026-06-11，选项3）**：guest-facing PCIe 呈现拆独立 adapter crate（依赖上游 `pci_core`）推迟 W6——pci_core 重依赖会破坏 vfio_user_device 的 standalone Linux 可测，且"向 guest 呈现"本质需 openvmm/underhill partition 无法 standalone 测；server 已是 config/BAR 真值源，client 透传 REGION_RW 即可（不本地 pci_core 模拟）。spec §4 "复用 pcie_remote_device" 系 W0 audit 已纠的错误归因。
```

### Step F.4: 单 commit

```bash
cd /home/xp/refs/openvmm && git status --short
```
确认 W2 范围；DECISIONS.md / poc6 Cargo.lock 不碰。精确 add：

```bash
cd /home/xp/refs/openvmm
git add \
  usnvmemu/crates/vfio_user_device/src/client.rs \
  usnvmemu/crates/vfio_user_device/Cargo.toml \
  usnvmemu/crates/vfio_user_device/tests/loopback_region.rs \
  docs/superpowers/plans/2026-06-11-w2-region-wire.md \
  docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md
# guards
git diff --cached --name-only | grep -i decisions && echo "ERR DECISIONS" || echo "OK no DECISIONS"
git diff --cached --name-only | grep -E 'vfio_user_device/(target|Cargo\.lock)' && echo "ERR artifact" || echo "OK no artifact"
git diff --cached --name-only
```

commit：
```bash
git commit -m "feat(vfio_user_device): W2 client REGION wire（GET_INFO / REGION_RW / RESET）

client 侧 vfio-user region 收发：get_device_info / get_region_info /
get_irq_info / region_read / region_write / reset，复用 vfio_user_wire::proto
struct + W1 framing。VfioUserClient 加 next_msg_id 计数器（W1 握手写死 1，
W2 起每请求递增，reply echo 校验）+ expect_reply helper（packed 字段先 copy）。

loopback 集成测（tests/loopback_region.rs）：真 VfioUserSession + MockDev ⇄
client，剧本对标 interop_py enumerate_smoke——GET_INFO num_regions=9 /
GET_REGION_INFO BAR0=8192 CONFIG=4096 / CONFIG identity dword0 / BAR0
write-read roundtrip / RESET。dev-dep 加 pcie_device_core（造 MockDev，
零依赖 exclude crate，不破 standalone）。

scope ADR（用户 2026-06-11 拍板，选项3）：W2 = client REGION wire only；
guest-facing PCIe 呈现拆独立 adapter crate（依赖重量级 pci_core）推迟 W6。
理由：pci_core path-dep 进 exclude crate 拖 openvmm 全家桶破坏 standalone
Linux 可测；'向 guest 呈现' 本质需 openvmm/underhill partition 无法 standalone
测；server 已是 config/BAR 真值源，client 透传 REGION_RW 即可。spec §6 W2
同步记此 ADR。

oracle：
- vfio_user_device 5（W1 4 + W2 loopback_region 1）/ vfio_user_wire 45 /
  vfio_user_transport 68 不退化
- nvme_firmware bin clean / clippy --all-targets -D warnings clean
- production deps 不变（wire+zerocopy+anyhow），仅 dev-dep 增 pcie_device_core

经 spec §6 W2 + explore + architect review + inline 执行（单 commit）。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
git log --oneline -1
```

---

## Self-Review

**1. Spec 覆盖**：spec §6 W2 "REGION_RW" → Stage 1-2（GET_INFO/REGION_RW/RESET wire）；"PCIe 呈现" → ADR 推迟 W6（F.3 记录）。loopback 测覆盖 enumerate 剧本。

**2. Placeholder 扫描**：无 TBD。所有代码完整。framing 接口适配（encode_msg vs write_message）已在 Stage 1.3 明确"采用 write_message 版本"，非二义。BAR0 size-probe 标注可选 + TODO，非占位（明确 W2 不做）。

**3. 类型一致性**：
- `request<T: IntoBytes+Immutable>(cmd, &T) -> WireMessage` / `decode_reply_payload<T: FromBytes+KnownLayout+Immutable+Copy>(reply, cmd) -> T` —— Stage 1.3 定义 / get_* 调用一致
- `expect_reply(&WireMessage, u16, Command) -> Result<()>` —— Stage 1.2 定义 / region 方法调用
- `region_read(region: u32, offset: u64, count: u32) -> Vec<u8>` / `region_write(region, offset, &[u8])` / `reset()` —— Stage 2 定义 / loopback 测调用
- `RegionAccessPayload { offset: u64, region: u32, count: u32 }` —— proto.rs 确认 / region 方法构造一致
- `alloc_msg_id(&mut self) -> u16` / `next_msg_id` field —— Stage 1.1 定义 / request 调用
- MockDev impl PcieDevice（describe/mmio_read/mmio_write/reset）—— 复制 session.rs:809 形态，字段名一致

**4. 单 commit 纪律**：F.4 精确 pathspec + DECISIONS/artifact 防吞 guard。

**5. 风险（explore 标的）**：
- standalone 可测：production deps 不变，仅 dev-dep 加零依赖 pcie_device_core → 守住
- pci_core 不引入：W2 完全规避，ADR 推迟 W6 → 落实
- config 真值源：client 透传 CONFIG region 给 server，不本地模拟 → region_read(7,...) 体现
- packed 字段：所有 reply struct 字段读取先 copy 到 local（expect_reply / region_read echo / loopback 断言）→ 落实

---

**Plan 完。** 4 Stage + Final = 5 段，单 commit 收口。
