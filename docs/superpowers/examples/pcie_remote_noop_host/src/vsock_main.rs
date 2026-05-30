//! vsock client variant of noop host stub for OpenHCL Path C.
//!
//! Connects via AF_HYPERV to a Hyper-V VM's VTL2 vsock port (Windows-only).
//! On non-Windows, this binary errors out at runtime — vmsocket AF_HYPERV
//! is Windows-specific.
//!
//! Usage (Windows host):
//!   pcie_remote_noop_host_vsock --vm-id <guid> --port <number>

use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
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
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(about = "Noop pcie_remote host stub (vsock client variant).")]
struct Args {
    /// Hyper-V VM Id (Guid format).
    #[arg(long)]
    vm_id: String,
    /// vsock port (matches OPENHCL_PCIE_REMOTE_INSTANCE/TAKEOVER cmdline).
    #[arg(long, default_value_t = 50000)]
    port: u32,
    /// Connect retry count.
    #[arg(long, default_value_t = 20)]
    retries: u32,
    /// Per-retry sleep (ms).
    #[arg(long, default_value_t = 500)]
    retry_ms: u64,
}

#[cfg(not(windows))]
fn main() -> Result<()> {
    Err(anyhow!(
        "vsock variant requires Windows host (AF_HYPERV unavailable on this platform)"
    ))
}

#[cfg(windows)]
fn main() -> Result<()> {
    pal_async::DefaultPool::run_with(|driver| async move {
        tracing_subscriber::fmt::init();
        let args = Args::parse();
        let vm_id: guid::Guid = args
            .vm_id
            .parse()
            .map_err(|e| anyhow!("invalid vm_id {}: {e}", args.vm_id))?;
        tracing::info!(?vm_id, port = args.port, "vsock client starting (persistent reconnect loop)");

        // 外层 reconnect loop：guest OOBE 期间 VM 会 reboot 多次，每次
        // OpenHCL 重起新的 vsock listener，host 端需要重连维持设备 live。
        loop {
            let mut attempts = 0;
            let polled = loop {
                attempts += 1;
                match try_connect(&driver, vm_id, args.port).await {
                    Ok(p) => break p,
                    Err(e) if attempts < args.retries => {
                        tracing::warn!(error = %e, attempt = attempts, "connect failed; retrying");
                        pal_async::timer::PolledTimer::new(&driver)
                            .sleep(Duration::from_millis(args.retry_ms))
                            .await;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "connect retries exhausted");
                        return Err(e);
                    }
                }
            };
            tracing::info!("connected");
            // serve 直到 EOF 或 error → 立刻重连
            match serve(polled).await {
                Ok(()) => {
                    tracing::info!("serve returned normally; reconnecting after 1s");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "serve ended; reconnecting after 1s");
                }
            }
            pal_async::timer::PolledTimer::new(&driver)
                .sleep(Duration::from_secs(1))
                .await;
        }
    })
}

#[cfg(windows)]
async fn try_connect(
    driver: &pal_async::DefaultDriver,
    vm_id: guid::Guid,
    port: u32,
) -> Result<pal_async::socket::PolledSocket<vmsocket::VmStream>> {
    use pal_async::socket::PolledSocket;
    use vmsocket::VmAddress;
    use vmsocket::VmSocket;

    // 1) configure raw VmSocket (set_high_vtl etc.)
    let socket = VmSocket::new()?;
    socket.set_connect_timeout(Duration::from_secs(2))?;
    socket.set_high_vtl(true)?;

    // 2) polled wrapper over socket2::Socket to do async connect
    let raw_sock: socket2::Socket = socket.into();
    let mut polled: PolledSocket<socket2::Socket> = PolledSocket::new(driver, raw_sock)?;
    polled
        .connect(&VmAddress::hyperv_vsock(vm_id, port).into())
        .await?;

    // 3) convert PolledSocket<Socket> → PolledSocket<VmStream> for codec API
    let polled_vm: PolledSocket<vmsocket::VmStream> = polled.convert();
    Ok(polled_vm)
}

#[cfg(windows)]
async fn serve(
    mut polled: pal_async::socket::PolledSocket<vmsocket::VmStream>,
) -> Result<()> {
    let hello: pcie_remote_protocol::Hello = codec::read_frame(&mut polled).await?;
    tracing::info!(
        magic = format_args!("{:#x}", hello.magic),
        version = hello.version,
        "received Hello"
    );

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
    codec::write_frame(&mut polled, &ack).await?;
    tracing::info!("sent HelloAck");

    loop {
        let req: ToHost = codec::read_frame(&mut polled).await?;
        let seq = req.seq;
        match req.body {
            Some(HostBody::MmioRead(m)) => {
                tracing::debug!(seq, bar = m.bar, offset = m.offset, size = m.size, "MMIO read");
                let resp = ToOpenhcl {
                    seq,
                    body: Some(OpenhclBody::MmioReadResult(MmioReadResult { value: 0 })),
                };
                codec::write_frame(&mut polled, &resp).await?;
            }
            Some(HostBody::MmioWrite(m)) => {
                tracing::debug!(seq, bar = m.bar, offset = m.offset, "MMIO write (ignored)");
            }
            Some(HostBody::CfgWriteSideEffect(c)) => {
                tracing::debug!(seq, offset = c.offset, "cfg side-effect (ignored)");
            }
            Some(HostBody::Reset(r)) => {
                tracing::info!(seq, kind = r.kind, "reset (ignored)");
            }
            Some(HostBody::DmaCompletion(d)) => {
                // OpenHCL → host 的 DMA 完成回执（host 自身没发 DMA 时不该收到）。
                tracing::debug!(seq, token = d.token, ok = d.ok, data_len = d.data.len(), "DmaCompletion (unexpected on noop client)");
            }
            None => {
                tracing::warn!(seq, "ToHost missing body");
            }
        }
    }
}
