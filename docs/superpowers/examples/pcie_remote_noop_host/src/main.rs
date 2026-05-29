//! Minimal host stub for pcie_remote 实验。
//!
//! 仅 bind 127.0.0.1:48914（spec §3.2 路径 A/D loopback enforce）。
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
use tokio::net::TcpListener;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::compat::TokioAsyncWriteCompatExt;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // 安全 hard-code：只允许 127.0.0.1。CLI 不支持任何 `--bind`。
    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(48914);
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    if !addr.ip().is_loopback() {
        return Err(anyhow!("refusing non-loopback bind: {addr}"));
    }

    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "pcie_remote_noop_host: listening (loopback only)");

    loop {
        let (stream, peer) = listener.accept().await?;
        tracing::info!(%peer, "client connected");
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

    // Hello
    let hello: pcie_remote_protocol::Hello = codec::read_frame(&mut rd).await?;
    tracing::info!(
        magic = format_args!("{:#x}", hello.magic),
        version = hello.version,
        instance_id_len = hello.instance_id.len(),
        "received Hello"
    );

    // HelloAck
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
            None => {
                tracing::warn!(seq, "ToHost missing body");
            }
        }
    }
}
