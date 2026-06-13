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
//! # 1. 准备 backing 文件
//! fsutil file createnew C:\temp\nvme_ns1.img 1073741824
//! fsutil file createnew C:\temp\nvme_ns2.img 1073741824  # 可选 NS 2
//!
//! # 2. 跑 controller — Phase H4：支持多 namespace
//! nvme_firmware --vm-id <vm-guid> --port 50000 \
//!     --backing-file C:\temp\nvme_ns1.img \
//!     --backing-file C:\temp\nvme_ns2.img
//! # 等价：--backing-file C:\temp\nvme_ns1.img,C:\temp\nvme_ns2.img
//! ```
//!
//! VM 内 Windows guest 装好 OS 后会自动加载 nvme.sys 并把这个虚拟 NVMe
//! 设备识别为 `Disk` —— `Get-Disk` 应该能看到（每 NS 一块盘），且可
//! Initialize / format。

mod cmd {
    pub use nvme_firmware::cmd::*;
}
mod controller {
    pub use nvme_firmware::controller::*;
}
mod pi {
    pub use nvme_firmware::pi::*;
}
mod regs {
    pub use nvme_firmware::regs::*;
}
mod sgl {
    pub use nvme_firmware::sgl::*;
}

use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use clap::ValueEnum;

/// **CMB-P5** — `--cmb-mode` 的取值（设计 §3 配置面）。
/// - `off`：不 advertise CMB（默认，保持现状）。
/// - `trap`：trap-based CMB（内部 `Vec` backing，经 REGION_READ/WRITE 转发；任何 transport 可用）。
/// - `map`：map-based CMB（memfd backing，client mmap 零拷贝；仅 vfio-user transport 有意义）。
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[value(rename_all = "lower")]
enum CmbMode {
    #[default]
    Off,
    Trap,
    Map,
}

/// **CMB-P5** — transport 类别（由互斥的 CLI 参数推导），用于 CMB 模式协商。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportKind {
    /// `--vfio-user-sock`：QEMU vfio-user over AF_UNIX，拥有 guest 内存映射，可 mmap region fd。
    VfioUser,
    /// `--tcp-addr`：NVMe-oF TCP / 测试，无 PCIe BAR 概念，CMB 结构性不适用（设计 §6）。
    Tcp,
    /// `--vm-id`：OpenHCL vsock（VTL2 underhill），有 PCIe BAR 但无 MemoryMapper，map 走不通（设计 §5）。
    Vsock,
}

/// **CMB-P5** — CMB 模式协商结果（设计 §3）。`effective` 是实际启用的模式；
/// `warning` 非 `None` 时表示发生了降级，main 应显式 `tracing::warn!`（对齐
/// silent-failure 纪律：知道意图的 CLI 层把不兼容选择以 warn 暴露，不静默丢弃）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CmbNegotiation {
    effective: CmbMode,
    warning: Option<&'static str>,
}

/// **CMB-P5** — 按请求模式 + transport 类别协商实际 CMB 模式（设计 §3 模式协商/降级）。
///
/// 规则：
/// - `off` → off（任何 transport，无 warn）。
/// - `trap` → trap（任何 transport 可用，trap 经协议 REGION_READ/WRITE 转发，无 warn）。
/// - `map` + vfio-user → map（QEMU 拥有 guest 内存表，能 mmap region fd，真零拷贝）。
/// - `map` + TCP → **off** + warn：TCP 无 PCIe BAR，CMB 整体不适用（设计 §6，CMBLOC/CMBSZ 返 0），
///   降级为不 advertise。
/// - `map` + vsock(OpenHCL) → **trap** + warn：underhill VTL2 无 `MemoryMapper`，map 走不通
///   （设计 §5），但有 PCIe BAR 故可退到 trap（功能正确，非零拷贝）。
fn negotiate_cmb_mode(requested: CmbMode, transport: TransportKind) -> CmbNegotiation {
    match (requested, transport) {
        (CmbMode::Off, _) => CmbNegotiation {
            effective: CmbMode::Off,
            warning: None,
        },
        (CmbMode::Trap, _) => CmbNegotiation {
            effective: CmbMode::Trap,
            warning: None,
        },
        (CmbMode::Map, TransportKind::VfioUser) => CmbNegotiation {
            effective: CmbMode::Map,
            warning: None,
        },
        (CmbMode::Map, TransportKind::Tcp) => CmbNegotiation {
            effective: CmbMode::Off,
            warning: Some(
                "map CMB 需 vfio-user transport：TCP 无 PCIe BAR，CMB 不适用（设计 §6）→ 关闭 CMB（off）",
            ),
        },
        (CmbMode::Map, TransportKind::Vsock) => CmbNegotiation {
            effective: CmbMode::Trap,
            warning: Some(
                "map CMB 需 client 能 mmap region fd：OpenHCL VTL2 underhill 无 MemoryMapper（设计 §5）→ 降级 trap-based CMB（功能正确，非零拷贝）",
            ),
        },
    }
}
// **Phase W3 (review M-1)** — controller 入口仅 transport 路径用；neither build
// 下显式不引用，不靠 blanket `allow(unused_imports)` 掩盖悬空。
#[cfg(any(feature = "openhcl", feature = "vfio-user"))]
use controller::NvmeController;
#[cfg(feature = "openhcl")]
use pcie_device_sdk::*;
#[cfg(feature = "openhcl")]
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
    /// Backing 文件路径（必填，可重复 / 逗号分隔）。每个文件成为一个
    /// namespace（NSID 1, 2, ...）。Phase H4：支持 multiple namespaces。
    ///
    /// 例：`--backing-file ns1.img --backing-file ns2.img`
    /// 或：`--backing-file ns1.img,ns2.img,ns3.img`
    ///
    /// 文件大小决定 NS 容量（÷ 512 round down 到 LBA 数）。
    #[arg(long = "backing-file", value_delimiter = ',', num_args = 1..)]
    backing_files: Vec<String>,
    /// **Phase L1** — 把指定 NSID 列表标记为 ZNS namespace (Zoned)。
    /// 例：`--zns-nsid 2 --zns-nsid 3` 让 NS 2/3 走 ZNS Command Set，
    /// 受 Sequential Write Required 约束。每 zone 1 MiB (= 2048 LBA at
    /// 512B sector / 256 LBA at 4 KiB sector)，教学短化。NSID 1 默认
    /// 保留为普通 NVM 让 driver 可走 enumerate flow。
    #[arg(long = "zns-nsid", value_delimiter = ',', num_args = 0..)]
    zns_nsids: Vec<u32>,
    /// **NS-not-ready spec-completeness** — 把指定 NSID 列表的 NS 开机标记为 not-ready
    /// （NSTAT.NRDY=1）：其 IO 返 NAMESPACE_NOT_READY (0x82)，直到对它 Format NVM
    /// 初始化 media 后转 ready。模拟"刚 attach / media 未初始化"的 NS。
    /// 例：`--not-ready-nsid 1` 或 `--not-ready-nsid 1,2`。默认空（所有 NS 开机即 ready）。
    #[arg(long = "not-ready-nsid", value_delimiter = ',', num_args = 0..)]
    not_ready_nsids: Vec<u32>,
    /// **Boot Partition（spec § 8.13）** — 把指定文件内容装载为只读出厂 boot partition。
    /// 给定后 controller 广告 BPINFO.BPSZ>0 并服务 Boot Partition Read（driver 写 BPRSEL
    /// 触发 → DMA 到 BPMBL）；该 BP write-protected（FW Commit BPID 写它 →
    /// BOOT_PARTITION_WRITE_PROHIBITED）。不给 = 不广告 boot partition（BPSZ=0）。
    #[arg(long = "boot-partition-file")]
    boot_partition_file: Option<String>,
    /// PCI Vendor ID（默认 0x1414 = Microsoft，配合 OpenHCL 的 default 路由）。
    #[arg(long, default_value_t = 0x1414)]
    vid: u16,
    /// PCI Subsystem Vendor ID
    #[arg(long, default_value_t = 0)]
    ssvid: u16,
    /// **2026-06-09** — 队列深度（MQES = 单 SQ/CQ 最大 entry 数），模拟不同档位
    /// 设备。∈ [2, 65536]（MQES 0-based 16-bit；spec 不要求 2 的幂）。默认 128。
    #[arg(long = "max-queue-entries", default_value_t = nvme_firmware::DEFAULT_MAX_QUEUE_ENTRIES)]
    max_queue_entries: u32,
    /// **2026-06-09** — 本次运行模拟的 IO queue 对数上限 ∈ [1, 256]。默认 256。
    #[arg(long = "io-queue-pairs", default_value_t = nvme_firmware::IO_QUEUE_SLOT_CAPACITY)]
    io_queue_pairs: u16,
    /// **2026-06-09** — 本次运行模拟的 namespace 容量上限 ∈ [1, 8]。默认 8。
    #[arg(long = "max-namespaces", default_value_t = nvme_firmware::NAMESPACE_SLOT_CAPACITY)]
    max_namespaces: u32,
    /// 仅 TCP 模式：连入此 host:port（非 Windows 测试用）。
    #[arg(long)]
    tcp_addr: Option<String>,
    /// **Phase U-followup** — vfio-user backend：绑定 UNIX socket 监听，
    /// 让 QEMU `-device vfio-user-pci,socket=...` 接管本 NVMe 控制器。
    /// 与 `--tcp-addr` / `--vm-id` 互斥；指定后走 vfio_user_transport
    /// 而非 pcie_remote 协议。
    /// **review M2** — clap 强制互斥校验。
    #[arg(long, conflicts_with_all = ["vm_id", "tcp_addr"])]
    vfio_user_sock: Option<String>,
    /// Connect retry count。
    #[arg(long, default_value_t = 20)]
    retries: u32,
    /// Per-retry sleep (ms)。
    #[arg(long, default_value_t = 500)]
    retry_ms: u64,
    /// **CMB-P5** — Controller Memory Buffer 模式（设计 §3 配置面）：
    /// `off`（默认，不 advertise CMB）/ `trap`（内部 Vec backing，经 REGION_READ/WRITE
    /// 转发，任何 transport 可用）/ `map`（memfd backing，client mmap 零拷贝，仅
    /// vfio-user 有意义；TCP→关闭、OpenHCL vsock→降级 trap，均带 warn）。
    #[arg(long = "cmb-mode", value_enum, default_value_t = CmbMode::Off)]
    cmb_mode: CmbMode,
    /// **CMB-P5** — CMB 大小（字节）。须为 2 的幂 + 4 KiB 倍数（`enable_cmb` 校验）。
    /// 默认 2 MiB。仅 `--cmb-mode trap|map` 生效。
    #[arg(long = "cmb-size", default_value_t = 2 * 1024 * 1024)]
    cmb_size: u64,
    /// **CMB-P5** — CMB 所用 BAR 索引（BIR）。当前**仅支持 2**（P3b：client 第二 BAR
    /// 暴露 + worker 转发只对 BAR2 接线）；非 2 时报错退出（不 panic）。默认 2。
    #[arg(long = "cmb-bir", default_value_t = 2)]
    cmb_bir: u8,
}

#[cfg(not(windows))]
fn main() -> Result<()> {
    let args = Args::parse();
    if args.tcp_addr.is_none() && args.vfio_user_sock.is_none() {
        return Err(anyhow!(
            "non-Windows build requires --tcp-addr or --vfio-user-sock (vsock AF_HYPERV unavailable)"
        ));
    }
    dispatch(args)
}

#[cfg(windows)]
fn main() -> Result<()> {
    let args = Args::parse();
    if args.tcp_addr.is_none() && args.vm_id.is_none() && args.vfio_user_sock.is_none() {
        return Err(anyhow!(
            "provide --vm-id (vsock) or --tcp-addr or --vfio-user-sock"
        ));
    }
    dispatch(args)
}

/// 按 CLI 参数选 transport；按 feature 编译时裁剪未启用的路径。
fn dispatch(args: Args) -> Result<()> {
    if args.vfio_user_sock.is_some() {
        #[cfg(feature = "vfio-user")]
        {
            return run_vfio_user(args);
        }
        #[cfg(not(feature = "vfio-user"))]
        {
            return Err(anyhow!(
                "--vfio-user-sock 需在编译时启用 'vfio-user' feature"
            ));
        }
    }
    #[cfg(feature = "openhcl")]
    {
        run_main(args)
    }
    #[cfg(not(feature = "openhcl"))]
    {
        Err(anyhow!(
            "vsock/tcp transport 需在编译时启用 'openhcl' feature"
        ))
    }
}

/// **CMB-P5** — 当前仅支持的 CMB BAR 索引（BIR）。P3b 的 client 第二 BAR 暴露 +
/// worker 转发只对 BAR2 接线（`vfio_user_pci_device` / transport region_info）；其余
/// BIR 虽 `enable_cmb` 接受（∈[1,5]），但 client 侧无对应 BAR 槽。
#[cfg(any(feature = "openhcl", feature = "vfio-user"))]
const CMB_SUPPORTED_BIR: u8 = 2;

/// **CMB-P5** — 校验 `--cmb-bir`：当前仅支持 BAR2（见 [`CMB_SUPPORTED_BIR`]）。非 2
/// 时返 `Err`（报错退出，**不 panic**——任务书要求）。
#[cfg(any(feature = "openhcl", feature = "vfio-user"))]
fn validate_cmb_bir(bir: u8) -> Result<()> {
    if bir != CMB_SUPPORTED_BIR {
        return Err(anyhow!(
            "--cmb-bir {bir} 暂不支持：当前 CMB 仅接线 BAR{CMB_SUPPORTED_BIR}（P3b client 第二 BAR 转发只对 BAR{CMB_SUPPORTED_BIR}）"
        ));
    }
    Ok(())
}

/// **CMB-P5** — 在 controller 上按 `off`/`trap` 模式接线 CMB（内部 `Vec` backing，
/// 任何 transport 可用）。`map` 模式因需 transport 注入 memfd backing，**不**经本函数
/// （见 vfio-user 路径的 `apply_cmb_map`）。non-vfio-user 路径（vsock/tcp）经 `negotiate_cmb_mode`
/// 保证 effective ∈ {off, trap}，故本函数对 map 返 `Err`（防御性，正常不可达）。
#[cfg(any(feature = "openhcl", feature = "vfio-user"))]
fn apply_cmb_trap_or_off(c: &mut NvmeController, mode: CmbMode, size: u64, bir: u8) -> Result<()> {
    match mode {
        CmbMode::Off => Ok(()),
        CmbMode::Trap => {
            validate_cmb_bir(bir)?;
            c.enable_cmb(size, bir)
                .map_err(|e| anyhow!("enable_cmb(size={size}, bir={bir}): {e}"))?;
            tracing::info!(size, bir, "CMB enabled（trap-based，内部 Vec backing）");
            Ok(())
        }
        CmbMode::Map => Err(anyhow!(
            "internal: apply_cmb_trap_or_off 收到 map 模式（应已被 negotiate 降级）；CLI dispatch bug"
        )),
    }
}

/// **CMB-P5** — vfio-user 路径的 CMB 接线：off/trap 复用 [`apply_cmb_trap_or_off`]；
/// `map` 模式由 transport（`vfio_user_transport`，允许 unsafe）造 `MemfdRamRegion`
/// （memfd backing，可经 SCM_RIGHTS fd 暴露给 client mmap 零拷贝）注入 controller。
/// backing 注入时序：controller 已 open 后、serve 前在工厂闭包内造 memfd 注入——firmware
/// core `forbid(unsafe_code)` 不能自造 memfd，故由 transport crate 提供（设计 §2/§4）。
#[cfg(feature = "vfio-user")]
fn apply_cmb_vfio_user(c: &mut NvmeController, mode: CmbMode, size: u64, bir: u8) -> Result<()> {
    match mode {
        CmbMode::Off | CmbMode::Trap => apply_cmb_trap_or_off(c, mode, size, bir),
        CmbMode::Map => {
            validate_cmb_bir(bir)?;
            // memfd backing：size 决定 region 长度（enable_cmb_with_backing 从 backing.len()
            // 取 size 并校验 2 的幂 + 4 KiB 倍数 + BIR∈[1,5]）。
            let backing = vfio_user_transport::MemfdRamRegion::new(size as usize)
                .map_err(|e| anyhow!("CMB memfd backing 创建失败（size={size}）: {e}"))?;
            c.enable_cmb_with_backing(Box::new(backing), bir)
                .map_err(|e| anyhow!("enable_cmb_with_backing(bir={bir}): {e}"))?;
            tracing::info!(
                size,
                bir,
                "CMB enabled（map-based，memfd backing → client mmap 零拷贝）"
            );
            Ok(())
        }
    }
}

/// **Phase U-followup** — vfio-user 模式：绑定 UNIX socket，accept QEMU
/// 接管，跑同一份 NvmeController（与 pcie_remote 路径共享 controller code）。
#[cfg(feature = "vfio-user")]
fn run_vfio_user(args: Args) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "nvme_firmware=debug,vfio_user_transport=debug,info".into());
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let Some(sock) = args.vfio_user_sock.clone() else {
        // **review M4** — caller 应已经校验，但 unwrap 误判会 panic；
        // 走 Result 路径让 clap 的错误信息保留。
        return Err(anyhow!(
            "internal: run_vfio_user requires --vfio-user-sock; this is a CLI dispatch bug"
        ));
    };
    tracing::info!(
        sock = sock.as_str(),
        backing_files = ?args.backing_files,
        vid = format_args!("{:#x}", args.vid),
        "vfio-user NVMe server starting"
    );
    // **CMB-P5** — 模式协商（vfio-user transport）+ 降级 warn（一次，serve 前；闭包内
    // 每次 reconnect 重建 controller 时按 effective 模式接线）。vfio-user 是唯一 map
    // 真零拷贝 transport，故 map 不降级。
    let cmb = negotiate_cmb_mode(args.cmb_mode, TransportKind::VfioUser);
    if let Some(w) = cmb.warning {
        tracing::warn!(
            requested = ?args.cmb_mode,
            effective = ?cmb.effective,
            "{w}"
        );
    }
    if cmb.effective != CmbMode::Off {
        // BIR 校验前置（serve 前 fail-fast，不进 accept loop 才报错）。
        validate_cmb_bir(args.cmb_bir)?;
    }
    let cmb_effective = cmb.effective;
    vfio_user_transport::serve_unix(&sock, || {
        let mut c =
            NvmeController::open(&args.backing_files, args.vid, args.ssvid, &args.zns_nsids)
                .map_err(|e| anyhow!("NvmeController::open: {e}"))?;
        c.set_max_queue_entries(args.max_queue_entries)?;
        c.set_io_queue_pairs(args.io_queue_pairs)?;
        c.set_max_namespaces(args.max_namespaces)?;
        c.set_namespaces_not_ready(&args.not_ready_nsids);
        if let Some(ref bp) = args.boot_partition_file {
            let content =
                std::fs::read(bp).map_err(|e| anyhow!("read --boot-partition-file {bp}: {e}"))?;
            c.set_boot_partition(content);
        }
        // **CMB-P5** — 接线 CMB（vfio-user：off/trap 走内部 Vec backing，map 注入
        // memfd backing 经 fd 暴露给 client mmap 零拷贝）。
        apply_cmb_vfio_user(&mut c, cmb_effective, args.cmb_size, args.cmb_bir)?;
        Ok(c)
    })
}

#[cfg(feature = "openhcl")]
fn run_main(args: Args) -> Result<()> {
    // 默认开 device + SDK debug log；用户可用 RUST_LOG 覆盖。
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "nvme_firmware=debug,pcie_device_sdk=debug,info".into());
    tracing_subscriber::fmt().with_env_filter(filter).init();
    pal_async::DefaultPool::run_with(|driver| async move {
        tracing::info!(
            backing_files = ?args.backing_files,
            vid = format_args!("{:#x}", args.vid),
            "NVMe userspace starting"
        );

        // **CMB-P5** — 模式协商（transport：tcp_addr → TCP，否则 vsock/OpenHCL）+ 降级
        // warn（loop 前一次）。effective 经 negotiate 保证 ∈ {off, trap}（map 在 TCP
        // 降级为 off、在 vsock 降级为 trap），故下方 reconnect loop 内只接线 off/trap。
        let transport_kind = if args.tcp_addr.is_some() {
            TransportKind::Tcp
        } else {
            TransportKind::Vsock
        };
        let cmb = negotiate_cmb_mode(args.cmb_mode, transport_kind);
        if let Some(w) = cmb.warning {
            tracing::warn!(
                requested = ?args.cmb_mode,
                effective = ?cmb.effective,
                "{w}"
            );
        }
        if cmb.effective != CmbMode::Off {
            validate_cmb_bir(args.cmb_bir)?;
        }
        let cmb_effective = cmb.effective;

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
            let mut device =
                NvmeController::open(&args.backing_files, args.vid, args.ssvid, &args.zns_nsids)?;
            device.set_max_queue_entries(args.max_queue_entries)?;
            device.set_io_queue_pairs(args.io_queue_pairs)?;
            device.set_max_namespaces(args.max_namespaces)?;
            device.set_namespaces_not_ready(&args.not_ready_nsids);
            if let Some(ref bp) = args.boot_partition_file {
                let content = std::fs::read(bp)
                    .map_err(|e| anyhow!("read --boot-partition-file {bp}: {e}"))?;
                device.set_boot_partition(content);
            }
            // **CMB-P5** — 接线 CMB（vsock/tcp：effective 仅 off/trap，trap 用内部 Vec
            // backing 经 REGION_READ/WRITE 转发；map 已被 negotiate 降级）。
            apply_cmb_trap_or_off(&mut device, cmb_effective, args.cmb_size, args.cmb_bir)?;
            let opts = RunOptions {
                // tick 用于：① Sanitize / Self-Test 进度推进 ② AEN 派发
                // ③ **Phase M1b** Interrupt Coalescing time flush。原 60s
                // 对前两个够用，但 IRQ coalescing 时间维度需更细——降到
                // 100 ms（每秒 10 次，开销可忽略；driver 设的 AGGR_TIME
                // 最小 100us 实际仍会被 tick 抹粗到 100ms，足够教学）。
                tick_interval: Duration::from_millis(100),
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

#[cfg(feature = "openhcl")]
async fn connect_with_retry(driver: &pal_async::DefaultDriver, args: &Args) -> Result<WireStream> {
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

#[cfg(feature = "openhcl")]
async fn try_one(driver: &pal_async::DefaultDriver, args: &Args) -> Result<WireStream> {
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
mod cmb_cli_tests {
    use super::*;

    /// **CMB-P5** — `--cmb-mode` 解析为 `CmbMode`（含默认 off）。
    #[test]
    fn cmb_mode_parses_off_trap_map_default_off() {
        let base = [
            "nvme_firmware",
            "--backing-file",
            "x.img",
            "--tcp-addr",
            "127.0.0.1:9",
        ];
        // 默认 off。
        let a = Args::try_parse_from(base).expect("default parse");
        assert_eq!(a.cmb_mode, CmbMode::Off, "默认 --cmb-mode off");
        assert_eq!(a.cmb_size, 2 * 1024 * 1024, "默认 cmb-size 2 MiB");
        assert_eq!(a.cmb_bir, 2, "默认 cmb-bir 2");
        // 显式三值。
        for (s, want) in [
            ("off", CmbMode::Off),
            ("trap", CmbMode::Trap),
            ("map", CmbMode::Map),
        ] {
            let mut v = base.to_vec();
            v.extend(["--cmb-mode", s]);
            let a = Args::try_parse_from(v).unwrap_or_else(|_| panic!("parse --cmb-mode {s}"));
            assert_eq!(a.cmb_mode, want, "--cmb-mode {s}");
        }
        // 非法值 → clap Err（不 panic）。
        let mut bad = base.to_vec();
        bad.extend(["--cmb-mode", "bogus"]);
        assert!(
            Args::try_parse_from(bad).is_err(),
            "非法 --cmb-mode 应被 clap 拒"
        );
    }

    /// **CMB-P5** — `--cmb-size` / `--cmb-bir` 解析。
    #[test]
    fn cmb_size_and_bir_parse() {
        let v = [
            "nvme_firmware",
            "--backing-file",
            "x.img",
            "--tcp-addr",
            "127.0.0.1:9",
            "--cmb-size",
            "1048576",
            "--cmb-bir",
            "3",
        ];
        let a = Args::try_parse_from(v).expect("parse cmb-size/bir");
        assert_eq!(a.cmb_size, 1048576);
        assert_eq!(a.cmb_bir, 3);
    }

    /// **CMB-P5（模式协商）** — `off`/`trap` 任何 transport 不降级、无 warn。
    #[test]
    fn negotiate_off_and_trap_no_downgrade() {
        for t in [
            TransportKind::VfioUser,
            TransportKind::Tcp,
            TransportKind::Vsock,
        ] {
            let off = negotiate_cmb_mode(CmbMode::Off, t);
            assert_eq!(off.effective, CmbMode::Off);
            assert!(off.warning.is_none(), "off 无 warn");
            let trap = negotiate_cmb_mode(CmbMode::Trap, t);
            assert_eq!(trap.effective, CmbMode::Trap, "trap 任何 transport 可用");
            assert!(trap.warning.is_none(), "trap 无 warn");
        }
    }

    /// **CMB-P5（模式协商）** — `map`：vfio-user 保持 map（无 warn）；TCP 降级 off + warn；
    /// vsock 降级 trap + warn。
    #[test]
    fn negotiate_map_downgrades_per_transport() {
        // vfio-user：map 保持（真零拷贝），无 warn。
        let vfio = negotiate_cmb_mode(CmbMode::Map, TransportKind::VfioUser);
        assert_eq!(vfio.effective, CmbMode::Map);
        assert!(vfio.warning.is_none(), "map + vfio-user 不降级");
        // TCP：无 PCIe BAR → 降级 off + warn。
        let tcp = negotiate_cmb_mode(CmbMode::Map, TransportKind::Tcp);
        assert_eq!(tcp.effective, CmbMode::Off, "map + TCP → off");
        assert!(tcp.warning.is_some(), "map + TCP 必带降级 warn");
        // vsock(OpenHCL)：无 MemoryMapper → 降级 trap + warn。
        let vsock = negotiate_cmb_mode(CmbMode::Map, TransportKind::Vsock);
        assert_eq!(vsock.effective, CmbMode::Trap, "map + vsock → trap");
        assert!(vsock.warning.is_some(), "map + vsock 必带降级 warn");
    }

    /// **CMB-P5** — `--cmb-bir` 当前仅支持 2；非 2 报错（不 panic）。
    #[cfg(any(feature = "openhcl", feature = "vfio-user"))]
    #[test]
    fn validate_cmb_bir_only_supports_2() {
        assert!(validate_cmb_bir(2).is_ok(), "BAR2 支持");
        for bir in [0u8, 1, 3, 4, 5] {
            assert!(validate_cmb_bir(bir).is_err(), "BIR {bir} 暂不支持 → Err");
        }
    }
}
