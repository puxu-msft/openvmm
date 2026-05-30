// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Transport 抽象 + 两个内置 connector（TCP / vsock）。
//!
//! SDK 不关心 transport 类型 — 只要满足 `AsyncRead + AsyncWrite + Unpin`
//! 即可走 codec。便于测试（TCP loopback）和生产（vsock）共用。

use anyhow::Result;
use anyhow::anyhow;
use pal_async::driver::Driver;
use pal_async::socket::PolledSocket;

/// `run` 接受的 transport 类型 = type-erased async byte stream。
pub type Transport = Box<dyn TransportTrait>;

/// 抽象 trait — `PolledSocket<T>` 满足。`PolledSocket<TcpStream>` 和
/// `PolledSocket<VmStream>` 自动 impl，所以两条 transport 共用一个 SDK。
pub trait TransportTrait:
    futures::io::AsyncRead + futures::io::AsyncWrite + Unpin + 'static
{
}

impl<T> TransportTrait for T where
    T: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin + 'static
{
}

/// 连入 OpenVMM TCP listener（开发/CI 用，guest 在同机 KVM）。
///
/// 调用方负责 retries / backoff —— 本函数失败即返 Err。
/// 用 socket2 + nonblocking 实现 async connect，不阻塞 worker。
pub async fn connect_tcp(driver: &impl Driver, addr: &str) -> Result<Transport> {
    use std::net::SocketAddr;
    let socket_addr: SocketAddr = addr
        .parse()
        .map_err(|e| anyhow!("parse addr {addr}: {e}"))?;
    let domain = match socket_addr {
        SocketAddr::V4(_) => socket2::Domain::IPV4,
        SocketAddr::V6(_) => socket2::Domain::IPV6,
    };
    let sock = socket2::Socket::new(domain, socket2::Type::STREAM, None)
        .map_err(|e| anyhow!("socket2::Socket::new: {e}"))?;
    sock.set_nonblocking(true)
        .map_err(|e| anyhow!("set_nonblocking: {e}"))?;
    let mut polled: PolledSocket<socket2::Socket> =
        PolledSocket::new(driver, sock).map_err(|e| anyhow!("PolledSocket::new: {e}"))?;
    polled
        .connect(&socket_addr.into())
        .await
        .map_err(|e| anyhow!("async connect {addr}: {e}"))?;
    Ok(Box::new(polled))
}

/// 连入 Hyper-V VM 的 VTL2 vsock listener（生产 OpenHCL 路径，Windows-only）。
///
/// `port` 应匹配 `OPENHCL_PCIE_REMOTE_INSTANCE` cmdline 中的 `:port` 字段
/// （未指定时默认 50000）。
///
/// 调用方负责 retries / backoff。
#[cfg(windows)]
pub async fn connect_vsock(
    driver: &impl Driver,
    vm_id: guid::Guid,
    port: u32,
) -> Result<Transport> {
    use std::time::Duration;
    use vmsocket::VmAddress;
    use vmsocket::VmSocket;

    let socket = VmSocket::new()?;
    socket.set_connect_timeout(Duration::from_secs(2))?;
    socket.set_high_vtl(true)?;

    let raw_sock: socket2::Socket = socket.into();
    let mut polled: PolledSocket<socket2::Socket> = PolledSocket::new(driver, raw_sock)?;
    polled
        .connect(&VmAddress::hyperv_vsock(vm_id, port).into())
        .await?;

    let polled_vm: PolledSocket<vmsocket::VmStream> = polled.convert();
    Ok(Box::new(polled_vm))
}
