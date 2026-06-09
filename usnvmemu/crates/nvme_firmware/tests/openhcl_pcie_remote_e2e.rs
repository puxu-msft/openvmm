// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # OpenHCL pcie_remote transport — 跨进程 e2e harness
//!
//! 验证 `nvme_firmware` 经 **pcie_remote 协议**（OpenHCL/OpenVMM 第 1 条接入）被正确
//! 描述与驱动。本 harness 是 nvme-of `scripts/interop_py/` 与 vfio `scripts/qemu_interop/`
//! 在 pcie_remote transport 上的**对位独立 oracle**——此前该 transport 只有 noop 设备
//! 烟雾测试（`pcie_remote_test_harness`），从未驱动过真 NVMe firmware。
//!
//! ## 角色
//!
//! pcie_remote 协议里 **device 侧** 收 `Hello` / 回 `HelloAck` / 收 `ToHost`(MMIO、
//! DMA 回执) / 发 `ToOpenhcl`(DMA 请求、中断)；**OpenHCL/VTL2 侧** 反之。
//!
//! - 被测：真 `nvme_firmware` bin，`--tcp-addr`（= pcie_remote **device 侧**，TCP client）。
//! - 本 harness：**OpenHCL 侧**（TCP server）——发 Hello、收 HelloAck、发 MMIO 寄存器/
//!   doorbell、按自己的 "guest memory" 缓冲服务 firmware 的 DMA(ReadGpa/WriteGpa)、收中断。
//!
//! ## 并发 pump（关键正确性）
//!
//! pcie_remote 的 DMA 是**异步 token 关联、fire-and-forget**（非 request/reply）：device
//! `ctx.dma_read/write` 推一个 `ReadGpa/WriteGpa{token}` 即返回，host 之后用同 token 回
//! `DmaCompletion`；CQE 写 + 中断也是独立帧。故 harness 必须并发多路复用单条 wire。
//! `codec::read_frame` **cancel-unsafe**（多段 await），不能直接塞进 `select!` 被取消
//! → 用**专职 read-task** 不断读帧塞进 channel（永不被取消），pump 再 `select!` 两个
//! cancel-safe 的 channel recv（device 帧 / test 命令）。详 reviewer HIGH 设计说明。
//!
//! ## 增量
//!
//! - O1：握手 + NVMe 身份描述（`openhcl_handshake_and_nvme_identity`）。
//! - **O2（本文件当前）**：admin queue —— CC.EN→CSTS.RDY + ASQ/ACQ + doorbell→SQE-DMA→
//!   Identify Controller→CQE-DMA→IRQ（`openhcl_admin_identify_controller`）。
//! - O3：4K Format + IO round-trip + fused C&W（以 guest-mem 作独立 oracle）。
//!
//! ## 跑法
//!
//! ```bash
//! cargo test --test openhcl_pcie_remote_e2e        # 默认 features 含 openhcl
//! ```
//!
//! 仅 Unix：依赖 bin 的 `--tcp-addr` 路径（vsock `--vm-id` 是 Windows-only，留 L3 真
//! Hyper-V guest e2e）。

#![cfg(unix)]

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use pcie_remote_protocol::DeviceDescribe;
use pcie_remote_protocol::DmaCompletion;
use pcie_remote_protocol::Hello;
use pcie_remote_protocol::HelloAck;
use pcie_remote_protocol::MmioAccess;
use pcie_remote_protocol::PROTOCOL_MAGIC;
use pcie_remote_protocol::PROTOCOL_VERSION;
use pcie_remote_protocol::ToHost;
use pcie_remote_protocol::ToOpenhcl;
use pcie_remote_protocol::codec;
use pcie_remote_protocol::to_host::Body as HostBody;
use pcie_remote_protocol::to_openhcl::Body as DevBody;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::compat::TokioAsyncWriteCompatExt;

/// NVMe class code：Mass Storage(0x01) / NVM(0x08) / NVMe programming IF(0x02)。
const NVME_CLASS_CODE: u32 = 0x01_0802;
/// bin `--vid` 默认 0x1414 = Microsoft（配合 OpenHCL 默认路由）。
const DEFAULT_VID: u32 = 0x1414;

// ── NVMe BAR0 寄存器 offset ──
const REG_CC: u64 = 0x14;
const REG_CSTS: u64 = 0x1c;
const REG_AQA: u64 = 0x24;
const REG_ASQ: u64 = 0x28;
const REG_ACQ: u64 = 0x30;
/// doorbell 数组基址（admin SQ tail = base + 0；stride 假定 DSTRD=0 → 4 字节）。
const DOORBELL_BASE: u64 = 0x1000;

// ── 我们为队列/缓冲选的 GPA 布局（都 4K 对齐，落在 GUEST_MEM_BYTES 内）──
const ASQ_GPA: u64 = 0x1_0000;
const ACQ_GPA: u64 = 0x2_0000;
const IDENTIFY_GPA: u64 = 0x3_0000;

const SQE_BYTES: usize = 64;
const CQE_BYTES: usize = 16;
const ADMIN_Q_DEPTH: u16 = 8;
/// guest physical memory 模型大小（flat，GPA [0, SIZE)）。
const GUEST_MEM_BYTES: usize = 16 << 20;

// ── O3：IO 队列对 + 数据缓冲 GPA（都 4K 对齐，落在 GUEST_MEM_BYTES 内）──
const IO_SQ_GPA: u64 = 0x4_0000;
const IO_CQ_GPA: u64 = 0x5_0000;
const WRITE_BUF_GPA: u64 = 0x6_0000;
const READ_BUF_GPA: u64 = 0x7_0000;
const IO_QID: u16 = 1;
const IO_Q_DEPTH: u16 = 8;

// ── O3b（fused C&W）数据缓冲 GPA ──
const COMPARE_BUF_GPA: u64 = 0x8_0000;
const WRITE_BUF2_GPA: u64 = 0x9_0000;

// ═══════════════════════════ 子进程 / 临时文件守卫 ═══════════════════════════

/// 子进程 + 临时文件守卫：Drop 时 kill child + 删 backing/log；测试 panic 时打印
/// firmware 日志尾部（否则跨进程失败盲调）。**先于 spawn 构造**（child=None），
/// 这样即便 `Command::spawn` 失败，临时文件也被清掉。
struct Harness {
    child: Option<std::process::Child>,
    backing: PathBuf,
    /// firmware stdout+stderr 落盘处（tracing 默认走 stdout）。
    log: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // 仅在测试 panic（断言失败 / 超时）时把 firmware 日志尾部吐出来，便于诊断。
        if std::thread::panicking()
            && let Ok(s) = std::fs::read_to_string(&self.log)
        {
            let lines: Vec<&str> = s.lines().collect();
            let tail = lines[lines.len().saturating_sub(40)..].join("\n");
            if !tail.trim().is_empty() {
                eprintln!("\n--- nvme_firmware 日志尾部 (诊断用) ---\n{tail}\n--- end ---\n");
            }
        }
        let _ = std::fs::remove_file(&self.backing);
        let _ = std::fs::remove_file(&self.log);
    }
}

/// 起 TCP listener（127.0.0.1:0 取空闲端口）→ 起真 `nvme_firmware --tcp-addr`（它作为
/// client 连过来，自带 connect 重试）→ accept。返回**裸 `TcpStream`** + 守卫。
async fn spawn_and_accept() -> Result<(TcpStream, Harness)> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind 127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    let mut backing = std::env::temp_dir();
    backing.push(format!(
        "openhcl_pcie_remote_e2e_ns_{}_{}.img",
        std::process::id(),
        port
    ));
    let mut log = std::env::temp_dir();
    log.push(format!(
        "openhcl_pcie_remote_e2e_log_{}_{}.txt",
        std::process::id(),
        port
    ));

    // 4 MiB backing → open() 有合法 NS 容量（÷512 = 8192 LBA）。
    let f = std::fs::File::create(&backing).context("create backing file")?;
    f.set_len(4 << 20).context("set_len backing")?;
    drop(f);

    // **先构造守卫**（child=None），保证后续任何 `?` 早退都清理临时文件。
    let mut harness = Harness {
        child: None,
        backing: backing.clone(),
        log: log.clone(),
    };

    // 捕获 firmware stdout+stderr 到 log 文件。
    let log_file = std::fs::File::create(&log).context("create log file")?;
    let log_file2 = log_file.try_clone().context("clone log fd")?;

    let child = std::process::Command::new(env!("CARGO_BIN_EXE_nvme_firmware"))
        .arg("--tcp-addr")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--backing-file")
        .arg(&backing)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file2))
        .spawn()
        .context("spawn nvme_firmware bin（确认 default features 含 openhcl）")?;
    harness.child = Some(child);

    let (stream, _peer) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
        .await
        .context("accept 超时 —— nvme_firmware bin 未在 15s 内连上 TCP")?
        .context("accept")?;
    Ok((stream, harness))
}

// ════════════════════════ OpenHCL 侧 NVMe driver harness ════════════════════════

/// test body → pump 的命令。
enum Cmd {
    /// fire-and-forget 寄存器/doorbell 写。
    MmioWrite { offset: u64, size: u32, value: u64 },
    /// 寄存器读（device 用同 seq 回 MmioReadResult）。
    MmioRead {
        offset: u64,
        size: u32,
        resp: oneshot::Sender<u64>,
    },
    /// 读 harness guest-mem（查 CQE / Identify 输出 / IO 数据）。
    ReadGuest {
        gpa: u64,
        len: usize,
        resp: oneshot::Sender<Vec<u8>>,
    },
    /// 写 harness guest-mem（放 SQE / IO 写数据）。
    WriteGuest { gpa: u64, data: Vec<u8> },
}

/// OpenHCL 侧 NVMe driver：握手后接管单条 wire，spawn (read-task + pump) 多路复用——
/// 服务 firmware 的 ReadGpa/WriteGpa(对 guest-mem)、把 MmioReadResult 按 seq 回执、
/// 把 InterruptFire 转发给 int channel。
struct NvmeDriver {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    int_rx: mpsc::UnboundedReceiver<u32>,
}

impl NvmeDriver {
    /// 接管 stream：先做 pcie_remote 握手（发 Hello / 收 HelloAck），再 spawn pump。
    /// 返回 (driver, DeviceDescribe)。
    async fn start(stream: TcpStream) -> Result<(Self, DeviceDescribe)> {
        let (rd, wr) = stream.into_split();
        let mut rd = rd.compat();
        let mut wr = wr.compat_write();

        // ─ 握手（OpenHCL 侧主动发 Hello）─
        let hello = Hello {
            magic: PROTOCOL_MAGIC,
            version: PROTOCOL_VERSION,
            instance_id: Vec::new(),
        };
        codec::write_frame(&mut wr, &hello)
            .await
            .context("write Hello")?;
        let ack: HelloAck =
            tokio::time::timeout(Duration::from_secs(5), codec::read_frame(&mut rd))
                .await
                .context("read HelloAck 超时")?
                .context("read HelloAck")?;
        if !ack.ok {
            return Err(anyhow!("HelloAck.ok=false: {}", ack.reason));
        }
        let describe = ack.device.context("HelloAck 缺 DeviceDescribe")?;

        // ─ read-task：永不取消地读 device 帧，塞进 channel ─
        let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<ToOpenhcl>();
        tokio::spawn(async move {
            // read_frame 返回 Err（EOF / 协议错）即结束（pump 见 frame_rx 关闭后退出）。
            while let Ok(f) = codec::read_frame::<_, ToOpenhcl>(&mut rd).await {
                if frame_tx.send(f).is_err() {
                    break; // pump 已退出
                }
            }
        });

        // ─ pump：select 两个 cancel-safe channel；独占 wr + guest-mem ─
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();
        let (int_tx, int_rx) = mpsc::unbounded_channel::<u32>();
        tokio::spawn(async move {
            let mut guest = vec![0u8; GUEST_MEM_BYTES];
            let mut seq: u64 = 1;
            let mut pending_mmio: HashMap<u64, oneshot::Sender<u64>> = HashMap::new();
            loop {
                tokio::select! {
                    biased;
                    // device → openhcl
                    maybe_frame = frame_rx.recv() => {
                        let Some(frame) = maybe_frame else { break };
                        match frame.body {
                            Some(DevBody::ReadGpa(r)) => {
                                let start = r.gpa as usize;
                                let end = start.saturating_add(r.len as usize);
                                let ok = end <= guest.len();
                                let data = if ok { guest[start..end].to_vec() } else { Vec::new() };
                                let reply = ToHost {
                                    seq,
                                    body: Some(HostBody::DmaCompletion(DmaCompletion { token: r.token, ok, data })),
                                };
                                seq += 1;
                                if codec::write_frame(&mut wr, &reply).await.is_err() { break; }
                            }
                            Some(DevBody::WriteGpa(w)) => {
                                let start = w.gpa as usize;
                                let end = start.saturating_add(w.data.len());
                                let ok = end <= guest.len();
                                if ok { guest[start..end].copy_from_slice(&w.data); }
                                let reply = ToHost {
                                    seq,
                                    body: Some(HostBody::DmaCompletion(DmaCompletion { token: w.token, ok, data: Vec::new() })),
                                };
                                seq += 1;
                                if codec::write_frame(&mut wr, &reply).await.is_err() { break; }
                            }
                            Some(DevBody::MmioReadResult(m)) => {
                                if let Some(resp) = pending_mmio.remove(&frame.seq) {
                                    let _ = resp.send(m.value);
                                }
                            }
                            Some(DevBody::InterruptFire(i)) => {
                                let _ = int_tx.send(i.msix_index);
                            }
                            None => {}
                        }
                    }
                    // test body → openhcl wire
                    maybe_cmd = cmd_rx.recv() => {
                        let Some(cmd) = maybe_cmd else { break };
                        match cmd {
                            Cmd::MmioWrite { offset, size, value } => {
                                let f = ToHost { seq, body: Some(HostBody::MmioWrite(MmioAccess { bar: 0, offset, size, value })) };
                                seq += 1;
                                if codec::write_frame(&mut wr, &f).await.is_err() { break; }
                            }
                            Cmd::MmioRead { offset, size, resp } => {
                                let s = seq;
                                seq += 1;
                                pending_mmio.insert(s, resp);
                                let f = ToHost { seq: s, body: Some(HostBody::MmioRead(MmioAccess { bar: 0, offset, size, value: 0 })) };
                                if codec::write_frame(&mut wr, &f).await.is_err() { break; }
                            }
                            Cmd::ReadGuest { gpa, len, resp } => {
                                let start = gpa as usize;
                                let end = start.saturating_add(len);
                                let data = if end <= guest.len() { guest[start..end].to_vec() } else { Vec::new() };
                                let _ = resp.send(data);
                            }
                            Cmd::WriteGuest { gpa, data } => {
                                let start = gpa as usize;
                                let end = start.saturating_add(data.len());
                                if end <= guest.len() { guest[start..end].copy_from_slice(&data); }
                            }
                        }
                    }
                }
            }
        });

        Ok((NvmeDriver { cmd_tx, int_rx }, describe))
    }

    fn mmio_write(&self, offset: u64, size: u32, value: u64) {
        let _ = self.cmd_tx.send(Cmd::MmioWrite {
            offset,
            size,
            value,
        });
    }

    async fn mmio_read(&self, offset: u64, size: u32) -> Result<u64> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::MmioRead {
                offset,
                size,
                resp: tx,
            })
            .map_err(|_| anyhow!("pump 已退出"))?;
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .context("mmio_read 超时")?
            .context("pump 丢了 resp")
    }

    fn write_guest(&self, gpa: u64, data: Vec<u8>) {
        let _ = self.cmd_tx.send(Cmd::WriteGuest { gpa, data });
    }

    async fn read_guest(&self, gpa: u64, len: usize) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::ReadGuest { gpa, len, resp: tx })
            .map_err(|_| anyhow!("pump 已退出"))?;
        rx.await.context("pump 丢了 resp")
    }

    /// 等下一个 MSI-X 中断（返回 msix_index）。
    async fn wait_interrupt(&mut self, dur: Duration) -> Result<u32> {
        tokio::time::timeout(dur, self.int_rx.recv())
            .await
            .context("等中断超时")?
            .context("int channel 已关闭")
    }

    /// CC.EN=1 + AQA/ASQ/ACQ，轮询 CSTS.RDY=1。
    async fn enable_controller(&self) -> Result<()> {
        // AQA：admin SQ/CQ size（0-based）。
        let aqa = (((ADMIN_Q_DEPTH - 1) as u64) << 16) | (ADMIN_Q_DEPTH - 1) as u64;
        self.mmio_write(REG_AQA, 4, aqa);
        self.mmio_write(REG_ASQ, 8, ASQ_GPA);
        self.mmio_write(REG_ACQ, 8, ACQ_GPA);
        // CC：EN(bit0) | IOSQES=6(<<16, 2^6=64) | IOCQES=4(<<20, 2^4=16)。
        let cc = 1u64 | (6 << 16) | (4 << 20);
        self.mmio_write(REG_CC, 4, cc);
        // 轮询 CSTS.RDY。
        for _ in 0..100 {
            let csts = self.mmio_read(REG_CSTS, 4).await?;
            if csts & 1 == 1 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(anyhow!("CC.EN 后 CSTS.RDY 始终未置位"))
    }
}

// ── doorbell offset（DSTRD=0 → stride 4；SQ=偶 idx，CQ=奇 idx）──
fn sq_db(qid: u16) -> u64 {
    DOORBELL_BASE + (2 * qid as u64) * 4
}
fn cq_db(qid: u16) -> u64 {
    DOORBELL_BASE + (2 * qid as u64 + 1) * 4
}

/// NVMe Submission Queue Entry 构造器（64 字节，常用字段）。
#[derive(Default)]
struct Sqe {
    opcode: u8,
    /// fuse bits（cdw0[9:8]）：1=FIRST(Compare)，2=SECOND(Write)。
    fuse: u8,
    cid: u16,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
}

impl Sqe {
    fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; SQE_BYTES];
        let cdw0 = self.opcode as u32 | ((self.fuse as u32 & 0x3) << 8) | ((self.cid as u32) << 16);
        b[0..4].copy_from_slice(&cdw0.to_le_bytes());
        b[4..8].copy_from_slice(&self.nsid.to_le_bytes());
        b[24..32].copy_from_slice(&self.prp1.to_le_bytes());
        b[32..40].copy_from_slice(&self.prp2.to_le_bytes());
        b[40..44].copy_from_slice(&self.cdw10.to_le_bytes());
        b[44..48].copy_from_slice(&self.cdw11.to_le_bytes());
        b[48..52].copy_from_slice(&self.cdw12.to_le_bytes());
        b
    }
}

/// CQE 关键字段。
struct CqeResult {
    cid: u16,
    sc: u8,
    #[allow(dead_code)]
    dw0: u32,
}

/// 一对 SQ/CQ 的 driver 侧队列状态：跟踪 SQ tail / CQ head / 期望 phase（处理 wrap），
/// 让多命令不串槽（reviewer M1：CQ phase-wrap 跟踪）。
struct QueueState {
    qid: u16,
    sq_base: u64,
    cq_base: u64,
    depth: u16,
    sq_tail: u16,
    cq_head: u16,
    /// 期望的 CQE phase bit（首轮=1，每 CQ wrap 翻转）。
    cq_phase: bool,
}

impl QueueState {
    fn admin() -> Self {
        Self {
            qid: 0,
            sq_base: ASQ_GPA,
            cq_base: ACQ_GPA,
            depth: ADMIN_Q_DEPTH,
            sq_tail: 0,
            cq_head: 0,
            cq_phase: true,
        }
    }
    fn io() -> Self {
        Self {
            qid: IO_QID,
            sq_base: IO_SQ_GPA,
            cq_base: IO_CQ_GPA,
            depth: IO_Q_DEPTH,
            sq_tail: 0,
            cq_head: 0,
            cq_phase: true,
        }
    }

    /// 放 1 个 SQE 到当前 tail 槽（推进 tail，**不**敲 doorbell）。
    fn place_sqe(&mut self, drv: &NvmeDriver, sqe: Vec<u8>) {
        let slot = self.sq_tail;
        drv.write_guest(self.sq_base + slot as u64 * SQE_BYTES as u64, sqe);
        self.sq_tail = (self.sq_tail + 1) % self.depth;
    }

    /// 敲 SQ tail doorbell = 当前 tail（提交已 place 的 SQE）。
    fn ring_sq(&self, drv: &NvmeDriver) {
        drv.mmio_write(sq_db(self.qid), 4, self.sq_tail as u64);
    }

    /// 轮询当前 CQ head 槽到期望 phase → 推进 head（wrap 翻 phase）+ ring CQ head doorbell。
    async fn poll_cqe(&mut self, drv: &NvmeDriver) -> Result<CqeResult> {
        for _ in 0..400 {
            let cqe = drv
                .read_guest(
                    self.cq_base + self.cq_head as u64 * CQE_BYTES as u64,
                    CQE_BYTES,
                )
                .await?;
            let dw3 = u32::from_le_bytes([cqe[12], cqe[13], cqe[14], cqe[15]]);
            let phase = (dw3 >> 16) & 1 == 1;
            if phase == self.cq_phase {
                let res = CqeResult {
                    cid: (dw3 & 0xffff) as u16,
                    sc: ((dw3 >> 17) & 0xff) as u8,
                    dw0: u32::from_le_bytes([cqe[0], cqe[1], cqe[2], cqe[3]]),
                };
                self.cq_head = (self.cq_head + 1) % self.depth;
                if self.cq_head == 0 {
                    self.cq_phase = !self.cq_phase;
                }
                drv.mmio_write(cq_db(self.qid), 4, self.cq_head as u64);
                return Ok(res);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Err(anyhow!("CQE phase 未达预期（qid={} 命令未完成）", self.qid))
    }

    /// 提交 1 条 SQE，等其 CQE。
    async fn submit(&mut self, drv: &NvmeDriver, sqe: Vec<u8>) -> Result<CqeResult> {
        self.place_sqe(drv, sqe);
        self.ring_sq(drv);
        self.poll_cqe(drv).await
    }

    /// 提交**一对连续 SQE**（fused 用：FIRST+SECOND 必须同 SQ 相邻），1 次 doorbell，
    /// 收 2 个 CQE。返回 (CQE1, CQE2)，按 CQ 槽顺序（调用方按 CID 区分 Compare/Write）。
    async fn submit_pair(
        &mut self,
        drv: &NvmeDriver,
        sqe1: Vec<u8>,
        sqe2: Vec<u8>,
    ) -> Result<(CqeResult, CqeResult)> {
        self.place_sqe(drv, sqe1);
        self.place_sqe(drv, sqe2);
        self.ring_sq(drv);
        let c1 = self.poll_cqe(drv).await?;
        let c2 = self.poll_cqe(drv).await?;
        Ok((c1, c2))
    }
}

// ═══════════════════════════════ 测试 ═══════════════════════════════

/// O1 —— pcie_remote 握手 + NVMe 身份。
///
/// 该 transport 上**第一个**驱动真 NVMe firmware 的跨进程测试：证明 firmware 经真
/// pcie_remote wire 把自己描述成**一个 NVMe 设备**（vendor/class/BAR/MSI-X）。
///
/// 诚实边界：旧 noop harness 也广告同样的 vendor/class/BAR，故 O1 只证 "经真 wire 描述成
/// NVMe"，钉死"就是这份 firmware controller"由 O2（真 enable + Identify payload）赚到。
#[tokio::test]
async fn openhcl_handshake_and_nvme_identity() -> Result<()> {
    let (stream, _harness) = spawn_and_accept().await?;
    let (_driver, dev) = NvmeDriver::start(stream).await?;

    assert_eq!(
        dev.vendor_id, DEFAULT_VID,
        "vendor_id 应 0x1414 (Microsoft 默认)"
    );
    assert_eq!(
        dev.class_code, NVME_CLASS_CODE,
        "class_code 应 0x010802 (NVMe)，实为 {:#08x}",
        dev.class_code
    );
    assert!(!dev.bars.is_empty(), "NVMe 须至少 BAR0");
    let bar0 = &dev.bars[0];
    assert_eq!(bar0.index, 0, "首 BAR 应是 index 0");
    assert!(
        bar0.size >= 0x1000,
        "BAR0 应 ≥ 4 KiB(NVMe 寄存器组)，实为 {:#x}",
        bar0.size
    );
    assert!(dev.msix_count >= 1, "NVMe 须 ≥ 1 个 MSI-X vector");
    Ok(())
}

/// O2 —— admin queue 经 pcie_remote 全路径：enable controller → Identify Controller。
///
/// 这把"firmware 是真 NVMe controller"钉死：经真 wire 走完 CC.EN→CSTS.RDY、doorbell→
/// firmware DMA-fetch SQE（harness 服务 ReadGpa）→处理 Identify→DMA-write 4K payload +
/// CQE（harness 服务 WriteGpa）→fire MSI-X。断言 CQE 成功 + Identify payload 里 VID=0x1414
/// （**独立 oracle**：payload 由 firmware DMA 进 harness guest-mem，非 harness 自造）。
#[tokio::test]
async fn openhcl_admin_identify_controller() -> Result<()> {
    let (stream, _harness) = spawn_and_accept().await?;
    let (mut driver, _dev) = NvmeDriver::start(stream).await?;

    // 1) enable controller。
    driver
        .enable_controller()
        .await
        .context("enable controller")?;
    let mut admin = QueueState::admin();

    // 2) 提交 Identify Controller（CNS=1）→ 4K payload DMA 到 IDENTIFY_GPA。
    let cid = 0x0042u16;
    let sqe = Sqe {
        opcode: 0x06,
        cid,
        prp1: IDENTIFY_GPA,
        cdw10: 1, // CNS=1 Identify Controller
        ..Default::default()
    }
    .encode();
    let cqe = admin
        .submit(&driver, sqe)
        .await
        .context("submit Identify")?;
    assert_eq!(cqe.cid, cid, "CQE CID 应回显 0x42");
    assert_eq!(cqe.sc, 0, "Identify SC 应=0(成功)，实为 {:#x}", cqe.sc);

    // 3) 独立 oracle：读 firmware DMA 进来的 Identify Controller payload，验 VID。
    let id = driver.read_guest(IDENTIFY_GPA, 4096).await?;
    let vid = u16::from_le_bytes([id[0], id[1]]);
    assert_eq!(vid, 0x1414, "Identify Controller VID 字段应 0x1414");

    // 4) 全路径含 MSI-X：firmware 完成 admin 命令应 fire 中断（vector 0）。
    let msix = driver
        .wait_interrupt(Duration::from_secs(3))
        .await
        .context("Identify 完成后应 fire MSI-X")?;
    assert_eq!(msix, 0, "admin CQ 中断应走 vector 0");

    Ok(())
}

/// 公共 setup：enable controller + 建 IO 队列对（qid 1）+ Format NS1→LBAF[2](纯 4K)。
/// 返回 (admin, io) 两个 QueueState。O3a / O3b 共用。
async fn setup_enabled_4k_io(driver: &NvmeDriver) -> Result<(QueueState, QueueState)> {
    driver.enable_controller().await.context("enable")?;
    let mut admin = QueueState::admin();
    let cdw10_q = (IO_QID as u32) | (((IO_Q_DEPTH - 1) as u32) << 16);

    // Create IO CQ（qid 1，PC|IEN，IV=0）。cdw10 = QID | (QSIZE-1)<<16。
    let cqe = admin
        .submit(
            driver,
            Sqe {
                opcode: 0x05,
                cid: 0x10,
                prp1: IO_CQ_GPA,
                cdw10: cdw10_q,
                cdw11: 0b11, // PC(bit0) | IEN(bit1)，IV=0
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("Create IO CQ")?;
    if cqe.sc != 0 {
        return Err(anyhow!("Create IO CQ sc={:#x}", cqe.sc));
    }

    // Create IO SQ（qid 1，绑 CQID 1）。cdw11 = PC(bit0) | CQID<<16。
    let cqe = admin
        .submit(
            driver,
            Sqe {
                opcode: 0x01,
                cid: 0x11,
                prp1: IO_SQ_GPA,
                cdw10: cdw10_q,
                cdw11: 1 | ((IO_QID as u32) << 16),
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("Create IO SQ")?;
    if cqe.sc != 0 {
        return Err(anyhow!("Create IO SQ sc={:#x}", cqe.sc));
    }

    // Format NS1 → LBAF[2]（纯 4K，no-meta）。cdw10 LBAF(bits3:0)=2。
    let cqe = admin
        .submit(
            driver,
            Sqe {
                opcode: 0x80,
                cid: 0x12,
                nsid: 1,
                cdw10: 2,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("Format NVM")?;
    if cqe.sc != 0 {
        return Err(anyhow!("Format sc={:#x}", cqe.sc));
    }

    // io() 是刚在 wire 上建好的 IO 队列的 driver 侧本地状态（tail=0/head=0/phase=1），
    // 与 firmware 侧新建队列初值一致。
    Ok((admin, QueueState::io()))
}

/// O3a —— 纯-4K Format + IO round-trip 经 pcie_remote（**parity payoff**）。
///
/// 把本会话已在 nvme-of(真 nvme-cli) / vfio(真 QEMU) 上验过的纯-4K 数据路径，第一次经
/// OpenHCL pcie_remote transport 跑通：建 IO 队列对 → Format NS1→LBAF[2](纯 4K) →
/// Identify NS 验 in-use=4K → IO Write/Read 4K round-trip → **直读 backing file 独立
/// oracle**（数据落在 slba\*4096 而非 slba\*512，非 round-trip 自洽）。
#[tokio::test]
async fn openhcl_format_4k_and_io_roundtrip() -> Result<()> {
    let (stream, harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    let (mut admin, mut io) = setup_enabled_4k_io(&driver).await?;

    // Identify NS1（CNS=0）→ 验 in-use LBAF=2 + LBAF[2] LBADS=12(4096)。
    let cqe = admin
        .submit(
            &driver,
            Sqe {
                opcode: 0x06,
                cid: 0x13,
                nsid: 1,
                prp1: IDENTIFY_GPA,
                cdw10: 0, // CNS=0 Identify Namespace
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("Identify NS")?;
    assert_eq!(cqe.sc, 0, "Identify NS sc 应=0");
    let idns = driver.read_guest(IDENTIFY_GPA, 4096).await?;
    // FLBAS(byte 26) bits3:0 = 当前 LBAF index。
    assert_eq!(
        idns[26] & 0xf,
        2,
        "Format 后 FLBAS in-use LBAF 应=2(4K)，实={}",
        idns[26] & 0xf
    );
    // LBAF[2] @ byte 128+2*4=136；LBADS(bits 23:16) 在该 4 字节的 byte 2 = 138。
    assert_eq!(idns[138], 12, "LBAF[2] LBADS 应=12(4096)，实={}", idns[138]);

    // 5) 4K Write @ slba=5（distinct-per-512 pattern，避免 uniform 掩盖偏移错）。
    let mut pattern = vec![0u8; 4096];
    for i in 0..8 {
        pattern[i * 512..(i + 1) * 512].fill(0xC0 + i as u8);
    }
    driver.write_guest(WRITE_BUF_GPA, pattern.clone());
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x01, // Write
                cid: 0x20,
                nsid: 1,
                prp1: WRITE_BUF_GPA,
                cdw10: 5, // slba lo（slba hi=cdw11=0）
                cdw12: 0, // nlb=0 → 1 block（= 1 个 4K LBA）
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("4K Write")?;
    assert_eq!(cqe.sc, 0, "4K Write sc 应=0，实={:#x}", cqe.sc);

    // 6) 4K Read @ slba=5 → guest-mem round-trip（READ_BUF 初始全 0，由 firmware DMA 填）。
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x02, // Read
                cid: 0x21,
                nsid: 1,
                prp1: READ_BUF_GPA,
                cdw10: 5,
                cdw12: 0,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("4K Read")?;
    assert_eq!(cqe.sc, 0, "4K Read sc 应=0，实={:#x}", cqe.sc);
    let readback = driver.read_guest(READ_BUF_GPA, 4096).await?;
    assert_eq!(
        readback, pattern,
        "4K round-trip 数据不符（slba=5 经 pcie_remote）"
    );

    // 7) Flush → **独立 oracle**：直读 backing file 验数据落在 slba*4096 而非 slba*512。
    //    （round-trip 测不出"读写都用错 ×512"的自洽 bug；backing file 是 firmware 看不到
    //    harness 在驱动的 ground truth 的另一面——这里是 firmware→file vs harness→file。）
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x00, // Flush
                cid: 0x22,
                nsid: 1,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("Flush")?;
    assert_eq!(cqe.sc, 0, "Flush sc 应=0");
    let file = std::fs::read(&harness.backing).context("读 backing file")?;
    let at_4k = &file[5 * 4096..5 * 4096 + 4096];
    assert_eq!(
        at_4k,
        &pattern[..],
        "backing[5*4096] 应=写入 pattern（独立 oracle，非 round-trip 自洽）"
    );
    // 5*512=2560 落在 LBA0 区[0,4096)，从未写过 → 应仍是 0（未退化到 ×512 偏移）。
    assert_ne!(
        file[5 * 512],
        0xC0,
        "backing[5*512] 不应出现 4K 写数据（firmware 未退化用 ×512 偏移）"
    );

    // 8) CQ phase-wrap 覆盖：连发 10 个 read 让 IO CQ head 越过 depth(8) wrap 一圈，
    //    真正执行 poll_cqe 的 phase 翻转分支（reviewer LOW：否则该分支从未被跑到）。
    //    若 phase 翻转逻辑错，wrap 后 poll_cqe 永等不到期望 phase → 此循环会超时 panic。
    for i in 0..10u16 {
        let cqe = io
            .submit(
                &driver,
                Sqe {
                    opcode: 0x02, // Read
                    cid: 0x40 + i,
                    nsid: 1,
                    prp1: READ_BUF_GPA,
                    cdw10: 5,
                    ..Default::default()
                }
                .encode(),
            )
            .await
            .context("phase-wrap 覆盖 read")?;
        assert_eq!(cqe.sc, 0, "wrap-cover read[{i}] sc 应=0");
    }

    Ok(())
}

/// O3b —— Fused Compare-and-Write 原子 CAS 经 pcie_remote。
///
/// fused C&W（spec §6.2）：两条相邻 SQE（Compare FUSE_FIRST + Write FUSE_SECOND）原子执行
/// ——Compare 命中才应用 Write。验两路：① 匹配→Write 生效；② 不匹配→Write 不生效（原子性
/// 不变量）。把本会话在 nvme-of fabric 上验过的 fused 真原子 CAS，经 OpenHCL pcie_remote
/// 跑通（pcie_remote/SDK 走 doorbell→dispatch_sqe→pending_fused 路径）。
#[tokio::test]
async fn openhcl_fused_compare_and_write() -> Result<()> {
    const SLBA: u32 = 10;
    let (stream, _harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    let (_admin, mut io) = setup_enabled_4k_io(&driver).await?;

    // 0) seed slba=10 = 全 0xAA（fused Compare 的已知基线）。
    driver.write_guest(WRITE_BUF_GPA, vec![0xAA; 4096]);
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x01,
                cid: 0x30,
                nsid: 1,
                prp1: WRITE_BUF_GPA,
                cdw10: SLBA,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("seed write")?;
    assert_eq!(cqe.sc, 0, "seed write sc 应=0");

    // 1) Fused CAS（**匹配**）：Compare 0xAA(==基线) → Write 0xBB。期望 Write 生效。
    driver.write_guest(COMPARE_BUF_GPA, vec![0xAA; 4096]); // 与基线一致 → 匹配
    driver.write_guest(WRITE_BUF2_GPA, vec![0xBB; 4096]); // 新值
    let cmp = Sqe {
        opcode: 0x05, // Compare
        fuse: 1,      // FUSE_FIRST
        cid: 0x31,
        nsid: 1,
        prp1: COMPARE_BUF_GPA,
        cdw10: SLBA,
        ..Default::default()
    }
    .encode();
    let wr = Sqe {
        opcode: 0x01, // Write
        fuse: 2,      // FUSE_SECOND
        cid: 0x32,
        nsid: 1,
        prp1: WRITE_BUF2_GPA,
        cdw10: SLBA,
        ..Default::default()
    }
    .encode();
    let (c1, c2) = io
        .submit_pair(&driver, cmp, wr)
        .await
        .context("fused 匹配")?;
    for c in [&c1, &c2] {
        assert_eq!(
            c.sc, 0,
            "fused 匹配路 CQE(cid={:#x}) sc 应=0（原子 CAS 应用 Write），实={:#x}",
            c.cid, c.sc
        );
    }
    // 验 Write 生效：read slba=10 → 0xBB。
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x02,
                cid: 0x33,
                nsid: 1,
                prp1: READ_BUF_GPA,
                cdw10: SLBA,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("read after match")?;
    assert_eq!(cqe.sc, 0);
    let rb = driver.read_guest(READ_BUF_GPA, 4096).await?;
    assert!(
        rb.iter().all(|&b| b == 0xBB),
        "fused 匹配后 slba=10 应全 0xBB（Write 生效）"
    );

    // 2) Fused CAS（**不匹配**）：Compare 0xCC(!=当前 0xBB) → Write 0xDD。期望 Write 不生效。
    driver.write_guest(COMPARE_BUF_GPA, vec![0xCC; 4096]); // != 当前 0xBB → 不匹配
    driver.write_guest(WRITE_BUF2_GPA, vec![0xDD; 4096]);
    let cmp = Sqe {
        opcode: 0x05,
        fuse: 1,
        cid: 0x34,
        nsid: 1,
        prp1: COMPARE_BUF_GPA,
        cdw10: SLBA,
        ..Default::default()
    }
    .encode();
    let wr = Sqe {
        opcode: 0x01,
        fuse: 2,
        cid: 0x35,
        nsid: 1,
        prp1: WRITE_BUF2_GPA,
        cdw10: SLBA,
        ..Default::default()
    }
    .encode();
    let (c1, c2) = io
        .submit_pair(&driver, cmp, wr)
        .await
        .context("fused 不匹配")?;
    // Compare 失败 → Compare CQE sc 非 0（Compare Failure）。按 CID 找 Compare 的那条。
    let cmp_cqe = if c1.cid == 0x34 { &c1 } else { &c2 };
    assert_ne!(
        cmp_cqe.sc, 0,
        "fused 不匹配 Compare CQE(cid={:#x}) sc 应非 0（Compare Failure）",
        cmp_cqe.cid
    );
    // 原子性不变量：read slba=10 → 仍 0xBB（Write 未生效）。
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x02,
                cid: 0x36,
                nsid: 1,
                prp1: READ_BUF_GPA,
                cdw10: SLBA,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("read after mismatch")?;
    assert_eq!(cqe.sc, 0);
    let rb = driver.read_guest(READ_BUF_GPA, 4096).await?;
    assert!(
        rb.iter().all(|&b| b == 0xBB),
        "fused 不匹配后 slba=10 应仍 0xBB（Write 未生效 = 原子性）"
    );

    Ok(())
}
