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
use pal_async::socket::ReadHalf;
use pal_async::socket::WriteHalf;
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
    /// SCM_RIGHTS fd（防泄漏）；非 SCM_RIGHTS ancillary 报 InvalidData。任意位置读到 0
    /// （对端干净关闭 / 半包断开）→ `UnexpectedEof`。
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

    /// 取出内部 `PolledSocket`（消费 `AsyncSocket`）。供 [`split_full_duplex`] 拆全双工
    /// 读写半用（W6b worker 需并发收发）。`Mutex::into_inner` 无锁、无 panic。
    pub(crate) fn into_polled(self) -> PolledSocket<UnixStream> {
        self.socket.into_inner()
    }

    /// **CMB-P4 (map 模式)** — 收满 `buf.len()` 字节，并**捕获**随首字节到达的 SCM_RIGHTS
    /// fd（vfio-user spec：所有 fd 跟首字节走）。返回收到的 [`OwnedFd`]（drop 自动 close）。
    ///
    /// 与 [`recv_exact`](Self::recv_exact)（丢弃意外 fd）的区别：本方法**期待**并保留 fd
    /// （map 模式的 `GET_REGION_INFO` reply 带 CMB region memfd）。fd 仅在首段（含首字节）
    /// 收集——续读段若再带 fd（spec 不应发生）由 [`recv_exact`] 语义 drop 防泄漏。
    pub(crate) async fn recv_exact_with_fds(&self, buf: &mut [u8]) -> io::Result<Vec<OwnedFd>> {
        let mut read = 0;
        let mut fds: Vec<OwnedFd> = Vec::new();
        while read < buf.len() {
            let first = read == 0;
            let (n, got) = poll_fn(|cx| {
                self.socket
                    .lock()
                    .poll_io(cx, InterestSlot::Read, PollEvents::IN, |socket| {
                        try_recv_collect_fds(socket.get(), &mut buf[read..], first)
                    })
            })
            .await?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            fds.extend(got);
            read += n;
        }
        Ok(fds)
    }
}

/// 把一条 `PolledSocket` 拆成全双工读写半（W6b）。两半经 pal_async 内部 `Arc` 共享同一
/// socket，但 send/recv 各自 `poll_io`、**不跨内核调用持锁**——故 worker 可在 `select!`
/// 里同时发请求帧与收 reply 帧，互不阻塞（串行 `AsyncSocket` 的 `Mutex<PolledSocket>`
/// 做不到）。
pub(crate) fn split_full_duplex(socket: PolledSocket<UnixStream>) -> (WriterHalf, ReaderHalf) {
    let (read, write) = socket.split();
    (WriterHalf { write }, ReaderHalf { read })
}

/// 全双工**写**半：只发不收。复用 [`try_send`]/[`build_remaining_iov`]（cmsg/unsafe
/// 全在本模块自由函数里，不重复）。
pub(crate) struct WriterHalf {
    write: WriteHalf<UnixStream>,
}

impl WriterHalf {
    /// 发一组 iovec（首段带可选 fd，经 SCM_RIGHTS）。语义同 [`AsyncSocket::send_with_fds`]，
    /// 仅驱动 [`WriteHalf::poll_io`]（读写半各自 poll，不互锁）。fd 只随首次 sendmsg。
    pub(crate) async fn send_with_fds(
        &mut self,
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
                self.write
                    .poll_io(cx, |h| try_send(h.get(), &remaining_iov, send_fds))
            })
            .await?;
            sent += n;
        }
        Ok(())
    }
}

/// 全双工**读**半：只收不发。复用 [`try_recv`]（含意外-fd drop 防泄漏路径）。
pub(crate) struct ReaderHalf {
    read: ReadHalf<UnixStream>,
}

impl ReaderHalf {
    /// 收满 `buf.len()` 字节。语义同 [`AsyncSocket::recv_exact`]，仅驱动
    /// [`ReadHalf::poll_io`]。读到 0（对端关闭 / 半包断开）→ `UnexpectedEof`。
    pub(crate) async fn recv_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let mut read = 0;
        while read < buf.len() {
            let n = poll_fn(|cx| {
                self.read
                    .poll_io(cx, |h| try_recv(h.get(), &mut buf[read..]))
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
    assert!(fds.len() <= MAX_FDS, "fd 过多：{} > {MAX_FDS}", fds.len());
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
    // client 现实只收 0 fd，但既写防泄漏路径就要完整（照搬 gold standard socket.rs）。
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

    // 收到任何 ancillary（client 从不期望）：SCM_RIGHTS fd 全 drop 防泄漏；其它类型的
    // ancillary 报 InvalidData（异常/恶意 server）——与 gold standard 一致，不静默吞。
    if hdr.msg_controllen > 0 {
        if cmsg.hdr.cmsg_level != libc::SOL_SOCKET || cmsg.hdr.cmsg_type != libc::SCM_RIGHTS {
            // 非 SCM_RIGHTS ancillary：无 fd 可泄漏，但属协议异常 → 报错。
            return Err(io::ErrorKind::InvalidData.into());
        }
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

/// **CMB-P4** — recvmsg 收字节并**捕获** SCM_RIGHTS fd（仅 `collect=true` 的首段；续段
/// 仍 drop 任何意外 fd 防泄漏）。返回 `(读到字节数, 收到的 OwnedFd)`。可能 `WouldBlock`
/// （poll_io 重试）。镜像 [`try_recv`] 的截断/非 SCM_RIGHTS 处理，仅在 `collect` 时把
/// SCM_RIGHTS fd 转成 `OwnedFd` 返回而非 drop。
#[expect(clippy::allow_attributes)]
#[allow(
    clippy::unnecessary_cast,
    reason = "libc::cmsghdr 在 gnu vs musl 上类型定义不同"
)]
fn try_recv_collect_fds(
    socket: &UnixStream,
    buf: &mut [u8],
    collect: bool,
) -> io::Result<(usize, Vec<OwnedFd>)> {
    assert!(!buf.is_empty());
    let mut iov = IoSliceMut::new(buf);

    // SAFETY: type has no invariants
    let mut cmsg: CmsgScmRights = unsafe { std::mem::zeroed() };
    // SAFETY: type has no invariants
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = std::ptr::from_mut(&mut iov).cast::<libc::iovec>();
    hdr.msg_iovlen = 1;
    hdr.msg_control = std::ptr::from_mut(&mut cmsg).cast::<libc::c_void>();
    hdr.msg_controllen = size_of_val(&cmsg) as _;

    // SAFETY: 用正确初始化的缓冲调用 recvmsg。
    let n = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut hdr, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Ok((0, Vec::new()));
    }

    // 截断：先 close 已安装的部分 fd 再返 EMSGSIZE（防泄漏，对齐 try_recv）。
    if hdr.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        if hdr.msg_controllen > 0
            && cmsg.hdr.cmsg_level == libc::SOL_SOCKET
            && cmsg.hdr.cmsg_type == libc::SCM_RIGHTS
        {
            let fd_count = ((cmsg.hdr.cmsg_len as usize).saturating_sub(size_of_val(&cmsg.hdr))
                / size_of::<RawFd>())
            .min(MAX_FDS);
            for &raw_fd in &cmsg.fds[..fd_count] {
                // SAFETY: 内核已把这些 fd 所有权转移给我们；drop 关闭。
                drop(unsafe { OwnedFd::from_raw_fd(raw_fd) });
            }
        }
        return Err(io::Error::from_raw_os_error(libc::EMSGSIZE));
    }

    let mut fds: Vec<OwnedFd> = Vec::new();
    if hdr.msg_controllen > 0 {
        if cmsg.hdr.cmsg_level != libc::SOL_SOCKET || cmsg.hdr.cmsg_type != libc::SCM_RIGHTS {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let fd_count = ((cmsg.hdr.cmsg_len as usize).saturating_sub(size_of_val(&cmsg.hdr))
            / size_of::<RawFd>())
        .min(MAX_FDS);
        for &raw_fd in &cmsg.fds[..fd_count] {
            // SAFETY: 内核已把这些 fd 所有权转移给我们。collect 时返回（owner 给 caller），
            // 否则 drop 关闭防泄漏。
            let owned = unsafe { OwnedFd::from_raw_fd(raw_fd) };
            if collect {
                fds.push(owned);
            } else {
                drop(owned);
            }
        }
    }
    Ok((n as usize, fds))
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

    /// **CMB-P4** — `recv_exact_with_fds` 捕获随首字节到达的 SCM_RIGHTS fd：peer 发
    /// payload + 1 个 memfd（写了 marker），client 收字节 **且** 拿到 OwnedFd；经捕获的
    /// fd 读文件应读到 peer 写的 marker（证 fd 真传达 + 同一 memfd）。这是 map 模式
    /// client 收 region fd 的底层能力（真零拷贝 mmap 闭环由 nvme_firmware 的 map e2e 证）。
    #[test]
    fn recv_exact_with_fds_captures_memfd() {
        use std::io::Read as _;
        use std::io::Seek as _;
        use std::io::Write as _;
        DefaultPool::run_with(async |driver| {
            let (a, b) = UnixStream::pair().unwrap();
            let client = AsyncSocket::new(PolledSocket::new(&driver, a).unwrap());
            // peer 建 memfd，写 marker，经 SCM_RIGHTS 发出。
            let memfd =
                nix::sys::memfd::memfd_create(c"recv-fd-test", nix::sys::memfd::MFdFlags::empty())
                    .unwrap();
            let mut f = std::fs::File::from(memfd);
            let marker = 0xABCD_1234_5678_9999u64;
            f.write_all(&marker.to_le_bytes()).unwrap();
            f.flush().ok();
            let memfd = std::os::fd::OwnedFd::from(f);

            let payload = b"region-fd-msg!!!".to_vec();
            let iov = [IoSlice::new(&payload)];
            let peer = AsyncSocket::new(PolledSocket::new(&driver, b).unwrap());
            peer.send_with_fds(&iov, &[memfd.as_fd()]).await.unwrap();

            // client 收字节 + 捕获 fd。
            let mut got = vec![0u8; payload.len()];
            let fds = client.recv_exact_with_fds(&mut got).await.unwrap();
            assert_eq!(got, payload);
            assert_eq!(fds.len(), 1, "应捕获到 1 个 region fd");

            // 经捕获的 fd 读文件（同一 memfd）→ 读到 peer 写的 marker。
            let mut cf = std::fs::File::from(fds.into_iter().next().unwrap());
            cf.seek(std::io::SeekFrom::Start(0)).unwrap();
            let mut buf = [0u8; 8];
            cf.read_exact(&mut buf).unwrap();
            assert_eq!(
                u64::from_le_bytes(buf),
                marker,
                "经捕获 fd 读到 peer 写入的 marker（同一 memfd）"
            );
        });
    }

    /// 防泄漏负向测试：对端（同步 nix sendmsg）发 payload + 1 个**意外** SCM_RIGHTS fd
    /// （client 从不期望收 fd）。`recv_exact` 仍成功返回字节，且意外 fd 被 drop 关闭
    /// （`try_recv` 的 `:236-247` SCM_RIGHTS 分支）——覆盖照搬自 gold standard 的防泄漏路径。
    #[test]
    fn recv_drops_unexpected_fd() {
        use nix::sys::socket::ControlMessage;
        use nix::sys::socket::MsgFlags;
        use nix::sys::socket::sendmsg;
        DefaultPool::run_with(async |driver| {
            let (a, b) = UnixStream::pair().unwrap();
            let client = AsyncSocket::new(PolledSocket::new(&driver, a).unwrap());
            // peer (b)：同步 nix sendmsg 发 payload + 1 个意外 SCM_RIGHTS fd（/dev/null）。
            let devnull = std::fs::File::open("/dev/null").unwrap();
            let payload = b"unexpected-fd!!!".to_vec();
            let iov = [IoSlice::new(&payload)];
            let fds = [devnull.as_raw_fd()];
            let cmsgs = [ControlMessage::ScmRights(&fds)];
            sendmsg::<()>(b.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
                .expect("peer sendmsg with unexpected fd");
            // client recv_exact：即便收到意外 fd 也成功返回字节（fd 被 drop 防泄漏，不报错）。
            let mut got = vec![0u8; payload.len()];
            client.recv_exact(&mut got).await.unwrap();
            assert_eq!(got, payload);
            drop(b);
        });
    }
}
