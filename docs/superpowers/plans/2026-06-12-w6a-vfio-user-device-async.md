# W6a — vfio_user_device async 化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `vfio_user_device`（vfio-user client，underhill 侧）从同步阻塞 `UnixStream` 收发迁到 openvmm 原生 async 栈（pal_async `PolledSocket`），为 W6b/c/d 的真 underhill 集成（pcie_remote 风格 async worker + `select_biased!`）铺路。standalone（WSL）秒级可测，零 IGVM。

**Architecture:** 照搬同仓库已验证的 SCM_RIGHTS async fd-passing 金标准 `vm/devices/virtio/vhost_user_protocol/src/socket.rs` 的**混合范式**：pal_async `PolledSocket` 管 socket readiness（`poll_io` 钩子），裸 `libc::sendmsg`/`recvmsg` + `cmsghdr` 管 SCM_RIGHTS ancillary fd（gnu/musl 双 target，截断/fd 泄漏防护齐全）。client **只发 fd 不收 fd**（W3/W5a 已证 DMA_MAP/SET_IRQS reply 无 fd），比 vhost_user 更简单：recv 路径只收字节 + 主动 drop 任何意外 fd（防恶意 server 泄漏）。

**Tech Stack:** Rust 2024 / pal_async（path-dep root workspace member，POC 已验 exclude crate 可 standalone 构建）/ libc（裸 cmsg）/ parking_lot（`Mutex<PolledSocket>` 保 Send）/ zerocopy / anyhow。

---

## 关键约束与背景（执行者必读）

1. **单 coarse commit**：本 plan 所有 task 完成后**只打一个 commit**（遵用户 coarse-commit 纪律，与 W0-W5a 一致）。subagent-driven 执行仍逐 task review，但不逐 task commit；最后统一 `git commit -- <pathspec>`（多会话共享树，**绝不 `git add -A`**，只限定本 crate 路径 + plan 文件 + 必要 harness）。

2. **承重假设已 POC 验证**（`/tmp/w6a_palasync_poc`，2026-06-12 PASS）：exclude-style standalone crate path-dep `pal_async` + `unix_socket`（root workspace members，用 workspace 继承）**能 standalone 构建 + 跑 PolledSocket loopback**（11.98s 编译，loopback 通）。故 W6a 地基稳。

3. **client 只发 fd 不收 fd**（W3/W5a 硬事实）：DMA_MAP（1 fd）、SET_IRQS（N eventfd）发出；所有 reply（DMA_MAP/SET_IRQS/REGION_RW/…）都是 header-only 或纯字节，**无 ancillary fd**。故 recv 路径无需把收到的 fd 交出去，只需"始终提供 cmsg 缓冲 + drop 任何意外 fd"防泄漏。

4. **unsafe 隔离到一个模块**：用户已授权高性能部分用 unsafe。crate 级去掉 `#![deny(unsafe_code)]`；新 `async_socket.rs` 模块顶 `#![allow(unsafe_code)]`（裸 libc cmsg，每 block 加 `// SAFETY:`，照搬 vhost_user 的注释）；`framing.rs` / `client.rs` 顶 `#![deny(unsafe_code)]`（模块级，逻辑层维持纯 safe）。

5. **server 不动**：`vfio_user_transport`（firmware server）仍同步（阻塞 accept/recvmsg）。W6a 只动 client。loopback 测 = async client ↔ sync server thread（内核 socketpair 两端 blocking/nonblocking 模式独立，可行）。

6. **`unix_socket::UnixStream` 在 Linux ≡ `std::os::unix::net::UnixStream`**（`support/unix_socket/src/lib.rs:21` 是 re-export）。`UnixStream::pair()` 直接喂 `PolledSocket::new(&driver, stream)`（std `UnixStream: AsFd → socket2::SockRef`，pal_async `AsSockRef` blanket impl 满足）。**无需加 unix_socket dep，无需类型转换**——继续用 `std::os::unix::net::UnixStream`。

7. **gold standard 完整源**：`vm/devices/virtio/vhost_user_protocol/src/socket.rs`（381 行，已读）。本 plan 的 `async_socket.rs` 直接 port 其 `cmsg_align`/`cmsg_space`/`cmsg_len`/`CmsgScmRights`/`try_send`/`try_recv`/`build_remaining_iov`，把 `VHOST_USER_MAX_FDS` 换成 `MAX_FDS=256`（对齐 server `vfio_user_transport::framing::MAX_MSG_FDS=256`）。

---

## File Structure

- **Create** `usnvmemu/crates/vfio_user_device/src/async_socket.rs` — async SCM_RIGHTS socket 层（`AsyncSocket` + 裸 libc cmsg 收发，**唯一含 unsafe 的模块**）。
- **Modify** `usnvmemu/crates/vfio_user_device/src/framing.rs` — 收发原语从 `&mut UnixStream` 同步改为 `&AsyncSocket` async；统一 `write_message`（fds slice，空=无 fd）+ `read_message`；模块顶 `#![deny(unsafe_code)]`。
- **Modify** `usnvmemu/crates/vfio_user_device/src/client.rs` — `VfioUserClient` 持 `AsyncSocket`；`connect`/`from_stream` 加 `&driver` 参数；全部 pub 方法 + 内部 `request`/`send_set_irqs` 改 `async fn`；模块顶 `#![deny(unsafe_code)]`。
- **Modify** `usnvmemu/crates/vfio_user_device/src/lib.rs` — 去 crate 级 `#![deny(unsafe_code)]`；声明 `mod async_socket;`；doc 更新（W6a：async 化，unsafe 隔离 async_socket）。
- **Modify** `usnvmemu/crates/vfio_user_device/Cargo.toml` — 加 `pal_async`（path）/`libc`（target unix）/`parking_lot`（prod）；`nix` 从 prod 降为 dev-dep（仅测试建 eventfd）。
- **Modify** 4 个 `tests/loopback_{handshake,region,dma,msix}.rs` — 测试体包进 `DefaultPool::run_with(async |driver| {...})`，client 调用加 `.await` + `from_stream(&driver, ...)`；server thread 不动。
- **Modify** `usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/client/{Cargo.toml,src/main.rs}` — harness client 改 async-compile（`DefaultPool::run_with` 包裹 + `.await`）。仅保证编译；真 VM re-verify 推迟（W6b 真集成会重跑真 VM）。

---

### Task 1: async_socket.rs — 端口 vhost_user SCM_RIGHTS async 层 + 依赖

**Files:**
- Modify: `usnvmemu/crates/vfio_user_device/Cargo.toml`
- Create: `usnvmemu/crates/vfio_user_device/src/async_socket.rs`
- Modify: `usnvmemu/crates/vfio_user_device/src/lib.rs:21-28`

- [ ] **Step 1: 改 Cargo.toml deps**

把 `[dependencies]` 段整段替换为（`nix` 从 prod 移到 dev）：

```toml
[dependencies]
# 与其它 usnvmemu crate 对齐版本；本 crate 在 workspace exclude（不用 workspace.dependencies）。
vfio_user_wire = { path = "../vfio_user_wire" }
zerocopy = { version = "0.8", features = ["derive"] }
anyhow = "1.0"
# W6a: async 收发走 pal_async PolledSocket（path-dep root workspace member；POC 已验
# exclude crate 可 standalone 构建）。Mutex<PolledSocket> 用 parking_lot 保 future Send。
pal_async = { path = "../../../support/pal/pal_async" }
parking_lot = "0.12"

# W6a: SCM_RIGHTS ancillary fd 收发走裸 libc sendmsg/recvmsg + cmsghdr（照搬
# vhost_user_protocol::socket 金标准；unsafe 隔离在 async_socket.rs 一个模块）。
[target.'cfg(unix)'.dependencies]
libc = "0.2"

[dev-dependencies]
# loopback 单测对接真 server_handshake + VfioUserSession。
vfio_user_transport = { path = "../vfio_user_transport" }
# loopback 测的 MockDev / DmaTriggerDev（PcieDevice 实现）。零依赖 exclude crate，不破 standalone。
pcie_device_core = { path = "../pcie_device_core" }
# W3 loopback 测的 "guest RAM" backing file（避开 client 侧 mmap/unsafe，用 FileExt 读写）。
tempfile = "3"
# W4 loopback 测建/读 MSI-X eventfd（client 只发 BorrowedFd，不再依赖 nix 收发 → nix 降为 dev）。
nix = { version = "0.30", features = ["socket", "uio", "event"] }
```

- [ ] **Step 2: 写 async_socket.rs（端口 vhost_user）**

Create `usnvmemu/crates/vfio_user_device/src/async_socket.rs`：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! client 端 async Unix domain socket + SCM_RIGHTS fd 传递。
//!
//! **本模块是 `vfio_user_device` crate 唯一含 `unsafe` 的模块**（裸 `libc::sendmsg`/
//! `recvmsg` + `cmsghdr` 收发 ancillary fd），故顶部 `#![allow(unsafe_code)]`；其余模块
//! （framing/client）维持 `#![deny(unsafe_code)]`。
//!
//! 范式照搬同仓库已验证的金标准 `vm/devices/virtio/vhost_user_protocol/src/socket.rs`：
//! pal_async [`PolledSocket`] 管 socket readiness（`poll_io` 钩子，自动吸收 spurious
//! wakeup / `WouldBlock` 重试），裸 libc 管 cmsg。gnu/musl 双 target 已处理。
//!
//! **client 只发 fd 不收 fd**（DMA_MAP/SET_IRQS 发，所有 reply 无 ancillary fd）。故
//! recv 路径只收字节，但仍**始终提供 cmsg 缓冲 + 主动 drop 任何意外 fd**——防恶意/异常
//! server 发 fd 造成 fd 泄漏。
#![allow(unsafe_code)]

use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::socket::PolledSocket;
use std::future::poll_fn;
use std::io;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;

/// 单帧最多附带的 fd 数。对齐 server `vfio_user_transport::framing::MAX_MSG_FDS`（256）。
const MAX_FDS: usize = 256;

/// CMSG_ALIGN：把 `len` 向上对齐到指针大小边界（gnu/musl 内核一致）。
const fn cmsg_align(len: usize) -> usize {
    (len + size_of::<usize>() - 1) & !(size_of::<usize>() - 1)
}

/// CMSG_SPACE：`data_len` 字节控制数据所需的总 ancillary 缓冲。
const fn cmsg_space(data_len: usize) -> usize {
    cmsg_align(size_of::<libc::cmsghdr>()) + cmsg_align(data_len)
}

/// CMSG_LEN：`data_len` 字节控制数据时 `cmsg_len` 字段的值。
const fn cmsg_len(data_len: usize) -> usize {
    cmsg_align(size_of::<libc::cmsghdr>()) + data_len
}

#[repr(C)]
struct CmsgScmRights {
    hdr: libc::cmsghdr,
    fds: [RawFd; MAX_FDS],
}

/// async vfio-user client socket：收发协议消息（header+payload + 可选 fd）。
pub(crate) struct AsyncSocket {
    // parking_lot::Mutex（非 RefCell）使 future 保持 Send（poll_io 闭包跨 await）。
    socket: parking_lot::Mutex<PolledSocket<UnixStream>>,
}

impl AsyncSocket {
    /// 包一条已连接的 `PolledSocket<UnixStream>`。
    pub(crate) fn new(socket: PolledSocket<UnixStream>) -> Self {
        Self {
            socket: parking_lot::Mutex::new(socket),
        }
    }

    /// 发一组 iovec（首段带可选 fd，经 SCM_RIGHTS）。循环直到全部发完；fd 只随首次
    /// sendmsg（= 随首字节，vfio-user spec 要求）。
    pub(crate) async fn send_with_fds(
        &self,
        iov: &[IoSlice<'_>],
        fds: &[impl AsFd],
    ) -> io::Result<()> {
        let raw_fds: Vec<RawFd> = fds.iter().map(|f| f.as_fd().as_raw_fd()).collect();
        let mut sent = 0;
        let total: usize = iov.iter().map(|s| s.len()).sum();
        while sent < total {
            let remaining_iov = build_remaining_iov(iov, sent);
            let send_fds: &[RawFd] = if sent == 0 { &raw_fds } else { &[] };
            let n = poll_fn(|cx| {
                self.socket
                    .lock()
                    .poll_io(cx, InterestSlot::Write, PollEvents::OUT, |socket| {
                        try_send(socket.get(), &remaining_iov, send_fds)
                    })
            })
            .await?;
            sent += n;
        }
        Ok(())
    }

    /// 收满 `buf.len()` 字节。client 不期望 fd，但仍始终提供 cmsg 缓冲并 drop 任何意外
    /// fd（防泄漏）。对端关闭（首字节读到 0）→ `UnexpectedEof`。
    pub(crate) async fn recv_exact(&self, buf: &mut [u8]) -> io::Result<()> {
        let mut read = 0;
        while read < buf.len() {
            let n = poll_fn(|cx| {
                self.socket
                    .lock()
                    .poll_io(cx, InterestSlot::Read, PollEvents::IN, |socket| {
                        try_recv(socket.get(), &mut buf[read..])
                    })
            })
            .await?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            read += n;
        }
        Ok(())
    }
}

/// 构造 `skip` 字节之后剩余未发数据的 IoSlice。
fn build_remaining_iov<'a>(original: &'a [IoSlice<'a>], skip: usize) -> Vec<IoSlice<'a>> {
    let mut remaining = skip;
    let mut result = Vec::new();
    for slice in original {
        if remaining >= slice.len() {
            remaining -= slice.len();
        } else {
            result.push(IoSlice::new(&slice[remaining..]));
            remaining = 0;
        }
    }
    result
}

/// sendmsg 带可选 fd。可能返回 `WouldBlock`（由 poll_io 重试）。
#[cfg_attr(
    target_env = "gnu",
    expect(
        clippy::needless_update,
        clippy::useless_conversion,
        reason = "libc::cmsghdr 在 gnu vs musl 上类型定义不同"
    )
)]
fn try_send(socket: &UnixStream, msg: &[IoSlice<'_>], fds: &[RawFd]) -> io::Result<usize> {
    assert!(
        fds.len() <= MAX_FDS,
        "fd 过多：{} > {MAX_FDS}",
        fds.len()
    );
    let fds_data_len = size_of_val(fds);
    let mut cmsg = CmsgScmRights {
        hdr: libc::cmsghdr {
            cmsg_level: libc::SOL_SOCKET,
            cmsg_type: libc::SCM_RIGHTS,
            cmsg_len: cmsg_len(fds_data_len) as _,
            ..{
                // SAFETY: type has no invariants
                unsafe { std::mem::zeroed() }
            }
        },
        fds: [0; MAX_FDS],
    };
    for (src, dst) in fds.iter().zip(cmsg.fds.iter_mut()) {
        *dst = *src;
    }

    // SAFETY: type has no invariants
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = msg.as_ptr() as *mut libc::iovec;
    hdr.msg_iovlen = msg.len().try_into().unwrap();
    hdr.msg_control = if fds.is_empty() {
        std::ptr::null_mut()
    } else {
        std::ptr::from_mut(&mut cmsg).cast::<libc::c_void>()
    };
    hdr.msg_controllen = if fds.is_empty() {
        0
    } else {
        cmsg_space(fds_data_len) as _
    };

    // SAFETY: 用正确初始化的缓冲调用 sendmsg。
    let n = unsafe { libc::sendmsg(socket.as_raw_fd(), &hdr, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// recvmsg 收字节；始终提供 cmsg 缓冲并 drop 任何意外 fd（防泄漏）。可能返回
/// `WouldBlock`（由 poll_io 重试）。
#[expect(clippy::allow_attributes)]
#[allow(
    clippy::unnecessary_cast,
    reason = "libc::cmsghdr 在 gnu vs musl 上类型定义不同"
)]
fn try_recv(socket: &UnixStream, buf: &mut [u8]) -> io::Result<usize> {
    assert!(!buf.is_empty());
    let mut iov = IoSliceMut::new(buf);

    // SAFETY: type has no invariants
    let mut cmsg: CmsgScmRights = unsafe { std::mem::zeroed() };
    // SAFETY: type has no invariants
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = std::ptr::from_mut(&mut iov).cast::<libc::iovec>();
    hdr.msg_iovlen = 1;
    // 始终提供 cmsg 缓冲：若 server 发了非预期 fd，必须收下以便正确 close（OwnedFd
    // drop）而非泄漏。
    hdr.msg_control = std::ptr::from_mut(&mut cmsg).cast::<libc::c_void>();
    hdr.msg_controllen = size_of_val(&cmsg) as _;

    // SAFETY: 用正确初始化的缓冲调用 recvmsg。
    let n = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut hdr, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Ok(0);
    }

    // 截断检测先于取 fd：若 ancillary 被截断（server 发了超过 MAX_FDS 个 fd），内核已
    // 安装部分 fd 但 cmsg.fds 不全可达 → 必须先 close 已收的再返 EMSGSIZE，否则泄漏。
    // client 现实只收 0 fd，但既写防泄漏路径就要完整（照搬 gold standard socket.rs:338-354）。
    if hdr.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        if hdr.msg_controllen > 0
            && cmsg.hdr.cmsg_level == libc::SOL_SOCKET
            && cmsg.hdr.cmsg_type == libc::SCM_RIGHTS
        {
            let fd_count = ((cmsg.hdr.cmsg_len as usize).saturating_sub(size_of_val(&cmsg.hdr))
                / size_of::<RawFd>())
            .min(MAX_FDS);
            for &raw_fd in &cmsg.fds[..fd_count] {
                // SAFETY: 内核已把这些 fd 的所有权转移给我们；drop 关闭它们。
                drop(unsafe { OwnedFd::from_raw_fd(raw_fd) });
            }
        }
        return Err(io::Error::from_raw_os_error(libc::EMSGSIZE));
    }

    // 收到任何 ancillary fd（client 从不期望）→ 全部 drop 防泄漏。
    if hdr.msg_controllen > 0
        && cmsg.hdr.cmsg_level == libc::SOL_SOCKET
        && cmsg.hdr.cmsg_type == libc::SCM_RIGHTS
    {
        let fd_count = ((cmsg.hdr.cmsg_len as usize).saturating_sub(size_of_val(&cmsg.hdr))
            / size_of::<RawFd>())
        .min(MAX_FDS);
        for &raw_fd in &cmsg.fds[..fd_count] {
            // SAFETY: 内核已把这些 fd 的所有权转移给我们；drop 关闭它们。
            drop(unsafe { OwnedFd::from_raw_fd(raw_fd) });
        }
    }
    Ok(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::DefaultPool;
    use std::io::IoSlice;

    /// 纯字节 roundtrip（两端都 AsyncSocket，同 driver）：send_with_fds(空 fd) →
    /// recv_exact。证 async readiness + iov 拼接 + 短写循环正确。
    #[test]
    fn bytes_roundtrip_no_fds() {
        DefaultPool::run_with(async |driver| {
            let (a, b) = UnixStream::pair().unwrap();
            let sa = AsyncSocket::new(PolledSocket::new(&driver, a).unwrap());
            let sb = AsyncSocket::new(PolledSocket::new(&driver, b).unwrap());
            let no_fds: &[std::os::fd::BorrowedFd<'_>] = &[];
            let payload = b"w6a-async-bytes".to_vec();
            let iov = [IoSlice::new(&payload)];
            sa.send_with_fds(&iov, no_fds).await.unwrap();
            let mut got = vec![0u8; payload.len()];
            sb.recv_exact(&mut got).await.unwrap();
            assert_eq!(got, payload);
        });
    }

    /// 发 1 个 fd + 字节，对端只收字节（fd 路径不 panic/err；fd 端到端传达由
    /// loopback_dma 真 server mmap 证）。
    #[test]
    fn send_with_one_fd_ok() {
        DefaultPool::run_with(async |driver| {
            let (a, b) = UnixStream::pair().unwrap();
            let sa = AsyncSocket::new(PolledSocket::new(&driver, a).unwrap());
            let sb = AsyncSocket::new(PolledSocket::new(&driver, b).unwrap());
            let devnull = std::fs::File::open("/dev/null").unwrap();
            let payload = b"dma-map".to_vec();
            let iov = [IoSlice::new(&payload)];
            sa.send_with_fds(&iov, &[devnull.as_fd()]).await.unwrap();
            let mut got = vec![0u8; payload.len()];
            sb.recv_exact(&mut got).await.unwrap();
            assert_eq!(got, payload);
        });
    }
}
```

- [ ] **Step 3: lib.rs 声明模块 + 去 crate 级 deny**

Replace `usnvmemu/crates/vfio_user_device/src/lib.rs:21-28`（即从 `#![deny(unsafe_code)]` 到文件尾）为：

```rust
//! **async 收发**（W6a，2026-06-12）：迁到 pal_async `PolledSocket`，照搬
//! `vhost_user_protocol::socket` 的 SCM_RIGHTS 混合范式。**unsafe 隔离在
//! [`async_socket`] 一个模块**（裸 libc cmsg）；`framing`/`client` 模块级
//! `#![deny(unsafe_code)]` 维持纯 safe。**新增模块默认须加模块级
//! `#![deny(unsafe_code)]`，仅 async_socket 例外**（crate 级 deny 已移除，防回归）。
//! 为 W6b/c/d 真 underhill 集成（pcie_remote 风格 async worker）铺路。
//!
//! **W6b 前瞻**：当前 `AsyncSocket` 用 `Mutex<PolledSocket>`（request/reply 串行足够）。
//! 若 W6b 需 server-initiated 消息（DMA_READ command）与 client request 并发收发，
//! 可改 `PolledSocket::split()` 的读写半；W6a 不预置（YAGNI，W6b 真需求未定型）。

mod async_socket;
mod client;
mod framing;

pub use client::VfioUserClient;
pub use vfio_user_wire::handshake::NegotiatedClient;
```

（删掉原 `#![deny(unsafe_code)]` 这一 crate 级行；同时把 lib.rs 顶部 doc 中"同步阻塞"那段描述更新——保留 W1-W4 功能列表，把"async 化留 W6"改成上面这段 async 已落地的描述。）

- [ ] **Step 4: 编译 + 跑 async_socket 单测**

Run: `cd usnvmemu/crates/vfio_user_device && cargo test --lib async_socket -- --nocapture`
Expected: `bytes_roundtrip_no_fds` 与 `send_with_one_fd_ok` PASS（此时 framing/client 还是旧同步代码、不引用 async_socket，但因 `mod async_socket;` 已声明且其测试自洽，`--lib` 能编过 async_socket；若 framing/client 旧代码与新 deps 冲突导致 `--lib` 编不过，跳到 Task 2 一起编——见 Task 2 Step 5 的整体编译）。

> **注意**：本 task 结束后 `cargo build` 可能因 framing/client 仍同步、且 lib.rs 已去 crate-deny 但 framing/client 未加模块-deny 而仅是 warning，不影响编译。async_socket 是新增独立模块。若 `--lib` 整体编译受 framing/client 影响，async_socket 的验证并入 Task 2 的整体 `cargo test`。

---

### Task 2: framing.rs + client.rs → async（核心原子迁移）

**Files:**
- Modify: `usnvmemu/crates/vfio_user_device/src/framing.rs`（整文件重写）
- Modify: `usnvmemu/crates/vfio_user_device/src/client.rs`（整文件改 async）

> 本 task 是原子迁移：framing 改 async 会破坏 client，故二者同 task 完成，结束时整 crate（`--lib`）编译 + 单测通过。

- [ ] **Step 1: 重写 framing.rs（async，统一 write_message）**

整文件替换 `usnvmemu/crates/vfio_user_device/src/framing.rs` 为：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! client 端线消息收发（**async**，W6a）。
//!
//! 基于 [`crate::async_socket::AsyncSocket`]（pal_async PolledSocket + SCM_RIGHTS）。
//! 字节布局与 server 端 `vfio_user_transport::framing` 一致（同一份
//! `vfio_user_wire::proto` 编解码），仅收发原语不同（client async）。
//!
//! 本模块纯 safe（cmsg/unsafe 全在 [`crate::async_socket`]）。
#![deny(unsafe_code)]

use crate::async_socket::AsyncSocket;
use anyhow::Context as _;
use std::io::IoSlice;
use std::os::fd::BorrowedFd;
use vfio_user_wire::framing::WireMessage;
use vfio_user_wire::proto::HEADER_LEN;
use vfio_user_wire::proto::Header;
use vfio_user_wire::proto::decode_header;
use zerocopy::IntoBytes; // Header::as_bytes() 需要。

/// 写一帧（header + payload），可经 SCM_RIGHTS 附带 `fds`（空 slice = 无 fd）。
///
/// fd 必须随首字节发出（vfio-user spec：所有 fd 跟首字节走）；[`AsyncSocket::send_with_fds`]
/// 保证 fd 只随首次 sendmsg + 短写循环补齐。统一了原 `write_message` / `write_message_with_fds`。
pub(crate) async fn write_message(
    sock: &AsyncSocket,
    header: &Header,
    payload: &[u8],
    fds: &[BorrowedFd<'_>],
) -> anyhow::Result<()> {
    let hdr_bytes = header.as_bytes();
    let iov = [IoSlice::new(hdr_bytes), IoSlice::new(payload)];
    sock.send_with_fds(&iov, fds)
        .await
        .context("write_message: send_with_fds")?;
    Ok(())
}

/// 读一帧（header + payload，无 fd）。先读定长 header，据 `msg_size` 读 payload。
pub(crate) async fn read_message(sock: &AsyncSocket) -> anyhow::Result<WireMessage> {
    let mut hdr_buf = [0u8; HEADER_LEN];
    sock.recv_exact(&mut hdr_buf)
        .await
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
    sock.recv_exact(&mut payload)
        .await
        .context("read_message: read payload")?;
    Ok(WireMessage { header, payload })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::async_socket::AsyncSocket;
    use pal_async::DefaultPool;
    use pal_async::socket::PolledSocket;
    use std::os::unix::net::UnixStream;
    use vfio_user_wire::proto::Command;

    /// header + payload roundtrip（两端 AsyncSocket，无 fd）。
    #[test]
    fn roundtrip_header_and_payload() {
        DefaultPool::run_with(async |driver| {
            let (a, b) = UnixStream::pair().unwrap();
            let sa = AsyncSocket::new(PolledSocket::new(&driver, a).unwrap());
            let sb = AsyncSocket::new(PolledSocket::new(&driver, b).unwrap());
            let payload = b"hello-vfio-user".to_vec();
            let hdr = Header::command(7, Command::Version, payload.len() as u32);
            write_message(&sa, &hdr, &payload, &[]).await.unwrap();
            let got = read_message(&sb).await.unwrap();
            let msg_id = got.header.msg_id; // packed 字段先 copy
            assert_eq!(msg_id, 7);
            assert_eq!(got.payload, payload);
        });
    }

    /// 空 payload roundtrip。
    #[test]
    fn roundtrip_header_only() {
        DefaultPool::run_with(async |driver| {
            let (a, b) = UnixStream::pair().unwrap();
            let sa = AsyncSocket::new(PolledSocket::new(&driver, a).unwrap());
            let sb = AsyncSocket::new(PolledSocket::new(&driver, b).unwrap());
            let hdr = Header::command(1, Command::Version, 0);
            write_message(&sa, &hdr, &[], &[]).await.unwrap();
            let got = read_message(&sb).await.unwrap();
            let msg_id = got.header.msg_id; // packed 字段先 copy
            assert_eq!(msg_id, 1);
            assert!(got.payload.is_empty());
        });
    }

    /// write_message 带 1 fd：对端收回 header+payload（fd 路径不破坏字节流；fd 真正
    /// 传达由 loopback_dma 真 server mmap 端到端证）。
    #[test]
    fn write_with_fds_sends_header_and_payload() {
        use std::os::fd::AsFd;
        DefaultPool::run_with(async |driver| {
            let (a, b) = UnixStream::pair().unwrap();
            let sa = AsyncSocket::new(PolledSocket::new(&driver, a).unwrap());
            let sb = AsyncSocket::new(PolledSocket::new(&driver, b).unwrap());
            let devnull = std::fs::File::open("/dev/null").unwrap();
            let payload = b"dma-map-payload".to_vec();
            let hdr = Header::command(3, Command::DmaMap, payload.len() as u32);
            write_message(&sa, &hdr, &payload, &[devnull.as_fd()])
                .await
                .unwrap();
            let got = read_message(&sb).await.unwrap();
            let msg_id = got.header.msg_id; // packed 字段先 copy
            assert_eq!(msg_id, 3);
            assert_eq!(got.payload, payload);
        });
    }
}
```

- [ ] **Step 2: 改 client.rs 头部 + struct + 构造器为 async**

Replace `usnvmemu/crates/vfio_user_device/src/client.rs:1-70`（从文件头到 `alloc_msg_id` 结束）为：

```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! vfio-user client：connect AF_UNIX + VERSION 握手（**async**，W6a）。
//!
//! 本模块纯 safe（cmsg/unsafe 全在 [`crate::async_socket`]）。
#![deny(unsafe_code)]

use crate::async_socket::AsyncSocket;
use crate::framing::read_message;
use crate::framing::write_message;
use anyhow::Context as _;
use anyhow::anyhow;
use pal_async::driver::Driver;
use pal_async::socket::PolledSocket;
use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use vfio_user_wire::framing::WireMessage;
use vfio_user_wire::handshake::CLIENT_CAPS_JSON;
use vfio_user_wire::handshake::NegotiatedClient;
use vfio_user_wire::handshake::build_version_command_payload;
use vfio_user_wire::handshake::parse_caps_blob;
use vfio_user_wire::handshake::verify_server_version_reply;
use vfio_user_wire::proto::Command;
use vfio_user_wire::proto::DeviceInfoPayload;
use vfio_user_wire::proto::DmaMapPayload;
use vfio_user_wire::proto::DmaUnmapPayload;
use vfio_user_wire::proto::Header;
use vfio_user_wire::proto::IrqInfoPayload;
use vfio_user_wire::proto::IrqSetPayload;
use vfio_user_wire::proto::PROTOCOL_MAJOR;
use vfio_user_wire::proto::PROTOCOL_MINOR;
use vfio_user_wire::proto::RegionAccessPayload;
use vfio_user_wire::proto::RegionInfoPayload;
use vfio_user_wire::proto::VersionPayload;
use vfio_user_wire::proto::decode_payload;
use vfio_user_wire::proto::dma_unmap_flags;
use vfio_user_wire::proto::irq_set;
use zerocopy::IntoBytes;

/// vfio-user client 连接。持一条 async [`AsyncSocket`] + msg_id 计数器。
pub struct VfioUserClient {
    sock: AsyncSocket,
    /// 下一个请求的 msg_id（W1 握手用 1，W2 起每请求递增；reply 须 echo 同 id）。
    next_msg_id: u16,
}

impl VfioUserClient {
    /// connect 到 server 的 AF_UNIX socket 路径（async）。
    pub async fn connect(
        driver: &(impl ?Sized + Driver),
        path: impl AsRef<Path>,
    ) -> anyhow::Result<Self> {
        let ps = PolledSocket::connect_unix(driver, path.as_ref())
            .await
            .with_context(|| format!("connect vfio-user socket {:?}", path.as_ref()))?;
        Ok(Self {
            sock: AsyncSocket::new(ps),
            next_msg_id: 1,
        })
    }

    /// 从已建立的 `UnixStream` 构造（loopback 单测 / 已 connect 的场景）。包成
    /// `PolledSocket`（需 `driver`）。
    pub fn from_stream(
        driver: &(impl ?Sized + Driver),
        stream: UnixStream,
    ) -> anyhow::Result<Self> {
        let ps = PolledSocket::new(driver, stream).context("wrap UnixStream in PolledSocket")?;
        Ok(Self {
            sock: AsyncSocket::new(ps),
            next_msg_id: 1,
        })
    }

    /// 取下一个 msg_id 并自增（wrapping，避免溢出 panic）。
    fn alloc_msg_id(&mut self) -> u16 {
        let id = self.next_msg_id;
        self.next_msg_id = self.next_msg_id.wrapping_add(1);
        id
    }
```

- [ ] **Step 3: 把 client.rs 所有方法体改 async + `.await` + 新 write_message 签名**

对 `client.rs` 余下方法做如下机械改造（逐方法，签名 `pub fn`/`fn` → `pub async fn`/`async fn`，所有 `write_message(&mut self.stream, &hdr, &payload)` → `write_message(&self.sock, &hdr, &payload, &[]).await`，`write_message_with_fds(&mut self.stream, &hdr, bytes, &[raw])` → `write_message(&self.sock, &hdr, bytes, &[borrowed]).await`，`read_message(&mut self.stream)` → `read_message(&self.sock).await`）：

1. `handshake`：`pub async fn handshake(&mut self)`；`write_message(&self.sock, &hdr, &payload, &[]).await.context("send VERSION command")?;`；`let reply = read_message(&self.sock).await.context("recv VERSION reply")?;`。其余帧校验/解析逻辑**完全不变**。

2. `expect_reply`：保持 `fn`（纯函数，不收发，不加 async）。

3. `request<T>`：`async fn request<T: ...>(&mut self, cmd, req)`；内部
```rust
        write_message(&self.sock, &hdr, payload, &[]).await.with_context(|| format!("send {cmd:?}"))?;
        let reply = read_message(&self.sock).await.with_context(|| format!("recv {cmd:?} reply"))?;
```

4. `decode_reply_payload`：保持 `fn`（纯解析）。

5. `get_device_info` / `get_region_info` / `get_irq_info`：`pub async fn`；`let reply = self.request(...).await?;`。

6. `region_read`：`pub async fn region_read(...)`；`let reply = self.request(Command::RegionRead, &req).await?;`。其余 echo 校验不变。

7. `region_write`：`pub async fn region_write(...)`；
```rust
        write_message(&self.sock, &hdr, &payload, &[]).await.context("send REGION_WRITE")?;
        let reply = read_message(&self.sock).await.context("recv REGION_WRITE reply")?;
```

8. `reset`：`pub async fn reset(&mut self)`；`write_message(&self.sock, &hdr, &[], &[]).await...; let reply = read_message(&self.sock).await...;`。

9. `dma_map`：`pub async fn dma_map(...)`；fd 用 `BorrowedFd`（参数已是 `fd: BorrowedFd<'_>`），改：
```rust
        write_message(&self.sock, &hdr, req.as_bytes(), &[fd]).await.context("send DMA_MAP")?;
        let reply = read_message(&self.sock).await.context("recv DMA_MAP reply")?;
```
（删去 `fd.as_raw_fd()`——新 write_message 收 `&[BorrowedFd]`。）

10. `dma_unmap` / `dma_unmap_all`：`pub async fn`；`let _reply = self.request(...).await?;`。

11. `set_irqs`：`pub async fn set_irqs(&mut self, index, start, eventfds: &[BorrowedFd<'_>])`；删去 `let raw: Vec<RawFd> = ...`，直接把 `eventfds` 传给 `send_set_irqs`：
```rust
    pub async fn set_irqs(
        &mut self,
        index: u32,
        start: u32,
        eventfds: &[BorrowedFd<'_>],
    ) -> anyhow::Result<()> {
        let req = IrqSetPayload {
            argsz: core::mem::size_of::<IrqSetPayload>() as u32,
            flags: irq_set::DATA_EVENTFD | irq_set::ACTION_TRIGGER,
            index,
            start,
            count: eventfds.len() as u32,
        };
        self.send_set_irqs(&req, eventfds).await
    }
```

12. `set_irqs_deassign` / `set_irqs_clear`：`pub async fn`；末行 `self.send_set_irqs(&req, &[]).await`。

13. `send_set_irqs`：签名改
```rust
    async fn send_set_irqs(&mut self, req: &IrqSetPayload, fds: &[BorrowedFd<'_>]) -> anyhow::Result<()> {
        let msg_id = self.alloc_msg_id();
        let hdr = Header::command(msg_id, Command::DeviceSetIrqs, core::mem::size_of::<IrqSetPayload>() as u32);
        write_message(&self.sock, &hdr, req.as_bytes(), fds).await.context("send SET_IRQS")?;
        let reply = read_message(&self.sock).await.context("recv SET_IRQS reply")?;
        Self::expect_reply(&reply, msg_id, Command::DeviceSetIrqs)?;
        Ok(())
    }
```

> 删除不再需要的 import：`std::os::fd::AsRawFd`、`std::os::fd::RawFd`、`nix::*`、`std::io::{Read,Write,IoSlice}`（这些原在 framing/client，现 framing 重写、client 不再直接收发）。保留 `BorrowedFd`、`zerocopy::IntoBytes`。

- [ ] **Step 4: 改 client.rs 的 in-file 测试为 async**

Replace `client.rs` 末尾 `#[cfg(test)] mod tests { ... }` 为：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::DefaultPool;
    use std::thread;
    use vfio_user_wire::proto::Header;

    /// server 回 error reply（flags F_ERROR）→ client handshake 报错。server thread
    /// 仍用**同步** server 端原语（独立线程），client async。
    #[test]
    fn handshake_rejects_error_reply() {
        let (server_end, client_end) = UnixStream::pair().unwrap();
        // server thread：同步读 client VERSION command（丢弃）+ 回 error reply。
        // 用 server 侧 vfio_user_transport::framing 同步原语，避免 client async 依赖。
        let srv = thread::spawn(move || {
            use vfio_user_transport::framing as srv_framing;
            let mut s = server_end;
            let _ = srv_framing::read_message(&mut s).unwrap();
            let hdr = Header::reply_err(1, Command::Version, 71); // EPROTO
            // server write_message 是 4 参数（末尾 fds: &[RawFd]）；空 fds。
            srv_framing::write_message(&mut s, &hdr, &[], &[]).unwrap();
        });
        DefaultPool::run_with(async |driver| {
            let mut client = VfioUserClient::from_stream(&driver, client_end).unwrap();
            let err = client.handshake().await.unwrap_err();
            assert!(format!("{err:#}").contains("error"), "应报 error reply：{err:#}");
        });
        srv.join().unwrap();
    }
}
```

> **签名/前置确认**（执行时核对，按真实情况微调）：
> - `vfio_user_transport::framing` 是 `pub mod`（lib.rs:44），`read_message`/`write_message` re-export 到 crate 根（lib.rs:58-59），可达。
> - server `write_message` 签名是 **4 参数** `(&mut UnixStream, &Header, &[u8], &[RawFd])`（framing.rs:216）——上面已传空 `&[]` fds。
> - server `write_message` 内有 `debug_assert_eq!(hdr_bytes.len()+payload.len(), header.msg_size)`（framing.rs:249）。`Header::reply_err(1, Command::Version, 71)` 须使 `msg_size == HEADER_LEN`（空 payload）。**执行前确认 `vfio_user_wire::proto::Header::reply_err`（proto.rs:203）把 `msg_size` 设为 `HEADER_LEN`**，否则 debug build 下 server thread panic。W1 已用过 `reply_err` 走真 server，大概率 OK，但因 debug_assert 在，必须确认。
> - **不要**残留 `use crate::framing::read_message;`（旧同步测试遗留；新测试 server 用 `vfio_user_transport::framing`、client 用 `VfioUserClient::handshake`，都不直接调 client framing）——会触发 `unused_import`，在 `-D warnings` 下 clippy 失败。

- [ ] **Step 5: 整 crate 编译 + lib 单测**

Run: `cd usnvmemu/crates/vfio_user_device && cargo test --lib -- --nocapture`
Expected: async_socket（2）+ framing（3）+ client（1）共 6 个 lib 单测全 PASS。`cargo build` 无 error（warning 容后 clippy 清）。

Run: `cargo clippy --lib --all-features -- -D warnings`
Expected: 0 warning（gnu target 上 try_send/try_recv 的 cfg_attr/allow 已照搬 vhost_user 处理 cmsghdr 类型差异）。

- [ ] **Step 6: 强制清 unused import（`-D warnings` 门）**

`-D warnings` 下任何残留 unused import 都 fail。clippy 报的逐个删，并显式 grep 确认迁移残留已清：

Run:
```bash
cd usnvmemu/crates/vfio_user_device
grep -rn "nix::\|AsRawFd\|RawFd\|std::io::Read\|std::io::Write" src/client.rs src/framing.rs || echo "clean"
```
Expected: `client.rs` / `framing.rs` 不再残留 `nix::`、`AsRawFd`、`RawFd`、`std::io::{Read,Write}`（这些是同步时代的收发原语，async 后全在 `async_socket.rs`）。`async_socket.rs` 内有 `RawFd`/`AsRawFd` 是预期的（cmsg 层）。

---

### Task 3: 4 个 loopback 集成测 → async

**Files:**
- Modify: `usnvmemu/crates/vfio_user_device/tests/loopback_handshake.rs`
- Modify: `usnvmemu/crates/vfio_user_device/tests/loopback_region.rs`
- Modify: `usnvmemu/crates/vfio_user_device/tests/loopback_dma.rs`
- Modify: `usnvmemu/crates/vfio_user_device/tests/loopback_msix.rs`

> 每个测试的改造模式**统一**：(a) 顶部加 `use pal_async::DefaultPool;`；(b) 测试 `#[test] fn xxx()` 体里，**server thread 部分完全不动**（仍同步 `VfioUserSession` / `server_handshake`）；(c) **client 部分**包进 `DefaultPool::run_with(async |driver| { ... })`，`VfioUserClient::from_stream(client_end)` → `VfioUserClient::from_stream(&driver, client_end).unwrap()`，所有 `client.xxx(...)?` → `client.xxx(...).await?`（注意 `run_with` 闭包返回 `()`，用 `.unwrap()` 替代 `?`，或闭包返回 `anyhow::Result<()>` 后在外 unwrap）；(d) `srv.join()` 移到 `run_with` 之后。

- [ ] **Step 1: 改 loopback_handshake.rs**

读现文件，按上述模式改：client 段包进 `DefaultPool::run_with(async |driver| {...})`，`from_stream(&driver, client_end).unwrap()`，`client.handshake().await` 等。server thread 不动。

Run: `cargo test --test loopback_handshake -- --nocapture`
Expected: PASS（握手协商剧本不变）。

- [ ] **Step 2: 改 loopback_region.rs**

同模式。client 的 `get_device_info`/`get_region_info`/`get_irq_info`/`region_read`/`region_write`/`reset` 全加 `.await`。剧本断言（num_regions=9/num_irqs=5/BAR0 8192/MSIX count=8/identity dword0/RESET）不变。

Run: `cargo test --test loopback_region -- --nocapture`
Expected: PASS。

- [ ] **Step 3: 改 loopback_dma.rs**

同模式。`client.dma_map(gpa, size, flags, fd, fd_offset).await?` —— 注意 `dma_map` 第 4 参现是 `BorrowedFd<'_>`（W3 已是），签名未变，只加 `.await`。三重证（反证/read_at GPA_DST marker/on_dma_complete data marker）逻辑不变；DMA 完成回调那部分（server 侧 thread）不动。

Run: `cargo test --test loopback_dma -- --nocapture`
Expected: PASS（**fd-passing 端到端 oracle**：server mmap client 传的 fd 零拷贝命中）。

- [ ] **Step 4: 改 loopback_msix.rs**

同模式。`client.set_irqs(index, start, &eventfds).await?`（eventfds 仍 `&[BorrowedFd]`，签名未变）。eventfd 计数验证（==1 + 其它 EAGAIN）不变；server fire 那部分不动。

Run: `cargo test --test loopback_msix -- --nocapture`
Expected: PASS。

- [ ] **Step 5: 全 crate 测试 + clippy**

Run: `cargo test -- --nocapture`
Expected: lib 6 + loopback 4 = 10 个测试全 PASS（baseline device 8 测 → 现 6 lib（async_socket 2 新增、framing 3、client 1）+ 4 loopback；数目变动因 framing 测从 3 保留、async_socket 新增 2、client 1 保留）。

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: 0 warning。

---

### Task 4: W5a harness async-compile + 全量验证

**Files:**
- Modify: `usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/client/Cargo.toml`
- Modify: `usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/client/src/main.rs`

> W5a harness client 用同步 `VfioUserClient` API（`from_stream(stream)` + `client.handshake()` 等）。client 改 async 后必须更新它**至少能编译**（cross-build musl）。真 VM re-verify 推迟（W6b 真集成会重跑真 VM；本次只保证不破坏树 + 静态构建可过）。

- [ ] **Step 1: harness Cargo.toml 加 pal_async**

在 `usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/client/Cargo.toml` 的 `[dependencies]` 加：

```toml
pal_async = { path = "../../../../support/pal/pal_async" }
```

（路径：harness client 在 `usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/client/`，到 repo root 是 `../../../../`，再 `support/pal/pal_async`。执行时 `ls` 确认该相对路径存在。）

- [ ] **Step 2: harness main.rs 包 async runtime**

改 `main.rs`：把 `fn main() -> anyhow::Result<()> { ... }` 的**第 2 步起**（connect + 握手 + 后续全部 client 调用）包进 `pal_async::DefaultPool::run_with(async |driver| -> anyhow::Result<()> { ... })`：

- 顶部加 `use pal_async::DefaultPool;`
- `let stream = UnixStream::connect(&sock)...?;` + `let mut client = VfioUserClient::from_stream(stream);` 改为：
```rust
    DefaultPool::run_with(async |driver| -> anyhow::Result<()> {
        let stream = UnixStream::connect(&sock).context("connect firmware sock")?;
        let mut client = VfioUserClient::from_stream(&driver, stream)
            .context("wrap stream")?;
        let neg = client.handshake().await.context("handshake")?;
        // ... 余下所有 client.xxx(...) 调用全加 .await ...
        // mmap/ram 那段（第 1 步）保持在 run_with 之前（不涉 async）；ram 引用需移进闭包或
        // 用 move 捕获——执行时把 step 1 的 mmap 也挪进 run_with 闭包内最前，最简。
        Ok(())
    })?;
    Ok(())
```
> 最简做法：把整个 `main` body（mmap + connect + NVMe bring-up + 验证）全挪进 `DefaultPool::run_with(async |driver| -> anyhow::Result<()> {...})` 闭包，client 调用加 `.await`，`from_stream(&driver, stream)`。`client.region_write/region_read/dma_map` 全加 `.await`。

- [ ] **Step 3: harness 静态构建验证（不跑真 VM）**

Run: `cd usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/client && cargo build --release --target x86_64-unknown-linux-musl 2>&1 | tail -20`
Expected: 编译成功（musl static）。若 musl target 未装，退而验 host：`cargo build --release`，并在 README 注明 musl 构建待 W6b 真集成时连同真 VM 一并验。

- [ ] **Step 4: harness README 注明 async 迁移**

在 `usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/README.md` 末尾加一节：

```markdown
## W6a async 迁移注记（2026-06-12）

`vfio_user_device` client 在 W6a 迁到 async（pal_async PolledSocket）。本 harness client
已同步更新为 async-compile（`DefaultPool::run_with` 包裹 + `.await`）。**上面记录的真机
e2e PASS 结果是 W5a（commit 6a633c4b，pre-async）的**；async 版 harness 仅验证了编译，
真 VM re-verify 随 W6b 真 underhill 集成一并重跑（W6b 会以 pcie_remote 风格 async worker
驱动 client，是更有代表性的真机验证点）。wire 行为与 pre-async 一致（同 `vfio_user_wire`
编解码 + 同 SCM_RIGHTS 字节），async 只改收发调度。
```

- [ ] **Step 5: 全量非回归验证 + 单 commit**

Run（验证不波及其它 crate）：
```bash
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_device && cargo test && cargo clippy --all-targets -- -D warnings
cd /home/xp/refs/openvmm/usnvmemu/crates/nvme_of_tcp_target && cargo test --lib 2>&1 | tail -5   # 非回归（不依赖 vfio_user_device，应不受影响）
cd /home/xp/refs/openvmm/usnvmemu/crates/vfio_user_transport && cargo test 2>&1 | tail -5         # server 未动，应 68 不退化
```
Expected: vfio_user_device 全绿 + clippy 0；transport/nvme_of 不退化。

Run（单 coarse commit，限定 pathspec，**不 `git add`、不 `git add -A`**——`git commit -- <pathspec>` 会自动 stage 这些路径的改动，等价且不污染 index 其余部分，遵 [[git-commit-shared-index-multisession]] 教训）：
```bash
cd /home/xp/refs/openvmm
git status   # 先看清 index：确认无对方会话 staged 的改动会被裹入
git commit -- usnvmemu/crates/vfio_user_device/ \
              usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/ \
              docs/superpowers/plans/2026-06-12-w6a-vfio-user-device-async.md \
  -m "feat(vfio_user_device): W6a client async 化（pal_async PolledSocket + SCM_RIGHTS）

照搬 vhost_user_protocol::socket 混合范式（PolledSocket readiness + 裸 libc cmsg fd
传递），client 全 async fn；unsafe 隔离到 async_socket 单模块（其余模块 deny）。
loopback 4 测 + lib 6 测 standalone PASS；server/transport/nvme_of 不退化。W6b/c/d 真
underhill 集成的前置。"
```

---

## Self-Review

**1. Spec coverage：** W6a = "vfio_user_device async 化"（explore C 段推荐的 W6 第一步、standalone 可测前置）。覆盖：async socket 层（Task 1）+ framing/client async（Task 2）+ loopback 测迁移（Task 3）+ harness 兼容 + 验证（Task 4）。✓

**2. Placeholder scan：** 无 TBD/TODO。所有 code step 给完整代码或精确机械改造清单（Task 2 Step 3 列了 13 个方法的逐一改法）。Task 2 Step 4 / Task 4 Step 1 标了"执行时确认相对路径/签名"——这是**真实需验证的外部接口事实**（server framing pub 签名、harness 相对路径深度），非占位；执行者按真实情况微调。

**3. Type consistency：**
- `AsyncSocket::new(PolledSocket<UnixStream>)` / `send_with_fds(&[IoSlice], &[impl AsFd])` / `recv_exact(&mut [u8])` —— framing 与测试调用一致。
- `write_message(&AsyncSocket, &Header, &[u8], &[BorrowedFd])` —— client 所有发送点、framing 测试一致（统一了原 write_message/write_message_with_fds）。
- `read_message(&AsyncSocket) -> WireMessage` —— client 所有接收点一致。
- `VfioUserClient::from_stream(&driver, UnixStream)` / `connect(&driver, path)` —— loopback 测 + harness 一致。
- `set_irqs(.., &[BorrowedFd])` / `dma_map(.., BorrowedFd, ..)` —— 签名 fd 类型与 framing `&[BorrowedFd]` 贯通（删了中间 `Vec<RawFd>`）。

**风险复核：**
- `run_with(async |driver| {...})` 闭包返回 `()` vs `anyhow::Result<()>`：测试里用 `.unwrap()`（闭包返 `()`）最简；harness 用 `-> anyhow::Result<()>` + 外层 `?`。POC 已验两种都编过。
- gnu/musl cmsghdr 差异：`try_send`/`try_recv` 的 `#[cfg_attr(target_env="gnu", ...)]` + `#[allow(clippy::unnecessary_cast)]` 照搬 vhost_user 原样，两 target 都过。
- nix 降 dev-dep：确认 client.rs 生产路径不再有 `nix::` 引用（收发改 libc/AsyncSocket）；仅 loopback_msix/loopback_dma 测建 eventfd/用 nix → dev-dep 足够。执行 Task 2 后 `grep -rn "nix::" src/` 应为空。
