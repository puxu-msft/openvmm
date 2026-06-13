// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **vfio-user 档2 frozen-vector replay gate** —— 见 [ADR-013](/usnvmemu/docs/DECISIONS.md)
//! per-transport 独立-oracle 覆盖矩阵 vfio 行的「档2 frozen-vector」。
//!
//! **它是什么**：把**真 Linux nvme 驱动**(真 QEMU guest)对 nvme_firmware 发的 byte-exact
//! vfio-user wire **枚举/寄存器前缀**(由 `run_qemu_vfio_guest_capture.py` 抓、guest PASS 自证)
//! 冻成黄金，replay 真 kernel 的 client stimulus 对**新起的 server** 断言响应**逐字节**等于黄金。
//! server 对真 kernel 实际用法的任何 wire 回归(framing / 字段 / 偏移漂移)→ 本测试红。
//!
//! **与 `vfio_user_wire_e2e.rs`(手搓 client)的区别 / 互补**：那个是「**我以为** kernel 该发
//! 什么」(我对协议的理解，独立于生产 server 但仍我写)；这个是「真 kernel **实际**发了什么」
//! ——stimulus 是真 Linux 内核的实际字节(更权威的独立 oracle，catch 我手搓时想不到的真实用法)。
//!
//! **为何只到"枚举/寄存器前缀"**：server 走 mmap-based DMA(经 DMA_MAP mmap guest memfd 后直读
//! 内存，**从不**主动发 wire DMA_READ/WRITE)。故前缀内是纯 client-request→server-reply，可同步
//! 线性 replay。但一旦首个 doorbell ring(帧 217)触发 admin SQ fetch，server 就读 mmap'd guest
//! 内存——replay 用零填充合成 memfd 会读到零、行为分叉。故黄金止于首 doorbell 之前(纯寄存器/
//! 枚举，响应与内存内容无关、确定，已 3× POC 0-mismatch)。覆盖 §28 bug#1(NO_REPLY framing)/
//! #2(GET_IRQ_INFO MSI-X)/#4(REGION_READ identity)；#3(SET_IRQS)在 CC.EN 后、由手搓 oracle 覆盖。
//!
//! **不传 fd**：前缀含 DMA_MAP(带 memfd)但其响应是 mode-无关的 OK(server 对无-fd DMA_MAP 走
//! message-mode fallback)，且前缀不访问内存 → 0-fd replay 的响应与原 mmap-mode 黄金逐字节相同
//! (已 POC)。故无需 SCM_RIGHTS，纯阻塞 `UnixStream`。
//!
//! **黄金数据** `tests/data/vfio_guest_wire_prefix.txt`(provenance + refresh owner 见文件头)。
//!
//! **revert-verify(牙检)**：改 server 的任一前缀-面 wire 行为即应让本测试红，例如：
//!   - 改 `controller/mmio.rs` 某寄存器读的返回值 → 对应 REGION_READ reply 不等黄金 → 红。
//!   - 改 `vfio_user_transport` 的 GET_REGION_INFO 某 region 的 size/flags → 红。
//!   - 改 `controller/mod.rs` 的 MSI-X count → GET_IRQ_INFO reply 不等黄金 → 红。
//!
//! 运行：`cargo test -p nvme_firmware --test vfio_user_guest_replay_e2e`(默认 features 含 vfio-user)。

#![cfg(all(unix, feature = "vfio-user"))]
#![allow(missing_docs)]

use anyhow::{Context, Result, bail, ensure};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 真 guest-kernel wire 前缀黄金(编译期嵌入；refresh 见文件头 provenance)。
const TRANSCRIPT: &str = include_str!("data/vfio_guest_wire_prefix.txt");

const HEADER_LEN: usize = 16;
const F_NO_REPLY: u32 = 0x10;

/// 一帧：方向 + 原始字节(header + payload)。
struct Frame {
    is_c2s: bool,
    bytes: Vec<u8>,
}

impl Frame {
    fn msg_id(&self) -> u16 {
        u16::from_le_bytes([self.bytes[0], self.bytes[1]])
    }
    fn flags(&self) -> u32 {
        u32::from_le_bytes([self.bytes[8], self.bytes[9], self.bytes[10], self.bytes[11]])
    }
    fn is_no_reply(&self) -> bool {
        self.flags() & F_NO_REPLY != 0
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    ensure!(s.len().is_multiple_of(2), "hex 长度非偶: {}", s.len());
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("hex 解码"))
        .collect()
}

/// 解析黄金 transcript：跳过 `#` 注释 / 空行；每行 `<c2s|s2c> <frame_hex>`。
fn parse_transcript() -> Result<Vec<Frame>> {
    let mut frames = Vec::new();
    for (lineno, line) in TRANSCRIPT.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (dir, hex) = line
            .split_once(' ')
            .with_context(|| format!("第 {} 行格式错(缺空格): {line}", lineno + 1))?;
        let is_c2s = match dir {
            "c2s" => true,
            "s2c" => false,
            other => bail!("第 {} 行未知方向 {other}", lineno + 1),
        };
        let bytes = hex_decode(hex.trim())?;
        ensure!(
            bytes.len() >= HEADER_LEN,
            "第 {} 行帧 < 16B header",
            lineno + 1
        );
        frames.push(Frame { is_c2s, bytes });
    }
    Ok(frames)
}

// ═══════════════════════════ server 守卫 ═══════════════════════════

struct Harness {
    child: Option<Child>,
    tmpdir: PathBuf,
    log: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if std::thread::panicking()
            && let Ok(s) = std::fs::read_to_string(&self.log)
        {
            let lines: Vec<&str> = s.lines().collect();
            let tail = lines[lines.len().saturating_sub(40)..].join("\n");
            if !tail.trim().is_empty() {
                eprintln!("\n--- nvme_firmware 日志尾部 ---\n{tail}\n--- end ---\n");
            }
        }
        let _ = std::fs::remove_dir_all(&self.tmpdir);
    }
}

/// 起真 `nvme_firmware --vfio-user-sock`(唯一 tmpdir socket，nextest 并行安全)+ poll-connect。
fn spawn_server() -> Result<(UnixStream, Harness)> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut tmpdir = std::env::temp_dir();
    tmpdir.push(format!("vfio_replay_{}_{}", std::process::id(), seq));
    std::fs::create_dir_all(&tmpdir).context("create tmpdir")?;

    let sock = tmpdir.join("nvme.sock");
    let backing = tmpdir.join("ns1.img");
    let log = tmpdir.join("server.log");
    let f = std::fs::File::create(&backing).context("create backing")?;
    f.set_len(64 << 20).context("set_len backing")?; // 64 MiB,同 capture 时
    drop(f);

    let mut harness = Harness {
        child: None,
        tmpdir: tmpdir.clone(),
        log: log.clone(),
    };
    let log_file = std::fs::File::create(&log).context("create log")?;
    let log_file2 = log_file.try_clone().context("clone log fd")?;

    let child = Command::new(env!("CARGO_BIN_EXE_nvme_firmware"))
        .arg("--vfio-user-sock")
        .arg(&sock)
        .arg("--backing-file")
        .arg(&backing)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file2))
        .spawn()
        .context("spawn nvme_firmware --vfio-user-sock")?;
    harness.child = Some(child);

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match UnixStream::connect(&sock) {
            Ok(s) => {
                s.set_read_timeout(Some(Duration::from_secs(5)))?;
                s.set_write_timeout(Some(Duration::from_secs(5)))?;
                return Ok((s, harness));
            }
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(e).context("connect vfio-user socket 超时");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn recv_frame(sock: &mut UnixStream) -> Result<Vec<u8>> {
    let mut hdr = [0u8; HEADER_LEN];
    sock.read_exact(&mut hdr).context("read header")?;
    let msg_size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
    ensure!(msg_size >= HEADER_LEN, "msg_size {msg_size} < header");
    ensure!(
        msg_size <= 1 << 20,
        "msg_size {msg_size} > 1 MiB(buggy server?)"
    ); // 对齐 max_data_xfer
    let mut frame = hdr.to_vec();
    frame.resize(msg_size, 0);
    sock.read_exact(&mut frame[HEADER_LEN..])
        .context("read payload")?;
    Ok(frame)
}

/// **核心 gate**：replay 真 kernel 前缀 stimulus，逐 reply 断言逐字节等黄金。
///
/// pipelining 处理：真 kernel 会连发数请求再读数 reply(如 3 个 DMA_MAP)，故按 **msg_id** 把
/// reply 配到黄金 s2c(而非按位置)。同步 send-one→read-one 把流水线化线性，响应集不变。
#[test]
fn vfio_guest_kernel_wire_prefix_replay_matches_golden() -> Result<()> {
    let frames = parse_transcript()?;
    ensure!(!frames.is_empty(), "黄金 transcript 空");

    // 黄金 s2c：msg_id → 完整帧字节。
    let mut golden: std::collections::HashMap<u16, &Frame> = std::collections::HashMap::new();
    for f in &frames {
        if !f.is_c2s {
            // s2c msg_id 唯一性锚定：refresh 若引入重复 id，HashMap 会静默覆盖、漏检 → 此处拒。
            let prev = golden.insert(f.msg_id(), f);
            ensure!(
                prev.is_none(),
                "黄金 s2c msg_id {:#06x} 重复(refresh 引入?配对会静默覆盖)",
                f.msg_id()
            );
        }
    }

    let (mut sock, _h) = spawn_server()?;

    let mut sent = 0usize;
    let mut checked = 0usize;
    for f in frames.iter().filter(|f| f.is_c2s) {
        // **不传 fd**(见模块 doc：前缀 0-fd replay 响应与 mmap-mode 黄金逐字节相同)。
        sock.write_all(&f.bytes)
            .with_context(|| format!("send c2s id={:#06x}", f.msg_id()))?;
        sent += 1;
        if f.is_no_reply() {
            continue; // posted write：server 不回。
        }
        let reply = recv_frame(&mut sock)?;
        let rid = u16::from_le_bytes([reply[0], reply[1]]);
        // 同步 send→read：reply 必配到刚发的请求;否则 server 失步/回错请求(把这也纳入牙)。
        ensure!(
            rid == f.msg_id(),
            "reply id={rid:#06x} != 刚发请求 id={:#06x}(server 失步/回错请求)",
            f.msg_id()
        );
        let g = golden
            .get(&rid)
            .with_context(|| format!("reply id={rid:#06x} 无对应黄金 s2c(server 多发/失步?)"))?;
        ensure!(
            reply == g.bytes,
            "reply id={rid:#06x} 不等黄金(server wire 回归):\n  got   ={}\n  golden={}",
            hex_head(&reply),
            hex_head(&g.bytes)
        );
        checked += 1;
    }

    // 锚定计数(防"server 静默少回 / 黄金被截断"漂移未被发现)。
    ensure!(sent == 112, "前缀 c2s 数 {sent} != 112(黄金被改动?)");
    ensure!(
        checked == 105,
        "校验的 reply 数 {checked} != 105(NO_REPLY 7 个)"
    );
    Ok(())
}

fn hex_head(b: &[u8]) -> String {
    let n = b.len().min(48);
    let mut s: String = b[..n].iter().map(|x| format!("{x:02x}")).collect();
    if b.len() > n {
        s.push('…');
    }
    s
}
