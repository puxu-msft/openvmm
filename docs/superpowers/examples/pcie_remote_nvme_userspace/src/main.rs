// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// 示例代码：放宽一些 clippy lint，便于按 NVMe spec 字段名照搬写代码。
#![allow(clippy::unnecessary_cast)] // u32 spec 字段保留 `as u32` 增强可读性
#![allow(clippy::too_many_arguments)] // NVMe SQE/CQE 字段多，wrap struct 反而绕
#![allow(clippy::enum_variant_names)] // PendingOp 全 Nvm 前缀强调 NVMe 语义
#![allow(unused_imports)] // cmd.rs 把 IntoBytes 既用于 derive 又用于函数体；该 lint 误判

//! Userspace NVMe controller — connects to OpenHCL via pcie_remote vsock
//! and exposes a real NVMe block device backed by a host file.
//!
//! 用法（Windows host）：
//!
//! ```bash
//! # 1. 准备 backing 文件（一次性，1 GiB 示例）
//! fsutil file createnew C:\temp\nvme_backing.img 1073741824
//!
//! # 2. 跑 controller
//! pcie_remote_nvme_userspace --vm-id <vm-guid> --port 50000 \
//!     --backing-file C:\temp\nvme_backing.img
//! ```
//!
//! VM 内 Windows guest 装好 OS 后会自动加载 nvme.sys 并把这个虚拟 NVMe
//! 设备识别为 `Disk` —— `Get-Disk` 应该能看到，且可 Initialize / format。

mod cmd;
mod controller;
mod regs;

use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use controller::NvmeController;
use pcie_remote_userspace_sdk::*;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(about = "Userspace NVMe controller via pcie_remote vsock.")]
struct Args {
    /// Hyper-V VM Id (Guid format)。生产 OpenHCL 路径 vsock 必填。
    #[arg(long)]
    vm_id: Option<String>,
    /// vsock port (matches OPENHCL_PCIE_REMOTE_INSTANCE/TAKEOVER cmdline)。
    #[arg(long, default_value_t = 50000)]
    port: u32,
    /// Backing file path（必填）。文件大小决定 namespace LBA 数（÷512）。
    #[arg(long)]
    backing_file: String,
    /// PCI Vendor ID（默认 0x1414 = Microsoft，配合 OpenHCL 的 default 路由）。
    #[arg(long, default_value_t = 0x1414)]
    vid: u16,
    /// PCI Subsystem Vendor ID
    #[arg(long, default_value_t = 0)]
    ssvid: u16,
    /// 仅 TCP 模式：连入此 host:port（非 Windows 测试用）。
    #[arg(long)]
    tcp_addr: Option<String>,
    /// Connect retry count。
    #[arg(long, default_value_t = 20)]
    retries: u32,
    /// Per-retry sleep (ms)。
    #[arg(long, default_value_t = 500)]
    retry_ms: u64,
}

#[cfg(not(windows))]
fn main() -> Result<()> {
    let args = Args::parse();
    if args.tcp_addr.is_none() {
        return Err(anyhow!(
            "non-Windows build requires --tcp-addr (vsock AF_HYPERV unavailable)"
        ));
    }
    run_main(args)
}

#[cfg(windows)]
fn main() -> Result<()> {
    let args = Args::parse();
    if args.tcp_addr.is_none() && args.vm_id.is_none() {
        return Err(anyhow!("provide --vm-id (vsock) or --tcp-addr"));
    }
    run_main(args)
}

fn run_main(args: Args) -> Result<()> {
    // 默认开 device + SDK debug log；用户可用 RUST_LOG 覆盖。
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "pcie_remote_nvme_userspace=debug,pcie_remote_userspace_sdk=debug,info".into()
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
    pal_async::DefaultPool::run_with(|driver| async move {
        tracing::info!(
            backing_file = %args.backing_file,
            vid = format_args!("{:#x}", args.vid),
            "NVMe userspace starting"
        );

        // Reconnect loop：transport EOF / err 重连 + 重新打开 backing file。
        // 重连每次重 new 一个 NvmeController（state machine 是 fresh 的；
        // backing file 自然保持持久化数据）。
        loop {
            let transport = match connect_with_retry(&driver, &args).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "connect failed permanently");
                    return Err(e);
                }
            };
            tracing::info!("connected; spawning NvmeController");

            // 每次重连重新 open file（让 hot-reconnect 也能切换 backing）
            let device = NvmeController::open(&args.backing_file, args.vid, args.ssvid)?;
            let opts = RunOptions {
                tick_interval: Duration::from_secs(60),
                read_timeout: Duration::from_secs(60),
            };
            match run(&driver, transport, device, opts).await {
                Ok(()) => {
                    tracing::info!("run() returned normally; reconnecting in 1s");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "run() ended; reconnecting in 1s");
                }
            }
            pal_async::timer::PolledTimer::new(&driver)
                .sleep(Duration::from_secs(1))
                .await;
        }
    })
}

async fn connect_with_retry(driver: &pal_async::DefaultDriver, args: &Args) -> Result<Transport> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let r = try_one(driver, args).await;
        match r {
            Ok(t) => return Ok(t),
            // retries=0 表示无限重试（适合"NVMe 先于 VM 启动"的场景；VM
            // VTL2 起来可能需要几分钟，不能让 retries 耗尽放弃）。
            Err(e) if args.retries == 0 || attempt < args.retries => {
                tracing::warn!(error = %e, attempt, "connect failed; retrying");
                pal_async::timer::PolledTimer::new(driver)
                    .sleep(std::time::Duration::from_millis(args.retry_ms))
                    .await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn try_one(driver: &pal_async::DefaultDriver, args: &Args) -> Result<Transport> {
    if let Some(addr) = &args.tcp_addr {
        return connect_tcp(driver, addr).await;
    }
    #[cfg(windows)]
    {
        let vm_id_str = args
            .vm_id
            .as_ref()
            .ok_or_else(|| anyhow!("missing --vm-id"))?;
        let vm_id: guid::Guid = vm_id_str
            .parse()
            .map_err(|e| anyhow!("invalid vm_id: {e}"))?;
        return connect_vsock(driver, vm_id, args.port).await;
    }
    #[cfg(not(windows))]
    {
        Err(anyhow!("vsock requires Windows; use --tcp-addr"))
    }
}
