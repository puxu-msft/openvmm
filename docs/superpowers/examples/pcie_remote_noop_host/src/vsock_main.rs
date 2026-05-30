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
            match serve(&driver, polled).await {
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
    driver: &pal_async::DefaultDriver,
    mut polled: pal_async::socket::PolledSocket<vmsocket::VmStream>,
) -> Result<()> {
    use futures::FutureExt;
    use futures::select_biased;
    use pcie_remote_protocol::InterruptFire;
    use pcie_remote_protocol::ToOpenhcl as OpenhclMsg;
    use pcie_remote_protocol::to_openhcl::Body as OpenhclBody2;
    use std::time::Duration;

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
            // class_code = 0x010802 (NVMe) 让 Windows stornvme 尝试 init：
            // 它会 read cfg + BAR0 NVMe registers → 这些 MMIO 访问就会
            // 真的转给 host noop → 我们用 magic pattern 回应 → stornvme
            // 看到 invalid NVMe 数据后 bail (CM_PROB_FAILED_START)。
            // 但**沿路 cfg_read/MMIO_read 都会被 OpenHCL 转发**，这是
            // 我们验证 v2 完整数据通路的关键。
            class_code: 0x0001_0802,
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

    // 主循环：在读 inbound 时设短超时；超时则发 InterruptFire（不占 polled
    // 借用），有 inbound 则正常处理。避免 select_biased 内同时双借
    // &mut polled。
    let mut next_int_seq: u64 = 1u64 << 32;
    let mut fire_count: u64 = 0;
    let mut timer = pal_async::timer::PolledTimer::new(driver);
    loop {
        let read_timeout = Duration::from_secs(5);
        // race read vs timer：要让 read_fut/timer_fut 在 select 出后立即
        // drop（归还 &mut polled），用 inner block 限定 lifetime。
        let req_opt = {
            let read_fut = codec::read_frame::<_, ToHost>(&mut polled).fuse();
            let timer_fut = timer.sleep(read_timeout).fuse();
            futures::pin_mut!(read_fut, timer_fut);
            futures::select_biased! {
                req_or_err = read_fut => Some(req_or_err),
                _ = timer_fut => None,
            }
        };
        match req_opt {
            Some(Ok(req)) => {
                let seq = req.seq;
                match req.body {
                    Some(HostBody::MmioRead(m)) => {
                        let value = ((m.offset & 0xffff) << 16) | 0xDEAD;
                        tracing::info!(
                            seq, bar = m.bar, offset = m.offset, size = m.size,
                            value = format_args!("{:#x}", value),
                            "MMIO read → pattern reply"
                        );
                        let resp = ToOpenhcl {
                            seq,
                            body: Some(OpenhclBody::MmioReadResult(MmioReadResult { value })),
                        };
                        codec::write_frame(&mut polled, &resp).await?;
                    }
                    Some(HostBody::MmioWrite(m)) => {
                        tracing::info!(
                            seq, bar = m.bar, offset = m.offset, size = m.size,
                            value = format_args!("{:#x}", m.value),
                            "MMIO write (observed)"
                        );
                    }
                    Some(HostBody::CfgWriteSideEffect(c)) => {
                        tracing::info!(seq, offset = c.offset, value = format_args!("{:#x}", c.value), "cfg side-effect (observed)");
                    }
                    Some(HostBody::Reset(r)) => {
                        tracing::info!(seq, kind = r.kind, "reset (ignored)");
                    }
                    Some(HostBody::DmaCompletion(d)) => {
                        tracing::debug!(seq, token = d.token, ok = d.ok, data_len = d.data.len(), "DmaCompletion (unexpected on noop client)");
                    }
                    None => {
                        tracing::warn!(seq, "ToHost missing body");
                    }
                }
            }
            Some(Err(e)) => return Err(e.into()),
            None => {
                // timer 到：主动发 InterruptFire (msix_index=0)。
                fire_count += 1;
                let int_seq = next_int_seq;
                next_int_seq = next_int_seq.wrapping_add(1);
                let fire = OpenhclMsg {
                    seq: int_seq,
                    body: Some(OpenhclBody2::InterruptFire(InterruptFire { msix_index: 0 })),
                };
                tracing::info!(
                    fire_count, int_seq,
                    "periodic InterruptFire msix_index=0 → guest MSI-X"
                );
                codec::write_frame(&mut polled, &fire).await?;
            }
        }
    }
}
