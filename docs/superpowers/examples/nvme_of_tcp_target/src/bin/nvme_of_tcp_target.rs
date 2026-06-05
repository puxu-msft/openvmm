// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V5d** — NVMe-over-Fabrics TCP target 长跑入口。
//!
//! 监听 4420（NVMe-oF TCP 常用端口），accept 一条 TCP 连接 → 在 thread
//! pool（最多 `--max-connections`）spawn 一个工作线程跑
//! `V2Session::accept_and_handshake` + `pump_one` 循环。每个 backing
//! file 一把 `Mutex`，同一文件同时只允许 1 个 active conn（V5d R-8）。
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
//! - 单 backing file 同时仅一 active connection
//! - 多 `--backing-file` 时仅 first file 受 R-8 互斥保护 → 启动 WARN
//! - 无 Discovery subsystem（必须 `nvme connect -n nqn...`）
//! - 无 TLS / DH-HMAC-CHAP

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{Context as _, Result};
use clap::Parser;
use nvme_of_tcp_target::V2Session;
use parking_lot::Mutex;
use pcie_remote_nvme_userspace::NvmeController;
use std::collections::HashMap;
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

    // **V5d-fix C-3** — 多 backing 配置仅 first file 受 R-8 互斥保护
    if cli.backing_files.len() > 1 {
        tracing::warn!(
            count = cli.backing_files.len(),
            "V5d R-8 教学版仅对 first --backing-file 加并发互斥锁；\
             其余 backing 不受跨 conn race 保护（V8 重构解除）"
        );
    }

    tracing::info!(
        listen = %parsed_listen,
        backing_files = ?cli.backing_files,
        vid = cli.vid,
        ssvid = cli.ssvid,
        zns_nsids = ?cli.zns_nsids,
        max_connections = cli.max_connections,
        "V5d nvme_of_tcp_target start"
    );

    // **V5d-fix C-2** — 启动时预 insert 所有 backing 的 lock entry，
    // 之后 worker 只读不写该 map → 杜绝动态增长 + path-aliasing race。
    // 用 std::path::absolute (不是 canonicalize) 因为 repo clippy 禁；
    // 在 path 真正存在前不解 symlink，与 NvmeController::open 真实行为一致。
    let backing_locks: HashMap<PathBuf, Arc<Mutex<()>>> = cli
        .backing_files
        .iter()
        .map(|p| {
            let canon = std::path::absolute(p).unwrap_or_else(|_| p.clone());
            (canon, Arc::new(Mutex::new(())))
        })
        .collect();
    let backing_locks = Arc::new(backing_locks);

    // **V5d-fix C-1** — 并发上限信号量
    let inflight = Arc::new(AtomicUsize::new(0));
    let max_conn = cli.max_connections;

    // **V5d-fix-2 (review M-1)** — SIGINT/SIGTERM graceful shutdown via `ctrlc` crate
    // （跨平台、无 unsafe）。flag flip → non-blocking listener 退 accept loop →
    // 等 in-flight worker 最多 2s drain → exit。
    let running = Arc::new(AtomicBool::new(true));
    {
        let running = Arc::clone(&running);
        if let Err(e) = ctrlc::set_handler(move || {
            tracing::info!("SIGINT/SIGTERM received; stopping accept loop");
            running.store(false, Ordering::SeqCst);
        }) {
            tracing::warn!(error = %e, "ctrlc::set_handler failed; Ctrl-C will terminate immediately");
        }
    }

    let listener =
        TcpListener::bind(parsed_listen).with_context(|| format!("bind {parsed_listen}"))?;
    listener
        .set_nonblocking(true)
        .context("set listener non-blocking")?;
    tracing::info!("listening on {}", listener.local_addr()?);

    // **V5d-fix H-3** — accept Err 指数退避
    let mut accept_backoff_ms: u64 = 0;
    while running.load(Ordering::SeqCst) {
        let (stream, peer) = match listener.accept() {
            Ok((s, addr)) => {
                accept_backoff_ms = 0;
                (s, addr)
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // non-blocking accept 没新连接；slight sleep 让 CPU 不打转。
                // **V5d-fix-2 (review H-3)** — 同步 reset backoff 防上次真 Err
                // 累加后第一次 retry 用陈旧大值。
                accept_backoff_ms = 0;
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let dur = Duration::from_millis(accept_backoff_ms.max(10));
                tracing::warn!(error = %e, backoff_ms = dur.as_millis(), "accept failed");
                std::thread::sleep(dur);
                accept_backoff_ms = (accept_backoff_ms.max(10) * 2).min(ACCEPT_BACKOFF_MAX_MS);
                continue;
            }
        };

        let cur = inflight.fetch_add(1, Ordering::SeqCst);
        if cur >= max_conn {
            inflight.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!(%peer, max_conn, "rejecting: max-connections reached");
            drop(stream);
            continue;
        }

        // **V5d-fix L-6** — handshake 阶段强 read/write timeout 防 slowloris
        let _ = stream.set_read_timeout(Some(Duration::from_secs(HANDSHAKE_TIMEOUT_SECS)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(HANDSHAKE_TIMEOUT_SECS)));

        tracing::info!(%peer, "accepted connection");

        let backing_files = cli.backing_files.clone();
        let vid = cli.vid;
        let ssvid = cli.ssvid;
        let zns_nsids = cli.zns_nsids.clone();
        let backing_locks = Arc::clone(&backing_locks);
        let inflight = Arc::clone(&inflight);

        std::thread::spawn(move || {
            let r = handle_conn(stream, backing_files, vid, ssvid, &zns_nsids, backing_locks);
            inflight.fetch_sub(1, Ordering::SeqCst);
            match r {
                Ok(()) => tracing::info!(%peer, "connection closed normally"),
                Err(e) => tracing::warn!(%peer, error = %e, "connection ended with error"),
            }
        });
    }
    tracing::info!("accept loop stopped; waiting up to 2s for in-flight workers");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while inflight.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    tracing::info!("exit");
    Ok(())
}

/// 每条 TCP 连接的工作流：
/// 1. acquire per-backing Mutex（R-8 教学版限制）
/// 2. open NvmeController over backing files
/// 3. V2Session::accept_and_handshake → pump_one loop 直至 peer close 或 Err
fn handle_conn(
    stream: TcpStream,
    backing_files: Vec<PathBuf>,
    vid: u16,
    ssvid: u16,
    zns_nsids: &[u32],
    backing_locks: Arc<HashMap<PathBuf, Arc<Mutex<()>>>>,
) -> Result<()> {
    // R-8：只 lock first backing（V8 重构）
    let first = backing_files
        .first()
        .ok_or_else(|| anyhow::anyhow!("--backing-file required"))?;
    let canon = std::path::absolute(first).unwrap_or_else(|_| first.clone());
    let lock = backing_locks
        .get(&canon)
        .ok_or_else(|| anyhow::anyhow!("backing lock entry missing (bin startup bug)"))?;
    let _guard = match lock.try_lock() {
        Some(g) => g,
        None => {
            tracing::warn!(
                backing = ?canon,
                "拒绝连接：同一 backing file 已有 active conn (V5d R-8 教学版限制)"
            );
            anyhow::bail!("backing file busy: another conn already holds the lock");
        }
    };

    let backing_strs: Vec<String> = backing_files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let controller = NvmeController::open(&backing_strs, vid, ssvid, zns_nsids)
        .context("NvmeController::open")?;

    let mut sess =
        V2Session::accept_and_handshake(stream, controller).context("V2Session handshake")?;
    while sess.pump_one()? {}
    Ok(())
}
