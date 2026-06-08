// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 字节流 transport 抽象 + 两个内置 connector（TCP / vsock）。
//!
//! 注意：本文件的 `WireStream` 是 *字节流* 抽象（pcie_remote wire-protocol
//! 的承载体）；与 [`crate::Transport`] 这个 *设备原语 trait*（Phase T
//! 抽出，dma/mmio/interrupt 等高层动作）是两件事，不要混淆。
//!
//! SDK 不关心字节流 transport 类型 — 只要满足 `AsyncRead + AsyncWrite +
//! Unpin` 即可走 codec。便于测试（TCP loopback）和生产（vsock）共用。

use anyhow::Result;
use anyhow::anyhow;
use pal_async::driver::Driver;
use pal_async::socket::PolledSocket;

/// `run` 接受的字节流 transport 类型 = type-erased async byte stream。
///
/// Phase T 前曾叫 `Transport`，与新的设备原语 trait 同名容易混淆，故改名。
pub type WireStream = Box<dyn WireStreamTrait>;

/// 字节流 trait — `PolledSocket<T>` 满足。`PolledSocket<TcpStream>` 和
/// `PolledSocket<VmStream>` 自动 impl，所以两条 wire 共用一个 SDK。
pub trait WireStreamTrait:
    futures::io::AsyncRead + futures::io::AsyncWrite + Unpin + 'static
{
}

impl<T> WireStreamTrait for T where
    T: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin + 'static
{
}

/// 连入 OpenVMM TCP listener（开发/CI 用，guest 在同机 KVM）。
///
/// 调用方负责 retries / backoff —— 本函数失败即返 Err。
/// 用 socket2 + nonblocking 实现 async connect，不阻塞 worker。
pub async fn connect_tcp(driver: &impl Driver, addr: &str) -> Result<WireStream> {
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
) -> Result<WireStream> {
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
