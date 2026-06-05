// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V5d / V8b / V8f / V8e-2 / V8e-7-4** — NVMe-over-Fabrics TCP target 长跑入口。
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
//! controller，第二端口跑独立 Discovery controller (512B tempfile backing，
//! 不接 IO)。两个 controller 实例 AER / qid 状态隔离防串扰。
//!
//! **V8e-2** — bin 主入口改 `#[tokio::main(multi_thread)]`；ctrlc + AtomicBool
//! → `tokio::signal::ctrl_c` + `tokio::sync::watch::Sender<bool>` 作 shutdown
//! 信号；`std::net::TcpListener` → `tokio::net::TcpListener`。
//!
//! **V8e-7-4** — 每条 conn 改 `tokio::spawn(handle_conn_async)` 直接走
//! `AsyncSession::dispatch_pdu_async`（V8e-2 的 `spawn_blocking(handle_conn)`
//! sync 桥已退役）。AsyncSession 端 select! 4 arm 接 shutdown / AER notify /
//! KATO Sleep / read_pdu_async；KATO timer 在 Connect 后真 arm（spec § 7.13）。
//! V8f 双 listener 共享 watch channel，SIGINT/SIGTERM 同时停。
//!
//! sync `handle_conn`（V8b legacy 路径）保留作 V8b/c/d/f 集成测试 + V-followup
//! 参考，bin 主路径已不再调用（`#[allow(dead_code)]`）。
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
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// **V5d-fix C-1** — 默认最大并发 conn 数。超出立即 drop。
const DEFAULT_MAX_CONNECTIONS: usize = 16;
/// **V5d-fix L-6** — handshake 阶段 socket read/write timeout。
/// **V8e-7-4**：bin 切到 AsyncSession 后不再用（KATO timer 接管 idle 防护）；
/// 保留作 sync `handle_conn` legacy 路径以及 V-followup 参考。
#[allow(dead_code)]
const HANDSHAKE_TIMEOUT_SECS: u64 = 30;
/// **V5d-fix H-3** — accept Err 退避上限。
const ACCEPT_BACKOFF_MAX_MS: u64 = 1000;
/// **V8e-2** — shutdown 信号收到后等 in-flight worker drain 上限。
const SHUTDOWN_DRAIN_SECS: u64 = 2;
/// **V-followup-tls-3 (plan R-10)** — TLS handshake 整 socket 超时（含
/// ClientHello 半句 slowloris 防护）。spec 不规定具体值；与 sync 路径
/// `HANDSHAKE_TIMEOUT_SECS=30` 对等。
const TLS_HANDSHAKE_TIMEOUT_SECS: u64 = 30;

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
    /// **V-followup-tls-3** 额外开 TLS listener（spec § 8.13 推荐端口 8009）。
    /// 同进程内 main IO listener 与本 TLS listener 共享同一 controller；
    /// plaintext 路径行为 100% 不变。TLS port 不 fallback plaintext，handshake
    /// fail 直接 drop 防 downgrade 漏洞（plan R-5）。
    /// 设此字段时必须同时配 `--tls-cert + --tls-key + --tls-i-trust-this-cert`。
    #[arg(long)]
    tls_listen: Option<String>,
    /// **V-followup-tls-3** X.509 cert PEM 路径（PEM block "CERTIFICATE"）。
    /// 单 cert 或 cert chain 均可。
    #[arg(long)]
    tls_cert: Option<PathBuf>,
    /// **V-followup-tls-3** 私钥 PEM 路径（接受 PKCS#8 / PKCS#1 RSA / SEC1 EC）。
    /// 文件权限请设 `chmod 600` 防误读。
    #[arg(long)]
    tls_key: Option<PathBuf>,
    /// **V-followup-tls-3** 显式 consent：本 TLS cert 不做 chain validation，
    /// 也不做 host NQN ↔ identity binding。自签 cert 或测试 cert 也接受 —
    /// 等于让任何能跑成 TLS handshake 的 peer 远程读写 backing file。
    /// 与 `--tls-listen` 同设强制。
    #[arg(long, default_value_t = false)]
    tls_i_trust_this_cert: bool,
    /// **V-followup-mtls** 启用 mTLS：强制 client 出示 cert，且 chain 必须
    /// anchor 到本 PEM bundle 的 trust roots。一旦设置，TLS listener 即
    /// 改走 [`nvme_of_tcp_target::build_acceptor_with_mtls`]；client
    /// 不带 cert / cert 不可信 → handshake 失败被 drop。
    /// 仅当 `--tls-listen` 启用时生效。
    #[arg(long)]
    tls_client_ca: Option<PathBuf>,
}

fn parse_hex_u16(s: &str) -> Result<u16, String> {
    // **V5d-fix H-4** — 只剥一次前缀，避免 `0x0x1234` 静默被吞。
    let stripped = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u16::from_str_radix(stripped, 16).map_err(|e| format!("invalid u16 hex {s:?}: {e}"))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
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

    // **V8e-2** — SIGINT/SIGTERM 改 tokio 原生支持：
    //   - `tokio::signal::ctrl_c()` 拿 SIGINT future
    //   - `tokio::sync::watch::Sender<bool>` 作 shutdown 信号 channel
    //   - main + V8f discovery accept loop 各 `subscribe()` 拿独立 Receiver
    //   - signal handler task 收到 SIGINT/SIGTERM → `send(true)`
    //
    // 替代 V5d/V5e/V8f 用的 `ctrlc` crate + `AtomicBool running`；watch 是 tokio
    // 内置（V8e-1 dep）无 unsafe，跨 unix/windows，比 AtomicBool 更适合 async。
    //
    // **V8e-2 reviewer M-2** — SIGTERM listener 在 spawn 前 main 内构造，让
    // 安装失败 hard-fail（spawn 内 `.expect()` panic 会被 tokio 吞导致 process
    // 僵尸 accept）。
    let (shutdown_tx, shutdown_rx_main) = tokio::sync::watch::channel(false);
    let shutdown_rx_disc = shutdown_tx.subscribe();
    let shutdown_rx_tls = shutdown_tx.subscribe();
    #[cfg(unix)]
    let mut sigterm_stream = {
        use tokio::signal::unix::{SignalKind, signal};
        signal(SignalKind::terminate()).context("install SIGTERM listener (V8e-2)")?
    };
    {
        let shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            // ctrl_c() 已跨平台抽象（unix=SIGINT；windows=Ctrl-C event）
            // SIGTERM (unix-only) 通过 main 端 signal() 构造的 stream 接收。
            #[cfg(unix)]
            {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("SIGINT received; stopping accept loops");
                    }
                    _ = sigterm_stream.recv() => {
                        tracing::info!("SIGTERM received; stopping accept loops");
                    }
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("Ctrl-C received; stopping accept loops");
            }
            let _ = shutdown_tx.send(true);
        });
    }
    // **V8e-2 reviewer M-1** — 之前留的 `shutdown_rx_signal` 哨值已删；watch
    // sender 由 `shutdown_tx`（main）+ clone（signal task）共持，receiver drop
    // 不影响 sender 生命周期。

    let listener = tokio::net::TcpListener::bind(parsed_listen)
        .await
        .with_context(|| format!("bind {parsed_listen}"))?;
    tracing::info!("listening on {}", listener.local_addr()?);

    // **V-followup-tls-3 (plan R-4/R-5)** — CLI 校验 + acceptor build。
    // 双 explicit consent 模型：`--tls-listen` 必须配 `--tls-cert + --tls-key
    // + --tls-i-trust-this-cert`。本 cert 不做 chain validation，缺一即 hard-fail。
    // acceptor build 在 controller open 之前做，PEM 解析错快速 abort。
    let parsed_tls_listen: Option<SocketAddr> = match cli.tls_listen.as_deref() {
        Some(addr_str) => {
            if cli.tls_cert.is_none() || cli.tls_key.is_none() {
                anyhow::bail!("--tls-listen 必须同时配 --tls-cert + --tls-key (V-followup-tls-3)");
            }
            if !cli.tls_i_trust_this_cert {
                eprintln!(
                    "\n\x1b[1;31m⛔ refused to start TLS listener — \
                     --tls-i-trust-this-cert required\x1b[0m"
                );
                eprintln!(
                    "   本 TLS cert 不做 chain validation 也不做 host NQN identity\n   \
                     binding；自签 cert 或测试 cert 直通。\n   \
                     仅在 dev / lab / CTF 等可控环境启用；生产请等 V-followup-auth\n   \
                     (NQN binding) + V-followup-mtls (client cert require) 落地。\n"
                );
                std::process::exit(2);
            }
            Some(
                addr_str
                    .parse()
                    .with_context(|| format!("--tls-listen {addr_str:?} 不是合法 SocketAddr"))?,
            )
        }
        None => {
            if cli.tls_cert.is_some()
                || cli.tls_key.is_some()
                || cli.tls_i_trust_this_cert
                || cli.tls_client_ca.is_some()
            {
                anyhow::bail!(
                    "--tls-cert / --tls-key / --tls-i-trust-this-cert / --tls-client-ca 仅在 --tls-listen 启用时生效"
                );
            }
            None
        }
    };

    // **V-followup-tls-3 / V-followup-mtls** — TlsAcceptor build；PEM 错快速 abort
    let tls_acceptor: Option<tokio_rustls::TlsAcceptor> = if parsed_tls_listen.is_some() {
        let cert_path = cli.tls_cert.as_ref().expect("CLI 校验保证非 None");
        let key_path = cli.tls_key.as_ref().expect("CLI 校验保证非 None");
        let acc = match cli.tls_client_ca.as_ref() {
            Some(ca_path) => {
                tracing::warn!(
                    cert = %cert_path.display(),
                    client_ca = %ca_path.display(),
                    "🔒 V-followup-mtls 启用：强制 client cert，chain 必须 anchor 到 --tls-client-ca"
                );
                nvme_of_tcp_target::build_acceptor_with_mtls(cert_path, key_path, ca_path)
                    .context("V-followup-mtls: TlsAcceptor build (with client CA)")?
            }
            None => {
                tracing::warn!(
                    cert = %cert_path.display(),
                    "⚠️  TLS listener enabled WITHOUT cert chain validation / NQN identity binding (server-auth only)"
                );
                nvme_of_tcp_target::build_acceptor_from_pem(cert_path, key_path)
                    .context("V-followup-tls-3: TlsAcceptor build")?
            }
        };
        Some(acc)
    } else {
        None
    };

    // **V8f / V8e-2** — 若 dual-listener 模式，先 spawn discovery accept loop
    // （独立 tokio task），主 task 跑主 IO listener。两个 loop 共享 watch
    // shutdown channel，SIGINT 同时停。
    let disc_task: Option<tokio::task::JoinHandle<Result<()>>> =
        if let (Some(disc_shared), Some(disc_addr)) =
            (discovery_shared.clone(), parsed_discovery_listen)
        {
            let disc_listener = tokio::net::TcpListener::bind(disc_addr)
                .await
                .with_context(|| format!("bind discovery {disc_addr}"))?;
            tracing::info!("V8f discovery listening on {}", disc_listener.local_addr()?);
            let disc_inflight = Arc::new(AtomicUsize::new(0));
            Some(tokio::spawn(run_accept_loop(
                disc_listener,
                disc_shared,
                shutdown_rx_disc,
                disc_inflight,
                max_conn,
                "discovery",
            )))
        } else {
            drop(shutdown_rx_disc);
            None
        };

    // **V-followup-tls-3** — TLS accept loop（共享同一 controller）。
    // 与 main / discovery loop 同结构；区别在 accept 后多一步 acceptor.accept
    // + timeout 包；session handler 用 `handle_conn_async_tls` (泛型 monomorphize
    // 一份给 TlsStream<TcpStream>)。
    let tls_task: Option<tokio::task::JoinHandle<Result<()>>> =
        if let (Some(acceptor), Some(tls_addr)) = (tls_acceptor, parsed_tls_listen) {
            let tls_listener = tokio::net::TcpListener::bind(tls_addr)
                .await
                .with_context(|| format!("bind tls {tls_addr}"))?;
            tracing::info!(
                "V-followup-tls-3 TLS listening on {}",
                tls_listener.local_addr()?
            );
            let tls_inflight = Arc::new(AtomicUsize::new(0));
            Some(tokio::spawn(run_accept_loop_tls(
                tls_listener,
                acceptor,
                Arc::clone(&shared_controller),
                shutdown_rx_tls,
                tls_inflight,
                max_conn,
            )))
        } else {
            drop(shutdown_rx_tls);
            None
        };

    // **V8f reviewer H-1 / V8e-2** — 不论 main loop 成功还是 Err 都先让
    // shutdown_tx 发信号通知 discovery task 停。
    let main_result = run_accept_loop(
        listener,
        Arc::clone(&shared_controller),
        shutdown_rx_main,
        Arc::clone(&inflight),
        max_conn,
        "main",
    )
    .await;
    let _ = shutdown_tx.send(true);

    // **V8f reviewer H-3 / V8e-2** — discovery task Err 应让 process exit code != 0。
    let disc_result: Result<()> = if let Some(t) = disc_task {
        match t.await {
            Ok(r) => r,
            Err(e) if e.is_cancelled() => Ok(()),
            Err(e) => Err(anyhow::anyhow!("V8f discovery task join: {e}")),
        }
    } else {
        Ok(())
    };

    // **V-followup-tls-3** — 与 discovery 同模式；TLS task Err 让 exit != 0
    let tls_result: Result<()> = if let Some(t) = tls_task {
        match t.await {
            Ok(r) => r,
            Err(e) if e.is_cancelled() => Ok(()),
            Err(e) => Err(anyhow::anyhow!("V-followup-tls-3 TLS task join: {e}")),
        }
    } else {
        Ok(())
    };

    tracing::info!("exit");
    // **V8f reviewer H-3 + V8e-2 reviewer M-3 + V-followup-tls-3** — 优先返 main
    // Err（启动后主路径失败更关键）；其余 task Err 按顺序传播让 process exit != 0。
    match (main_result, disc_result, tls_result) {
        (Ok(()), Ok(()), Ok(())) => Ok(()),
        (Err(e), _, _) => Err(e),
        (Ok(()), Err(e), _) => Err(e.context("V8f discovery listener failed")),
        (Ok(()), Ok(()), Err(e)) => Err(e.context("V-followup-tls-3 TLS listener failed")),
    }
}

/// **V8f / V8e-2** — accept loop 抽出复用（主 IO listener 与 discovery listener
/// 共用）。改 `tokio::net::TcpListener::accept().await` + `tokio::select!`
/// 与 shutdown watch 多路复用；每条 conn `spawn_blocking` 把当前 sync
/// `handle_conn` 跑在 blocking pool（V8e-3 后改 async `tokio::spawn`）。
async fn run_accept_loop(
    listener: tokio::net::TcpListener,
    shared: nvme_of_tcp_target::SharedController,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    inflight: Arc<AtomicUsize>,
    max_conn: usize,
    label: &'static str,
) -> Result<()> {
    let mut accept_backoff_ms: u64 = 0;
    loop {
        // **V8e-2** — select! 替代 V5d non-blocking + 10ms sleep poll。
        let accept_result = tokio::select! {
            biased; // 优先看 shutdown，防 burst 连接饥饿信号
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow_and_update() {
                    break;
                }
                continue;
            }
            r = listener.accept() => r,
        };

        let (stream, peer) = match accept_result {
            Ok((s, addr)) => {
                accept_backoff_ms = 0;
                (s, addr)
            }
            Err(e) => {
                let dur = Duration::from_millis(accept_backoff_ms.max(10));
                tracing::warn!(label, error = %e, backoff_ms = dur.as_millis(), "accept failed");
                tokio::time::sleep(dur).await;
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

        // **V8e-7-4** — bin 切到全 async：直接传 `tokio::net::TcpStream` 给
        // AsyncSession path，删 spawn_blocking 桥与 std_stream 转换。
        // KATO timer 在 dispatch_pdu_async 入口接管（Connect 后真 arm）；不再
        // 依赖 sync OS-level set_read_timeout 防 slowloris（handshake 期间靠
        // `ic_handshake_async` 自然短路 + V8e-5 KATO 真护盘）。
        tracing::info!(label, %peer, "accepted connection");

        let inflight = Arc::clone(&inflight);
        let shared_ctrl = Arc::clone(&shared);
        let shutdown_rx = shutdown_rx.clone();
        // **V8e-7-4 reviewer M-1 + security LOW-3** — detach `tokio::spawn`
        // JoinHandle 但闭包内：
        //   1. `InflightGuard` RAII：drop 时无条件 fetch_sub（即使 future panic）
        //   2. `futures::FutureExt::catch_unwind` 把 handle_conn_async panic 转
        //      Err 让 tracing::error 记录而非静默 detach
        // 教学版接受单 conn panic 不污染其它 conn。
        // `let _handle = ...` 表明故意 detach（clippy `let_underscore_future` 要求）。
        let _handle = tokio::spawn(async move {
            // RAII inflight 计数：保证 panic 路径也走 fetch_sub
            struct InflightGuard(Arc<AtomicUsize>);
            impl Drop for InflightGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _g = InflightGuard(inflight);

            let fut =
                std::panic::AssertUnwindSafe(handle_conn_async(stream, shared_ctrl, shutdown_rx));
            let r = futures::FutureExt::catch_unwind(fut).await;
            match r {
                Ok(Ok(())) => tracing::info!(label, %peer, "connection closed normally"),
                Ok(Err(e)) => {
                    tracing::warn!(label, %peer, error = %e, "connection ended with error")
                }
                Err(_) => tracing::error!(
                    label,
                    %peer,
                    "V8e-7-4 handle_conn_async panic（已 catch；其它 conn 不受影响）"
                ),
            }
        });
    }
    tracing::info!(
        label,
        secs = SHUTDOWN_DRAIN_SECS,
        "accept loop stopped; waiting for in-flight workers"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(SHUTDOWN_DRAIN_SECS);
    while inflight.load(Ordering::SeqCst) > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// **V8e-7-4** — async 版 conn 主循环（取代 V8b/V8e-2 的 sync `handle_conn`
/// + spawn_blocking 桥）。
///
/// 流程：
/// 1. `accept_and_handshake_async` ICReq/ICResp + 分 conn_id / token slab
/// 2. loop `pump_one_async`（select! 监听 shutdown / AER notify / KATO / read_pdu）
///    - `Pdu(p)` → `dispatch_pdu_async(p)`；DispatchOutcome.disconnected → break
///    - `AenReady` → `drain_aers_async`
///    - `KatoExpired / PeerClosed / Shutdown` → break
/// 3. session drop → V8c/V8d AER cleanup + IO queue sweep
async fn handle_conn_async(
    stream: tokio::net::TcpStream,
    shared_ctrl: nvme_of_tcp_target::SharedController,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let mut sess = nvme_of_tcp_target::accept_and_handshake_async(stream, shared_ctrl)
        .await
        .context("V8e-7-4 AsyncSession handshake")?;
    loop {
        let event = sess.pump_one_async(&mut shutdown_rx).await?;
        match event {
            nvme_of_tcp_target::PumpEvent::Pdu(pdu) => {
                let outcome = sess.dispatch_pdu_async(pdu).await?;
                if outcome.disconnected {
                    break;
                }
            }
            nvme_of_tcp_target::PumpEvent::AenReady { .. } => {
                sess.drain_aers_async().await?;
            }
            nvme_of_tcp_target::PumpEvent::KatoExpired
            | nvme_of_tcp_target::PumpEvent::PeerClosed
            | nvme_of_tcp_target::PumpEvent::Shutdown => break,
        }
    }
    Ok(())
}

/// **V8b（legacy）** — sync 版 conn 主循环；V8b/c/d/f sync 集成测试仍用
/// `V2Session::accept_and_handshake_shared`，不通过本函数。本函数 V8e-7-4
/// 后 bin 端不再调用，保留以避免破坏 `bin_smoke.rs` 间接依赖；标
/// `#[allow(dead_code)]` 防 unused warning。
#[allow(dead_code)]
fn handle_conn(stream: TcpStream, shared_ctrl: nvme_of_tcp_target::SharedController) -> Result<()> {
    let mut sess = V2Session::accept_and_handshake_shared(stream, shared_ctrl)
        .context("V2Session handshake (V8b shared)")?;
    while sess.pump_one_with_events(std::time::Duration::from_millis(100))? {}
    Ok(())
}

/// **V-followup-tls-3** — TLS accept loop。
///
/// 与 `run_accept_loop` 同骨架，accept 后多一步 `acceptor.accept(tcp_stream)`
/// + `tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT_SECS, ...)` 防 ClientHello
///   slowloris；TLS handshake 完成后 stream 类型为
///   `TlsStream<TcpStream>`，泛型 monomorphize 给
///   `AsyncSession<TlsStream<TcpStream>>`（V-followup-tls-1 已铺路）。
///
/// **TLS handshake 失败不 fallback plaintext**（plan R-5 downgrade 防护）：
/// timeout / cert reject / IO err 直接 drop conn。
async fn run_accept_loop_tls(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    shared: nvme_of_tcp_target::SharedController,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    inflight: Arc<AtomicUsize>,
    max_conn: usize,
) -> Result<()> {
    let label = "tls";
    let mut accept_backoff_ms: u64 = 0;
    loop {
        let accept_result = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow_and_update() {
                    break;
                }
                continue;
            }
            r = listener.accept() => r,
        };

        let (tcp_stream, peer) = match accept_result {
            Ok((s, addr)) => {
                accept_backoff_ms = 0;
                (s, addr)
            }
            Err(e) => {
                let dur = Duration::from_millis(accept_backoff_ms.max(10));
                tracing::warn!(label, error = %e, backoff_ms = dur.as_millis(), "accept failed");
                tokio::time::sleep(dur).await;
                accept_backoff_ms = (accept_backoff_ms.max(10) * 2).min(ACCEPT_BACKOFF_MAX_MS);
                continue;
            }
        };

        let cur = inflight.fetch_add(1, Ordering::SeqCst);
        if cur >= max_conn {
            inflight.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!(label, %peer, max_conn, "rejecting: max-connections reached");
            drop(tcp_stream);
            continue;
        }

        tracing::info!(label, %peer, "accepted TCP; starting TLS handshake");

        let inflight = Arc::clone(&inflight);
        let shared_ctrl = Arc::clone(&shared);
        let shutdown_rx = shutdown_rx.clone();
        let acceptor = acceptor.clone();
        let _handle = tokio::spawn(async move {
            struct InflightGuard(Arc<AtomicUsize>);
            impl Drop for InflightGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _g = InflightGuard(inflight);

            // **plan R-10** — TLS handshake 整 socket 30s 超时防 slowloris
            let tls_stream = match tokio::time::timeout(
                Duration::from_secs(TLS_HANDSHAKE_TIMEOUT_SECS),
                acceptor.accept(tcp_stream),
            )
            .await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::warn!(label, %peer, error = %e, "TLS handshake failed; dropping");
                    return;
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        label,
                        %peer,
                        timeout_secs = TLS_HANDSHAKE_TIMEOUT_SECS,
                        "TLS handshake timeout; dropping (slowloris guard)"
                    );
                    return;
                }
            };

            let fut = std::panic::AssertUnwindSafe(handle_conn_async_tls(
                tls_stream,
                shared_ctrl,
                shutdown_rx,
            ));
            let r = futures::FutureExt::catch_unwind(fut).await;
            match r {
                Ok(Ok(())) => {
                    tracing::info!(label, %peer, "TLS connection closed normally")
                }
                Ok(Err(e)) => {
                    tracing::warn!(label, %peer, error = %e, "TLS connection ended with error")
                }
                Err(_) => tracing::error!(
                    label,
                    %peer,
                    "V-followup-tls-3 handle_conn_async_tls panic"
                ),
            }
        });
    }
    tracing::info!(
        label,
        secs = SHUTDOWN_DRAIN_SECS,
        "TLS accept loop stopped; waiting for in-flight workers"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(SHUTDOWN_DRAIN_SECS);
    while inflight.load(Ordering::SeqCst) > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// **V-followup-tls-3** — TLS conn handler；与 `handle_conn_async` 结构 1:1
/// 等价，stream 类型由泛型 monomorphize 给 `TlsStream<TcpStream>`。
async fn handle_conn_async_tls(
    stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    shared_ctrl: nvme_of_tcp_target::SharedController,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let mut sess = nvme_of_tcp_target::accept_and_handshake_async(stream, shared_ctrl)
        .await
        .context("V-followup-tls-3 AsyncSession over TLS handshake")?;
    loop {
        let event = sess.pump_one_async(&mut shutdown_rx).await?;
        match event {
            nvme_of_tcp_target::PumpEvent::Pdu(pdu) => {
                let outcome = sess.dispatch_pdu_async(pdu).await?;
                if outcome.disconnected {
                    break;
                }
            }
            nvme_of_tcp_target::PumpEvent::AenReady { .. } => {
                sess.drain_aers_async().await?;
            }
            nvme_of_tcp_target::PumpEvent::KatoExpired
            | nvme_of_tcp_target::PumpEvent::PeerClosed
            | nvme_of_tcp_target::PumpEvent::Shutdown => break,
        }
    }
    Ok(())
}
