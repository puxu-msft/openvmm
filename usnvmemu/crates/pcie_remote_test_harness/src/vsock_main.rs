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
    /// **Stress** DMA：handshake 完后立即 burst 发 N 次 ReadGpa(gpa=0, len=65536=64KB)
    /// 触发 K-NEW-C rate limit (64 MiB/s)。设非 0 启用；默认 0 不 stress。
    /// 推荐 2048（128 MiB total）确保超阈。
    #[arg(long, default_value_t = 0)]
    stress_dma_count: u32,
    /// **Stress** bad-frame: handshake 完后立即 burst 发 N 个 OOB InterruptFire
    /// (msix_index=99，远超 msix_count=1)，触发 A4 ≥4 → Lost。设 ≥4 验证；
    /// 默认 0 不 stress。
    #[arg(long, default_value_t = 0)]
    stress_bad_frames: u32,
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
            match serve(
                &driver,
                polled,
                args.stress_dma_count,
                args.stress_bad_frames,
            )
            .await
            {
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
    stress_dma_count: u32,
    stress_bad_frames: u32,
) -> Result<()> {
    use futures::FutureExt;
    use pcie_remote_protocol::InterruptFire;
    use pcie_remote_protocol::ReadGpaRequest;
    use pcie_remote_protocol::ToOpenhcl as OpenhclMsg;
    use pcie_remote_protocol::WriteGpaRequest;
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
            // class_code = 0x07_00_02 (Serial Controller, 16550 16-byte FIFO).
            //
            // **实测**：Windows Server 2025 26100 guest 对此 class **不绑任何
            // inbox driver**（serial.sys 通过 ACPI/legacy 资源绑定，而非
            // generic PCI class）。guest 显示为：
            //   FriendlyName: "PCI Device"
            //   Class:        (empty)
            //   Service:      (empty)
            //   Status:       Error (CM_PROB_FAILED_INSTALL)
            //
            // 即"找不到匹配 driver"。比 NVMe class (FAILED_START) 更准确地
            // 表达 noop 的 "实验/未实现" 性质。
            //
            // 关键事实：**没有 driver attach → guest 不会发起 BAR MMIO read** →
            // worker_stats.mmio_read_results 永远是 0。这是 PCI 协议的正确
            // 行为，不是 bug。要在 e2e 中触发 mmio_read_results 必须：
            //   1) 写自签 KMDF dummy driver + INF（需 WDK + testsigning）
            //   2) Linux guest 用 vfio-pci uio_pci_generic
            //   3) 用 raw NT API 直接读 PCI BAR mapped pages
            //
            // 历史尝试：
            // - 0x010802 (NVMe) → stornvme cache 在 cfg-read VENDOR/DEV 后 bail
            //   状态 PROB_FAILED_START（看着更像"真的坏了"，不符合 noop 语义）
            // - 0xff0000 (Unclassified) → 完全没 driver attach，但 device manager
            //   显示更"普通"，不引人深究
            // - 0x070002 (Serial) → 同样无 driver 但状态描述更精确，且 PCIe 规范
            //   允许任何 vendor 注册 PCI serial controller，class 字段合规
            class_code: 0x0007_0002,
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

    // 可选：handshake 完成立即 burst 发 N 个 OOB InterruptFire (msix_index=99)
    // 触发 A4 ≥4 个连续 bad-frame → worker Lost。
    if stress_bad_frames > 0 {
        tracing::info!(
            stress_bad_frames,
            "STRESS: burst sending OOB InterruptFire(msix_index=99)"
        );
        for i in 0..stress_bad_frames {
            let req = ToOpenhcl {
                seq: (1u64 << 51) + i as u64,
                body: Some(pcie_remote_protocol::to_openhcl::Body::InterruptFire(
                    InterruptFire { msix_index: 99 },
                )),
            };
            // write 可能 fail（worker 进 Lost 后 transport EOF/close）
            if let Err(e) = codec::write_frame(&mut polled, &req).await {
                tracing::info!(
                    sent = i,
                    error = %e,
                    "STRESS: transport closed after sending bad frames (worker Lost as expected)"
                );
                return Ok(()); // 正常结束 serve；外层 reconnect loop 会重连
            }
        }
        tracing::info!(
            stress_bad_frames,
            "STRESS: all bad frames sent; worker should have transitioned to Lost"
        );
    }

    // 可选：handshake 完成立即 burst 发 N 个 64 KB ReadGpa 触发 K-NEW-C
    // rate limit (64 MiB/s = 1024 个 64 KB / 秒 上限)。
    // stress_dma_count=2048 → 128 MiB 一次发送，应至少 1024 个被 rate limit 拒绝。
    if stress_dma_count > 0 {
        tracing::info!(stress_dma_count, "STRESS: burst sending ReadGpa(len=64KB)");
        for i in 0..stress_dma_count {
            let req = ToOpenhcl {
                seq: (1u64 << 50) + i as u64,
                body: Some(pcie_remote_protocol::to_openhcl::Body::ReadGpa(
                    pcie_remote_protocol::ReadGpaRequest {
                        token: 9_000_000 + i as u64,
                        gpa: 0,
                        len: 65536, // 64 KB = MAX_DMA_BYTES
                    },
                )),
            };
            codec::write_frame(&mut polled, &req).await?;
        }
        tracing::info!(stress_dma_count, "STRESS: burst done, switching to normal serve");
    }

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
                // timer 到：主动发 InterruptFire (msix_index=0)，每隔几次 fire
                // 加一次 ReadGpa(gpa=0, len=4) 验证 DMA 路径。
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

                // 每 3 次 timer tick 发一次 ReadGpa（gpa=0, len=4）。OpenHCL
                // worker 会从 guest memory @0 读 4 字节回 DmaCompletion；
                // 我们 noop 不真用读出的字节，只验证 worker stats.read_gpa_requests 递增。
                if fire_count.is_multiple_of(3) {
                    let token = fire_count;
                    let dma_seq = next_int_seq;
                    next_int_seq = next_int_seq.wrapping_add(1);
                    let read_gpa = OpenhclMsg {
                        seq: dma_seq,
                        body: Some(OpenhclBody2::ReadGpa(ReadGpaRequest {
                            token,
                            gpa: 0,
                            len: 4,
                        })),
                    };
                    tracing::info!(
                        token, dma_seq,
                        "periodic ReadGpa gpa=0 len=4 → guest memory DMA"
                    );
                    codec::write_frame(&mut polled, &read_gpa).await?;
                }

                // 每 4 次 timer tick 发一次 WriteGpa（gpa=0x1000，写 4 字节 pattern）。
                // gpa=0x1000 通常在 RAM 内（避开 0..0x1000 IVT 区）；写出去的
                // 数据 noop 不验证—我们只看 worker stats.write_gpa_requests 递增。
                if fire_count.is_multiple_of(4) {
                    let token = 1_000_000 + fire_count;
                    let dma_seq = next_int_seq;
                    next_int_seq = next_int_seq.wrapping_add(1);
                    let pattern = (fire_count as u32).to_le_bytes();
                    let write_gpa = OpenhclMsg {
                        seq: dma_seq,
                        body: Some(OpenhclBody2::WriteGpa(WriteGpaRequest {
                            token,
                            gpa: 0x1000,
                            data: pattern.to_vec(),
                        })),
                    };
                    tracing::info!(
                        token, dma_seq,
                        "periodic WriteGpa gpa=0x1000 len=4 → guest memory DMA write"
                    );
                    codec::write_frame(&mut polled, &write_gpa).await?;
                }
            }
        }
    }
}
