//! Minimal host stub for pcie_remote 实验。
//!
//! 角色：**client**。OpenVMM 端是 server（spawn_tcp_handshakes bind 127.0.0.1:48914），
//! 本 host stub 主动 connect 该端口。
//!
//! 收到 Hello 后回 HelloAck 描述一个 noop 设备；
//! 任何 MMIO read 返回 0；cfg side-effect / write 静默丢弃。

use anyhow::Result;
use anyhow::anyhow;
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
use std::env;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::compat::TokioAsyncWriteCompatExt;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(48914);
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    if !addr.ip().is_loopback() {
        return Err(anyhow!("refusing non-loopback connect: {addr}"));
    }

    tracing::info!(%addr, "pcie_remote_test_harness: client mode — connecting to OpenVMM/OpenHCL");

    // 简单重试：OpenVMM 启动期 spawn_tcp_handshakes 可能还没 bind。
    let stream = loop {
        match TcpStream::connect(addr).await {
            Ok(s) => break s,
            Err(e) => {
                tracing::warn!(error = %e, "connect failed; retry in 200ms");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };
    tracing::info!("connected");
    if let Err(e) = serve(stream).await {
        tracing::warn!(error = %e, "session ended");
    }
    Ok(())
}

async fn serve(stream: TcpStream) -> Result<()> {
    let (rd, wr) = stream.into_split();
    let mut rd = rd.compat();
    let mut wr = wr.compat_write();

    // server 端会先发 Hello（按 spec §3.3，OpenVMM/OpenHCL 是协议主动方）
    let hello: pcie_remote_protocol::Hello = codec::read_frame(&mut rd).await?;
    tracing::info!(
        magic = format_args!("{:#x}", hello.magic),
        version = hello.version,
        instance_id_len = hello.instance_id.len(),
        "received Hello"
    );

    // 回 HelloAck
    let ack = HelloAck {
        ok: true,
        reason: String::new(),
        device: Some(DeviceDescribe {
            vendor_id: 0x1414,           // Microsoft
            device_id: 0xc0de,           // 实验设备
            class_code: 0x010802,        // NVMe storage
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
    tracing::info!("sent HelloAck");

    // 主循环
    loop {
        let req: ToHost = codec::read_frame(&mut rd).await?;
        let seq = req.seq;
        match req.body {
            Some(HostBody::MmioRead(m)) => {
                tracing::debug!(seq, bar = m.bar, offset = m.offset, size = m.size, "MMIO read");
                let resp = ToOpenhcl {
                    seq,
                    body: Some(OpenhclBody::MmioReadResult(MmioReadResult { value: 0 })),
                };
                codec::write_frame(&mut wr, &resp).await?;
            }
            Some(HostBody::MmioWrite(m)) => {
                tracing::debug!(
                    seq,
                    bar = m.bar,
                    offset = m.offset,
                    size = m.size,
                    value = format_args!("{:#x}", m.value),
                    "MMIO write (ignored)"
                );
            }
            Some(HostBody::CfgWriteSideEffect(c)) => {
                tracing::debug!(
                    seq,
                    offset = c.offset,
                    value = format_args!("{:#x}", c.value),
                    "cfg side-effect (ignored)"
                );
            }
            Some(HostBody::Reset(r)) => {
                tracing::info!(seq, kind = r.kind, "reset (ignored)");
            }
            Some(HostBody::DmaCompletion(d)) => {
                // 2026-06-08 audit-pass-4 修：搬到 usnvmemu/ 后发现
                // pcie_remote_protocol 早已加 DmaCompletion variant，
                // noop_host 是 noop 测试 stub，直接忽略即可（与 vsock_main
                // 同一处理）。
                tracing::debug!(seq, token = d.token, ok = d.ok, "DMA completion (ignored)");
            }
            None => {
                tracing::warn!(seq, "ToHost missing body");
            }
        }
    }
}
