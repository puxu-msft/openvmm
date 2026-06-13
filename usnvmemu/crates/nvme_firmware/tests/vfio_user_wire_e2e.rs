// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **vfio-user 控制面 wire 独立 oracle（standing gate）** —— 见 [ADR-013](/usnvmemu/docs/DECISIONS.md)
//! per-transport 独立-oracle 覆盖矩阵。本测试是 vfio-user 接入的「档1 独立-ish standing
//! gate」：手搓 vfio-user **client** wire（**不复用**生产 `VfioUserClient` / `VfioUserSession`
//! 的封帧/解帧——否则自家两端共享同一分帧假设，framing 失步类 bug 两端自洽测不出，即
//! [LESSONS §20](/usnvmemu/docs/LESSONS.md) 反模式），spawn 真 `nvme_firmware --vfio-user-sock`
//! server，覆盖 [LESSONS §28] 那 4 类「realize-only harness 看不见、真 guest 一驱动就炸」
//! 的控制面 framing bug：
//!   1. posted(`NO_REPLY`) `REGION_WRITE` 不回 reply → 分帧不失步（§28 bug#1）。
//!   2. MSI-X 经 `GET_IRQ_INFO` 广告 count>0（§28 bug#2）。
//!   3. `SET_IRQS` masked vector（count>0 + 0 fd / 全 -1）被接受、非 EINVAL（§28 bug#3）。
//!   4. `GET_REGION_INFO` / `REGION_READ` 身份正确（§28 region-info + W1 cfg-space identity）。
//!
//! **独立性边界**（ADR-013 / architect 裁定）：wire **struct 布局**等同 spec 常量可共识，
//! 但 16-byte header 封帧 + 命令-reply 配对（msg_id/cmd）+ socket 分帧由本测试**手搓**，
//! 不借生产那一跳——framing 失步正是目标 bug 面，借生产封帧则两端自洽照样测不出。
//!
//! 数据面（DMA / IO）全路径的独立验证留 Tier B libvfio-user differential / 档3 真 guest
//! ——那是数据面正确性，与本 gate 的控制面 framing 正交，不塞进一个 test 稀释牙。
//!
//! 运行：`cargo test -p nvme_firmware --test vfio_user_wire_e2e`（默认 features 含 vfio-user）。
//! spawn 目标 = `CARGO_BIN_EXE_nvme_firmware`（同 `openhcl_pcie_remote_e2e` 机制）。
//! 权威 wire 参考：`crates/vfio_user_transport/docs/specs/2026-06-04-vfio-user-wire-reference.md`。
//!
//! **revert-verify 配方（牙检——后人维护时验"oracle 真能咬"，已实证）**：
//!   - bug#1：把 `vfio_user_transport/src/session.rs` 的 `reply()` 去掉 `if no_reply
//!     { return Ok(()) }` 早返 → `vfio_posted_no_reply_write_does_not_desync_framing`
//!     红（`reply msg_id != id`，分帧失步）。
//!   - bug#3：把 `vfio_user_wire/src/irq.rs` 的 `fd_count==0 => Deassign` 改回
//!     `InvalidPartialFds`/EINVAL → `vfio_set_irqs_masked_vector_accepted` 红（errno=22）。
//!   - bug#2：把 `controller/mod.rs` 合成 config 的 `msix_count` 置 0 → `vfio_msix_irq_advertised` 红。
//!   - identity：改 `ConfigSpace` 合成的 vendor_id → `vfio_region_info_and_config_identity` 红。

#![cfg(all(unix, feature = "vfio-user"))]
#![allow(missing_docs)]

use anyhow::{Context, Result, bail, ensure};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ── 16-byte common header：msg_id u16 | cmd u16 | msg_size u32 | flags u32 | error_no u32（LE）──
const HEADER_LEN: usize = 16;

// ── flags ──
const F_TYPE_REPLY: u32 = 0x01;
const F_TYPE_MASK: u32 = 0x0F;
const F_NO_REPLY: u32 = 0x10;
const F_ERROR: u32 = 0x20;

// ── 命令号（include/vfio-user.h 权威）──
const CMD_VERSION: u16 = 1;
const CMD_DEVICE_GET_INFO: u16 = 4;
const CMD_DEVICE_GET_REGION_INFO: u16 = 5;
const CMD_DEVICE_GET_IRQ_INFO: u16 = 7;
const CMD_DEVICE_SET_IRQS: u16 = 8;
const CMD_REGION_READ: u16 = 9;
const CMD_REGION_WRITE: u16 = 10;

// ── PCI region / IRQ index ──
const REGION_BAR0: u32 = 0;
const REGION_CONFIG: u32 = 7;
const IRQ_MSIX: u32 = 2;

// ── SET_IRQS flags：bit2=DATA_EVENTFD, bit5=ACTION_TRIGGER（QEMU 典型 0x24）──
const IRQ_DATA_EVENTFD: u32 = 0x04;
const IRQ_ACTION_TRIGGER: u32 = 0x20;

// ── region flags：bit0=READ bit1=WRITE ──
const REGION_FLAG_READ: u32 = 0x1;
const REGION_FLAG_WRITE: u32 = 0x2;

const PROTOCOL_MAJOR: u16 = 0;
const PROTOCOL_MINOR: u16 = 1;

// ── 设备身份/拓扑契约（与 vfio interop_py `enumerate_smoke.py` 同一组 known-good）──
const EXPECT_NUM_REGIONS: u32 = 9;
const EXPECT_NUM_IRQS: u32 = 5;
const EXPECT_MSIX_COUNT_MIN: u32 = 1; // enumerate_smoke 实测 4；这里只验"已广告"
const EXPECT_CONFIG_SIZE: u64 = 4096;
const EXPECT_VENDOR_ID: u16 = 0x1414;
const EXPECT_DEVICE_ID: u16 = 0xc0de;

/// 一条收到的 vfio-user 消息（手搓解帧产物）。
struct Msg {
    msg_id: u16,
    cmd: u16,
    flags: u32,
    error_no: u32,
    payload: Vec<u8>,
}

/// 手搓 vfio-user **client**：拥有 UnixStream + 自增 msg_id。**刻意不依赖**生产
/// `VfioUserClient`——封帧/分帧/配对全在这里独立实现，才构成 §20 意义上的独立 oracle。
struct WireClient {
    sock: UnixStream,
    next_id: u16,
}

impl WireClient {
    /// poll-connect 到 server 创建的 UNIX socket（server 启动后才 bind，故需重试）。
    fn connect(path: &std::path::Path, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        loop {
            match UnixStream::connect(path) {
                Ok(sock) => {
                    sock.set_read_timeout(Some(Duration::from_secs(5)))
                        .context("set_read_timeout")?;
                    sock.set_write_timeout(Some(Duration::from_secs(5)))
                        .context("set_write_timeout")?;
                    return Ok(Self { sock, next_id: 1 });
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(e).context("connect vfio-user socket 超时（server 未起 socket）");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    fn alloc_id(&mut self) -> u16 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// 手搓封帧：16-byte header（LE）+ payload，一次 write_all。
    fn send(&mut self, msg_id: u16, cmd: u16, flags: u32, payload: &[u8]) -> Result<()> {
        let msg_size = (HEADER_LEN + payload.len()) as u32;
        let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
        buf.extend_from_slice(&msg_id.to_le_bytes());
        buf.extend_from_slice(&cmd.to_le_bytes());
        buf.extend_from_slice(&msg_size.to_le_bytes());
        buf.extend_from_slice(&flags.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // error_no=0 on command
        buf.extend_from_slice(payload);
        self.sock.write_all(&buf).context("write vfio-user message")?;
        Ok(())
    }

    /// 手搓解帧：读恰好 16-byte header → 解 msg_size → 读 payload。
    fn recv(&mut self) -> Result<Msg> {
        let mut hdr = [0u8; HEADER_LEN];
        self.sock
            .read_exact(&mut hdr)
            .context("read vfio-user header（peer 提前关闭？）")?;
        let msg_id = u16::from_le_bytes([hdr[0], hdr[1]]);
        let cmd = u16::from_le_bytes([hdr[2], hdr[3]]);
        let msg_size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let flags = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
        let error_no = u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]);
        ensure!(
            (msg_size as usize) >= HEADER_LEN,
            "msg_size {msg_size} < header {HEADER_LEN}"
        );
        let mut payload = vec![0u8; msg_size as usize - HEADER_LEN];
        self.sock
            .read_exact(&mut payload)
            .context("read vfio-user payload")?;
        Ok(Msg {
            msg_id,
            cmd,
            flags,
            error_no,
            payload,
        })
    }

    /// 发命令 → 收一条 → 校验是同 msg_id/cmd 的**成功** reply（非 error、TYPE=REPLY）。
    /// **这正是 framing 失步的捕获点**：若 server 多发/少发一条（如 §28 bug#1 给 posted
    /// write 回了 reply），下一次 recv 拿到的 header 的 msg_id/cmd 就对不上 → 这里报错。
    fn request(&mut self, cmd: u16, payload: &[u8]) -> Result<Vec<u8>> {
        let id = self.alloc_id();
        self.send(id, cmd, 0, payload)?;
        let reply = self.recv()?;
        ensure!(
            reply.msg_id == id,
            "reply msg_id {:#x} != {:#x}（分帧失步？§28 bug#1 类）",
            reply.msg_id,
            id
        );
        ensure!(
            reply.flags & F_ERROR == 0,
            "reply error: cmd={cmd} errno={}",
            reply.error_no
        );
        ensure!(
            reply.flags & F_TYPE_MASK == F_TYPE_REPLY,
            "not a REPLY: flags={:#x}",
            reply.flags
        );
        ensure!(reply.cmd == cmd, "reply cmd {} != {cmd}", reply.cmd);
        Ok(reply.payload)
    }

    fn version_handshake(&mut self) -> Result<()> {
        let mut pl = Vec::new();
        pl.extend_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
        pl.extend_from_slice(&PROTOCOL_MINOR.to_le_bytes());
        pl.extend_from_slice(b"{}\x00");
        self.request(CMD_VERSION, &pl).context("VERSION handshake")?;
        Ok(())
    }

    /// DEVICE_GET_INFO → (num_regions, num_irqs)。req=`<IIII>` argsz,flags,nr,ni。
    fn get_info(&mut self) -> Result<(u32, u32)> {
        let mut req = Vec::new();
        for v in [16u32, 0, 0, 0] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        let pl = self.request(CMD_DEVICE_GET_INFO, &req)?;
        ensure!(pl.len() >= 16, "GET_INFO reply too short: {}", pl.len());
        let num_regions = u32::from_le_bytes(pl[8..12].try_into().unwrap());
        let num_irqs = u32::from_le_bytes(pl[12..16].try_into().unwrap());
        Ok((num_regions, num_irqs))
    }

    /// DEVICE_GET_REGION_INFO(index) → (flags, size)。
    /// req/reply=`<IIIIQQ>` argsz,flags,index,cap_offset,size,offset。
    fn get_region_info(&mut self, index: u32) -> Result<(u32, u64)> {
        let mut req = Vec::new();
        for v in [32u32, 0, index, 0] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(&0u64.to_le_bytes()); // size
        req.extend_from_slice(&0u64.to_le_bytes()); // offset
        let pl = self.request(CMD_DEVICE_GET_REGION_INFO, &req)?;
        ensure!(pl.len() >= 32, "GET_REGION_INFO reply too short: {}", pl.len());
        let flags = u32::from_le_bytes(pl[4..8].try_into().unwrap());
        let size = u64::from_le_bytes(pl[16..24].try_into().unwrap());
        Ok((flags, size))
    }

    /// DEVICE_GET_IRQ_INFO(index) → count。req=`<IIII>` argsz,flags,index,count。
    fn get_irq_info(&mut self, index: u32) -> Result<u32> {
        let mut req = Vec::new();
        for v in [16u32, 0, index, 0] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        let pl = self.request(CMD_DEVICE_GET_IRQ_INFO, &req)?;
        ensure!(pl.len() >= 16, "GET_IRQ_INFO reply too short: {}", pl.len());
        let count = u32::from_le_bytes(pl[12..16].try_into().unwrap());
        Ok(count)
    }

    /// REGION_READ(region, offset, count) → 读出的 count 字节。
    /// req=`<QII>` offset,region,count；reply=16-byte echo + count 字节。
    fn region_read(&mut self, region: u32, offset: u64, count: u32) -> Result<Vec<u8>> {
        let mut req = Vec::new();
        req.extend_from_slice(&offset.to_le_bytes());
        req.extend_from_slice(&region.to_le_bytes());
        req.extend_from_slice(&count.to_le_bytes());
        let pl = self.request(CMD_REGION_READ, &req)?;
        ensure!(
            pl.len() >= 16 + count as usize,
            "REGION_READ reply too short: {} < {}",
            pl.len(),
            16 + count as usize
        );
        Ok(pl[16..16 + count as usize].to_vec())
    }

    /// **posted REGION_WRITE（带 F_NO_REPLY）** —— server 收到后**不应**回 reply。
    /// 不读 reply（按 wire 契约 server 静默）。用于 §28 bug#1 的失步探针。
    fn region_write_posted(&mut self, region: u32, offset: u64, value: &[u8]) -> Result<()> {
        let id = self.alloc_id();
        let mut req = Vec::new();
        req.extend_from_slice(&offset.to_le_bytes());
        req.extend_from_slice(&region.to_le_bytes());
        req.extend_from_slice(&(value.len() as u32).to_le_bytes());
        req.extend_from_slice(value);
        self.send(id, CMD_REGION_WRITE, F_NO_REPLY, &req)?;
        Ok(())
    }

    /// SET_IRQS（无 fd 路径）。req=`<IIIII>` argsz,flags,index,start,count。
    /// 返回 server 的 reply Msg（由调用方判成功/EINVAL）。
    fn set_irqs_no_fd(&mut self, flags: u32, index: u32, start: u32, count: u32) -> Result<Msg> {
        let id = self.alloc_id();
        let mut req = Vec::new();
        for v in [20u32, flags, index, start, count] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        self.send(id, CMD_DEVICE_SET_IRQS, 0, &req)?;
        let reply = self.recv()?;
        ensure!(
            reply.msg_id == id && reply.cmd == CMD_DEVICE_SET_IRQS,
            "SET_IRQS reply 配对错：id={:#x}/{:#x} cmd={}",
            reply.msg_id,
            id,
            reply.cmd
        );
        Ok(reply)
    }
}

// ═══════════════════════════ 子进程 / 临时目录守卫 ═══════════════════════════

/// 子进程 + 临时目录守卫：Drop 时 kill child + 删 tmpdir（含 socket / backing / log）；
/// 测试 panic 时打印 firmware 日志尾部（否则跨进程失败盲调）。**先于 spawn 构造**
/// （child=None），保证后续任何 `?` 早退都清理。
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
                eprintln!("\n--- nvme_firmware 日志尾部 (诊断用) ---\n{tail}\n--- end ---\n");
            }
        }
        let _ = std::fs::remove_dir_all(&self.tmpdir);
    }
}

/// 起真 `nvme_firmware --vfio-user-sock <唯一路径>`（server 监听 UNIX socket）→ poll-connect
/// 手搓 client。每个 test 用独立 tmpdir（`mkdtemp` 等价）→ nextest 并行安全。
fn spawn_server() -> Result<(WireClient, Harness)> {
    // 唯一 tmpdir：pid + 单调计数器（避免同进程内多 test 撞名；不用 Instant 以保确定性）。
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut tmpdir = std::env::temp_dir();
    tmpdir.push(format!("vfio_wire_e2e_{}_{}", std::process::id(), seq));
    std::fs::create_dir_all(&tmpdir).context("create tmpdir")?;

    let sock = tmpdir.join("nvme.sock");
    let backing = tmpdir.join("ns1.img");
    let log = tmpdir.join("server.log");

    // 4 MiB backing → 合法 NS 容量（÷512 = 8192 LBA）。
    let f = std::fs::File::create(&backing).context("create backing")?;
    f.set_len(4 << 20).context("set_len backing")?;
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
        .context("spawn nvme_firmware --vfio-user-sock（确认 default features 含 vfio-user）")?;
    harness.child = Some(child);

    let client = WireClient::connect(&sock, Duration::from_secs(8))?;
    Ok((client, harness))
}

/// 握手 + GET_INFO：建立连接、协商 VERSION、确认 region/irq 拓扑契约（骨架 + 兼 POC）。
#[test]
fn vfio_version_handshake_and_device_info() -> Result<()> {
    let (mut c, _h) = spawn_server()?;
    c.version_handshake()?;
    let (num_regions, num_irqs) = c.get_info()?;
    ensure!(
        num_regions == EXPECT_NUM_REGIONS,
        "num_regions {num_regions} != {EXPECT_NUM_REGIONS}"
    );
    ensure!(
        num_irqs == EXPECT_NUM_IRQS,
        "num_irqs {num_irqs} != {EXPECT_NUM_IRQS}"
    );
    Ok(())
}

/// §28 region-info + W1 cfg-space identity：CONFIG 大小、BAR0 RW、CONFIG 头 vendor/device。
/// （此前真 bug：CONFIG 误当 BAR MMIO 读不到 identity——见 enumerate_smoke。）
#[test]
fn vfio_region_info_and_config_identity() -> Result<()> {
    let (mut c, _h) = spawn_server()?;
    c.version_handshake()?;

    let (cfg_flags, cfg_size) = c.get_region_info(REGION_CONFIG)?;
    ensure!(
        cfg_size == EXPECT_CONFIG_SIZE,
        "CONFIG size {cfg_size} != {EXPECT_CONFIG_SIZE}"
    );
    ensure!(cfg_flags & REGION_FLAG_READ != 0, "CONFIG 应可读");

    let (bar0_flags, bar0_size) = c.get_region_info(REGION_BAR0)?;
    ensure!(bar0_size >= 0x1000, "BAR0 size {bar0_size} 太小");
    ensure!(
        bar0_flags & (REGION_FLAG_READ | REGION_FLAG_WRITE) == (REGION_FLAG_READ | REGION_FLAG_WRITE),
        "BAR0 应 RW，flags={bar0_flags:#x}"
    );

    // CONFIG 头 4 字节 = vendor(LE) + device(LE)。独立 oracle 验 W1 identity。
    let head = c.region_read(REGION_CONFIG, 0, 4)?;
    let vendor = u16::from_le_bytes([head[0], head[1]]);
    let device = u16::from_le_bytes([head[2], head[3]]);
    ensure!(vendor == EXPECT_VENDOR_ID, "vendor {vendor:#06x} != {EXPECT_VENDOR_ID:#06x}");
    ensure!(device == EXPECT_DEVICE_ID, "device {device:#06x} != {EXPECT_DEVICE_ID:#06x}");
    Ok(())
}

/// §28 bug#2：MSI-X 经 GET_IRQ_INFO 广告（count>0）。realize-only 不真探中断，曾漏。
#[test]
fn vfio_msix_irq_advertised() -> Result<()> {
    let (mut c, _h) = spawn_server()?;
    c.version_handshake()?;
    let count = c.get_irq_info(IRQ_MSIX)?;
    ensure!(
        count >= EXPECT_MSIX_COUNT_MIN,
        "MSI-X count {count} < {EXPECT_MSIX_COUNT_MIN}（未广告 → guest nvme_probe -EINVAL）"
    );
    Ok(())
}

/// §28 bug#1（最高信号）：posted(NO_REPLY) REGION_WRITE 后 server **不回** reply，分帧不失步。
/// 探针：发一条 NO_REPLY BAR0 写（CC 寄存器，写 EN=0 对刚起的 controller 无害）→ 紧跟一条
/// 要 reply 的 GET_INFO。若 server 错误地给 posted write 也回了 reply（旧 bug），下一次 recv
/// 拿到的 header 的 msg_id/cmd 就对不上 GET_INFO → `request` 报失步错 → 本测试红。
#[test]
fn vfio_posted_no_reply_write_does_not_desync_framing() -> Result<()> {
    let (mut c, _h) = spawn_server()?;
    c.version_handshake()?;

    // BAR0 偏移 0x14 = NVMe CC（Controller Configuration）；写 4 字节 0（EN=0，幂等无害）。
    c.region_write_posted(REGION_BAR0, 0x14, &0u32.to_le_bytes())?;

    // 紧接一条要 reply 的命令：必须正确配对到 GET_INFO 的 reply（证 server 未多发 write-reply）。
    let (num_regions, num_irqs) = c.get_info()?;
    ensure!(
        num_regions == EXPECT_NUM_REGIONS && num_irqs == EXPECT_NUM_IRQS,
        "posted write 后 GET_INFO 契约错：regions={num_regions} irqs={num_irqs}（疑分帧失步）"
    );

    // 再加一条，确认连续多轮不漂移（失步常在第二条后才显形）。
    let (_f, cfg_size) = c.get_region_info(REGION_CONFIG)?;
    ensure!(cfg_size == EXPECT_CONFIG_SIZE, "posted write 后 region-info 失步");
    Ok(())
}

/// §28 bug#3：SET_IRQS masked vector（DATA_EVENTFD + TRIGGER，count>0 但 **0 fd** = 全 -1）
/// 必须**被接受**（de-assign 路径），非 EINVAL。旧码 `fds.len()!=count → EINVAL` 拒掉真 QEMU
/// 的 masked 向量 → guest 起不来。这里 0 fd（不走 SCM_RIGHTS）即触发该路径，fd-free 可测。
#[test]
fn vfio_set_irqs_masked_vector_accepted() -> Result<()> {
    let (mut c, _h) = spawn_server()?;
    c.version_handshake()?;
    // count 来源须真广告（否则 masked 探针名不副实）；count=0 这里即明确失败，不靠 .max 掩盖。
    let count = c.get_irq_info(IRQ_MSIX)?;
    ensure!(count >= 1, "MSI-X count 0 → 无法测 masked 向量（应已广告，见 bug#2 test）");

    // DATA_EVENTFD|TRIGGER，index=MSI-X，start=0，count=N，**不附 fd** → 全 masked。
    let reply = c.set_irqs_no_fd(IRQ_DATA_EVENTFD | IRQ_ACTION_TRIGGER, IRQ_MSIX, 0, count)?;
    if reply.flags & F_ERROR != 0 {
        bail!(
            "SET_IRQS masked vector 被拒（errno={}）——§28 bug#3 回归：masked(0 fd) 应 de-assign 接受",
            reply.error_no
        );
    }
    ensure!(
        reply.flags & F_TYPE_MASK == F_TYPE_REPLY,
        "SET_IRQS 非 REPLY：flags={:#x}",
        reply.flags
    );
    Ok(())
}
