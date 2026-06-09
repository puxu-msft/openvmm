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

    /// 在 admin SQ slot 0 放 SQE，ring SQ0 tail doorbell=1，轮询 ACQ slot 0 的 CQE
    /// 直到 phase=1。返回 (cid, sc)。
    async fn submit_admin_and_poll(&self, sqe: Vec<u8>) -> Result<(u16, u8)> {
        self.write_guest(ASQ_GPA, sqe);
        self.mmio_write(DOORBELL_BASE, 4, 1); // SQ0 tail = 1
        for _ in 0..200 {
            let cqe = self.read_guest(ACQ_GPA, CQE_BYTES).await?;
            let dw3 = u32::from_le_bytes([cqe[12], cqe[13], cqe[14], cqe[15]]);
            let phase = (dw3 >> 16) & 1 == 1;
            if phase {
                let cid = (dw3 & 0xffff) as u16;
                let sc = ((dw3 >> 17) & 0xff) as u8;
                return Ok((cid, sc));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Err(anyhow!("CQE phase 始终未翻 1（admin 命令未完成）"))
    }
}

/// 造 Identify Controller SQE（64 字节）。opcode 0x06，CNS=0x01，PRP1=输出 GPA。
fn build_identify_controller_sqe(cid: u16, prp1: u64) -> Vec<u8> {
    let mut sqe = vec![0u8; SQE_BYTES];
    let cdw0 = 0x06u32 | ((cid as u32) << 16); // opcode Identify + CID
    sqe[0..4].copy_from_slice(&cdw0.to_le_bytes());
    // cdw1 nsid = 0（Identify Controller 不针对 NS）。
    sqe[24..32].copy_from_slice(&prp1.to_le_bytes()); // PRP1
    sqe[40..44].copy_from_slice(&1u32.to_le_bytes()); // cdw10 CNS=1
    sqe
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

    // 2) 提交 Identify Controller（CNS=1）→ 4K payload DMA 到 IDENTIFY_GPA。
    let cid = 0x0042u16;
    let sqe = build_identify_controller_sqe(cid, IDENTIFY_GPA);
    let (got_cid, sc) = driver
        .submit_admin_and_poll(sqe)
        .await
        .context("submit Identify")?;
    assert_eq!(got_cid, cid, "CQE CID 应回显 0x42");
    assert_eq!(sc, 0, "Identify SC 应=0(成功)，实为 {sc:#x}");

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
