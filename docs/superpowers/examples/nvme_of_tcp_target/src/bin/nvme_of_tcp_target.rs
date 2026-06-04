// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V5d** — NVMe-over-Fabrics TCP target 长跑入口。
//!
//! 监听 4420（spec 默认端口），accept 一条 TCP 连接 → spawn 一个工作线程
//! 跑 `V2Session::accept_and_handshake` + `pump_one` 循环。每个 backing
//! file 一把 `Mutex`，同一文件同时只允许 1 个 active conn（V5d R-8）。
//!
//! ```ignore
//! # Linux 真机 interop
//! sudo modprobe nvme_tcp
//! cargo run --release -- --backing-file /tmp/ns1.img --listen 0.0.0.0:4420
//! sudo nvme connect -t tcp -a 127.0.0.1 -s 4420 -n nqn.2026-06.io.openhcl:nvme.userspace
//! sudo nvme list
//! sudo dd if=/dev/zero of=/dev/nvme0n1 bs=512 count=1 oflag=direct
//! sudo nvme disconnect -n nqn.2026-06.io.openhcl:nvme.userspace
//! ```
//!
//! 已知限制：
//! - V5 教学版单 IO ≤ 512 byte (nlb=1)；Linux nvme-tcp host 通常发 4 KiB
//!   IO，driver 见 SC=0x18 自动拆 1 KiB→4 LBA 串行 cmd。性能不优但功能可用
//! - 单 backing file 同时仅一 active connection
//! - 无 Discovery subsystem（必须 `nvme connect -n nqn...`）
//! - 无 TLS / DH-HMAC-CHAP
//!
//! V8 计划：Arc<Mutex<NvmeController>> 移除单 conn 限制 + Disconnect 真清理。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{Context as _, Result};
use clap::Parser;
use nvme_of_tcp_target::V2Session;
use parking_lot::Mutex;
use pcie_remote_nvme_userspace::NvmeController;
use std::collections::HashMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(version, about = "NVMe-over-Fabrics TCP target (Phase V5d)")]
struct Cli {
    /// 监听地址（默认 0.0.0.0:4420，spec NVMe-oF TCP IANA 端口）。
    #[arg(long, default_value = "0.0.0.0:4420")]
    listen: String,
    /// Backing file（重复指定 → 多 namespace；nsid 按命令行顺序 1,2,...）。
    #[arg(long = "backing-file", required = true)]
    backing_files: Vec<PathBuf>,
    /// PCIe Vendor ID（写入 Identify Controller，driver 用以匹配）。
    #[arg(long, default_value = "0x1414", value_parser = parse_hex_u16)]
    vid: u16,
    /// PCIe Subsystem Vendor ID。
    #[arg(long, default_value = "0x0000", value_parser = parse_hex_u16)]
    ssvid: u16,
    /// 把指定 NSID 标记为 ZNS（Zoned Namespace）。可重复。
    #[arg(long = "zns-nsid")]
    zns_nsids: Vec<u32>,
}

fn parse_hex_u16(s: &str) -> Result<u16, String> {
    let s = s.trim_start_matches("0x").trim_start_matches("0X");
    u16::from_str_radix(s, 16).map_err(|e| format!("invalid u16 hex {s:?}: {e}"))
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    tracing::info!(
        listen = cli.listen.as_str(),
        backing_files = ?cli.backing_files,
        vid = cli.vid,
        ssvid = cli.ssvid,
        zns_nsids = ?cli.zns_nsids,
        "V5d nvme_of_tcp_target start"
    );

    // 每 backing file 一把 Mutex（教学版 R-8：单 backing 同时仅一 active conn）
    let backing_locks: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let listener =
        TcpListener::bind(&cli.listen).with_context(|| format!("bind {}", cli.listen))?;
    tracing::info!("listening on {}", listener.local_addr()?);

    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed; continuing");
                continue;
            }
        };
        let peer = stream
            .peer_addr()
            .map(|p| p.to_string())
            .unwrap_or_else(|_| "?".into());
        tracing::info!(%peer, "accepted connection");

        // 拷贝 CLI 参数 + backing locks 给 worker
        let backing_files = cli.backing_files.clone();
        let vid = cli.vid;
        let ssvid = cli.ssvid;
        let zns_nsids = cli.zns_nsids.clone();
        let backing_locks = Arc::clone(&backing_locks);

        std::thread::spawn(move || {
            if let Err(e) =
                handle_conn(stream, backing_files, vid, ssvid, &zns_nsids, backing_locks)
            {
                tracing::warn!(%peer, error = %e, "connection ended with error");
            } else {
                tracing::info!(%peer, "connection closed normally");
            }
        });
    }
    Ok(())
}

/// 每条 TCP 连接的工作流：
/// 1. acquire per-backing Mutex（R-8 教学版限制）
/// 2. open NvmeController over backing files
/// 3. V2Session::accept_and_handshake → pump_one loop 直至 peer close 或 Err
fn handle_conn(
    stream: std::net::TcpStream,
    backing_files: Vec<PathBuf>,
    vid: u16,
    ssvid: u16,
    zns_nsids: &[u32],
    backing_locks: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>>,
) -> Result<()> {
    // R-8：为本 conn 的 backing files 分别 get_or_insert Arc<Mutex<()>>，
    // 再 try_lock 失败即拒。这里只 lock 第 1 个 backing（多 NS 教学版均
    // 用同样语义；真正 acquire 多 file lock 留 V8 重构）。
    let key = backing_files
        .first()
        .ok_or_else(|| anyhow::anyhow!("--backing-file required"))?
        .clone();
    let guard_arc = {
        let mut locks = backing_locks.lock();
        locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _guard = match guard_arc.try_lock() {
        Some(g) => g,
        None => {
            tracing::warn!(
                backing = ?key,
                "拒绝连接：同一 backing file 已有 active conn (V5d R-8 教学版限制)"
            );
            anyhow::bail!("backing file busy: another conn already holds the lock");
        }
    };

    // open controller
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
