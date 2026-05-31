// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # pcie_remote_rng_userspace
//!
//! **第二个用户态 PCIe 设备示例**（继 NVMe example 之后）。目的：证明
//! `pcie_remote_userspace_sdk` 不只能写 NVMe — 任意 PCIe 设备都行。
//!
//! 实现一个最小可工作的 PCI **硬件 RNG** (Random Number Generator)，
//! 类似 Linux `/dev/hwrng` 后端的设备：driver 写一个 GPA + 字节数 +
//! doorbell，controller DMA-write 随机字节到该 GPA + 触发 MSI-X 中断告
//! 知 driver "数据已就绪"。
//!
//! ## BAR0 寄存器 (MMIO, 64 KiB)
//!
//! | Offset | 大小 | RW | 含义                                   |
//! |--------|------|----|----------------------------------------|
//! | 0x00   | 4    | RO | STATUS：bit 0 = ready (始终 1)         |
//! | 0x04   | 4    | RO | VERSION：固定 0x0000_0001              |
//! | 0x08   | 8    | RW | DEST_GPA：DMA 目标 guest 物理地址      |
//! | 0x10   | 4    | RW | LEN_BYTES：要生成多少字节（≤ 64 KiB）  |
//! | 0x14   | 4    | RW | DOORBELL：写 1 触发 DMA + 中断          |
//! | 0x18   | 8    | RO | STATS_GENERATED：总生成字节数（累计）  |
//!
//! ## MSI-X
//!
//! 单 vector (index 0)：DMA 完成时 fire。
//!
//! ## PCI ID
//!
//! - Vendor = 0x1414 (Microsoft，与 OpenHCL pcie_remote 默认路由匹配)
//! - Device = 0x70ce ("PCIe Remote RNG"，自创)
//! - Class  = 0x100000 (Generic system peripheral)
//!
//! ## 用法
//!
//! ```bash
//! # Windows host：
//! pcie_remote_rng_userspace --vm-id <vm-guid> --port 50001
//!
//! # 非 Windows 测试（TCP）：
//! pcie_remote_rng_userspace --tcp-addr 127.0.0.1:50001
//! ```
//!
//! Guest 内查看：`lspci -nn`（Linux）/ Device Manager（Windows）应看到
//! vendor=1414 device=70ce 的设备。
//!
//! ## 教学价值
//!
//! 比 NVMe 简单 20 倍但展示了 SDK 的所有核心 API：
//! - `describe` + BAR + MSI-X 声明
//! - `mmio_read` / `mmio_write` 寄存器抽象
//! - `ctx.dma_write` 发起 DMA + `on_dma_complete` 回调路由
//! - `ctx.fire_interrupt` 触发 guest 中断
//! - `reset` 清状态
//! - reconnect-friendly transport

#![allow(clippy::too_many_arguments)]

use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use pcie_remote_userspace_sdk::*;
use std::time::Duration;

// ----- 寄存器 offset -----

const REG_STATUS: u64 = 0x00;
const REG_VERSION: u64 = 0x04;
const REG_DEST_GPA: u64 = 0x08;
const REG_LEN_BYTES: u64 = 0x10;
const REG_DOORBELL: u64 = 0x14;
const REG_STATS_GENERATED: u64 = 0x18;

/// 64 KiB BAR0；DMA 单次也 cap 在 64 KiB（SDK 上限）。
const BAR0_SIZE: u64 = 64 * 1024;
const RNG_MAX_BYTES: u32 = 64 * 1024;

const VID: u16 = 0x1414;
const DID: u16 = 0x70ce;
const CLASS_CODE: u32 = 0x10_00_00; // Generic system peripheral

/// 单 vector：DMA 完成中断。
const MSIX_COUNT: u16 = 1;

/// MSI-X vector index 用于 RNG 完成
const RNG_DONE_VECTOR: u32 = 0;

/// Linear Congruential Generator — 不需密码学强度（教学用）；NetBSD
/// 默认 `rand()` 系数（Knuth）。如果用户需要真随机，请改用 `OsRng` /
/// `getrandom` 系；本 example 不引入额外依赖。
#[derive(Debug, Clone)]
struct Lcg64 {
    state: u64,
}

impl Lcg64 {
    fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_add(0x9E37_79B9_7F4A_7C15),
        }
    }
    fn next_u64(&mut self) -> u64 {
        // splitmix64 (Sebastiano Vigna, public domain)
        let mut z = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        self.state = z;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn fill(&mut self, buf: &mut [u8]) {
        let mut i = 0;
        while i + 8 <= buf.len() {
            let r = self.next_u64().to_le_bytes();
            buf[i..i + 8].copy_from_slice(&r);
            i += 8;
        }
        if i < buf.len() {
            let r = self.next_u64().to_le_bytes();
            let rem = buf.len() - i;
            buf[i..].copy_from_slice(&r[..rem]);
        }
    }
}

struct RngDevice {
    dest_gpa: u64,
    len_bytes: u32,
    stats_generated: u64,
    rng: Lcg64,
}

impl RngDevice {
    fn new(seed: u64) -> Self {
        Self {
            dest_gpa: 0,
            len_bytes: 0,
            stats_generated: 0,
            rng: Lcg64::new(seed),
        }
    }
}

impl PcieDevice for RngDevice {
    fn describe(&self) -> DeviceDescribe {
        DeviceDescribe {
            vendor_id: VID as u32,
            device_id: DID as u32,
            class_code: CLASS_CODE,
            revision: 0x01,
            subsystem_vendor: 0,
            subsystem_device: 0,
            bars: vec![pcie_remote_protocol::BarInfo {
                index: 0,
                size: BAR0_SIZE,
                kind: pcie_remote_protocol::bar_info::Kind::Mmio64 as i32,
                prefetchable: false,
            }],
            msix_count: MSIX_COUNT as u32,
            capabilities: vec![],
            cfg_write_side_effect_offsets: vec![],
        }
    }

    fn mmio_read(&mut self, bar: u32, offset: u64, size: u32) -> u64 {
        let _ = (bar, size);
        match offset {
            REG_STATUS => 0x1, // ready
            REG_VERSION => 0x0000_0001,
            REG_DEST_GPA => self.dest_gpa,
            REG_LEN_BYTES => self.len_bytes as u64,
            REG_DOORBELL => 0, // 写触发，读总返 0
            REG_STATS_GENERATED => self.stats_generated,
            _ => {
                tracing::debug!(offset, "MMIO read unmapped");
                0
            }
        }
    }

    fn mmio_write(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        bar: u32,
        offset: u64,
        size: u32,
        value: u64,
    ) {
        let _ = (bar, size);
        match offset {
            REG_DEST_GPA => {
                self.dest_gpa = value;
                tracing::debug!(gpa = format_args!("{:#x}", value), "RNG: set DEST_GPA");
            }
            REG_LEN_BYTES => {
                self.len_bytes = value as u32;
                tracing::debug!(len = self.len_bytes, "RNG: set LEN_BYTES");
            }
            REG_DOORBELL => {
                // 触发：生成 len_bytes 随机字节 → DMA-write 到 dest_gpa
                if value & 0x1 == 0 {
                    return; // 兼容 driver 写 0 不触发
                }
                let n = self.len_bytes.min(RNG_MAX_BYTES);
                if n == 0 {
                    tracing::warn!("RNG: doorbell with len=0, fire interrupt directly");
                    ctx.fire_interrupt(RNG_DONE_VECTOR);
                    return;
                }
                let mut buf = vec![0u8; n as usize];
                self.rng.fill(&mut buf);
                self.stats_generated += n as u64;
                tracing::debug!(
                    n,
                    gpa = format_args!("{:#x}", self.dest_gpa),
                    "RNG: dispatch DMA"
                );
                let _tok = ctx.dma_write(self.dest_gpa, buf);
                // 完成在 on_dma_complete 中 fire_interrupt（spec-style：
                // 数据到 guest memory 才中断告完成）。
            }
            REG_STATUS | REG_VERSION | REG_STATS_GENERATED => {
                tracing::debug!(offset, "MMIO write to RO reg ignored");
            }
            _ => {
                tracing::debug!(offset, value, "MMIO write unmapped");
            }
        }
    }

    fn on_dma_complete(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        token: u64,
        ok: bool,
        data: Vec<u8>,
    ) {
        let _ = (token, data);
        if ok {
            ctx.fire_interrupt(RNG_DONE_VECTOR);
        } else {
            tracing::warn!("RNG: DMA write failed; not firing completion interrupt");
        }
    }

    fn reset(&mut self, kind: u32) {
        tracing::info!(kind, "RNG: reset — clearing GPA/len");
        self.dest_gpa = 0;
        self.len_bytes = 0;
        // 统计 / RNG 状态跨 reset 不清（教学：与真硬件一致 — counter
        // 通常 sticky；rng 种子持续演化）
    }

    fn tick(&mut self, _ctx: &mut DeviceCtx<'_>) {
        // 设备是 reactive，doorbell 驱动 → tick 无事
    }
}

// ============================================================================
// main / transport / reconnect — 与 NVMe example 同结构，去 NVMe 特有部分
// ============================================================================

#[derive(Parser, Debug)]
#[command(about = "Userspace PCIe RNG device via pcie_remote vsock (or TCP).")]
struct Args {
    /// Hyper-V VM Id (Guid format)
    #[arg(long)]
    vm_id: Option<String>,
    /// vsock port
    #[arg(long, default_value_t = 50001)]
    port: u32,
    /// PCI Vendor ID 覆盖（不推荐，OpenHCL 默认按 0x1414 路由）
    #[arg(long, default_value_t = VID)]
    vid: u16,
    /// 非 Windows / 测试用 TCP 模式
    #[arg(long)]
    tcp_addr: Option<String>,
    /// Connect retry count (0=无限)
    #[arg(long, default_value_t = 0)]
    retries: u32,
    /// Per-retry sleep (ms)
    #[arg(long, default_value_t = 500)]
    retry_ms: u64,
    /// RNG seed（debug 可复现；默认 = 当前时间 ns）
    #[arg(long)]
    seed: Option<u64>,
}

#[cfg(not(windows))]
fn main() -> Result<()> {
    let args = Args::parse();
    if args.tcp_addr.is_none() {
        return Err(anyhow!(
            "non-Windows build requires --tcp-addr (vsock unavailable)"
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
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "pcie_remote_rng_userspace=debug,pcie_remote_userspace_sdk=info,info".into()
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let seed = args.seed.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    });
    let _ = args.vid; // 当前 describe 用 const VID；override 留 future
    pal_async::DefaultPool::run_with(|driver| async move {
        tracing::info!(seed, "RNG userspace starting");
        loop {
            let transport = match connect_with_retry(&driver, &args).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "connect failed permanently");
                    return Err(e);
                }
            };
            tracing::info!("connected; spawning RngDevice");
            let device = RngDevice::new(seed);
            let opts = RunOptions {
                tick_interval: Duration::from_secs(60),
                read_timeout: Duration::from_secs(60),
            };
            match run(&driver, transport, device, opts).await {
                Ok(()) => tracing::info!("run() returned normally; reconnecting in 1s"),
                Err(e) => tracing::warn!(error = %e, "run() ended; reconnecting in 1s"),
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
        match try_one(driver, args).await {
            Ok(t) => return Ok(t),
            Err(e) if args.retries == 0 || attempt < args.retries => {
                tracing::warn!(error = %e, attempt, "connect failed; retrying");
                pal_async::timer::PolledTimer::new(driver)
                    .sleep(Duration::from_millis(args.retry_ms))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// LCG 确定性 + 至少不全零（弱保证）。
    #[test]
    fn lcg_basic_distribution() {
        let mut r = Lcg64::new(42);
        let a = r.next_u64();
        let b = r.next_u64();
        assert_ne!(a, b);
        // 同 seed 应可复现
        let mut r2 = Lcg64::new(42);
        assert_eq!(a, r2.next_u64());
        assert_eq!(b, r2.next_u64());
    }

    /// fill 任意长度且不全零（统计上极不可能）。
    #[test]
    fn lcg_fill_lengths() {
        let mut r = Lcg64::new(0xdeadbeef);
        for &n in &[1usize, 7, 8, 9, 256, 4096, 65535] {
            let mut buf = vec![0u8; n];
            r.fill(&mut buf);
            assert!(buf.iter().any(|&b| b != 0), "fill({n}) returned all zeros?");
        }
    }
}
