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
    /// PSDT bits（cdw0[15:14]）：0=PRP，1=SGL inline，2=SGL Segment(PSDT=10)。
    psdt: u8,
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
        let cdw0 = self.opcode as u32
            | ((self.fuse as u32 & 0x3) << 8)
            | ((self.psdt as u32 & 0x3) << 14)
            | ((self.cid as u32) << 16);
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

/// **Phase R2** — 构造一个 16-byte SGL descriptor（放进 guest-mem segment 页）。
/// `id_byte` high nibble = type（0x00 Data Block / 0x30 Last Segment …），
/// low nibble = sub type（0 = Address）。
fn sgl_desc(address: u64, length: u32, id_byte: u8) -> [u8; 16] {
    let mut d = [0u8; 16];
    d[0..8].copy_from_slice(&address.to_le_bytes());
    d[8..12].copy_from_slice(&length.to_le_bytes());
    d[15] = id_byte;
    d
}

/// **Phase R2** — 把 SGL1 (Last Segment 指针) 编进 SQE 的 prp1/prp2 字段位置
/// （embedded_sgl_bytes：prp1=desc bytes 0..8，prp2=desc bytes 8..16）。
/// 返回 (prp1, prp2) 供 `Sqe { psdt: 2, prp1, prp2, .. }`。
fn sgl1_last_segment(seg_addr: u64, seg_len: u32) -> (u64, u64) {
    // byte 15 = 0x30 → Last Segment (type 3), sub 0。length 在 bytes 8..12。
    let prp1 = seg_addr;
    let prp2 = (0x30u64 << 56) | seg_len as u64;
    (prp1, prp2)
}

/// **Phase R2b** — SGL1 = Segment (type 2) 指针（chain 首段，其末位 descriptor 是
/// continuation 指向下一段）。返回 (prp1, prp2)。
fn sgl1_segment(seg_addr: u64, seg_len: u32) -> (u64, u64) {
    // byte 15 = 0x20 → Segment (type 2), sub 0。
    let prp1 = seg_addr;
    let prp2 = (0x20u64 << 56) | seg_len as u64;
    (prp1, prp2)
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

/// P1 —— admin 数据 DMA 的**非连续 PRP** 正确性（修 latent silent corruption）。
///
/// 旧 `dma_write_then_complete` 把整 buf 连续写单个 PRP1、无视 PRP2 → > 4 KiB 的数据
/// 在 PRP1/PRP2 **非连续**的 host 上把 page1 写到 `PRP1+4096` 而非 PRP2。本测试请求一个
/// 6 KiB(1.5 page) Get Log Page，**故意给非连续 PRP1/PRP2**，在 `PRP1+4096` 与 PRP2 各
/// 种 sentinel，断言：page1 落 PRP2、**不**碰 `PRP1+4096`。
///
/// **独立 oracle / revert-verify**：把 helper 退回"整 buf 连续写 PRP1"，本测试必 FAIL
/// （page1 覆盖 PRP1+4096 的 sentinel）→ 证明测试有牙。
#[tokio::test]
async fn openhcl_get_log_page_noncontiguous_prp() -> Result<()> {
    // 故意非连续：PRP1+4096 = 0xA1000 ≠ PRP2 = 0xC0000。
    const LOG_PRP1: u64 = 0xA_0000;
    const LOG_PRP2: u64 = 0xC_0000;
    const BUG_GPA: u64 = LOG_PRP1 + 4096; // 0xA1000 —— 连续写 bug 会把 page1 写这
    const LOG_BYTES: usize = 6144; // 1.5 page → 触发 2-page PRP1+PRP2 路径

    let (stream, _harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    driver.enable_controller().await.context("enable")?;
    let mut admin = QueueState::admin();

    // 种 sentinel：PRP1+4096 区 = 0x77（fix 下不该被碰）、PRP2 区 = 0x88（fix 下该被写）。
    driver.write_guest(BUG_GPA, vec![0x77; 4096]);
    driver.write_guest(LOG_PRP2, vec![0x88; 4096]);

    // Get Log Page（opcode 0x02）LID=0x07 Telemetry Host-Initiated，6 KiB，非连续 PRP。
    // NUMD = bytes/4 - 1（0-based dwords）；cdw10 bits31:16 = NUMDL，cdw11 = NUMDU。
    let numd = (LOG_BYTES / 4 - 1) as u32;
    let cdw10 = 0x07u32 | ((numd & 0xffff) << 16);
    let cdw11 = numd >> 16;
    let cqe = admin
        .submit(
            &driver,
            Sqe {
                opcode: 0x02,
                cid: 0x50,
                nsid: 0xffff_ffff, // controller-wide log
                prp1: LOG_PRP1,
                prp2: LOG_PRP2,
                cdw10,
                cdw11,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("Get Log Page")?;
    assert_eq!(cqe.sc, 0, "Get Log Page sc 应=0，实={:#x}", cqe.sc);

    // ★ 独立 oracle：page1 落 PRP2，**不**碰 PRP1+4096。
    let bug_region = driver.read_guest(BUG_GPA, 2048).await?;
    assert!(
        bug_region.iter().all(|&b| b == 0x77),
        "PRP1+4096(0xA1000) 应仍是 sentinel 0x77（firmware 未越界连续写 page1）—— \
         否则即 ×连续 PRP latent corruption"
    );
    let prp2_region = driver.read_guest(LOG_PRP2, 2048).await?;
    assert!(
        !prp2_region.iter().all(|&b| b == 0x88),
        "PRP2 应被 firmware 写入 page1 数据（不再全 0x88 sentinel）"
    );

    Ok(())
}

/// P2 —— admin 数据 DMA 的 **PRP list**（> 2 page，去 8 KiB 上限）。
///
/// > 2 page 时 PRP2 是 PRP **list 页**指针。本测试请求 12 KiB(3 page) Get Log Page，在
/// guest-mem 放一个**真 PRP list 页**（2 个 entry 指向**非连续**数据页 GPA），断言
/// firmware：① DMA-read 该 list 页 ② page0→PRP1、page1→list[0]、page2→list[1]（非连续
/// 落地）③ **不**碰连续写 bug 会命中的 PRP1+4096 / PRP1+8192。
///
/// **revert-verify**：把 helper > 2 页分支退回连续写，本测试必 FAIL。
#[tokio::test]
async fn openhcl_get_log_page_prp_list() -> Result<()> {
    const LOG_PRP1: u64 = 0xA_0000; // page 0 目标
    const LOG_LIST: u64 = 0xB_0000; // PRP list 页（= prp2）
    const LIST_E0: u64 = 0xC_0000; // list[0] → page 1 目标（与 PRP1 非连续）
    const LIST_E1: u64 = 0xE_0000; // list[1] → page 2 目标（非连续）
    const BUG_P1: u64 = LOG_PRP1 + 4096; // 0xA1000 —— 连续写 bug 会把 page1 写这
    const BUG_P2: u64 = LOG_PRP1 + 8192; // 0xA2000 —— 连续写 bug 会把 page2 写这
    const LOG_BYTES: usize = 12288; // 3 page → 触发 PRP list 路径

    let (stream, _harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    driver.enable_controller().await.context("enable")?;
    let mut admin = QueueState::admin();

    // 放真 PRP list 页：entry0 = LIST_E0、entry1 = LIST_E1（LE u64）。
    let mut list_page = vec![0u8; 16];
    list_page[0..8].copy_from_slice(&LIST_E0.to_le_bytes());
    list_page[8..16].copy_from_slice(&LIST_E1.to_le_bytes());
    driver.write_guest(LOG_LIST, list_page);

    // 种 sentinel：连续写 bug 命中的 PRP1+4096/+8192 = 0x77（fix 下不该碰）；
    // list entry 目标 = 0x88/0x99（fix 下该被写）。
    driver.write_guest(BUG_P1, vec![0x77; 4096]);
    driver.write_guest(BUG_P2, vec![0x77; 4096]);
    driver.write_guest(LIST_E0, vec![0x88; 4096]);
    driver.write_guest(LIST_E1, vec![0x99; 4096]);

    // Get Log Page 12 KiB，prp1=LOG_PRP1，prp2=LOG_LIST(PRP list 页)。
    let numd = (LOG_BYTES / 4 - 1) as u32;
    let cdw10 = 0x07u32 | ((numd & 0xffff) << 16);
    let cdw11 = numd >> 16;
    let cqe = admin
        .submit(
            &driver,
            Sqe {
                opcode: 0x02,
                cid: 0x60,
                nsid: 0xffff_ffff,
                prp1: LOG_PRP1,
                prp2: LOG_LIST,
                cdw10,
                cdw11,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("Get Log Page (PRP list)")?;
    assert_eq!(cqe.sc, 0, "Get Log Page(>8KiB) sc 应=0，实={:#x}", cqe.sc);

    // ★ 独立 oracle：page1/page2 落 list entry，不碰连续写 bug 位。
    let bug1 = driver.read_guest(BUG_P1, 4096).await?;
    assert!(
        bug1.iter().all(|&b| b == 0x77),
        "PRP1+4096 应仍 sentinel 0x77（page1 未越界连续写）—— 否则即 ×连续 PRP bug"
    );
    let bug2 = driver.read_guest(BUG_P2, 4096).await?;
    assert!(
        bug2.iter().all(|&b| b == 0x77),
        "PRP1+8192 应仍 sentinel 0x77（page2 未越界连续写）"
    );
    let e0 = driver.read_guest(LIST_E0, 4096).await?;
    assert!(
        !e0.iter().all(|&b| b == 0x88),
        "list[0](0xC0000) 应被写入 page1 数据（不再全 0x88）"
    );
    let e1 = driver.read_guest(LIST_E1, 4096).await?;
    assert!(
        !e1.iter().all(|&b| b == 0x99),
        "list[1](0xE0000) 应被写入 page2 数据（不再全 0x99）"
    );

    Ok(())
}

/// Shutdown 序列 —— CC.SHN → CSTS.SHST=complete（spec § 3.1.4.5）。
///
/// driver 清洁关机：写 CC.SHN=01(normal)，controller flush volatile 数据后置
/// CSTS.SHST=10(complete) 让 driver 轮询确认可安全断电。此前 firmware 忽略 SHN
/// （直接走 CSTS.RDY=0），driver 的 shutdown poll 永等不到 complete。
///
/// **revert-verify**：去掉 write_cc 的 SHN 处理 → CSTS.SHST 永停 00 → 本测试超时 FAIL。
#[tokio::test]
async fn openhcl_shutdown_sequence() -> Result<()> {
    let (stream, _harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    driver.enable_controller().await.context("enable")?;

    // enable 后 CSTS.SHST 应 = 00(normal operation)。
    let csts0 = driver.mmio_read(REG_CSTS, 4).await?;
    assert_eq!(
        (csts0 >> 2) & 0x3,
        0,
        "enable 后 CSTS.SHST 应=00(normal)，实 csts={csts0:#x}"
    );

    // 写 CC.SHN=01(normal shutdown)，保持 EN=1（典型清洁关机序列）。
    // CC = EN | IOSQES=6<<16 | IOCQES=4<<20 | SHN=01<<14。
    let cc = 1u64 | (6 << 16) | (4 << 20) | (0b01 << 14);
    driver.mmio_write(REG_CC, 4, cc);

    // 轮询 CSTS.SHST 到 10(complete)。
    let mut shst = 0u64;
    let mut complete = false;
    for _ in 0..100 {
        let csts = driver.mmio_read(REG_CSTS, 4).await?;
        shst = (csts >> 2) & 0x3;
        if shst == 0b10 {
            complete = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        complete,
        "CC.SHN=01 后 CSTS.SHST 应达 10(complete)，实={shst:#x}"
    );

    // 幂等（reviewer LOW-2）：再写一次 SHN=01，SHST 仍 10、无副作用。
    driver.mmio_write(REG_CC, 4, cc);
    let csts2 = driver.mmio_read(REG_CSTS, 4).await?;
    assert_eq!(
        (csts2 >> 2) & 0x3,
        0b10,
        "重复 SHN=01 应幂等，SHST 仍 10，实 csts={csts2:#x}"
    );

    // SHN 清回 00（reviewer M-1）：CSTS.SHST 回 00(normal)。
    let cc_clear = 1u64 | (6 << 16) | (4 << 20); // EN + IOSQES + IOCQES，SHN=00
    driver.mmio_write(REG_CC, 4, cc_clear);
    let csts3 = driver.mmio_read(REG_CSTS, 4).await?;
    assert_eq!(
        (csts3 >> 2) & 0x3,
        0,
        "SHN 清回 00 后 CSTS.SHST 应回 normal，实 csts={csts3:#x}"
    );

    Ok(())
}

/// 关机后 quiesce —— controller shut down 后下发的命令不被处理（reviewer M-2）。
///
/// shutdown 不清队列（只置 SHST），故 SQ 仍在。spec § 3.1.4.5：关机后 controller 不应
/// 再处理新命令。`on_sq_tail_doorbell` gate 在 CSTS.SHST=complete → 忽略 doorbell。
///
/// **negative oracle + revert-verify**：关机后放一条 Identify SQE + ring doorbell，验
/// Identify payload **未写入**（VID 仍 0 = 命令没处理）。去掉 gate → 命令被处理 →
/// payload=0x1414 → FAIL（已实测确认）。
#[tokio::test]
async fn openhcl_no_command_processing_after_shutdown() -> Result<()> {
    let (stream, _harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    driver.enable_controller().await.context("enable")?;

    // 立即关机（admin SQ/CQ 都还在 slot 0，无命令历史）。
    let cc = 1u64 | (6 << 16) | (4 << 20) | (0b01 << 14);
    driver.mmio_write(REG_CC, 4, cc);
    let mut done = false;
    for _ in 0..100 {
        let csts = driver.mmio_read(REG_CSTS, 4).await?;
        if (csts >> 2) & 0x3 == 0b10 {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(done, "shutdown 应 complete");

    // 关机后下发 admin Identify：放 SQE 到 ASQ slot 0，ring SQ0 tail=1。
    let sqe = Sqe {
        opcode: 0x06,
        cid: 0x70,
        prp1: IDENTIFY_GPA,
        cdw10: 1, // CNS=1 Identify Controller
        ..Default::default()
    }
    .encode();
    driver.write_guest(ASQ_GPA, sqe);
    driver.mmio_write(DOORBELL_BASE, 4, 1);

    // 给足时间（命令若会处理，早处理完了）。
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 验命令**未被处理**：Identify Controller 的 payload(VID) 未写入 IDENTIFY_GPA（仍 0）。
    // Identify payload 是可靠 oracle：命令处理 ⟺ payload 写入；比 CQE-slot 稳（实测：无 gate
    // 时命令处理 → vid=0x1414，但 CQE 不一定落在手算 slot，CQE-slot oracle 会假阴）。
    let id = driver.read_guest(IDENTIFY_GPA, 4).await?;
    let vid = u16::from_le_bytes([id[0], id[1]]);
    assert_eq!(
        vid, 0,
        "关机后下发的命令不应被处理（Identify payload 不应写入）—— quiesce，实 vid={vid:#06x}"
    );

    Ok(())
}

/// **Phase R2a** — SGL Segment **Read** scatter（PSDT=10，单 Last Segment 多 Data Block）。
///
/// 先用普通 PRP Write 把 distinct-per-512 pattern 写到 slba=6（backing 已知），再发
/// PSDT=10 Read：SGL1=Last Segment 指向 segment 页，内含 3 个 Data Block 指向**非连续**
/// 且**不等长**的 fragment（2048 / 1024 / 1024）。firmware 应把 4096 数据按 fragment
/// 边界 scatter 到各自 GPA。
///
/// **差分独立 oracle**：① 每个 fragment 区落对自己的数据切片；② FRAG0 区 [2048..4096)
/// 保持 sentinel 0x77（若 firmware 退化成"整 4096 连续写 FRAG0"会被覆盖）。
///
/// **revert-verify（已实测）**：把 completion.rs NvmSglFetch 的 `stream_offset: walk_offset`
/// 改成 `stream_offset: 0`（所有 fragment 都从 data[0] 取）→ FRAG1/FRAG2 收到 data[0..len]
/// 而非正确切片 → 本测试 FAIL。
#[tokio::test]
async fn openhcl_sgl_segment_read_scatter() -> Result<()> {
    const SEG_GPA: u64 = 0xA_0000;
    const FRAG0_GPA: u64 = 0xC_0000;
    const FRAG1_GPA: u64 = 0xD_0000;
    const FRAG2_GPA: u64 = 0xE_0000;

    let (stream, _harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    let (_admin, mut io) = setup_enabled_4k_io(&driver).await?;

    // distinct-per-512 pattern（8 段 0xD0..0xD7）避免 uniform 掩盖偏移错。
    let mut pattern = vec![0u8; 4096];
    for i in 0..8 {
        pattern[i * 512..(i + 1) * 512].fill(0xD0 + i as u8);
    }
    // 用普通 PRP Write 播种 backing@slba=6。
    driver.write_guest(WRITE_BUF_GPA, pattern.clone());
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x01,
                cid: 0x30,
                nsid: 1,
                prp1: WRITE_BUF_GPA,
                cdw10: 6,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("seed Write")?;
    assert_eq!(cqe.sc, 0, "seed Write sc 应=0，实={:#x}", cqe.sc);

    // 放 segment 页：3 个 Data Block，非连续 + 不等长（2048/1024/1024 = 4096）。
    let mut seg = Vec::new();
    seg.extend_from_slice(&sgl_desc(FRAG0_GPA, 2048, 0x00));
    seg.extend_from_slice(&sgl_desc(FRAG1_GPA, 1024, 0x00));
    seg.extend_from_slice(&sgl_desc(FRAG2_GPA, 1024, 0x00));
    driver.write_guest(SEG_GPA, seg);

    // sentinel：三个 fragment 区整页 0x77（FRAG0 只用前 2048，[2048..4096) 不该碰）。
    driver.write_guest(FRAG0_GPA, vec![0x77; 4096]);
    driver.write_guest(FRAG1_GPA, vec![0x77; 4096]);
    driver.write_guest(FRAG2_GPA, vec![0x77; 4096]);

    // SGL Read @ slba=6，PSDT=10。
    let (p1, p2) = sgl1_last_segment(SEG_GPA, 3 * 16);
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x02,
                psdt: 2,
                cid: 0x31,
                nsid: 1,
                prp1: p1,
                prp2: p2,
                cdw10: 6,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("SGL Read")?;
    assert_eq!(cqe.sc, 0, "SGL Read sc 应=0，实={:#x}", cqe.sc);

    // ★ 差分 oracle：各 fragment 落对自己的切片（非连续 + 不等长）。
    let f0 = driver.read_guest(FRAG0_GPA, 2048).await?;
    assert_eq!(f0, pattern[0..2048], "FRAG0 应=pattern[0..2048]");
    let f1 = driver.read_guest(FRAG1_GPA, 1024).await?;
    assert_eq!(
        f1,
        pattern[2048..3072],
        "FRAG1 应=pattern[2048..3072]（非连续 scatter，偏移正确）"
    );
    let f2 = driver.read_guest(FRAG2_GPA, 1024).await?;
    assert_eq!(f2, pattern[3072..4096], "FRAG2 应=pattern[3072..4096]");
    // FRAG0 区 [2048..4096) 保持 sentinel（未被"整 4096 连续写 FRAG0"覆盖）。
    let f0_tail = driver.read_guest(FRAG0_GPA + 2048, 2048).await?;
    assert!(
        f0_tail.iter().all(|&b| b == 0x77),
        "FRAG0+2048 应仍 sentinel 0x77（firmware 未越界连续写整 buffer）"
    );

    Ok(())
}

/// **Phase R2a** — SGL Segment **Write** gather（PSDT=10）。
///
/// SGL1=Last Segment → 3 个 Data Block 指向**非连续**且**不等长**的 source fragment
/// （2048/1024/1024），各填 distinct pattern。firmware 应按 fragment 边界 gather 成
/// 连续 4096 写 backing@slba=7。
///
/// **直读 backing file 独立 oracle**（非 round-trip 自洽）：backing[7*4096..+4096] ==
/// concat(frag0, frag1, frag2)。
///
/// **revert-verify（已实测）**：completion.rs NvmSglData 的 gather copy `stream_offset`
/// 改 0 → 三个 fragment 都写进 data[0..]，backing 前段 = 错数据 + 尾段全 0 → FAIL。
#[tokio::test]
async fn openhcl_sgl_segment_write_gather() -> Result<()> {
    const SEG_GPA: u64 = 0xA_0000;
    const FRAG0_GPA: u64 = 0xC_0000;
    const FRAG1_GPA: u64 = 0xD_0000;
    const FRAG2_GPA: u64 = 0xE_0000;

    let (stream, harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    let (_admin, mut io) = setup_enabled_4k_io(&driver).await?;

    // 三段 source，各 distinct（0xA0 / 0xB0 / 0xC0），拼成期望 4096。
    let f0 = vec![0xA0u8; 2048];
    let f1 = vec![0xB0u8; 1024];
    let f2 = vec![0xC0u8; 1024];
    driver.write_guest(FRAG0_GPA, f0.clone());
    driver.write_guest(FRAG1_GPA, f1.clone());
    driver.write_guest(FRAG2_GPA, f2.clone());
    let mut expected = Vec::new();
    expected.extend_from_slice(&f0);
    expected.extend_from_slice(&f1);
    expected.extend_from_slice(&f2);

    let mut seg = Vec::new();
    seg.extend_from_slice(&sgl_desc(FRAG0_GPA, 2048, 0x00));
    seg.extend_from_slice(&sgl_desc(FRAG1_GPA, 1024, 0x00));
    seg.extend_from_slice(&sgl_desc(FRAG2_GPA, 1024, 0x00));
    driver.write_guest(SEG_GPA, seg);

    let (p1, p2) = sgl1_last_segment(SEG_GPA, 3 * 16);
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x01,
                psdt: 2,
                cid: 0x32,
                nsid: 1,
                prp1: p1,
                prp2: p2,
                cdw10: 7,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("SGL Write")?;
    assert_eq!(cqe.sc, 0, "SGL Write sc 应=0，实={:#x}", cqe.sc);

    // Flush → 直读 backing file 独立 oracle。
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x00,
                cid: 0x33,
                nsid: 1,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("Flush")?;
    assert_eq!(cqe.sc, 0, "Flush sc 应=0");
    let file = std::fs::read(&harness.backing).context("读 backing")?;
    let at = &file[7 * 4096..7 * 4096 + 4096];
    assert_eq!(
        at,
        &expected[..],
        "backing[7*4096] 应=gather 拼接（独立 oracle，非 round-trip 自洽）"
    );

    // 额外：普通 PRP Read 读回再验一次（round-trip 自洽，补充信号）。
    driver.write_guest(READ_BUF_GPA, vec![0u8; 4096]);
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x02,
                cid: 0x34,
                nsid: 1,
                prp1: READ_BUF_GPA,
                cdw10: 7,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("readback")?;
    assert_eq!(cqe.sc, 0, "readback sc 应=0");
    let rb = driver.read_guest(READ_BUF_GPA, 4096).await?;
    assert_eq!(rb, expected, "PRP readback 应=gather 数据");

    Ok(())
}

/// **Phase R2b** — SGL Segment **chain** Read（PSDT=10，SGL1=Segment → 2 段链）。
///
/// SGL1 是 Segment(type 2) 指向 SEG0；SEG0 末位是 Last Segment continuation 指向 SEG1。
/// 数据 fragment 跨段：SEG0 含 FRAG0(2048)，SEG1 含 FRAG1(1024)+FRAG2(1024)，总 4096。
/// firmware 须递归 fetch SEG1 才能拿到 FRAG1/FRAG2 → 验证 chain walk + 跨段 stream 偏移。
///
/// **差分独立 oracle**：每个 fragment（含**第二段**的 FRAG1/FRAG2）落对自己的切片；
/// 若 chain 未跟进，FRAG1/FRAG2 保持 sentinel → assert 失败。
///
/// **revert-verify（已实测）**：completion.rs NvmSglFetch 的 `Some(c)` 分支改成
/// `self.start_sgl_transfer(ctx, op_id)`（不 fetch 下一段）→ 只覆盖 2048 ≠ expected 4096
/// → coverage mismatch → CQE sc != 0 → 本测试 FAIL。
#[tokio::test]
async fn openhcl_sgl_segment_chain_read() -> Result<()> {
    const SEG0_GPA: u64 = 0xA_0000; // 首段（SGL1=Segment 指向）
    const SEG1_GPA: u64 = 0xB_0000; // 末段（SEG0 continuation 指向）
    const FRAG0_GPA: u64 = 0xC_0000;
    const FRAG1_GPA: u64 = 0xD_0000;
    const FRAG2_GPA: u64 = 0xE_0000;

    let (stream, _harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    let (_admin, mut io) = setup_enabled_4k_io(&driver).await?;

    let mut pattern = vec![0u8; 4096];
    for i in 0..8 {
        pattern[i * 512..(i + 1) * 512].fill(0xD0 + i as u8);
    }
    // 播种 backing@slba=8。
    driver.write_guest(WRITE_BUF_GPA, pattern.clone());
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x01,
                cid: 0x40,
                nsid: 1,
                prp1: WRITE_BUF_GPA,
                cdw10: 8,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("seed Write")?;
    assert_eq!(cqe.sc, 0, "seed Write sc 应=0，实={:#x}", cqe.sc);

    // SEG0：[Data Block FRAG0(2048), Last Segment continuation → SEG1(len 32)]。
    let mut seg0 = Vec::new();
    seg0.extend_from_slice(&sgl_desc(FRAG0_GPA, 2048, 0x00));
    seg0.extend_from_slice(&sgl_desc(SEG1_GPA, 32, 0x30)); // 0x30 = Last Segment
    driver.write_guest(SEG0_GPA, seg0);
    // SEG1（末段）：[Data Block FRAG1(1024), Data Block FRAG2(1024)]。
    let mut seg1 = Vec::new();
    seg1.extend_from_slice(&sgl_desc(FRAG1_GPA, 1024, 0x00));
    seg1.extend_from_slice(&sgl_desc(FRAG2_GPA, 1024, 0x00));
    driver.write_guest(SEG1_GPA, seg1);

    driver.write_guest(FRAG0_GPA, vec![0x77; 4096]);
    driver.write_guest(FRAG1_GPA, vec![0x77; 4096]);
    driver.write_guest(FRAG2_GPA, vec![0x77; 4096]);

    // SGL Read @ slba=8：SGL1 = Segment → SEG0（len 32 = 2 descriptor）。
    let (p1, p2) = sgl1_segment(SEG0_GPA, 32);
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x02,
                psdt: 2,
                cid: 0x41,
                nsid: 1,
                prp1: p1,
                prp2: p2,
                cdw10: 8,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("SGL chain Read")?;
    assert_eq!(cqe.sc, 0, "SGL chain Read sc 应=0，实={:#x}", cqe.sc);

    // ★ 差分 oracle：跨段 fragment 各落对切片。
    let f0 = driver.read_guest(FRAG0_GPA, 2048).await?;
    assert_eq!(f0, pattern[0..2048], "FRAG0(SEG0) 应=pattern[0..2048]");
    let f1 = driver.read_guest(FRAG1_GPA, 1024).await?;
    assert_eq!(
        f1,
        pattern[2048..3072],
        "FRAG1(SEG1，第二段) 应=pattern[2048..3072]（chain walk 跟进 + 跨段偏移正确）"
    );
    let f2 = driver.read_guest(FRAG2_GPA, 1024).await?;
    assert_eq!(
        f2,
        pattern[3072..4096],
        "FRAG2(SEG1) 应=pattern[3072..4096]"
    );

    Ok(())
}

/// **Phase R2b** — SGL Segment **chain** Write（gather 跨段）。
///
/// 同 chain 拓扑，方向相反：3 个非连续 source fragment 跨 2 段 gather 成连续 4096 写盘。
/// **直读 backing file 独立 oracle**：backing[9*4096..] == concat(frag0, frag1, frag2)。
///
/// **revert-verify（已实测）**：同 chain_read，去掉 continuation fetch → coverage 不足 →
/// CQE sc != 0 → FAIL。
#[tokio::test]
async fn openhcl_sgl_segment_chain_write() -> Result<()> {
    const SEG0_GPA: u64 = 0xA_0000;
    const SEG1_GPA: u64 = 0xB_0000;
    const FRAG0_GPA: u64 = 0xC_0000;
    const FRAG1_GPA: u64 = 0xD_0000;
    const FRAG2_GPA: u64 = 0xE_0000;

    let (stream, harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    let (_admin, mut io) = setup_enabled_4k_io(&driver).await?;

    let f0 = vec![0xA0u8; 2048];
    let f1 = vec![0xB0u8; 1024];
    let f2 = vec![0xC0u8; 1024];
    driver.write_guest(FRAG0_GPA, f0.clone());
    driver.write_guest(FRAG1_GPA, f1.clone());
    driver.write_guest(FRAG2_GPA, f2.clone());
    let mut expected = Vec::new();
    expected.extend_from_slice(&f0);
    expected.extend_from_slice(&f1);
    expected.extend_from_slice(&f2);

    // SEG0：[FRAG0(2048), Last Segment → SEG1]。SEG1：[FRAG1(1024), FRAG2(1024)]。
    let mut seg0 = Vec::new();
    seg0.extend_from_slice(&sgl_desc(FRAG0_GPA, 2048, 0x00));
    seg0.extend_from_slice(&sgl_desc(SEG1_GPA, 32, 0x30));
    driver.write_guest(SEG0_GPA, seg0);
    let mut seg1 = Vec::new();
    seg1.extend_from_slice(&sgl_desc(FRAG1_GPA, 1024, 0x00));
    seg1.extend_from_slice(&sgl_desc(FRAG2_GPA, 1024, 0x00));
    driver.write_guest(SEG1_GPA, seg1);

    let (p1, p2) = sgl1_segment(SEG0_GPA, 32);
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x01,
                psdt: 2,
                cid: 0x42,
                nsid: 1,
                prp1: p1,
                prp2: p2,
                cdw10: 9,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("SGL chain Write")?;
    assert_eq!(cqe.sc, 0, "SGL chain Write sc 应=0，实={:#x}", cqe.sc);

    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x00,
                cid: 0x43,
                nsid: 1,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("Flush")?;
    assert_eq!(cqe.sc, 0, "Flush sc 应=0");
    let file = std::fs::read(&harness.backing).context("读 backing")?;
    let at = &file[9 * 4096..9 * 4096 + 4096];
    assert_eq!(
        at,
        &expected[..],
        "backing[9*4096] 应=跨段 gather 拼接（独立 oracle）"
    );

    Ok(())
}

/// **Phase R2b（reviewer LOW 加固）** — 0 长度 Data Block 被**跳过**而非报错。
///
/// 单 Last Segment 含 [Data 2048, **Data 0**, Data 2048]（中间 0 长度）。firmware
/// 应跳过 0 长度块（不发 no-op DMA），剩两块覆盖正好 4096 → 命令成功 + 数据落对。
/// 防恶意 driver 用海量 0-length block 制造 no-op DMA 放大。
#[tokio::test]
async fn openhcl_sgl_zero_length_datablock_skipped() -> Result<()> {
    const SEG_GPA: u64 = 0xA_0000;
    const FRAG0_GPA: u64 = 0xC_0000;
    const FRAG1_GPA: u64 = 0xE_0000;

    let (stream, _harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    let (_admin, mut io) = setup_enabled_4k_io(&driver).await?;

    let mut pattern = vec![0u8; 4096];
    for i in 0..8 {
        pattern[i * 512..(i + 1) * 512].fill(0xD0 + i as u8);
    }
    driver.write_guest(WRITE_BUF_GPA, pattern.clone());
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x01,
                cid: 0x50,
                nsid: 1,
                prp1: WRITE_BUF_GPA,
                cdw10: 10,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("seed Write")?;
    assert_eq!(cqe.sc, 0, "seed Write sc 应=0，实={:#x}", cqe.sc);

    // 段：[Data 2048, Data **0**, Data 2048] = 3 descriptor (48 byte)，覆盖 4096。
    let mut seg = Vec::new();
    seg.extend_from_slice(&sgl_desc(FRAG0_GPA, 2048, 0x00));
    seg.extend_from_slice(&sgl_desc(0xBAD0_0000, 0, 0x00)); // 0 长度（地址应被忽略）
    seg.extend_from_slice(&sgl_desc(FRAG1_GPA, 2048, 0x00));
    driver.write_guest(SEG_GPA, seg);
    driver.write_guest(FRAG0_GPA, vec![0x77; 4096]);
    driver.write_guest(FRAG1_GPA, vec![0x77; 4096]);

    let (p1, p2) = sgl1_last_segment(SEG_GPA, 3 * 16);
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x02,
                psdt: 2,
                cid: 0x51,
                nsid: 1,
                prp1: p1,
                prp2: p2,
                cdw10: 10,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("SGL Read with 0-len block")?;
    assert_eq!(cqe.sc, 0, "0 长度块应被跳过，命令成功，实 sc={:#x}", cqe.sc);

    let f0 = driver.read_guest(FRAG0_GPA, 2048).await?;
    assert_eq!(f0, pattern[0..2048], "FRAG0 应=pattern[0..2048]");
    let f1 = driver.read_guest(FRAG1_GPA, 2048).await?;
    assert_eq!(
        f1,
        pattern[2048..4096],
        "FRAG1 应=pattern[2048..4096]（0 长度块跳过后偏移正确）"
    );

    Ok(())
}

/// **Phase R2c** — SGL **Bit Bucket** Read（controller→host 丢弃区段，spec § 4.4.1）。
///
/// 单 Last Segment：[Data 1024 @FRAG0, **Bit Bucket 1024**, Data 2048 @FRAG1]，总流
/// 4096。Bit Bucket 区在 READ 被 firmware 跳过（discard，不写 host），其后 Data 的
/// stream 偏移**前移**越过被丢弃区。
///
/// **差分独立 oracle**：FRAG0 == pattern[0..1024]；FRAG1 == pattern[**2048**..4096]
/// （证 Bit Bucket 推进了 stream 偏移：被丢弃的是 pattern[1024..2048]）。
///
/// **revert-verify（已实测）**：completion.rs BitBucket arm 去掉 `op.walk_offset +=`
/// （READ 不再推进）→ FRAG1 收 pattern[1024..3072] ≠ [2048..4096] → FAIL。
#[tokio::test]
async fn openhcl_sgl_bit_bucket_read_discard() -> Result<()> {
    const SEG_GPA: u64 = 0xA_0000;
    const FRAG0_GPA: u64 = 0xC_0000;
    const FRAG1_GPA: u64 = 0xE_0000;

    let (stream, _harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    let (_admin, mut io) = setup_enabled_4k_io(&driver).await?;

    let mut pattern = vec![0u8; 4096];
    for i in 0..8 {
        pattern[i * 512..(i + 1) * 512].fill(0xD0 + i as u8);
    }
    driver.write_guest(WRITE_BUF_GPA, pattern.clone());
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x01,
                cid: 0x60,
                nsid: 1,
                prp1: WRITE_BUF_GPA,
                cdw10: 11,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("seed Write")?;
    assert_eq!(cqe.sc, 0, "seed Write sc 应=0，实={:#x}", cqe.sc);

    // 段：[Data 1024 @FRAG0, Bit Bucket 1024 (id 0x10), Data 2048 @FRAG1] = 48 byte。
    let mut seg = Vec::new();
    seg.extend_from_slice(&sgl_desc(FRAG0_GPA, 1024, 0x00));
    seg.extend_from_slice(&sgl_desc(0, 1024, 0x10)); // Bit Bucket (type 1)，address 忽略
    seg.extend_from_slice(&sgl_desc(FRAG1_GPA, 2048, 0x00));
    driver.write_guest(SEG_GPA, seg);
    driver.write_guest(FRAG0_GPA, vec![0x77; 4096]);
    driver.write_guest(FRAG1_GPA, vec![0x77; 4096]);

    let (p1, p2) = sgl1_last_segment(SEG_GPA, 3 * 16);
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x02,
                psdt: 2,
                cid: 0x61,
                nsid: 1,
                prp1: p1,
                prp2: p2,
                cdw10: 11,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("SGL Read bit bucket")?;
    assert_eq!(cqe.sc, 0, "SGL Read(bit bucket) sc 应=0，实={:#x}", cqe.sc);

    let f0 = driver.read_guest(FRAG0_GPA, 1024).await?;
    assert_eq!(f0, pattern[0..1024], "FRAG0 应=pattern[0..1024]");
    let f1 = driver.read_guest(FRAG1_GPA, 2048).await?;
    assert_eq!(
        f1,
        pattern[2048..4096],
        "FRAG1 应=pattern[2048..4096]（Bit Bucket 跳过 [1024..2048] 后偏移前移）"
    );
    // FRAG1 之后区保持 sentinel（只写 2048）。
    let tail = driver.read_guest(FRAG1_GPA + 2048, 1024).await?;
    assert!(tail.iter().all(|&b| b == 0x77), "FRAG1+2048 应仍 sentinel");

    Ok(())
}

/// **Phase R2c** — SGL **Bit Bucket** Write（host→controller，spec § 4.4 视 length 为 0）。
///
/// 单 Last Segment：[Data 2048 @FRAG0, **Bit Bucket 5000**, Data 2048 @FRAG1]。WRITE
/// 方向 Bit Bucket length 视为 0（如同不存在）→ 两 Data 块须正好覆盖 4096，gather 成
/// 连续写盘。
///
/// **直读 backing 独立 oracle**：backing[12*4096..] == concat(f0, f1)。
///
/// **revert-verify（已实测）**：BitBucket arm 去掉 `if !op.is_write` 守卫（WRITE 也
/// 推进 offset 5000）→ walk_offset 9096 ≠ 4096 → coverage mismatch → sc != 0 → FAIL。
#[tokio::test]
async fn openhcl_sgl_bit_bucket_write_ignored() -> Result<()> {
    const SEG_GPA: u64 = 0xA_0000;
    const FRAG0_GPA: u64 = 0xC_0000;
    const FRAG1_GPA: u64 = 0xE_0000;

    let (stream, harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    let (_admin, mut io) = setup_enabled_4k_io(&driver).await?;

    let f0 = vec![0xA0u8; 2048];
    let f1 = vec![0xB0u8; 2048];
    driver.write_guest(FRAG0_GPA, f0.clone());
    driver.write_guest(FRAG1_GPA, f1.clone());
    let mut expected = Vec::new();
    expected.extend_from_slice(&f0);
    expected.extend_from_slice(&f1);

    // [Data 2048, Bit Bucket 5000 (WRITE 忽略), Data 2048] → 数据覆盖正好 4096。
    let mut seg = Vec::new();
    seg.extend_from_slice(&sgl_desc(FRAG0_GPA, 2048, 0x00));
    seg.extend_from_slice(&sgl_desc(0, 5000, 0x10));
    seg.extend_from_slice(&sgl_desc(FRAG1_GPA, 2048, 0x00));
    driver.write_guest(SEG_GPA, seg);

    let (p1, p2) = sgl1_last_segment(SEG_GPA, 3 * 16);
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x01,
                psdt: 2,
                cid: 0x62,
                nsid: 1,
                prp1: p1,
                prp2: p2,
                cdw10: 12,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("SGL Write bit bucket")?;
    assert_eq!(
        cqe.sc, 0,
        "SGL Write(bit bucket 忽略) sc 应=0，实={:#x}",
        cqe.sc
    );

    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x00,
                cid: 0x63,
                nsid: 1,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("Flush")?;
    assert_eq!(cqe.sc, 0, "Flush sc 应=0");
    let file = std::fs::read(&harness.backing).context("读 backing")?;
    let at = &file[12 * 4096..12 * 4096 + 4096];
    assert_eq!(
        at,
        &expected[..],
        "backing[12*4096] 应=两 Data 块拼接（Bit Bucket 视为 0 忽略）"
    );

    Ok(())
}

/// **Phase R2d** — SGL fragment 总长度与命令传输大小不符 → Data SGL Length Invalid
/// (spec SC 0x0f)。单 Last Segment 只含 [Data 2048]，但命令是 1 个 4K LBA（4096）→
/// 覆盖不足 → firmware 拒，返 0x0f（**非** R1 手填错的 0x14）。
///
/// **revert-verify（已实测）**：completion.rs start_sgl_transfer 去掉 `walk_offset !=
/// expected` 检查 → 命令变 sc=0（只 scatter 2048）→ 本测试 assert sc==0x0f FAIL。
#[tokio::test]
async fn openhcl_sgl_length_mismatch_rejected() -> Result<()> {
    const SEG_GPA: u64 = 0xA_0000;
    const FRAG0_GPA: u64 = 0xC_0000;
    const SC_DATA_SGL_LENGTH_INVALID: u8 = 0x0f;

    let (stream, _harness) = spawn_and_accept().await?;
    let (driver, _dev) = NvmeDriver::start(stream).await?;
    let (_admin, mut io) = setup_enabled_4k_io(&driver).await?;

    // 段只覆盖 2048，但 1 个 4K LBA = 4096 → 覆盖不足。
    let mut seg = Vec::new();
    seg.extend_from_slice(&sgl_desc(FRAG0_GPA, 2048, 0x00));
    driver.write_guest(SEG_GPA, seg);

    let (p1, p2) = sgl1_last_segment(SEG_GPA, 16);
    let cqe = io
        .submit(
            &driver,
            Sqe {
                opcode: 0x02,
                psdt: 2,
                cid: 0x70,
                nsid: 1,
                prp1: p1,
                prp2: p2,
                cdw10: 13,
                ..Default::default()
            }
            .encode(),
        )
        .await
        .context("SGL Read 覆盖不足")?;
    assert_eq!(
        cqe.sc, SC_DATA_SGL_LENGTH_INVALID,
        "覆盖不足应返 Data SGL Length Invalid (0x0f)，实 sc={:#x}",
        cqe.sc
    );

    Ok(())
}
