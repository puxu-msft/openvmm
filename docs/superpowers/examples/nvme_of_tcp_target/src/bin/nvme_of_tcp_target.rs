// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V5d / V8b / V8f** — NVMe-over-Fabrics TCP target 长跑入口。
//!
//! 监听 4420（NVMe-oF TCP 常用端口），accept 一条 TCP 连接 → 在 thread
//! pool（最多 `--max-connections`）spawn 一个工作线程跑
//! `V2Session::accept_and_handshake_shared` + `pump_one` 循环。
//!
//! **V8b** — startup 一次 `NvmeController::open` → 用
//! `SharedControllerInner` (内含 `parking_lot::Mutex<NvmeController>` +
//! `AtomicU64` per-conn token slab 分配器) wrap，每条 conn `Arc::clone` 共享
//! 同一 controller 实例。V5d R-8 per-backing Mutex 已拆除。
//!
//! **V8f** — 可选 `--discovery-listen` 开 dual-listener：主 `--listen` 跑 IO
//! controller，第二端口跑独立 Discovery controller (0-byte tempfile backing，
//! 不接 IO)。两个 controller 实例 AER / qid 状态隔离防串扰。共享 SIGINT
//! `running` flag 让 SIGINT/SIGTERM 同时停两 loop。
//!
//! # 安全 (V5d-fix security review)
//!
//! **本 bin 无 TLS / 无 in-band auth / 无 host NQN 白名单**。Default listen
//! 改为 `127.0.0.1:4420`，强行配 0.0.0.0 必须 `--i-know-this-is-insecure`。
//! 生产环境请绑 loopback 或 VPN 内网，并加 IP 层 ACL。
//!
//! 已知限制：
//! - V5e-1 教学版单 IO ≤ 4 KiB（nlb ≤ 8 @ LBADS=9 单 PRP1 上限）；Linux
//!   nvme-cli 默认 `dd bs=4k` 1 cmd 完成。bs > 4 KiB 时 driver 见 SC=0x18
//!   自动拆分。
//! - V8c 待办：per-conn AER 路由 + per-conn IO queue 命名空间隔离；当前并发
//!   同 qid IO 会撞 controller 单 qid 表（教学版）。
//! - 无 TLS / DH-HMAC-CHAP

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{Context as _, Result};
use clap::Parser;
use nvme_of_tcp_target::V2Session;
use pcie_remote_nvme_userspace::NvmeController;
use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// **V5d-fix C-1** — 默认最大并发 conn 数。超出立即 drop。
const DEFAULT_MAX_CONNECTIONS: usize = 16;
/// **V5d-fix L-6** — handshake 阶段 socket read/write timeout。
const HANDSHAKE_TIMEOUT_SECS: u64 = 30;
/// **V5d-fix H-3** — accept Err 退避上限。
const ACCEPT_BACKOFF_MAX_MS: u64 = 1000;

#[derive(Parser, Debug)]
#[command(version, about = "NVMe-over-Fabrics TCP target (Phase V5d)")]
struct Cli {
    /// 监听地址（**V5d-fix C-4** 默认 `127.0.0.1:4420`；要绑 0.0.0.0
    /// 必须显式 `--i-know-this-is-insecure`）。
    #[arg(long, default_value = "127.0.0.1:4420")]
    listen: String,
    /// Backing file（重复指定 → 多 namespace；nsid 按命令行顺序 1,2,...）。
    #[arg(long = "backing-file", required = true)]
    backing_files: Vec<PathBuf>,
    /// PCIe Vendor ID（写入 Identify Controller，driver 用以匹配）。
    /// 默认 0x1414 为本教学项目所选；非 Microsoft 官方组件。
    #[arg(long, default_value = "0x1414", value_parser = parse_hex_u16)]
    vid: u16,
    /// PCIe Subsystem Vendor ID。
    #[arg(long, default_value = "0x0000", value_parser = parse_hex_u16)]
    ssvid: u16,
    /// 把指定 NSID 标记为 ZNS（Zoned Namespace）。可重复。
    #[arg(long = "zns-nsid")]
    zns_nsids: Vec<u32>,
    /// 最大并发 connection 数（**V5d-fix C-1** 防 thread/fd 耗尽）。
    #[arg(long, default_value_t = DEFAULT_MAX_CONNECTIONS)]
    max_connections: usize,
    /// **V5d-fix C-4** 显式确认你知道在非 loopback 接口暴露此 bin
    /// 等于让任何能到达端口的人远程读写 backing file（无 auth / 无 TLS）。
    #[arg(long, default_value_t = false)]
    i_know_this_is_insecure: bool,
    /// **V7** 启用 Discovery subsystem 模式（spec § 5.1.4）。bin 启动后
    /// 不开放 IO，仅响应 Discovery Log Page (LID=0x70)。Linux nvme-cli
    /// `nvme discover -t tcp -a <ip> -s <port>` 将列出 `--discovery-target-*`
    /// 注入的 portal 列表。
    #[arg(long, default_value_t = false)]
    discovery_mode: bool,
    /// **V7 / V8a** Discovery target NQN（discovery mode 必填，重复指定 = 多 portal）。
    /// 例：`--discovery-target-nqn nqn.foo --discovery-target-nqn nqn.bar`
    /// 必须与 `--discovery-target-addr` 数量相同（zip 配对）。
    #[arg(long)]
    discovery_target_nqn: Vec<String>,
    /// **V7 / V8a** Discovery target 网络地址 `IP:PORT`（discovery mode 必填，
    /// 重复指定 = 多 portal）。例：`--discovery-target-addr 127.0.0.1:4421
    /// --discovery-target-addr 127.0.0.1:4422`
    #[arg(long)]
    discovery_target_addr: Vec<String>,
    /// **V8f** 额外开 Discovery listener 监听独立端口。同进程内起两个 accept
    /// loop：主 `--listen` 跑 IO controller，本字段端口跑 discovery controller
    /// （独立 `NvmeController` 实例，AER / qid 状态隔离防串扰）。配合
    /// `--discovery-target-nqn/-addr` 注入 portal 列表。
    /// 不设此字段时 V8f 路径不启用，行为同 V8a（单 listener；`--discovery-mode`
    /// 仍可让主 listener 切 Discovery，但不再支持"同时 IO + Discovery"）。
    #[arg(long)]
    discovery_listen: Option<String>,
}

fn parse_hex_u16(s: &str) -> Result<u16, String> {
    // **V5d-fix H-4** — 只剥一次前缀，避免 `0x0x1234` 静默被吞。
    let stripped = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u16::from_str_radix(stripped, 16).map_err(|e| format!("invalid u16 hex {s:?}: {e}"))
}

fn main() -> Result<()> {
    // **V5d-fix H-1** — 默认 RUST_LOG=info；未设时也至少出 info。
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    // **V5d-fix-2 (review H-2)** — Rust std 在 unix startup 自动 SIG_IGN
    // SIGPIPE（见 `library/std/src/sys/pal/unix/mod.rs::init`），所以
    // `TcpStream::write` 撞 broken pipe 时返 `ErrorKind::BrokenPipe`
    // 而非 process terminate。worker thread 的 `?` 把 Err 上抛，连接
    // 终止但其他 worker 不受影响 —— 本 bin 不需要再加任何 SIGPIPE 处理。
    //
    // 如果未来仓库 / 用户禁用此 std 默认行为（极端罕见），需要加
    // signal-hook 或 nix dep 手动 SIG_IGN。当前 `forbid(unsafe_code)`
    // 下无法 manual `libc::signal` —— 接受 std 默认即可。

    let cli = Cli::parse();

    // **V5d-fix C-4** — non-loopback 必须 explicit consent
    let parsed_listen: SocketAddr = cli
        .listen
        .parse()
        .with_context(|| format!("--listen {} 不是合法 SocketAddr", cli.listen))?;
    let is_loopback = match parsed_listen.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    };
    if !is_loopback && !cli.i_know_this_is_insecure {
        eprintln!(
            "\n\x1b[1;31m⛔ refused to bind {} — non-loopback address requires \
             --i-know-this-is-insecure\x1b[0m",
            parsed_listen
        );
        eprintln!(
            "   本 bin 无 TLS / 无 in-band auth / 无 host NQN 白名单。\n   \
             任何能到达 {} 的 host 可远程读写所有 --backing-file 内容。\n   \
             默认推荐 --listen 127.0.0.1:4420；公网/共享 LAN 部署务必先加\n   \
             IP 层 ACL / VPN / Wireguard，再加 --i-know-this-is-insecure 启动。\n",
            parsed_listen
        );
        std::process::exit(2);
    }
    if !is_loopback {
        tracing::warn!(
            listen = %parsed_listen,
            "⚠️  binding non-loopback address WITHOUT auth/TLS; \
             any host reaching this port has full backing-file access"
        );
    }

    // **V8b** — V5d-fix C-3 多 backing R-8 限制已拆除；现单 controller 实例
    // open 所有 backing files，多 conn 通过 `Arc<Mutex<NvmeController>>` 共享。
    // 多 namespace = 多 backing 现可全部并发访问（spec 兼容）。

    tracing::info!(
        listen = %parsed_listen,
        backing_files = ?cli.backing_files,
        vid = cli.vid,
        ssvid = cli.ssvid,
        zns_nsids = ?cli.zns_nsids,
        max_connections = cli.max_connections,
        discovery_mode = cli.discovery_mode,
        "V5d/V7 nvme_of_tcp_target start"
    );

    // **V7 / V8a** — discovery mode 必填 target NQN + addr 多 portal；
    // zip 配对（必须等长且非空）；spawn 时 clone 给每条 conn handler
    let discovery_portals: Vec<
        pcie_remote_nvme_userspace::controller::discovery_log::DiscoveryPortal,
    > = if cli.discovery_mode {
        if cli.discovery_target_nqn.is_empty() || cli.discovery_target_addr.is_empty() {
            anyhow::bail!(
                "--discovery-mode 必须配至少一对 --discovery-target-nqn + --discovery-target-addr"
            );
        }
        if cli.discovery_target_nqn.len() != cli.discovery_target_addr.len() {
            anyhow::bail!(
                "--discovery-target-nqn ({}) 与 --discovery-target-addr ({}) 数量必须相同 (zip 配对)",
                cli.discovery_target_nqn.len(),
                cli.discovery_target_addr.len()
            );
        }
        let mut portals = Vec::with_capacity(cli.discovery_target_nqn.len());
        for (nqn, addr) in cli
            .discovery_target_nqn
            .iter()
            .zip(cli.discovery_target_addr.iter())
        {
            let portal = pcie_remote_nvme_userspace::controller::discovery_log::DiscoveryPortal::from_ipv4_addr(
                nqn, addr,
            )
            .with_context(|| format!("parse --discovery-target-addr {addr:?}"))?;
            tracing::info!(
                nqn = portal.nqn.as_str(),
                traddr = portal.traddr.as_str(),
                trsvcid = portal.trsvcid.as_str(),
                "V8a discovery portal"
            );
            portals.push(portal);
        }
        tracing::info!(count = portals.len(), "V8a discovery mode active");
        portals
    } else {
        Vec::new()
    };

    // **V8b** — 拆 V5d R-8 per-backing Mutex；改为 startup 一次性 open
    // NvmeController 并 wrap into Arc<Mutex<>>，每条 conn `Arc::clone` 共享。
    // Linux nvme-cli 默认 4 IO queue（= 4 TCP conn）现可全连同一 controller，
    // 多 conn 之间通过 controller 一把 parking_lot::Mutex 序列化（短锁 dispatch
    // 模型，session.rs `with_controller` helper 已实现 R-1 死锁规避）。
    // **V8b reviewer C-1** — per-conn token slab 通过 `SharedControllerInner` 内
    // `AtomicU64` 原子分配，避免多 conn 共享 `pending_ios` 全局 token 池撞 key。
    //
    // NvmeController::open 不是 thread-safe internal state；必须 startup 仅
    // 调一次。后续 spawn 每条 conn 只 Arc::clone 共享同一实例。
    let backing_strs: Vec<String> = cli
        .backing_files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let mut shared_ctrl_inner =
        NvmeController::open(&backing_strs, cli.vid, cli.ssvid, &cli.zns_nsids)
            .context("NvmeController::open (startup, V8b shared)")?;
    // **V7 / V8a** — discovery_portals 在 controller share 时 startup 注入一次。
    if !discovery_portals.is_empty() {
        shared_ctrl_inner.nvme_set_discovery_target(discovery_portals);
    }
    let shared_controller: nvme_of_tcp_target::SharedController = Arc::new(
        nvme_of_tcp_target::SharedControllerInner::new(shared_ctrl_inner),
    );

    // **V8f (reviewer M-1/M-2)** — 早期校验 `--discovery-listen` 字符串 +
    // `--discovery-target-*` 配对，把 cheap validation 抬到 controller open 之前
    // 防 disc_ctrl 白白开起再 drop；parsed addr 用变量缓存避免后续 .expect()。
    let parsed_discovery_listen: Option<SocketAddr> =
        match cli.discovery_listen.as_deref() {
            Some(addr_str) => {
                if cli.discovery_target_nqn.is_empty() {
                    anyhow::bail!("--discovery-listen 需配 --discovery-target-nqn/-addr (V8f Q6)");
                }
                Some(addr_str.parse().with_context(|| {
                    format!("--discovery-listen {addr_str:?} 不是合法 SocketAddr")
                })?)
            }
            None => None,
        };

    // **V8f** — 可选 discovery listener：独立 NvmeController + 独立 SharedControllerInner，
    // AER / qid 状态不与主 IO controller 串扰（plan §6 Q6）。
    // **V8f reviewer H-2** — discovery controller 用独立 0-byte tempfile 作 backing：
    // 不复用主 backing 文件防同文件双 open 跨 controller race；discovery 路径走
    // session.discovery_mode 拒 IO，0-byte 文件足矣。
    // **V8f reviewer H-2** — discovery controller 用独立最小 tempfile 作 backing：
    // 不复用主 backing 文件防同文件双 open 跨 controller race；512B（1 LBA）足
    // 让 NvmeController::open 通过最小校验，discovery 路径走 session.discovery_mode
    // 拒 IO，文件实际不会被读写。
    let _disc_backing_keepalive: Option<tempfile::NamedTempFile>;
    let discovery_shared: Option<nvme_of_tcp_target::SharedController> = if let Some(disc_addr) =
        parsed_discovery_listen
    {
        // 512B tempfile（1 LBA @ LBADS=9）：drop 后被自动清；持有到 main 退出
        let f = tempfile::NamedTempFile::new()
            .context("V8f: 创建 discovery controller 最小 tempfile")?;
        f.as_file()
            .set_len(512)
            .context("V8f: discovery tempfile set_len(512)")?;
        let path = f.path().to_string_lossy().into_owned();
        let mut disc_ctrl =
            NvmeController::open(std::slice::from_ref(&path), cli.vid, cli.ssvid, &[])
                .context("NvmeController::open (V8f discovery, isolated backing)")?;
        let mut ps = Vec::with_capacity(cli.discovery_target_nqn.len());
        for (nqn, addr) in cli
            .discovery_target_nqn
            .iter()
            .zip(cli.discovery_target_addr.iter())
        {
            let p = pcie_remote_nvme_userspace::controller::discovery_log::DiscoveryPortal::from_ipv4_addr(
                    nqn, addr,
                )
                .with_context(|| format!("parse V8f discovery_target_addr {addr:?}"))?;
            ps.push(p);
        }
        disc_ctrl.nvme_set_discovery_target(ps);
        tracing::info!(addr = %disc_addr, "V8f 启用 dual-listener 模式 (discovery)");
        _disc_backing_keepalive = Some(f);
        Some(Arc::new(nvme_of_tcp_target::SharedControllerInner::new(
            disc_ctrl,
        )))
    } else {
        _disc_backing_keepalive = None;
        None
    };

    // **V5d-fix C-1** — 并发上限信号量
    let inflight = Arc::new(AtomicUsize::new(0));
    let max_conn = cli.max_connections;

    // **V5d-fix-2 (review M-1) + V5e-1-fix (review H-2)** — SIGINT/SIGTERM
    // graceful shutdown via `ctrlc` crate（跨平台、无 unsafe）。flag flip
    // → non-blocking listener exit accept loop → 等 in-flight worker 最多
    // 2s drain → exit 0。
    //
    // **H-2 fix**：startup 失败必须 hard-fail。本 bin 唯一 graceful shutdown
    // 机制就是 ctrlc handler；silent degrade 到 "Ctrl-C 整 process 死" 与
    // M-1 立意冲突。
    let running = Arc::new(AtomicBool::new(true));
    {
        let running = Arc::clone(&running);
        ctrlc::set_handler(move || {
            tracing::info!("SIGINT/SIGTERM received; stopping accept loop");
            running.store(false, Ordering::SeqCst);
        })
        .context("install SIGINT/SIGTERM handler (V5e-1-fix H-2 require)")?;
    }

    let listener =
        TcpListener::bind(parsed_listen).with_context(|| format!("bind {parsed_listen}"))?;
    listener
        .set_nonblocking(true)
        .context("set listener non-blocking")?;
    tracing::info!("listening on {}", listener.local_addr()?);

    // **V8f** — 若 dual-listener 模式，先 spawn discovery accept loop（独立 thread），
    // 主 thread 跑主 IO listener。两个 loop 共享 `running` flag，SIGINT 同时停。
    let disc_thread: Option<std::thread::JoinHandle<Result<()>>> =
        if let (Some(disc_shared), Some(disc_addr)) =
            (discovery_shared.clone(), parsed_discovery_listen)
        {
            let disc_listener = TcpListener::bind(disc_addr)
                .with_context(|| format!("bind discovery {disc_addr}"))?;
            disc_listener
                .set_nonblocking(true)
                .context("set discovery listener non-blocking")?;
            tracing::info!("V8f discovery listening on {}", disc_listener.local_addr()?);
            let running_clone = Arc::clone(&running);
            let disc_inflight = Arc::new(AtomicUsize::new(0));
            Some(std::thread::spawn(move || -> Result<()> {
                run_accept_loop(
                    disc_listener,
                    disc_shared,
                    running_clone,
                    disc_inflight,
                    max_conn,
                    "discovery",
                )
            }))
        } else {
            None
        };

    // **V8f reviewer H-1** — 不论 main loop 成功还是 Err 都先 flip running 通知
    // discovery thread 停，防 main 早 Err return 留下僵尸 discovery thread。
    let main_result = run_accept_loop(
        listener,
        Arc::clone(&shared_controller),
        Arc::clone(&running),
        Arc::clone(&inflight),
        max_conn,
        "main",
    );
    running.store(false, Ordering::SeqCst);

    // **V8f reviewer H-3** — discovery thread Err 应让 process exit code != 0。
    let disc_result: Result<()> = if let Some(t) = disc_thread {
        match t.join() {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("V8f discovery thread panic")),
        }
    } else {
        Ok(())
    };

    tracing::info!("exit");
    // 优先返 main Err（启动后主路径失败更关键）；main OK 时返 disc Err。
    match (main_result, disc_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), _) => Err(e),
        (Ok(()), Err(e)) => Err(e.context("V8f discovery listener failed")),
    }
}

/// **V8f** — accept loop 抽出复用（主 IO listener 与 discovery listener 共用）。
/// 与 V5d 原 inline 实现 1:1 等价；只把 `&listener` / `shared_ctrl` / 标签提
/// 成参数。`label` 出现在日志，便于区分两 loop。
fn run_accept_loop(
    listener: TcpListener,
    shared: nvme_of_tcp_target::SharedController,
    running: Arc<AtomicBool>,
    inflight: Arc<AtomicUsize>,
    max_conn: usize,
    label: &'static str,
) -> Result<()> {
    // **V5d-fix H-3** — accept Err 指数退避
    let mut accept_backoff_ms: u64 = 0;
    while running.load(Ordering::SeqCst) {
        let (stream, peer) = match listener.accept() {
            Ok((s, addr)) => {
                accept_backoff_ms = 0;
                (s, addr)
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                accept_backoff_ms = 0;
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let dur = Duration::from_millis(accept_backoff_ms.max(10));
                tracing::warn!(label, error = %e, backoff_ms = dur.as_millis(), "accept failed");
                std::thread::sleep(dur);
                accept_backoff_ms = (accept_backoff_ms.max(10) * 2).min(ACCEPT_BACKOFF_MAX_MS);
                continue;
            }
        };

        let cur = inflight.fetch_add(1, Ordering::SeqCst);
        if cur >= max_conn {
            inflight.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!(label, %peer, max_conn, "rejecting: max-connections reached");
            drop(stream);
            continue;
        }

        let _ = stream.set_read_timeout(Some(Duration::from_secs(HANDSHAKE_TIMEOUT_SECS)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(HANDSHAKE_TIMEOUT_SECS)));
        tracing::info!(label, %peer, "accepted connection");

        let inflight = Arc::clone(&inflight);
        let shared_ctrl = Arc::clone(&shared);
        std::thread::spawn(move || {
            let r = handle_conn(stream, shared_ctrl);
            inflight.fetch_sub(1, Ordering::SeqCst);
            match r {
                Ok(()) => tracing::info!(label, %peer, "connection closed normally"),
                Err(e) => tracing::warn!(label, %peer, error = %e, "connection ended with error"),
            }
        });
    }
    tracing::info!(
        label,
        "accept loop stopped; waiting up to 2s for in-flight workers"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while inflight.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// **V8b** — 每条 TCP 连接的工作流（多 conn 共享 controller）：
/// 1. 走 `V2Session::accept_and_handshake_shared` 把已 wrap 的 controller
///    Arc clone 给 session
/// 2. `pump_one_with_events` loop 直至 peer close 或 Err
///
/// V5d R-8 per-backing Mutex 已拆；多 conn 通过 `Arc<Mutex<NvmeController>>`
/// 共享同一 controller 实例（短锁 dispatch + R-1 死锁规避在 session 端）。
fn handle_conn(stream: TcpStream, shared_ctrl: nvme_of_tcp_target::SharedController) -> Result<()> {
    let mut sess = V2Session::accept_and_handshake_shared(stream, shared_ctrl)
        .context("V2Session handshake (V8b shared)")?;
    // **V6b** — pump_one_with_events 每 100ms drain pending AEN + try read_pdu
    while sess.pump_one_with_events(std::time::Duration::from_millis(100))? {}
    Ok(())
}
