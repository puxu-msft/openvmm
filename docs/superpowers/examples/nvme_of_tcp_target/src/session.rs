// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V2** — NVMe-oF TCP session 状态机最小子集：
//!
//! ICReq → ICResp → CapsuleCmd(Fabric Connect/Property) → CapsuleResp。
//!
//! V2 范围：
//! - ICReq/ICResp 握手（pfv 协商 + digest 双方都关 + hpda/cpda 写死 0）
//! - CapsuleCmd 派发到 fabric 子命令：
//!   - **Connect** (fctype=0x01)：读 1024-byte Connect Data SGL；分配
//!     cntlid（教学单 controller = 1）；回 CapsuleResp result=cntlid
//!   - **Property Get** (fctype=0x04)：调 `NvmeController.mmio_read_impl(0, ofst, size)`
//!     直接复用 BAR0 已有实现；CQE result 填值
//!   - **Property Set** (fctype=0x00)：同上调 mmio_write_impl
//! - 其它 cmd（Identify / NVM Read/Write / SET_FEATURES / AER / ...）V3+ 加
//!
//! 同步模型：每个 TCP 连接走一个 thread，session 阻塞 read_pdu → 处理 → write reply。

use crate::fabric;
use crate::fabric::ConnectData;
use crate::fabric::fabric_sc;
use crate::fabric::fctype;
use crate::framing::FramingError;
use crate::framing::Pdu;
use crate::framing::read_pdu;
use crate::framing::write_pdu;
use crate::h2c_reassembler::{AcceptOutcome, H2cReassembler};
use crate::io_queue::IoQueueState;
use crate::pdu::CommonHdr;
use crate::pdu::DataPsh;
use crate::pdu::IcPsh;
use crate::pdu::TermPsh;
use crate::pdu::flags;
use crate::pdu::pdu_type;
use crate::pdu::term_fes;
use crate::r2t::encode_r2t;
use crate::tcp_transport::TcpAdminTransport;
use crate::ttag::TtagAllocator;
use anyhow::Context as _;
use pcie_remote_nvme_userspace::NvmeController;
use pcie_remote_nvme_userspace::cmd::Sqe;
use std::collections::HashMap;
use std::net::TcpStream;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// **Phase V3** — admin CQ base GPA 哨值。
///
/// NVMe-oF TCP 无真 GPA；我们给 NvmeController 安装一个"假"admin CQ，
/// base_gpa = 此哨值。controller 的 `post_cqe` 调 `ctx.dma_write(cq_base + slot*16, cqe)`
/// 时，session 识别 dma_write.gpa ≥ CQ_BASE_GPA 即"这是 CQE bytes，要走
/// CapsuleResp 路径"，否则即"PRP1 data，要走 C2HData PDU"。
pub const CQ_BASE_GPA: u64 = 0xC0DE_0000_0000_0000;

/// **Phase V3** — admin CQ 容量（slot 数）。
const ADMIN_CQ_SIZE: u32 = 64;

/// **Phase V3** — 每次 CapsuleCmd 给 SQE.prp1 填的哨值（让 controller
/// `dma_write(prp1, data)` 时 session 能识别这是 admin data payload）。
/// PRP1 sentinel 必须 < CQ_BASE_GPA 才能区分。
pub const PRP1_SENTINEL: u64 = 0x1000_0000;

/// **Phase V4c** — 单个 R2T 一次准许 host 上传的最大字节数。
/// = ICResp 里宣告的 `maxh2cdata`（64 KiB，与 Linux nvmet default 对齐）。
/// 单常量两处用（[`ic_handshake`] 宣告 + V4c [`dma_read_via_r2t`] 切片），
/// `MAXH2CDATA_CONST_MATCHES_HANDSHAKE` 单测把这个不变式锁死。
pub const MAXH2CDATA_BYTES: u32 = 64 * 1024;

/// **Phase V4c (review H-2)** — 单条 dma_read total_len 上限。
/// 与 controller 内部 `FW_MAX = 8 MiB` 对齐，防 R2T 循环爆炸（u32::MAX/64K
/// 次 read_pdu 同步阻塞）+ Vec::with_capacity 数 GiB 分配 OOM。
/// V5 引入真 MDTS（controller IDENTIFY.MDTS 派生）后改为运行时决定。
pub const V4_MAX_DMA_READ_BYTES: u32 = 8 * 1024 * 1024;

/// **Phase V5a** — IO CQ sentinel 步长。
/// 每个 IO CQ 分到 `[CQ_BASE_GPA + qid*STRIDE, CQ_BASE_GPA + (qid+1)*STRIDE)` 区间，
/// 16 slot × 16B = 256B 已够本教学版（V4 admin CQ size=64 也只 1 KiB）。
/// admin CQ (qid=0) 用 `CQ_BASE_GPA` 本身（offset=0），与 V3 兼容。
pub const CQ_SENTINEL_STRIDE: u64 = 256;

/// 算指定 qid 的 CQ sentinel GPA。`qid=0` 返 `CQ_BASE_GPA`（admin）；
/// `qid≥1` 返 `CQ_BASE_GPA + qid*CQ_SENTINEL_STRIDE`。
pub fn cq_sentinel(qid: u16) -> u64 {
    CQ_BASE_GPA + (qid as u64) * CQ_SENTINEL_STRIDE
}

/// 握手后的协商参数。
#[derive(Debug, Clone, Copy)]
pub struct NegotiatedIc {
    /// PDU Format Version（写死 0）。
    pub pfv: u16,
    /// host PDU data alignment（dword count，0-based；写死 0 ⇒ 4-byte 对齐）。
    pub hpda: u8,
    /// HDGST 协商结果（client 提议 AND server 接受）。当前 server 全关。
    pub hdgst: bool,
    /// DDGST 同上。
    pub ddgst: bool,
    /// max H2C data per single PDU。我们 advertise 64 KiB。
    pub maxh2cdata: u32,
    /// host 最大未决 R2T 数 - 1。教学：信 client 上报。
    pub maxr2t: u32,
}

/// V2 server-side session：握手完成后 pump 一条条 fabric cmd 直到 peer 关。
pub struct V2Session {
    stream: TcpStream,
    /// 当前 controller 实例。整个 session 共享一个 controller；多连接
    /// 多 controller 留 V8 加 Arc<Mutex<...>>。
    controller: NvmeController,
    /// 握手后的协商参数。
    pub negotiated: NegotiatedIc,
    /// Connect 后分配的 CNTLID（写死 1，单 controller 教学版）。
    pub cntlid: u16,
    /// admin queue 是否已 Connect (qid=0)。第二次 Connect qid=0 应拒。
    pub admin_connected: bool,
    /// **V4b** — TcpAdminTransport token 跨 cmd 单调（修 review M-1）。
    /// 每次 handle_admin_cmd 用 `TcpAdminTransport::new_with_token_base(self.next_token)`，
    /// 结束后 `self.next_token = tcp_t.token_high_water()`。
    next_token: u64,
    /// **V4b** — R2T TTAG 分配器（单 session 共享，跨 cmd 单调）。
    ttag_alloc: TtagAllocator,
    /// **V5a** — session 镜像的 IO queue 表。admin Create IO CQ/SQ 成功后
    /// session 自己 insert，Fabric Connect qid≥1 时校验存在性。
    pub io_queues: HashMap<u16, IoQueueState>,
    /// **V5a** — 当前 conn 所属 qid。教学单 conn 单 qid（per Linux nvme-tcp
    /// 真实行为：per-qid 一 TCP conn）。初始 0=未 Connect；admin Connect
    /// 后 = 0；IO Connect 后 = N。`dispatch_capsule_cmd` 根据它区分 admin/IO。
    pub current_qid: u16,
}

impl V2Session {
    /// 给一个已 accept 的 [`TcpStream`] + 一个 fresh [`NvmeController`] 跑握手。
    pub fn accept_and_handshake(
        mut stream: TcpStream,
        mut controller: NvmeController,
    ) -> anyhow::Result<Self> {
        let negotiated = ic_handshake(&mut stream).context("ICReq/ICResp handshake")?;
        // **Phase V3** — 给 controller 装一个"假" admin CQ，让它能 post_cqe
        // 通过 ctx.dma_write(CQ_BASE_GPA + slot*16, cqe_16B)。session 后续
        // 通过 gpa ≥ CQ_BASE_GPA 识别这是 CQE bytes vs PRP1 data。
        controller.nvme_install_admin_cq(CQ_BASE_GPA, ADMIN_CQ_SIZE);
        Ok(Self {
            stream,
            controller,
            negotiated,
            cntlid: fabric::TEACHING_CNTLID,
            admin_connected: false,
            // **V4b** — token 起点用 1<<48 保持与 V3 测试日志一致；后续每条
            // cmd 通过 token_high_water 累加。
            next_token: 1u64 << 48,
            ttag_alloc: TtagAllocator::default(),
            // **V5a** — IO queue 表初空，Create IO CQ/SQ 成功后填充。
            io_queues: HashMap::new(),
            current_qid: 0,
        })
    }

    /// 阻塞拿一条 CapsuleCmd PDU + 派发。返回 false 表示 peer 关，caller 应退出 loop。
    pub fn pump_one(&mut self) -> anyhow::Result<bool> {
        let pdu = match read_pdu(&mut self.stream) {
            Ok(p) => p,
            Err(e) => {
                if e.downcast_ref::<FramingError>()
                    .is_some_and(|f| matches!(f, FramingError::PeerClosed { .. }))
                {
                    return Ok(false);
                }
                return Err(e);
            }
        };
        let ptype = pdu.header.pdu_type;
        match ptype {
            pdu_type::CMD => self.dispatch_capsule_cmd(pdu)?,
            pdu_type::H2C_TERM => {
                tracing::warn!("host sent H2CTermReq; closing");
                return Ok(false);
            }
            _ => {
                // V2 不处理 H2CData / 其它入站 type；返 TermReq 后关。
                self.send_c2h_term(term_fes::PDU_SEQ_ERR)?;
                anyhow::bail!("unexpected inbound PDU type {ptype:#x} (V2 only handles CMD)");
            }
        }
        Ok(true)
    }

    /// 派发 CapsuleCmd：解 SQE → 看 opc/fctype → 走不同分支。
    fn dispatch_capsule_cmd(&mut self, pdu: Pdu) -> anyhow::Result<()> {
        if pdu.psh.len() < 64 {
            self.send_c2h_term(term_fes::INVALID_PDU_HDR)?;
            anyhow::bail!("CapsuleCmd PSH too short: {} < 64", pdu.psh.len());
        }
        let sqe = &pdu.psh[..64];
        let opc = sqe[0];
        let cid = u16::from_le_bytes([sqe[2], sqe[3]]);
        let _nsid = u32::from_le_bytes([sqe[4], sqe[5], sqe[6], sqe[7]]);
        if opc == fabric::NVME_OPC_FABRIC {
            let ft = fabric::sqe_fctype(sqe).inspect_err(|_| {
                let _ = self.send_capsule_resp_err(cid, 0x02); // 非法 opcode
            })?;
            match ft {
                fctype::CONNECT => self.handle_connect(cid, sqe, &pdu.data),
                fctype::PROPERTY_GET => self.handle_property_get(cid, sqe),
                fctype::PROPERTY_SET => self.handle_property_set(cid, sqe),
                fctype::DISCONNECT => {
                    tracing::info!("Disconnect (V8 完整实现；V2 ack + close)");
                    self.send_capsule_resp_ok(cid, 0)?;
                    anyhow::bail!("disconnect received");
                }
                other => {
                    tracing::warn!(fctype = other, "unsupported fctype; reply INVALID_FIELD");
                    self.send_capsule_resp_err(cid, 0x02)
                }
            }
        } else {
            // **Phase V3 / V5a** — 真 NVMe 命令派发：当前 conn 所属 qid
            // 决定走 admin 还是 IO path（教学单 conn 单 qid，per Linux
            // nvme-tcp 真实行为）。
            if self.current_qid == 0 {
                self.handle_admin_cmd(cid, sqe)
            } else {
                self.handle_io_cmd(cid, sqe)
            }
        }
    }

    /// **Phase V5b / V5c** — IO CapsuleCmd 入口。共享 `run_post_dispatch`
    /// 闭环；与 `handle_admin_cmd` 唯一差异：dispatch 走 `nvme_io_dispatch`
    /// + 需查 sq_id→cq_id 映射 + nlb=1 guard（V5 教学版单 PRP1 上限）。
    fn handle_io_cmd(&mut self, cid: u16, sqe_bytes: &[u8]) -> anyhow::Result<()> {
        let mut sqe =
            Sqe::read_from_bytes(sqe_bytes).map_err(|_| anyhow::anyhow!("IO SQE 不是 64 byte"))?;

        // **V5b (R-5)** — 清 PSDT bits（cdw0 bits 15:14）让 controller 走
        // PRP path；Linux nvme-tcp host 默认 PSDT=01 SGL Transport-specific 0x5，
        // controller PRP/SGL resolver 见 0x5 直接 reject。session 端清掉
        // 让 controller 用 prp1+prp2 走 PRP path（对 host 透明）。
        sqe.cdw0 &= !(0b11u32 << 14);

        // sentinel 改写 prp1（V5 IO 单段 ≤ 4 KiB，prp2 不用）
        sqe.prp1 = PRP1_SENTINEL;
        sqe.prp2 = 0;

        let opc = (sqe.cdw0 & 0xff) as u8;
        let sq_id = self.current_qid;
        // 查 sq_id → cq_id
        let cq_id = match self.io_queues.get(&sq_id) {
            Some(IoQueueState::Sq { cq_id, .. }) => *cq_id,
            _ => anyhow::bail!(
                "V5b invariant violation: handle_io_cmd 但 current_qid={sq_id} 不是已建 IO SQ"
            ),
        };

        // **V5b/V5c (R-4)** — IO Read/Write nlb=1 guard。cdw12 bits 15:0 = NLB
        // (0-based) → nlb_real = +1。nlb_real > 1 → 拒 SC=0x18
        // SGL_DATA_LENGTH_INVALID 让 driver 重发分片。当前 cmd 不进 dispatch
        // （防 controller 已起 IO 后又 reject 的 wire 混乱）。
        if matches!(opc, 0x01 /* WRITE */ | 0x02 /* READ */) {
            let nlb_real = (sqe.cdw12 & 0xffff) + 1;
            if nlb_real > 1 {
                tracing::warn!(
                    opc,
                    nlb_real,
                    "V5 IO nlb>1 unsupported (单 PRP 上限)，回 SC=0x18 让 driver 拆"
                );
                return self.send_capsule_resp_err(cid, /*SGL_DATA_LENGTH_INVALID=*/ 0x18);
            }
        }

        tracing::debug!(cid, opc, sq_id, cq_id, "V5b/V5c IO dispatch");

        let mut tcp_t = TcpAdminTransport::new_with_token_base(self.next_token);

        // ─── Phase 1：dispatch ─────────────────────────────────────────
        let immediate_cqe = {
            let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
            self.controller
                .nvme_io_dispatch(&mut ctx, sq_id, sqe, cid, cq_id)
        };

        // ─── Phase 1.5..5 → shared helper ──────────────────────────────
        self.run_post_dispatch(cid, immediate_cqe, tcp_t)
    }

    /// **Phase V3 + V4b** — 把 CapsuleCmd 里的 NVMe SQE 派发到 controller，
    /// captured 出来的 dma_write 转 C2HData + CapsuleResp；captured 的
    /// dma_read 转 R2T → H2CData round-trip。
    ///
    /// 状态机 — 三种 controller 内部子路径，session 全部映射到同一出口：
    /// 1. 同步路径（Set Features 等）：dispatch 返 `Some(Cqe)` →
    ///    立刻 `nvme_post_cqe(cqe)`，captured 仅 CQE write。
    /// 2. 异步 write-out 路径（Identify、Get Log Page）：dispatch 返 None；
    ///    captured 含 data writes（gpa<CQ_BASE_GPA）。逐 token 调
    ///    `nvme_admin_complete_dma(ok=true)` 让 controller post_cqe。
    /// 3. **V4b** 异步 read-in 路径（NS Attachment 0x15 等）：dispatch 返
    ///    None；captured 含 pending_reads。逐条 alloc ttag → emit R2T →
    ///    `await_host_data` 收齐 H2CData → `nvme_admin_complete_dma(ok=true, bytes)`
    ///    让 controller 把 bytes 吃进去后 post_cqe。
    ///
    /// 三条路径出口都保证 captured.writes = [data writes...] + [1 条 CQE write]。
    /// drain captured 严格按 "data 先 / CQE 后" 顺序拼 C2HData + CapsuleResp；
    /// 任何顺序/计数违例 → `anyhow::bail!`（review L1 / L2）。
    ///
    /// **修 review M-1**：token 由 `self.next_token` 跨 cmd 单调注入，
    /// 避免 controller pending_ios 残留撞同 token。
    ///
    /// **修 review M-2**：dma_read 不再 silent-drop；走 R2T 闭环。
    fn handle_admin_cmd(&mut self, cid: u16, sqe_bytes: &[u8]) -> anyhow::Result<()> {
        let mut sqe =
            Sqe::read_from_bytes(sqe_bytes).map_err(|_| anyhow::anyhow!("SQE 不是 64 byte"))?;
        // 用哨值替换 client 给的 prp1，让 controller dma_write / dma_read
        // 都打到我们能识别的 gpa（V4b 单 read：复用同一哨值；V4c+ 多 read
        // 引入 per-ttag sentinel 池防撞）。
        sqe.prp1 = PRP1_SENTINEL;
        sqe.prp2 = 0;
        let opc = (sqe.cdw0 & 0xff) as u8;

        // **V5a** — admin Create IO CQ (0x05) / Create IO SQ (0x01) peek：
        // 改写 prp1 哨值 + 提取 qid/cq_id 准备 io_queues 记账。
        // Create IO CQ: cdw10 bits 15:0 = qid；prp1 应改为 cq_sentinel(qid)
        // Create IO SQ: cdw10 bits 15:0 = sq_id；cdw11 bits 31:16 = cq_id；
        //               prp1 在 controller 内部 io.rs:382 不用（SQ 不需要
        //               session 写 CQE）；保留 PRP1_SENTINEL 即可。
        let create_io_cq_qid: Option<u16> = (opc == 0x05).then_some((sqe.cdw10 & 0xffff) as u16);
        let create_io_sq_pair: Option<(u16, u16)> = (opc == 0x01).then(|| {
            let sq_id = (sqe.cdw10 & 0xffff) as u16;
            let cq_id = ((sqe.cdw11 >> 16) & 0xffff) as u16;
            (sq_id, cq_id)
        });
        if let Some(qid) = create_io_cq_qid {
            // controller dispatch_admin Create IO CQ 内部 `cqs.insert(qid,
            // CompletionQueue { base_gpa: sqe.prp1, ... })`；我们把 prp1
            // 改成 cq_sentinel(qid)，post_cqe 时 ctx.dma_write 落到该哨值。
            sqe.prp1 = cq_sentinel(qid);
        }

        tracing::debug!(opc, cid, "V3/V4b/V5a admin dispatch");

        // **修 M-1** — token 起点用 self.next_token 跨 cmd 单调；结束保存
        let mut tcp_t = TcpAdminTransport::new_with_token_base(self.next_token);

        // ─── Phase 1：dispatch ─────────────────────────────────────────
        let immediate_cqe = {
            let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
            self.controller.nvme_admin_dispatch(&mut ctx, sqe, cid, 0)
        };

        // **V5a** — Create IO CQ/SQ 成功后镜像到 session.io_queues。
        // 同步路径 (Some(Cqe))：Cqe.dw3 bits 17..32 是 SF (SC/SCT/...)，
        // bits 17..25 = SC（status code）；SC=0 表示 success（spec §5.2）。
        if let Some(cqe) = immediate_cqe.as_ref() {
            let dw3 = cqe.dw3;
            let sc = ((dw3 >> 17) & 0xff) as u8;
            let success = sc == 0;
            if success {
                if let Some(qid) = create_io_cq_qid {
                    let sentinel = cq_sentinel(qid);
                    self.io_queues.insert(qid, IoQueueState::new_cq(sentinel));
                    tracing::info!(
                        qid,
                        sentinel = format_args!("{sentinel:#x}"),
                        "V5a: session mirrors Create IO CQ"
                    );
                }
                if let Some((sq_id, cq_id)) = create_io_sq_pair {
                    self.io_queues.insert(sq_id, IoQueueState::new_sq(cq_id));
                    tracing::info!(sq_id, cq_id, "V5a: session mirrors Create IO SQ");
                }
            }
        }

        // ─── Phase 1.5..5 → 抽出共享 helper（V5b 起 admin/IO 都走它）─
        self.run_post_dispatch(cid, immediate_cqe, tcp_t)
    }

    /// **V5b** — `handle_admin_cmd` / `handle_io_cmd` 共享的"post-dispatch"
    /// Phase 1.5..5 闭环：mixed-path guard → R2T read loop → write-out
    /// completion 投递 → drain captured.writes → C2HData + CapsuleResp。
    ///
    /// 调用方负责：
    /// 1. 改写 sqe.prp1=PRP1_SENTINEL（+ Create IO CQ 的 cq_sentinel(qid)）
    /// 2. 用 `TcpAdminTransport::new_with_token_base(self.next_token)` 起 transport
    /// 3. 调 `nvme_admin_dispatch` 或 `nvme_io_dispatch`
    /// 4. 把 `immediate_cqe + tcp_t` 喂给本函数
    ///
    /// 本函数结束时 `self.next_token = tcp_t.token_high_water()` 已保存。
    /// review H-1 / H-2 / L-1 / L-2 / M-2 invariant 已在内部统一处理。
    fn run_post_dispatch(
        &mut self,
        cid: u16,
        immediate_cqe: Option<pcie_remote_nvme_userspace::cmd::Cqe>,
        mut tcp_t: TcpAdminTransport,
    ) -> anyhow::Result<()> {
        // ─── Phase 1.5：mixed-path guard ─────────────────────────────
        let dispatch_data_writes = tcp_t.writes.iter().filter(|w| w.gpa < CQ_BASE_GPA).count();
        let dispatch_pending_reads = tcp_t.pending_reads.len();
        if dispatch_data_writes > 0 && dispatch_pending_reads > 0 {
            anyhow::bail!(
                "V4b/V5 invariant violation: cmd produced both data_write ({}) and \
                 dma_read ({}) in dispatch — mixed path not supported until V5e",
                dispatch_data_writes,
                dispatch_pending_reads
            );
        }
        let had_pending_reads = dispatch_pending_reads > 0;

        // ─── Phase 2：处理 captured pending_reads（V4b dma_read 闭环）───
        while let Some(read_req) = tcp_t.pop_read() {
            tracing::debug!(
                cid,
                token = read_req.token,
                len = read_req.len,
                "V4b/V4c dispatch dma_read"
            );
            let bytes = self.dma_read_via_r2t(cid, read_req.len).with_context(|| {
                format!(
                    "V4 dma_read failed (cid={cid}, token={tok}, len={l})",
                    tok = read_req.token,
                    l = read_req.len
                )
            })?;
            let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
            self.controller
                .nvme_admin_complete_dma(&mut ctx, read_req.token, true, bytes);
        }

        // ─── Phase 3：同步 / 异步 write-out 路径 ─────────────────────
        if let Some(cqe) = immediate_cqe {
            let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
            self.controller.nvme_post_cqe(&mut ctx, cqe);
        } else if !had_pending_reads {
            let mut data_tokens = Vec::with_capacity(tcp_t.writes.len());
            for w in tcp_t.writes.iter() {
                if w.gpa >= CQ_BASE_GPA {
                    anyhow::bail!(
                        "V3 invariant violation: async dispatch produced CQE write \
                         before on_dma_complete (gpa={:#x})",
                        { w.gpa }
                    );
                }
                data_tokens.push(w.token);
            }
            for tok in data_tokens {
                let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
                self.controller
                    .nvme_admin_complete_dma(&mut ctx, tok, true, Vec::new());
            }
        }

        // 保存 token high water 跨 cmd
        self.next_token = tcp_t.token_high_water();

        // ─── Phase 4：drain captured.writes → data + cqe ────────────
        let mut data_payload = Vec::new();
        let mut cqe_bytes: Option<Vec<u8>> = None;
        while let Some(w) = tcp_t.pop_write() {
            if w.gpa >= CQ_BASE_GPA {
                if cqe_bytes.is_some() {
                    anyhow::bail!(
                        "V3 invariant violation: multiple CQE writes captured for single cmd"
                    );
                }
                if w.data.len() != 16 {
                    anyhow::bail!("captured CQE write len={} != 16", w.data.len());
                }
                cqe_bytes = Some(w.data);
            } else {
                if cqe_bytes.is_some() {
                    anyhow::bail!(
                        "V3 invariant violation: data write captured after CQE write \
                         (gpa={:#x}, {} bytes)",
                        { w.gpa },
                        w.data.len()
                    );
                }
                data_payload.extend_from_slice(&w.data);
            }
        }
        let cqe_bytes = cqe_bytes
            .ok_or_else(|| anyhow::anyhow!("V3: controller did not produce CQE for cmd"))?;

        // ─── Phase 5：emit C2HData + CapsuleResp ─────────────────────
        if !data_payload.is_empty() {
            self.send_c2h_data(cid, &data_payload)?;
        }
        self.write_capsule_resp_bytes(&cqe_bytes)
    }

    /// **Phase V4c** — 把 controller 一条 `dma_read(len)` 拆成多条
    /// `MAXH2CDATA_BYTES` 大小的 R2T 串行拉回，拼接成 `Vec<u8>` 返。
    ///
    /// 流程（每片）：
    /// 1. 计算 `chunk = min(remaining, MAXH2CDATA_BYTES)`
    /// 2. 分配新 ttag
    /// 3. emit `R2T(cid, ttag, offset, chunk)`
    /// 4. 调 [`await_host_data`] 带 `base_offset = offset` 阻塞收齐 chunk 字节
    ///    （**review H-1 fix**：host 端 Linux nvme-tcp 填的 `psh.data_offset`
    ///    是 cmd 累计 offset，reassembler 用 `base_offset + received` 匹配）
    /// 5. push 到 accumulator
    ///
    /// 串行 vs 流水线：spec 允许 controller 同时发多 R2T（多 ttag 并发），
    /// 但本教学版单线程 read_pdu 阻塞，做不到。串行 multi-R2T 仍然合规
    /// （Linux nvme-tcp host 会按 ttag 严格 demux），只是性能不优。
    ///
    /// **R-3 (plan)**：read_pdu 阻塞 → 整条 admin cmd 处理期间不能并发处理
    /// 其他 cmd；V8 + tokio refactor 解决。
    ///
    /// **review H-2 fix**：单条 dma_read 长度 cap 到 [`V4_MAX_DMA_READ_BYTES`]
    /// （8 MiB，与 controller 内部 FW_MAX 对齐）。超出 → bail 防 R2T 循环
    /// 爆炸 + Vec::with_capacity 大块分配 OOM。
    fn dma_read_via_r2t(&mut self, cid: u16, total_len: u32) -> anyhow::Result<Vec<u8>> {
        if total_len > V4_MAX_DMA_READ_BYTES {
            anyhow::bail!(
                "V4c: dma_read total_len {} exceeds policy cap {} (防 R2T 循环爆炸 / OOM)",
                total_len,
                V4_MAX_DMA_READ_BYTES
            );
        }
        let max = MAXH2CDATA_BYTES;
        let mut buf: Vec<u8> = Vec::with_capacity(total_len as usize);
        let mut offset: u32 = 0;
        while offset < total_len {
            let remaining = total_len - offset;
            let chunk = remaining.min(max);
            let bytes = self.dma_read_one_chunk(cid, offset, chunk)?;
            buf.extend_from_slice(&bytes);
            offset += chunk;
        }
        debug_assert_eq!(buf.len(), total_len as usize);
        Ok(buf)
    }

    /// **V4c (review M-1)** — 单片 R2T 子路径：alloc ttag → emit R2T →
    /// await_host_data 收齐 → 返字节。从 [`dma_read_via_r2t`] 拆出降低
    /// 函数体积，invariant 集中。
    fn dma_read_one_chunk(&mut self, cid: u16, offset: u32, chunk: u32) -> anyhow::Result<Vec<u8>> {
        let ttag = self.ttag_alloc.alloc();
        tracing::debug!(cid, ttag, offset, chunk, "V4c emit R2T (chunk)");
        let (hdr, psh) = encode_r2t(cid, ttag, offset, chunk);
        write_pdu(&mut self.stream, &hdr, psh.as_bytes(), &[]).context("V4c: write R2T PDU")?;
        await_host_data(&mut self.stream, &self.negotiated, cid, ttag, offset, chunk).with_context(
            || format!("V4c await_host_data failed (ttag={ttag}, offset={offset}, chunk={chunk})"),
        )
    }

    /// 发 C2HData PDU：一次性投递整段 data，标 DATA_LAST。
    fn send_c2h_data(&mut self, cid: u16, data: &[u8]) -> anyhow::Result<()> {
        let hdr = CommonHdr {
            pdu_type: pdu_type::C2H_DATA,
            flags: flags::DATA_LAST,
            hlen: 24,
            pdo: 24,
            plen: 24 + data.len() as u32,
        };
        let psh = DataPsh {
            cccid: cid,
            ttag_or_rsvd: 0,
            data_offset: 0,
            data_length: data.len() as u32,
            rsvd: [0u8; 4],
        };
        write_pdu(&mut self.stream, &hdr, psh.as_bytes(), data).context("write C2HData")
    }

    /// 直接发 16-byte CQE bytes 作为 CapsuleResp PSH（NVMe-oF 协议规定 CQE
    /// 就是 RSP PDU 的 PSH 内容）。
    fn write_capsule_resp_bytes(&mut self, cqe_bytes: &[u8]) -> anyhow::Result<()> {
        debug_assert_eq!(cqe_bytes.len(), 16);
        let hdr = CommonHdr {
            pdu_type: pdu_type::RSP,
            flags: 0,
            hlen: 24,
            pdo: 0,
            plen: 24,
        };
        write_pdu(&mut self.stream, &hdr, cqe_bytes, &[]).context("write CapsuleResp from raw CQE")
    }

    /// 同步路径用：把 Cqe struct 序列化成 16-byte CapsuleResp PSH。
    ///
    /// **V3-polish (review M2)** — 同步路径已统一走 controller.nvme_post_cqe
    /// + drain captured CQE write，本函数不再被 handle_admin_cmd 调用，但
    ///   保留作为外部 caller（V6 AER 等）将 Cqe 直接 emit 的 helper。
    #[allow(dead_code)]
    fn write_capsule_resp_from_cqe(
        &mut self,
        cqe: &pcie_remote_nvme_userspace::cmd::Cqe,
    ) -> anyhow::Result<()> {
        self.write_capsule_resp_bytes(cqe.as_bytes())
    }

    fn handle_connect(&mut self, cid: u16, sqe: &[u8], data: &[u8]) -> anyhow::Result<()> {
        if data.len() != fabric::CONNECT_DATA_SIZE {
            return self.send_capsule_resp_err(cid, fabric_sc::CONNECT_INVALID_PARAM);
        }
        let cd = match ConnectData::read_from_bytes(data) {
            Ok(c) => c,
            Err(_) => {
                return self.send_capsule_resp_err(cid, fabric_sc::CONNECT_INVALID_PARAM);
            }
        };
        let fields = match fabric::decode_connect_fields(sqe) {
            Ok(f) => f,
            Err(_) => {
                return self.send_capsule_resp_err(cid, fabric_sc::CONNECT_INVALID_PARAM);
            }
        };
        let qid = fields.qid;
        let kato = fields.kato;
        tracing::info!(
            qid,
            kato,
            subnqn = cd.subnqn_str(),
            hostnqn = cd.hostnqn_str(),
            "Fabric Connect"
        );
        if qid == 0 {
            if self.admin_connected {
                return self.send_capsule_resp_err(cid, fabric_sc::CONNECT_INVALID_PARAM);
            }
            self.admin_connected = true;
            self.current_qid = 0;
        } else {
            // **Phase V5a** — IO queue Connect：必须前置 admin Connect
            // 已 ack 且 host 已通过 admin path 跑过 Create IO CQ + Create
            // IO SQ for this qid（session.io_queues[qid] 应为 Sq{connected=false}）。
            if !self.admin_connected {
                return self.send_capsule_resp_err(cid, fabric_sc::CONNECT_INVALID_PARAM);
            }
            match self.io_queues.get_mut(&qid) {
                Some(state @ IoQueueState::Sq { .. }) => {
                    state.mark_connected();
                    self.current_qid = qid;
                }
                _ => {
                    tracing::warn!(qid, "Connect qid≥1 before Create IO SQ — reject");
                    return self.send_capsule_resp_err(cid, fabric_sc::CONNECT_INVALID_PARAM);
                }
            }
        }
        // CQE.result DW0 = cntlid（低 16 位）；CQE status = success
        self.send_capsule_resp_ok(cid, self.cntlid as u32)
    }

    fn handle_property_get(&mut self, cid: u16, sqe: &[u8]) -> anyhow::Result<()> {
        let pf = match fabric::decode_property_fields(sqe) {
            Ok(p) => p,
            Err(_) => return self.send_capsule_resp_err(cid, 0x02),
        };
        let size = match fabric::property_size_bytes(pf.attrib) {
            Ok(s) => s,
            Err(_) => return self.send_capsule_resp_err(cid, 0x02),
        };
        // **review H2** — 用 narrow wrapper 强制 NVMe-oF spec 白名单
        let ofst = pf.ofst;
        let value = match self.controller.nvme_property_get(ofst, size) {
            Some(v) => v,
            None => return self.send_capsule_resp_err(cid, 0x02),
        };
        // **review H1** — 8-byte value 必须把高 32 位放 CQE DW1 (cqe[4..8])，
        // 否则 Linux nvme-tcp driver 读 CAP 拿不到 MPSMIN/MPSMAX/CSS/TO/AMS
        // 等高位字段会拒绝 enumerate controller。
        let lo = (value & 0xFFFF_FFFF) as u32;
        let hi = (value >> 32) as u32;
        let hi_for_dw1 = if size == 8 { hi } else { 0 };
        self.send_capsule_resp_ok_with_dw1(cid, lo, hi_for_dw1)
    }

    fn handle_property_set(&mut self, cid: u16, sqe: &[u8]) -> anyhow::Result<()> {
        let pf = match fabric::decode_property_fields(sqe) {
            Ok(p) => p,
            Err(_) => return self.send_capsule_resp_err(cid, 0x02),
        };
        let size = match fabric::property_size_bytes(pf.attrib) {
            Ok(s) => s,
            Err(_) => return self.send_capsule_resp_err(cid, 0x02),
        };
        let ofst = pf.ofst;
        let value = pf.value;
        // **review M2** — NoopTransport 只在 V2 验证安全（CC.EN=1 等 reg
        // 写入路径不 invoke ctx.dma_*/fire_interrupt）。V5 真 IO 上线后
        // 应换成完整 transport bridge。
        let mut t = pcie_vfio_user_sdk::NoopTransport;
        let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut t);
        // **review H2** — 用 narrow wrapper；offset 不在白名单时返 false。
        if !self
            .controller
            .nvme_property_set(&mut ctx, ofst, size, value)
        {
            return self.send_capsule_resp_err(cid, 0x02);
        }
        self.send_capsule_resp_ok(cid, 0)
    }

    /// 写一条 CapsuleResp CQE（16 byte）回 client。`status_sf` = SF 字段
    /// （bits 1..15 of status；phase bit 我们填 0 — NVMe-oF spec 不用 phase）。
    fn send_capsule_resp_ok(&mut self, cid: u16, result_dw0: u32) -> anyhow::Result<()> {
        self.send_capsule_resp_ok_with_dw1(cid, result_dw0, 0)
    }

    /// 同 [`send_capsule_resp_ok`] 但允许填 CQE DW1（用于 8-byte Property Get 高位）。
    fn send_capsule_resp_ok_with_dw1(
        &mut self,
        cid: u16,
        result_dw0: u32,
        result_dw1: u32,
    ) -> anyhow::Result<()> {
        let mut cqe = [0u8; 16];
        cqe[0..4].copy_from_slice(&result_dw0.to_le_bytes());
        cqe[4..8].copy_from_slice(&result_dw1.to_le_bytes()); // **review H1** Property Get 高 32 位
        // bytes 8..10 sq_head（V2 不真追踪 SQ head，填 0）；
        // bytes 10..12 sq_id（V2 admin queue = 0）；
        cqe[12..14].copy_from_slice(&cid.to_le_bytes());
        // bytes 14..16 status = 0 (success)
        let hdr = CommonHdr {
            pdu_type: pdu_type::RSP,
            flags: 0,
            hlen: 24,
            pdo: 0,
            plen: 24,
        };
        write_pdu(&mut self.stream, &hdr, &cqe, &[]).context("write CapsuleResp")
    }

    fn send_capsule_resp_err(&mut self, cid: u16, sc: u8) -> anyhow::Result<()> {
        let mut cqe = [0u8; 16];
        cqe[12..14].copy_from_slice(&cid.to_le_bytes());
        // **review M3** — 对 fabric SC (0x80-0x9F)：SCT=0x07 (Command Specific)；
        // 其它 generic SC 用 SCT=0。Linux nvme-tcp host 接受两种，但 spec 严格。
        let sct: u8 = if (0x80..=0x9F).contains(&sc) {
            0x07
        } else {
            0x00
        };
        // status word: bit 0 = phase (= 0 NVMe-oF); bits 1..8 = SC; bits 9..11 = SCT
        let status: u16 = (sct as u16) << 9 | (sc as u16) << 1;
        cqe[14..16].copy_from_slice(&status.to_le_bytes());
        let hdr = CommonHdr {
            pdu_type: pdu_type::RSP,
            flags: 0,
            hlen: 24,
            pdo: 0,
            plen: 24,
        };
        write_pdu(&mut self.stream, &hdr, &cqe, &[]).context("write CapsuleResp err")
    }

    fn send_c2h_term(&mut self, fes: u16) -> anyhow::Result<()> {
        let hdr = CommonHdr {
            pdu_type: pdu_type::C2H_TERM,
            flags: 0,
            hlen: 24,
            pdo: 0,
            plen: 24,
        };
        let psh = TermPsh {
            fes,
            fei: [0u8; 4],
            rsvd: [0u8; 10],
        };
        write_pdu(&mut self.stream, &hdr, psh.as_bytes(), &[]).context("write C2HTermReq")
    }
}

fn write_term(stream: &mut TcpStream, fes: u16) -> anyhow::Result<()> {
    let hdr = CommonHdr {
        pdu_type: pdu_type::C2H_TERM,
        flags: 0,
        hlen: 24,
        pdo: 0,
        plen: 24,
    };
    let psh = TermPsh {
        fes,
        fei: [0u8; 4],
        rsvd: [0u8; 10],
    };
    write_pdu(stream, &hdr, psh.as_bytes(), &[])
}

/// **V4b** — 阻塞读 H2CData PDU 直到收齐 `expected_len` 字节。
///
/// caller 先 emit R2T(cid, ttag, 0, expected_len)，然后调用本函数等 host
/// 上传数据。函数循环 `read_pdu`：
/// - 收到合法 H2CData → 喂 [`H2cReassembler`]，`Done` 返还 bytes
/// - 收到非 H2CData / cccid 或 ttag 错配 / DATA_LAST 缺失 →
///   发 C2HTermReq(fes) 然后 bail
/// - 收到 H2C_TERM → bail（host 主动 abort）
///
/// **R-3 (V4 plan)**：单线程 read_pdu 模型下，此函数阻塞会导致
/// dispatch 阶段暂停；V4b 接受这个简化，multi-pipeline 留 V8 + tokio。
///
/// `_negotiated` 当前未用，预留 V4c 切片用（MAXH2CDATA 上限校验）。
///
/// TODO(V4c, review M-6): 用 `_negotiated.maxh2cdata` 校验单 PDU
/// `data_length` 上限；超出 → write_term(DATA_OUT_OF_RANGE) + bail。
fn await_host_data(
    stream: &mut TcpStream,
    _negotiated: &NegotiatedIc,
    cid: u16,
    ttag: u16,
    base_offset: u32,
    expected_len: u32,
) -> anyhow::Result<Vec<u8>> {
    let mut r = H2cReassembler::with_base_offset(cid, ttag, base_offset, expected_len);
    loop {
        let pdu = read_pdu(stream).context("V4b: read H2CData")?;
        let pt = pdu.header.pdu_type;
        if pt == pdu_type::H2C_TERM {
            anyhow::bail!("V4b: host sent H2CTermReq while awaiting H2CData");
        }
        match r.accept_pdu(&pdu) {
            AcceptOutcome::Continue => continue,
            AcceptOutcome::Done(bytes) => return Ok(bytes),
            AcceptOutcome::Error { fes, reason } => {
                let _ = write_term(stream, fes);
                anyhow::bail!("V4b H2CData reassembly failed: fes={fes:#x} reason={reason}");
            }
        }
    }
}

/// V2 ICReq/ICResp 握手。
pub fn ic_handshake(stream: &mut TcpStream) -> anyhow::Result<NegotiatedIc> {
    let pdu = read_pdu(stream).context("read ICReq")?;
    let t = pdu.header.pdu_type;
    if t != pdu_type::ICREQ {
        // 违反协议 — 发 TermReq INVALID_PDU_HDR 然后 caller close。
        let _ = write_term(stream, term_fes::INVALID_PDU_HDR);
        anyhow::bail!("expected ICReq as first PDU, got {t:#x}");
    }
    let ic: IcPsh = crate::pdu::decode_psh(&pdu.psh).map_err(|e| {
        let _ = write_term(stream, term_fes::INVALID_PDU_HDR);
        anyhow::anyhow!("decode IcPsh: {e}")
    })?;
    let client_pfv = ic.pfv;
    let _client_digest = ic.digest;
    let client_maxr2t = ic.maxr2t_or_maxh2cdata;
    let client_hpda = ic.hpda_or_cpda;
    if client_pfv != 0 {
        let _ = write_term(stream, term_fes::UNSUPPORTED_PARAM);
        anyhow::bail!("PDU Format Version mismatch: client={client_pfv}, want 0");
    }
    // V2 教学路径：digest 全部 disable（让 nvme-cli 默认行为通过；hdgst/ddgst
    // 路径已在 framing 验证过）。
    let hdgst = false;
    let ddgst = false;
    // maxh2cdata 我们 advertise 64 KiB（与 Linux nvmet default 一致）。
    let our_maxh2cdata: u32 = MAXH2CDATA_BYTES;
    let our_cpda: u8 = 0; // 4-byte align

    let resp_hdr = CommonHdr {
        pdu_type: pdu_type::ICRESP,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let mut digest_bits = 0u8;
    if hdgst {
        digest_bits |= flags::HDGST;
    }
    if ddgst {
        digest_bits |= flags::DDGST;
    }
    let resp_psh = IcPsh {
        pfv: 0,
        hpda_or_cpda: our_cpda,
        digest: digest_bits,
        maxr2t_or_maxh2cdata: our_maxh2cdata,
        rsvd: [0u8; 112],
    };
    write_pdu(stream, &resp_hdr, resp_psh.as_bytes(), &[]).context("write ICResp")?;

    Ok(NegotiatedIc {
        pfv: 0,
        hpda: client_hpda,
        hdgst,
        ddgst,
        maxh2cdata: our_maxh2cdata,
        maxr2t: client_maxr2t,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fabric::ConnectFabricFields;
    use crate::fabric::PropertyFabricFields;
    use crate::fabric::fctype;
    use crate::fabric::property_offset;
    use crate::pdu::DataPsh;
    use std::net::TcpListener;
    use std::thread;

    fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let t = thread::spawn(move || listener.accept().unwrap().0);
        let client = TcpStream::connect(addr).unwrap();
        let server = t.join().unwrap();
        (client, server)
    }

    /// 临时 backing file + NvmeController 用于测试。
    ///
    /// **V3-polish (review L6)** — 用 [`tempfile::NamedTempFile`] 而非
    /// `temp_dir().join(pid+tid)` 拼路径；NamedTempFile drop 时自动 unlink，
    /// 不需要测试函数手工清理（之前的方案会在 /tmp 永久积垢，CI 上每次
    /// `cargo test` 跑都漏 ~1 KiB 文件 × N tests）。
    ///
    /// 返 `(controller, guard)` —— **调用者必须把 guard 绑到 `_guard` 之类
    /// 的名字**（不能直接丢；丢即 drop，文件被 unlink，NvmeController
    /// 的 backing file fd 还能正常 read/write（Unix unlink semantics），
    /// 但 path-based 操作会消失）。
    fn make_test_controller() -> (NvmeController, tempfile::NamedTempFile) {
        let f = tempfile::NamedTempFile::new().expect("create tempfile");
        f.as_file().set_len(1024 * 1024).expect("set_len");
        let path = f.path().to_str().expect("temp path utf8").to_string();
        let c = NvmeController::open(&[path], 0x1414, 0, &[]).expect("NvmeController::open");
        (c, f)
    }

    /// ICReq → ICResp 握手 happy path（无 digest）。
    #[test]
    fn ic_handshake_no_digest() {
        let (mut client, mut server) = tcp_pair();
        let h = thread::spawn(move || ic_handshake(&mut server));

        // client 端发 ICReq
        let hdr = CommonHdr {
            pdu_type: pdu_type::ICREQ,
            flags: 0,
            hlen: 128,
            pdo: 0,
            plen: 128,
        };
        let psh = IcPsh {
            pfv: 0,
            hpda_or_cpda: 0,
            digest: 0,
            maxr2t_or_maxh2cdata: 7,
            rsvd: [0u8; 112],
        };
        write_pdu(&mut client, &hdr, psh.as_bytes(), &[]).unwrap();

        // 收 ICResp
        let resp = read_pdu(&mut client).unwrap();
        let rt = resp.header.pdu_type;
        assert_eq!(rt, pdu_type::ICRESP);
        let rp: IcPsh = crate::pdu::decode_psh(&resp.psh).unwrap();
        let pfv = rp.pfv;
        let max = rp.maxr2t_or_maxh2cdata;
        let dig = rp.digest;
        assert_eq!(pfv, 0);
        assert_eq!(max, 64 * 1024);
        assert_eq!(dig, 0);

        let neg = h.join().unwrap().unwrap();
        assert!(!neg.hdgst);
        assert!(!neg.ddgst);
        assert_eq!(neg.maxh2cdata, 64 * 1024);
        assert_eq!(neg.maxr2t, 7);
    }

    /// 不是 ICReq → server 发 TermReq + Err 返。
    #[test]
    fn ic_handshake_rejects_non_icreq() {
        let (mut client, mut server) = tcp_pair();
        let h = thread::spawn(move || ic_handshake(&mut server));
        // 客户端先发 C2HData（错的）
        let hdr = CommonHdr {
            pdu_type: pdu_type::H2C_DATA,
            flags: 0,
            hlen: 24,
            pdo: 0,
            plen: 24,
        };
        let psh = DataPsh::default();
        write_pdu(&mut client, &hdr, psh.as_bytes(), &[]).unwrap();
        // 收 TermReq
        let term = read_pdu(&mut client).unwrap();
        let tt = term.header.pdu_type;
        assert_eq!(tt, pdu_type::C2H_TERM);
        assert!(h.join().unwrap().is_err());
    }

    /// pfv != 0 → UNSUPPORTED_PARAM TermReq。
    #[test]
    fn ic_handshake_rejects_bad_pfv() {
        let (mut client, mut server) = tcp_pair();
        let h = thread::spawn(move || ic_handshake(&mut server));
        let hdr = CommonHdr {
            pdu_type: pdu_type::ICREQ,
            flags: 0,
            hlen: 128,
            pdo: 0,
            plen: 128,
        };
        let psh = IcPsh {
            pfv: 99,
            ..IcPsh::default()
        };
        write_pdu(&mut client, &hdr, psh.as_bytes(), &[]).unwrap();
        let term = read_pdu(&mut client).unwrap();
        let tt = term.header.pdu_type;
        assert_eq!(tt, pdu_type::C2H_TERM);
        let p: TermPsh = crate::pdu::decode_psh(&term.psh).unwrap();
        let fes = p.fes;
        assert_eq!(fes, term_fes::UNSUPPORTED_PARAM);
        assert!(h.join().unwrap().is_err());
    }

    /// Connect happy path：admin queue (qid=0)，CQE.result = cntlid。
    #[test]
    fn fabric_connect_admin_returns_cntlid() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect
            Ok(())
        });
        // client 先 ICReq/ICResp
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();

        // 然后 CapsuleCmd Fabric Connect
        let mut sqe = [0u8; 64];
        sqe[0] = fabric::NVME_OPC_FABRIC;
        sqe[2..4].copy_from_slice(&0x0042u16.to_le_bytes()); // cid
        sqe[4] = fctype::CONNECT;
        let f = ConnectFabricFields {
            recfmt: 0,
            qid: 0,
            sqsize: 31,
            cattr: 0,
            rsvd1: 0,
            kato: 60000,
            rsvd2: [0u8; 12],
        };
        sqe[40..64].copy_from_slice(f.as_bytes());
        let mut cdata = ConnectData::default();
        let nqn = b"nqn.2026-06.io.test:client";
        cdata.hostnqn[..nqn.len()].copy_from_slice(nqn);
        let snqn = b"nqn.2026-06.io.openhcl:nvme.userspace";
        cdata.subnqn[..snqn.len()].copy_from_slice(snqn);
        let cmd_hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 72,
            plen: 72 + 1024,
        };
        write_pdu(&mut client, &cmd_hdr, &sqe, cdata.as_bytes()).unwrap();

        let resp = read_pdu(&mut client).unwrap();
        let rt = resp.header.pdu_type;
        assert_eq!(rt, pdu_type::RSP);
        // CQE byte 0..4 = result DW0 = cntlid (1)
        assert_eq!(u32::from_le_bytes(resp.psh[..4].try_into().unwrap()), 1);
        // CQE byte 12..14 = cid
        assert_eq!(
            u16::from_le_bytes(resp.psh[12..14].try_into().unwrap()),
            0x42
        );
        // status SF = 0 (success)
        assert_eq!(u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()), 0);
        h.join().unwrap().unwrap();
    }

    /// Property Get CAP — read 8-byte BAR0 reg via Fabric。
    #[test]
    fn property_get_cap_reads_bar0() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect
            sess.pump_one()?; // Property Get
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();

        // Property Get CAP (size=1 → 8B)
        let mut sqe = [0u8; 64];
        sqe[0] = fabric::NVME_OPC_FABRIC;
        sqe[2..4].copy_from_slice(&0x0099u16.to_le_bytes());
        sqe[4] = fctype::PROPERTY_GET;
        let f = PropertyFabricFields {
            attrib: 1,
            rsvd1: [0u8; 3],
            ofst: property_offset::CAP,
            value: 0,
            rsvd2: [0u8; 8],
        };
        sqe[40..64].copy_from_slice(f.as_bytes());
        let cmd_hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(&mut client, &cmd_hdr, &sqe, &[]).unwrap();
        let resp = read_pdu(&mut client).unwrap();
        // result DW0 = CAP 低 32 位（应 != 0；NvmeController 的 CAP 非 0）
        let dw0 = u32::from_le_bytes(resp.psh[..4].try_into().unwrap());
        assert_ne!(dw0, 0, "CAP DW0 应非零");
        h.join().unwrap().unwrap();
    }

    fn send_icreq(client: &mut TcpStream) {
        let hdr = CommonHdr {
            pdu_type: pdu_type::ICREQ,
            flags: 0,
            hlen: 128,
            pdo: 0,
            plen: 128,
        };
        let psh = IcPsh::default();
        write_pdu(client, &hdr, psh.as_bytes(), &[]).unwrap();
    }

    fn send_connect_admin(client: &mut TcpStream) {
        let mut sqe = [0u8; 64];
        sqe[0] = fabric::NVME_OPC_FABRIC;
        sqe[2..4].copy_from_slice(&0x0001u16.to_le_bytes());
        sqe[4] = fctype::CONNECT;
        let f = ConnectFabricFields {
            recfmt: 0,
            qid: 0,
            sqsize: 31,
            cattr: 0,
            rsvd1: 0,
            kato: 60000,
            rsvd2: [0u8; 12],
        };
        sqe[40..64].copy_from_slice(f.as_bytes());
        let cdata = ConnectData::default();
        let cmd_hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 72,
            plen: 72 + 1024,
        };
        write_pdu(client, &cmd_hdr, &sqe, cdata.as_bytes()).unwrap();
    }

    fn send_connect_io(client: &mut TcpStream, qid: u16) {
        let mut sqe = [0u8; 64];
        sqe[0] = fabric::NVME_OPC_FABRIC;
        sqe[2..4].copy_from_slice(&0x0007u16.to_le_bytes());
        sqe[4] = fctype::CONNECT;
        let f = ConnectFabricFields {
            recfmt: 0,
            qid,
            sqsize: 31,
            cattr: 0,
            rsvd1: 0,
            kato: 0,
            rsvd2: [0u8; 12],
        };
        sqe[40..64].copy_from_slice(f.as_bytes());
        let cdata = ConnectData::default();
        let cmd_hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 72,
            plen: 72 + 1024,
        };
        write_pdu(client, &cmd_hdr, &sqe, cdata.as_bytes()).unwrap();
    }

    fn send_property_set(client: &mut TcpStream, ofst: u32, size: u8, value: u64) {
        let mut sqe = [0u8; 64];
        sqe[0] = fabric::NVME_OPC_FABRIC;
        sqe[2..4].copy_from_slice(&0x00AAu16.to_le_bytes());
        sqe[4] = fctype::PROPERTY_SET;
        let f = PropertyFabricFields {
            attrib: size,
            rsvd1: [0u8; 3],
            ofst,
            value,
            rsvd2: [0u8; 8],
        };
        sqe[40..64].copy_from_slice(f.as_bytes());
        let cmd_hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(client, &cmd_hdr, &sqe, &[]).unwrap();
    }

    fn send_property_get(client: &mut TcpStream, ofst: u32, size: u8) {
        let mut sqe = [0u8; 64];
        sqe[0] = fabric::NVME_OPC_FABRIC;
        sqe[2..4].copy_from_slice(&0x00BBu16.to_le_bytes());
        sqe[4] = fctype::PROPERTY_GET;
        let f = PropertyFabricFields {
            attrib: size,
            rsvd1: [0u8; 3],
            ofst,
            value: 0,
            rsvd2: [0u8; 8],
        };
        sqe[40..64].copy_from_slice(f.as_bytes());
        let cmd_hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(client, &cmd_hdr, &sqe, &[]).unwrap();
    }

    /// **review M4 / H1** — Property Get CAP (8B) 必须把高 32 位放 CQE DW1。
    /// 验证 mmio_read_impl(0,0,8) 返 self.cap (u64) 后 CQE byte 0..8 全字段。
    #[test]
    fn property_get_cap_8byte_uses_dw1() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let expected_cap = {
            // 用同一份 controller 在另一个临时实例上读出 CAP，作为对照
            let (mut tmp, _tmp_backing) = make_test_controller();
            tmp.nvme_property_get(0, 8).unwrap()
        };
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect
            sess.pump_one()?; // Property Get
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_property_get(&mut client, property_offset::CAP, 1); // size=1 → 8B
        let resp = read_pdu(&mut client).unwrap();
        // CQE byte 0..4 = DW0 lo, 4..8 = DW1 hi（H1 修复后）
        let dw0 = u32::from_le_bytes(resp.psh[0..4].try_into().unwrap());
        let dw1 = u32::from_le_bytes(resp.psh[4..8].try_into().unwrap());
        let full = (dw1 as u64) << 32 | dw0 as u64;
        assert_eq!(
            full, expected_cap,
            "8B Property Get 必须返完整 CAP，含高位字段"
        );
        h.join().unwrap().unwrap();
    }

    /// **review M4** — Connect with qid=1 before admin → CONNECT_INVALID_PARAM。
    #[test]
    fn connect_io_before_admin_rejected() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?;
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        // 直接 IO Connect qid=1，admin 没建过
        send_connect_io(&mut client, 1);
        let resp = read_pdu(&mut client).unwrap();
        let status = u16::from_le_bytes(resp.psh[14..16].try_into().unwrap());
        let sc = ((status >> 1) & 0xff) as u8;
        assert_eq!(sc, fabric_sc::CONNECT_INVALID_PARAM);
        h.join().unwrap().unwrap();
    }

    /// **review M4** — 第二次 Connect admin (qid=0) → CONNECT_INVALID_PARAM。
    #[test]
    fn connect_admin_twice_rejected() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?;
            sess.pump_one()?;
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client); // 第二次
        let resp = read_pdu(&mut client).unwrap();
        let status = u16::from_le_bytes(resp.psh[14..16].try_into().unwrap());
        let sc = ((status >> 1) & 0xff) as u8;
        let sct = ((status >> 9) & 0x07) as u8;
        assert_eq!(sc, fabric_sc::CONNECT_INVALID_PARAM);
        // **review M3** — fabric SC 0x80-0x9F 必须 SCT=0x07
        assert_eq!(sct, 0x07);
        h.join().unwrap().unwrap();
    }

    /// **review M4** — Property Set CC.EN=1 后 Get CSTS 应见 RDY=1。
    #[test]
    fn property_set_cc_enables_csts_rdy() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect
            sess.pump_one()?; // Property Set CC
            sess.pump_one()?; // Property Get CSTS
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        // 先 AQA/ASQ/ACQ 应该已经 0 默认；直接 enable
        // NvmeController write_cc 在 ASQ=0 时仍能进 enable（教学路径不强校验
        // 这些 reg；真 Linux driver 会先 set ASQ/ACQ 才 enable）。
        send_property_set(&mut client, property_offset::CC, 1, 0x0046_0001); // CC.EN=1
        let _ = read_pdu(&mut client).unwrap();
        send_property_get(&mut client, property_offset::CSTS, 0); // 4B
        let resp = read_pdu(&mut client).unwrap();
        let dw0 = u32::from_le_bytes(resp.psh[0..4].try_into().unwrap());
        // CSTS.RDY = bit 0；enable 成功后应为 1
        assert_ne!(
            dw0 & 1,
            0,
            "Property Set CC.EN=1 后 CSTS.RDY 应为 1，得到 dw0={dw0:#x}"
        );
        h.join().unwrap().unwrap();
    }

    /// **Phase V3** — 完整端到端：Identify Controller (CNS=1)，期望
    /// C2HData PDU 带 4096B + CapsuleResp（status=success）。
    #[test]
    fn admin_identify_controller_emits_c2hdata_and_resp() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect
            sess.pump_one()?; // Identify Controller
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();

        // Identify Controller: opc=0x06, CNS in cdw10 bits 7:0 = 0x01
        let mut sqe = [0u8; 64];
        sqe[0] = 0x06; // admin_opc::IDENTIFY
        sqe[2..4].copy_from_slice(&0x00C1u16.to_le_bytes()); // cid
        // nsid=0；prp1 这里随便（session 会改写为 PRP1_SENTINEL）
        sqe[40..44].copy_from_slice(&0x0000_0001u32.to_le_bytes()); // cdw10 = CNS=1 (Identify Controller)
        let cmd_hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(&mut client, &cmd_hdr, &sqe, &[]).unwrap();

        // 先收 C2HData
        let data_pdu = read_pdu(&mut client).unwrap();
        let pt = data_pdu.header.pdu_type;
        let fl = data_pdu.header.flags;
        assert_eq!(pt, pdu_type::C2H_DATA);
        assert!(fl & flags::DATA_LAST != 0, "应标 DATA_LAST");
        let psh: DataPsh = crate::pdu::decode_psh(&data_pdu.psh).unwrap();
        let cccid = psh.cccid;
        let dlen = psh.data_length;
        assert_eq!(cccid, 0x00C1);
        assert_eq!(dlen, 4096);
        assert_eq!(data_pdu.data.len(), 4096);
        // Identify Controller bytes 0..2 = VID = 0x1414
        assert_eq!(
            u16::from_le_bytes(data_pdu.data[0..2].try_into().unwrap()),
            0x1414
        );

        // 然后收 CapsuleResp（CQE）
        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(resp.header.pdu_type, pdu_type::RSP);
        let resp_cid = u16::from_le_bytes(resp.psh[12..14].try_into().unwrap());
        let status = u16::from_le_bytes(resp.psh[14..16].try_into().unwrap());
        let sc = ((status >> 1) & 0xff) as u8;
        assert_eq!(resp_cid, 0x00C1);
        assert_eq!(sc, 0, "Identify Controller 应 success");
        h.join().unwrap().unwrap();
    }

    /// **V3-polish (review M4)** — Identify Namespace (CNS=0x00, nsid=1)：
    /// 异步路径（controller 走 dma_write_then_complete），断言我们收到
    /// 4 KiB C2HData + 成功 CapsuleResp。
    #[test]
    fn admin_identify_namespace_emits_c2hdata_and_resp() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect
            sess.pump_one()?; // Identify NS
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();

        let mut sqe = [0u8; 64];
        sqe[0] = 0x06; // IDENTIFY
        sqe[2..4].copy_from_slice(&0x00B2u16.to_le_bytes()); // cid
        sqe[4..8].copy_from_slice(&1u32.to_le_bytes()); // nsid = 1
        sqe[40..44].copy_from_slice(&0x0000_0000u32.to_le_bytes()); // CNS=0 (Identify NS)
        let cmd_hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(&mut client, &cmd_hdr, &sqe, &[]).unwrap();

        // C2HData PDU
        let data_pdu = read_pdu(&mut client).unwrap();
        let pt = data_pdu.header.pdu_type;
        assert_eq!(pt, pdu_type::C2H_DATA);
        let psh: DataPsh = crate::pdu::decode_psh(&data_pdu.psh).unwrap();
        let cccid = psh.cccid;
        let dlen = psh.data_length;
        assert_eq!(cccid, 0x00B2);
        assert_eq!(dlen, 4096);
        assert_eq!(data_pdu.data.len(), 4096);

        // CapsuleResp success
        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(resp.header.pdu_type, pdu_type::RSP);
        let status = u16::from_le_bytes(resp.psh[14..16].try_into().unwrap());
        let sc = ((status >> 1) & 0xff) as u8;
        assert_eq!(sc, 0, "Identify NS 应 success");
        h.join().unwrap().unwrap();
    }

    /// **V3-polish (review M4)** — Set Features 是 controller 内部
    /// 同步路径（dispatch_admin 直接返 `Some(Cqe)`）。验证 V3 统一后
    /// 同步路径也走 nvme_post_cqe → captured CQE write → CapsuleResp
    /// 无 C2HData。Set FID=0x07 NUMBER_OF_QUEUES，CQE DW0 应回 (NSQA-1)
    /// | ((NCQA-1) << 16)。
    #[test]
    fn admin_set_features_sync_path_no_c2hdata() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect
            sess.pump_one()?; // Set Features
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();

        let mut sqe = [0u8; 64];
        sqe[0] = 0x09; // SET_FEATURES
        sqe[2..4].copy_from_slice(&0x0011u16.to_le_bytes()); // cid
        // CDW10 = FID=0x07 (NUMBER_OF_QUEUES)
        sqe[40..44].copy_from_slice(&0x0000_0007u32.to_le_bytes());
        // CDW11 = (NSQR-1) | ((NCQR-1) << 16)：要 4 SQ + 4 CQ
        let cdw11: u32 = 3 | (3 << 16);
        sqe[44..48].copy_from_slice(&cdw11.to_le_bytes());
        let cmd_hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(&mut client, &cmd_hdr, &sqe, &[]).unwrap();

        // 同步路径：不应有 C2HData，直接 CapsuleResp
        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(
            resp.header.pdu_type,
            pdu_type::RSP,
            "Set Features 同步路径不应先发 C2HData"
        );
        let resp_cid = u16::from_le_bytes(resp.psh[12..14].try_into().unwrap());
        let status = u16::from_le_bytes(resp.psh[14..16].try_into().unwrap());
        let sc = ((status >> 1) & 0xff) as u8;
        assert_eq!(resp_cid, 0x0011);
        assert_eq!(sc, 0, "Set Features 应 success");
        // CQE DW0 = granted；IO_QUEUE_CAP=4，请求 NSQR=NCQR=4 应授满 → NSQA-1=3。
        // **V3-polish (review M-3)** — 之前 `>= 1` 过宽，regression 时不易定位；
        // 锁死 3 让 IO_QUEUE_CAP 漂移立即被这条断言捕获。
        let dw0 = u32::from_le_bytes(resp.psh[..4].try_into().unwrap());
        let nsqa_minus_1 = dw0 & 0xffff;
        let ncqa_minus_1 = (dw0 >> 16) & 0xffff;
        assert_eq!(
            nsqa_minus_1, 3,
            "应授满 IO_QUEUE_CAP=4 个 SQ → NSQA-1=3, dw0={dw0:#x}"
        );
        assert_eq!(nsqa_minus_1, ncqa_minus_1, "NSQA 应等于 NCQA");
        h.join().unwrap().unwrap();
    }

    /// **V3-polish (review M4)** — Identify NS 用非法 NSID：controller 内部
    /// 走 `Cqe::error(INVALID_NAMESPACE)` 同步返回路径。验证 V3 统一后
    /// 没有 C2HData，CapsuleResp 带 SC=0x0B (INVALID_NAMESPACE_OR_FORMAT)。
    #[test]
    fn admin_identify_namespace_invalid_nsid_returns_error_cqe() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect
            sess.pump_one()?; // Identify NS bad NSID
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();

        let mut sqe = [0u8; 64];
        sqe[0] = 0x06; // IDENTIFY
        sqe[2..4].copy_from_slice(&0x0099u16.to_le_bytes()); // cid
        sqe[4..8].copy_from_slice(&999u32.to_le_bytes()); // NSID=999 不存在
        sqe[40..44].copy_from_slice(&0x0000_0000u32.to_le_bytes()); // CNS=0
        let cmd_hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(&mut client, &cmd_hdr, &sqe, &[]).unwrap();

        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(
            resp.header.pdu_type,
            pdu_type::RSP,
            "Identify NS bad NSID 同步错误路径不应先发 C2HData"
        );
        let resp_cid = u16::from_le_bytes(resp.psh[12..14].try_into().unwrap());
        let status = u16::from_le_bytes(resp.psh[14..16].try_into().unwrap());
        let sc = ((status >> 1) & 0xff) as u8;
        assert_eq!(resp_cid, 0x0099);
        // INVALID_NAMESPACE_OR_FORMAT = 0x0B
        assert_eq!(sc, 0x0B, "应回 INVALID_NAMESPACE_OR_FORMAT, got sc={sc:#x}");
        h.join().unwrap().unwrap();
    }

    // ─── Phase V4b — controller-initiated dma_read 走 R2T/H2CData 闭环 ───

    /// 发一条 admin SQE（普通 NVMe opcode，非 fabric）。session 会改写 prp1。
    fn send_admin_sqe(client: &mut TcpStream, opc: u8, cid: u16, nsid: u32, cdw10: u32) {
        let mut sqe = [0u8; 64];
        sqe[0] = opc;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        sqe[40..44].copy_from_slice(&cdw10.to_le_bytes());
        let hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(client, &hdr, &sqe, &[]).unwrap();
    }

    /// 收一条 R2T；返 (ttag, length)。
    fn read_r2t(client: &mut TcpStream) -> (u16, u32) {
        let p = read_pdu(client).unwrap();
        let pt = p.header.pdu_type;
        assert_eq!(pt, pdu_type::R2T, "expected R2T, got {pt:#x}");
        let r: crate::pdu::R2tPsh = crate::pdu::decode_psh(&p.psh).unwrap();
        let ttag = r.ttag;
        let length = r.r2t_length;
        (ttag, length)
    }

    /// 发一条 H2CData PDU 覆盖整个 R2T，`data_offset=0`（V4b 单 R2T 用）。
    fn send_h2cdata(client: &mut TcpStream, cid: u16, ttag: u16, data: &[u8]) {
        send_h2cdata_at(client, cid, ttag, 0, data);
    }

    /// **V4c** — 发一条 H2CData PDU 带 cmd 累计 `data_offset`。
    /// 对应 Linux nvme-tcp host 真实行为（`psh.data_offset = req->data_sent`）。
    fn send_h2cdata_at(client: &mut TcpStream, cid: u16, ttag: u16, data_offset: u32, data: &[u8]) {
        let plen = 24 + data.len() as u32;
        let hdr = CommonHdr {
            pdu_type: pdu_type::H2C_DATA,
            flags: flags::DATA_LAST,
            hlen: 24,
            pdo: 24,
            plen,
        };
        let psh = DataPsh {
            cccid: cid,
            ttag_or_rsvd: ttag,
            data_offset,
            data_length: data.len() as u32,
            rsvd: [0u8; 4],
        };
        write_pdu(client, &hdr, psh.as_bytes(), data).unwrap();
    }

    /// **V4b** — NS Attachment 0x15 触发 controller dma_read(prp1, 4096)。
    /// session 发 R2T → 等 host 回 H2CData → controller post_cqe → CapsuleResp。
    ///
    /// 注：NS Attachment 0x15 的 controller list buffer host 一般填 16-bit
    /// count + 2047×16-bit cntlid。这里 host 填一个简单 list（count=1,
    /// cntlid=1）。controller 应返 success（V1 + V5 多 controller 时校验更严）。
    #[test]
    fn v4b_admin_dma_read_single_segment_round_trip() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect
            sess.pump_one()?; // NS Attachment (dma_read)
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();

        // NS Attachment 0x15, sel=0 (attach), nsid=1
        // cdw10 bits 7:0 = sel (0 = attach, 1 = detach)
        send_admin_sqe(&mut client, 0x15, 0x00A4, 1, 0x0000_0000);

        // 期望先收 R2T(ttag=1, length=4096)
        let (ttag, length) = read_r2t(&mut client);
        assert_eq!(ttag, 1, "first cmd, first ttag should be 1");
        assert_eq!(length, 4096, "NS Attachment controller list = 4 KiB");

        // 构造合法 controller list buffer：count=1（LE u16），cntlid=1
        let mut buf = vec![0u8; 4096];
        buf[0..2].copy_from_slice(&1u16.to_le_bytes());
        buf[2..4].copy_from_slice(&1u16.to_le_bytes());
        send_h2cdata(&mut client, 0x00A4, ttag, &buf);

        // 收 CapsuleResp（V4b: NS Attachment 完成后 CQE）
        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(resp.header.pdu_type, pdu_type::RSP);
        let resp_cid = u16::from_le_bytes(resp.psh[12..14].try_into().unwrap());
        let status = u16::from_le_bytes(resp.psh[14..16].try_into().unwrap());
        let sc = ((status >> 1) & 0xff) as u8;
        assert_eq!(resp_cid, 0x00A4);
        // **V4b-polish (review M-4)** — controller 默认 attach 所有 NS，
        // 重复 attach 返 NAMESPACE_ALREADY_ATTACHED=0x18 是合法响应。
        // 本测试核心是验 *wire round-trip 闭环*（R2T → H2CData → CqeResp），
        // 不是验 controller NS attach 语义；接受 success (0) 或
        // already-attached (0x18) 都表示闭环正常。
        assert!(
            sc == 0 || sc == 0x18,
            "NS Attachment 0x15 应 success(0) 或 ALREADY_ATTACHED(0x18), got sc={sc:#x}"
        );
        h.join().unwrap().unwrap();
    }

    /// **V4b 修 review M-1** — token 跨 cmd 单调；跑两条 dma_read cmd，
    /// 第二条的 token 必 ≥ 第一条结束后的 high water。session 内部不可见
    /// token，但 ttag 严格单调可作为 proxy（每条 cmd 用一个新 ttag）。
    #[test]
    fn v4b_token_and_ttag_monotonic_across_cmds() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect
            sess.pump_one()?; // NS Attachment #1
            sess.pump_one()?; // NS Attachment #2
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();

        let mut buf = vec![0u8; 4096];
        buf[0..2].copy_from_slice(&1u16.to_le_bytes());
        buf[2..4].copy_from_slice(&1u16.to_le_bytes());

        // 第一条
        send_admin_sqe(&mut client, 0x15, 0x0001, 1, 0);
        let (ttag1, _) = read_r2t(&mut client);
        send_h2cdata(&mut client, 0x0001, ttag1, &buf);
        let _ = read_pdu(&mut client).unwrap(); // CapsuleResp

        // 第二条
        send_admin_sqe(&mut client, 0x15, 0x0002, 1, 0);
        let (ttag2, _) = read_r2t(&mut client);
        send_h2cdata(&mut client, 0x0002, ttag2, &buf);
        let _ = read_pdu(&mut client).unwrap();

        assert!(
            ttag2 > ttag1,
            "ttag must monotonically advance across cmds, got ttag1={ttag1} ttag2={ttag2}"
        );
        h.join().unwrap().unwrap();
    }

    /// **V4b-polish (review H-1)** — host 在 R2T 后发 H2C_TERM；session
    /// 应通过 await_host_data Err `?` 上抛，pump_one 返 Err，连接断；
    /// **不再** emit "TermReq + CapsuleResp" 双发（spec § 5.2 TermReq 后
    /// 任何 PDU 都是协议违例）。
    ///
    /// 之前 V4b 初版策略是投 ok=false completion → controller post_cqe(error)
    /// → emit CapsuleResp，但 wire 上 H2C_TERM 后 client 已 abort，server
    /// 再 push RSP 会让 Linux nvme-tcp host log "unexpected PDU after term"。
    #[test]
    fn v4b_dma_read_h2c_term_bails_no_double_wire() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect
            // pump_one 内部 await_host_data bail 后整条 cmd 失败
            let _ = sess.pump_one();
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();

        send_admin_sqe(&mut client, 0x15, 0x00B5, 1, 0);
        let (_ttag, _len) = read_r2t(&mut client);

        // host 不发 H2CData，改发 H2CTermReq
        let hdr = CommonHdr {
            pdu_type: pdu_type::H2C_TERM,
            flags: 0,
            hlen: 24,
            pdo: 0,
            plen: 24,
        };
        let psh = TermPsh {
            fes: term_fes::PDU_SEQ_ERR,
            fei: [0u8; 4],
            rsvd: [0u8; 10],
        };
        write_pdu(&mut client, &hdr, psh.as_bytes(), &[]).unwrap();

        // session bail 后会在外层 close；这里不强求收 CapsuleResp
        // （V4b ok=false 路径走 controller post_cqe，但 await_host_data
        // 在 H2C_TERM 分支 bail，cmd 终止于此）
        let _ = h.join().unwrap();
    }

    /// **V4b** — host 发错 ttag → reassembler 拒 → session 发 C2HTermReq。
    #[test]
    fn v4b_h2c_data_wrong_ttag_yields_term() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect
            let _ = sess.pump_one();
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();

        send_admin_sqe(&mut client, 0x15, 0x00C6, 1, 0);
        let (ttag, _len) = read_r2t(&mut client);

        // host 发错 ttag = ttag ^ 0xFFFF（保证不同）
        let bad = ttag ^ 0xFFFF;
        let buf = vec![0u8; 4096];
        send_h2cdata(&mut client, 0x00C6, bad, &buf);

        // session 应发 C2HTermReq
        let p = read_pdu(&mut client).unwrap();
        let pt = p.header.pdu_type;
        assert_eq!(pt, pdu_type::C2H_TERM, "expected TermReq, got {pt:#x}");
        let _ = h.join().unwrap();
    }

    // ─── Phase V4c — MAXH2CDATA 分片 + 多 R2T 串行 ───────────────────

    /// **V4c invariant** — 单一来源：[`MAXH2CDATA_BYTES`] const 必须与
    /// ICResp 宣告的 `maxh2cdata` 字段保持一致。两处定义漂移会让 host 看到
    /// 与我们 R2T 切片大小不符的协商值，立即拒连接。
    #[test]
    fn v4c_maxh2cdata_const_matches_handshake() {
        let (mut client, mut server) = tcp_pair();
        let h = thread::spawn(move || ic_handshake(&mut server));
        send_icreq(&mut client);
        let resp = read_pdu(&mut client).unwrap();
        let rp: IcPsh = crate::pdu::decode_psh(&resp.psh).unwrap();
        let advertised = rp.maxr2t_or_maxh2cdata;
        let neg = h.join().unwrap().unwrap();
        assert_eq!(neg.maxh2cdata, MAXH2CDATA_BYTES);
        assert_eq!(advertised, MAXH2CDATA_BYTES);
    }

    /// **V4c** — 触发一段 >MAXH2CDATA 的 dma_read，断 session 发出 *多* R2T
    /// 串行，最终 CapsuleResp 成功。用 FW Image Download (opcode 0x11)
    /// 请求 128 KiB（NUMD = 128 KiB/4 - 1 = 32767）。MAXH2CDATA = 64 KiB
    /// 应切成 2 段 R2T (offset 0/65536, length 65536 each)。
    #[test]
    fn v4c_dma_read_128kib_emits_two_r2t() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect
            sess.pump_one()?; // FW Download
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();

        // FW_IMAGE_DOWNLOAD = 0x11；CDW10 = NUMD = (128 KiB/4) - 1 = 32767；
        // CDW11 = OFFSET dwords = 0
        let total_bytes: u32 = 128 * 1024;
        let numd: u32 = total_bytes / 4 - 1;
        let mut sqe = [0u8; 64];
        sqe[0] = 0x11;
        sqe[2..4].copy_from_slice(&0x00D7u16.to_le_bytes());
        // nsid=0 for admin
        sqe[40..44].copy_from_slice(&numd.to_le_bytes()); // cdw10 = NUMD
        sqe[44..48].copy_from_slice(&0u32.to_le_bytes()); // cdw11 = OFFSET
        let cmd_hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(&mut client, &cmd_hdr, &sqe, &[]).unwrap();

        // 期望连续收 2 条 R2T（offset 0 / MAXH2CDATA），每条 length=MAXH2CDATA
        let mut total_received: u32 = 0;
        let mut r2t_count = 0;
        let mut expected_offset: u32 = 0;
        loop {
            let p = read_pdu(&mut client).unwrap();
            let pt = p.header.pdu_type;
            if pt == pdu_type::RSP {
                break;
            }
            assert_eq!(pt, pdu_type::R2T, "expected R2T or RSP, got {pt:#x}");
            let r: crate::pdu::R2tPsh = crate::pdu::decode_psh(&p.psh).unwrap();
            let ttag = r.ttag;
            let length = r.r2t_length;
            let offset = r.r2t_offset;
            assert_eq!(offset, expected_offset, "R2T offset 必递增连续");
            assert_eq!(length, MAXH2CDATA_BYTES, "每条 R2T 必 = MAXH2CDATA");
            assert!(ttag != 0, "ttag 不可为 0");
            // **V4c (review H-1)** — 回 H2CData 带 cmd 累计 data_offset（与
            // Linux nvme-tcp host 真实行为对齐：psh.data_offset = req->data_sent）。
            let data = vec![0xCDu8; length as usize];
            send_h2cdata_at(&mut client, 0x00D7, ttag, offset, &data);
            r2t_count += 1;
            total_received += length;
            expected_offset += length;
            assert!(r2t_count <= 8, "防 R2T 无限循环");
        }
        assert_eq!(r2t_count, 2, "128 KiB / 64 KiB = 2 条 R2T");
        assert_eq!(total_received, total_bytes);
        h.join().unwrap().unwrap();
    }

    /// **V4c** — 边界值 dma_read 64 KiB 恰好等于 MAXH2CDATA，应单条 R2T。
    /// 用 FW Image Download (NUMD=64KiB/4-1=16383)。
    #[test]
    fn v4c_dma_read_64kib_exact_one_r2t() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?;
            sess.pump_one()?;
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();

        let total: u32 = MAXH2CDATA_BYTES;
        let numd = total / 4 - 1;
        let mut sqe = [0u8; 64];
        sqe[0] = 0x11;
        sqe[2..4].copy_from_slice(&0x00E8u16.to_le_bytes());
        sqe[40..44].copy_from_slice(&numd.to_le_bytes());
        let cmd_hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(&mut client, &cmd_hdr, &sqe, &[]).unwrap();

        // 单 R2T
        let p = read_pdu(&mut client).unwrap();
        let pt = p.header.pdu_type;
        assert_eq!(pt, pdu_type::R2T);
        let r: crate::pdu::R2tPsh = crate::pdu::decode_psh(&p.psh).unwrap();
        let ttag = r.ttag;
        let length = r.r2t_length;
        assert_eq!(length, total);
        let data = vec![0xEEu8; length as usize];
        send_h2cdata(&mut client, 0x00E8, ttag, &data);

        // 然后 CapsuleResp
        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(resp.header.pdu_type, pdu_type::RSP);
        h.join().unwrap().unwrap();
    }

    /// **V4c** — off-by-one 边界：dma_read 64 KiB + 4 byte → 2 段 R2T
    /// (64 KiB + 4 byte)。第二段 length 必恰 = 4，否则 host 会拒。
    #[test]
    fn v4c_dma_read_64kib_plus_4_emits_two_r2t() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?;
            sess.pump_one()?;
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();

        let total: u32 = MAXH2CDATA_BYTES + 4;
        let numd = total / 4 - 1;
        let mut sqe = [0u8; 64];
        sqe[0] = 0x11;
        sqe[2..4].copy_from_slice(&0x00F9u16.to_le_bytes());
        sqe[40..44].copy_from_slice(&numd.to_le_bytes());
        let cmd_hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(&mut client, &cmd_hdr, &sqe, &[]).unwrap();

        // R2T #1: length = 65536
        let p = read_pdu(&mut client).unwrap();
        let r: crate::pdu::R2tPsh = crate::pdu::decode_psh(&p.psh).unwrap();
        let ttag1 = r.ttag;
        let len1 = r.r2t_length;
        let off1 = r.r2t_offset;
        assert_eq!(len1, MAXH2CDATA_BYTES);
        assert_eq!(off1, 0);
        send_h2cdata_at(
            &mut client,
            0x00F9,
            ttag1,
            off1,
            &vec![0xA1u8; len1 as usize],
        );

        // R2T #2: length = 4
        let p = read_pdu(&mut client).unwrap();
        let r: crate::pdu::R2tPsh = crate::pdu::decode_psh(&p.psh).unwrap();
        let ttag2 = r.ttag;
        let len2 = r.r2t_length;
        let off2 = r.r2t_offset;
        assert_eq!(len2, 4, "remainder R2T length 必恰 = 4");
        assert_eq!(off2, MAXH2CDATA_BYTES);
        assert!(ttag2 != ttag1, "每片新 ttag");
        send_h2cdata_at(
            &mut client,
            0x00F9,
            ttag2,
            off2,
            &[0xB2u8, 0xB2, 0xB2, 0xB2],
        );

        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(resp.header.pdu_type, pdu_type::RSP);
        h.join().unwrap().unwrap();
    }

    // ─── Phase V5a — IO queue 安装 + Connect qid≥1 + dispatch 二分 ───

    /// V5a helper：发一条带 cdw11 的 admin SQE（Create IO SQ 等需 cdw11）。
    fn send_admin_sqe_cdw11(
        client: &mut TcpStream,
        opc: u8,
        cid: u16,
        nsid: u32,
        cdw10: u32,
        cdw11: u32,
    ) {
        let mut sqe = [0u8; 64];
        sqe[0] = opc;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        sqe[40..44].copy_from_slice(&cdw10.to_le_bytes());
        sqe[44..48].copy_from_slice(&cdw11.to_le_bytes());
        let hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(client, &hdr, &sqe, &[]).unwrap();
    }

    /// 帮 V5a 跑前置：ICReq → Connect admin → Create IO CQ(qid=1, size=16)
    /// → Create IO SQ(sq=1, cq=1, size=16) → Connect qid=1。
    /// 返还 (client, server-thread join handle, backing-tempfile guard)。
    /// **V5-P9 fix**：调用者必须把 backing guard 绑到一个 `_` 前缀的本地
    /// 变量让 NamedTempFile 与 test 生命周期对齐（drop 时 unlink 干净）；
    /// 之前的 `std::mem::forget(_backing)` 会永久泄漏 /tmp 文件。
    fn v5a_full_setup_qid1(
        sess_pumps: usize,
    ) -> (
        TcpStream,
        std::thread::JoinHandle<anyhow::Result<()>>,
        tempfile::NamedTempFile,
    ) {
        let (mut client, server) = tcp_pair();
        let (controller, backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            for _ in 0..sess_pumps {
                let _ = sess.pump_one()?;
            }
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        // Create IO CQ qid=1, qsize-1=15 (16 slot), cdw11: bit 0 PC=1
        let cdw10_cq = 1u32 | (15u32 << 16);
        send_admin_sqe_cdw11(&mut client, 0x05, 0x0501, 0, cdw10_cq, 0x0000_0001);
        let _ = read_pdu(&mut client).unwrap();
        // Create IO SQ qid=1, qsize-1=15; cdw11: bit 0 PC=1, bits 31:16 = CQ id=1
        let cdw10_sq = 1u32 | (15u32 << 16);
        let cdw11_sq = 0x0001_0001u32; // CQ id=1 + PC=1
        send_admin_sqe_cdw11(&mut client, 0x01, 0x0502, 0, cdw10_sq, cdw11_sq);
        let _ = read_pdu(&mut client).unwrap();
        // Connect qid=1
        send_connect_io(&mut client, 1);
        let _ = read_pdu(&mut client).unwrap();
        (client, h, backing)
    }

    /// **V5a-1** — 完整 setup 流：4 步 admin 全 success + Connect qid=1 success。
    #[test]
    fn v5a_create_io_cq_sq_then_connect_io_qid1_succeeds() {
        // setup 跑 5 个 pump：Connect admin + Create IO CQ + Create IO SQ
        // + Connect qid=1 + 1 个 stub IO cmd 让 thread 不悬挂
        let (mut client, h, _backing) = v5a_full_setup_qid1(5);
        // 发一条 stub IO cmd（IO Read opc=0x02）让 thread 走 handle_io_cmd 后退出
        send_admin_sqe_cdw11(&mut client, 0xFE, 0x0AAA, 1, 0, 0);
        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(resp.header.pdu_type, pdu_type::RSP);
        let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
        // V5a stub: IO cmd 返 INVALID_OPCODE
        assert_eq!(sc, 0x01, "V5a IO cmd 应 stub-return INVALID_OPCODE");
        h.join().unwrap().unwrap();
    }

    /// **V5a-2** — Connect qid=1 之前没 Create IO SQ → CONNECT_INVALID_PARAM。
    #[test]
    fn v5a_connect_io_qid_without_create_io_sq_rejected() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect admin
            sess.pump_one()?; // Connect qid=1（应被拒）
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        // 没 Create IO CQ/SQ 直接 Connect qid=1
        send_connect_io(&mut client, 1);
        let resp = read_pdu(&mut client).unwrap();
        let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
        assert_eq!(sc, fabric_sc::CONNECT_INVALID_PARAM);
        h.join().unwrap().unwrap();
    }

    /// **V5a-3** — `cq_sentinel(qid)` 步长正确、不与 admin CQ 撞、属 [CQ_BASE_GPA, ..) 区。
    #[test]
    fn v5a_create_io_cq_sentinel_distinct_from_admin() {
        assert_eq!(cq_sentinel(0), CQ_BASE_GPA);
        assert_eq!(cq_sentinel(1), CQ_BASE_GPA + CQ_SENTINEL_STRIDE);
        assert_eq!(cq_sentinel(2), CQ_BASE_GPA + 2 * CQ_SENTINEL_STRIDE);
        assert!(cq_sentinel(1) >= CQ_BASE_GPA);
        assert!(cq_sentinel(255) > cq_sentinel(254));
    }

    /// **V5a-4** — V5a stub：IO cmd 进 dispatch 后返 CapsuleResp INVALID_OPCODE。
    /// （与 V5a-1 合并验证；保留独立 named test 表意。）
    #[test]
    fn v5a_io_cmd_after_connect_returns_invalid_opcode() {
        let (mut client, h, _backing) = v5a_full_setup_qid1(5);
        send_admin_sqe_cdw11(&mut client, 0xFE, 0x0BBB, 1, 0, 0);
        let resp = read_pdu(&mut client).unwrap();
        let cid = u16::from_le_bytes(resp.psh[12..14].try_into().unwrap());
        let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
        assert_eq!(cid, 0x0BBB);
        assert_eq!(sc, 0x01);
        h.join().unwrap().unwrap();
    }

    /// **V5a-5** — admin path 在 V5a 改动后仍跑通：Identify Controller 完整闭环。
    /// 防 io_queues 记账误伤 admin 路径。
    #[test]
    fn v5a_admin_path_still_green_after_io_queue_tracking() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            sess.pump_one()?; // Connect admin
            sess.pump_one()?; // Identify Controller
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_admin_sqe_cdw11(&mut client, 0x06, 0x00C1, 0, 0x0000_0001, 0);
        // 先收 C2HData
        let p = read_pdu(&mut client).unwrap();
        assert_eq!(p.header.pdu_type, pdu_type::C2H_DATA);
        // 再收 CapsuleResp success
        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(resp.header.pdu_type, pdu_type::RSP);
        let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
        assert_eq!(sc, 0);
        h.join().unwrap().unwrap();
    }

    // ─── Phase V5b — IO Read (nlb=1 / 4 KiB / C2HData 闭环) ──────────

    /// V5b helper: 用预填 pattern 起一个 controller（512 byte / sector，默认 LBADS=9）。
    fn make_test_controller_with_pattern(pattern: u8) -> (NvmeController, tempfile::NamedTempFile) {
        let f = tempfile::NamedTempFile::new().expect("create tempfile");
        f.as_file().set_len(1024 * 1024).expect("set_len");
        let buf = vec![pattern; 4096]; // 写 8 sector 的 pattern
        std::io::Write::write_all(
            &mut std::fs::OpenOptions::new()
                .write(true)
                .open(f.path())
                .unwrap(),
            &buf,
        )
        .unwrap();
        let path = f.path().to_str().expect("temp path utf8").to_string();
        let c = NvmeController::open(&[path], 0x1414, 0, &[]).expect("NvmeController::open");
        (c, f)
    }

    /// 发一条 IO Read SQE：opc=0x02, cid, nsid, cdw10/11 = SLBA, cdw12 = NLB-1
    fn send_io_read(client: &mut TcpStream, cid: u16, nsid: u32, slba: u64, nlb: u32) {
        let mut sqe = [0u8; 64];
        sqe[0] = 0x02;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        sqe[40..44].copy_from_slice(&((slba & 0xffff_ffff) as u32).to_le_bytes()); // cdw10
        sqe[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes()); // cdw11
        sqe[48..52].copy_from_slice(&(nlb - 1).to_le_bytes()); // cdw12 = NLB-1
        let hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(client, &hdr, &sqe, &[]).unwrap();
    }

    /// **V5b-1** — IO Read nlb=1 → C2HData(512B) + CapsuleResp success；
    /// 内容应 = backing file LBA 0 内容（0xAB pattern）。
    #[test]
    fn v5b_io_read_nlb1_emits_c2hdata_and_resp() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller_with_pattern(0xAB);
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            // setup 5 cmds: Connect admin + Create IO CQ + Create IO SQ +
            // Connect qid=1 + 1 IO Read
            for _ in 0..5 {
                sess.pump_one()?;
            }
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        let cdw10_cq = 1u32 | (15u32 << 16);
        send_admin_sqe_cdw11(&mut client, 0x05, 0x0501, 0, cdw10_cq, 0x0000_0001);
        let _ = read_pdu(&mut client).unwrap();
        let cdw10_sq = 1u32 | (15u32 << 16);
        send_admin_sqe_cdw11(&mut client, 0x01, 0x0502, 0, cdw10_sq, 0x0001_0001);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_io(&mut client, 1);
        let _ = read_pdu(&mut client).unwrap();

        // IO Read nsid=1 SLBA=0 nlb=1
        send_io_read(&mut client, 0x0700, 1, 0, 1);

        // 先收 C2HData(512 byte = 1 LBA at LBADS=9)
        let p = read_pdu(&mut client).unwrap();
        assert_eq!(p.header.pdu_type, pdu_type::C2H_DATA);
        assert_eq!(p.data.len(), 512);
        assert!(
            p.data.iter().all(|&b| b == 0xAB),
            "IO Read 返的 512B 必为 backing file pattern 0xAB"
        );

        // 再收 CapsuleResp success
        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(resp.header.pdu_type, pdu_type::RSP);
        let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
        assert_eq!(sc, 0, "IO Read 应 success");
        h.join().unwrap().unwrap();
    }

    /// **V5b-2** — IO Read nlb=2 → CapsuleResp SC=0x18，未发 C2HData。
    #[test]
    fn v5b_io_read_nlb2_rejected_with_sgl_data_length_invalid() {
        let (mut client, h, _backing) = v5a_full_setup_qid1(5);
        send_io_read(&mut client, 0x0801, 1, 0, 2);
        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(
            resp.header.pdu_type,
            pdu_type::RSP,
            "nlb>1 应直接 reject 不发 C2HData"
        );
        let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
        assert_eq!(sc, 0x18, "应回 SGL_DATA_LENGTH_INVALID");
        h.join().unwrap().unwrap();
    }

    /// **V5b-3** — IO Read 非法 NSID → controller 返 INVALID_NAMESPACE (0x0B)。
    #[test]
    fn v5b_io_read_invalid_nsid_returns_invalid_namespace() {
        let (mut client, h, _backing) = v5a_full_setup_qid1(5);
        send_io_read(&mut client, 0x0902, /*nsid=*/ 999, 0, 1);
        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(resp.header.pdu_type, pdu_type::RSP);
        let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
        assert_eq!(sc, 0x0B, "INVALID_NAMESPACE_OR_FORMAT");
        h.join().unwrap().unwrap();
    }

    /// **V5b-4** — IO Read 带 PSDT=01 SGL Transport-specific：session 清掉
    /// PSDT bits 让 controller 走 PRP path；host 视角与 PSDT=00 无差。
    #[test]
    fn v5b_io_read_psdt01_transparent_to_host() {
        let (mut client, server) = tcp_pair();
        let (controller, _backing) = make_test_controller_with_pattern(0xCD);
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            for _ in 0..5 {
                sess.pump_one()?;
            }
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        let cdw10_cq = 1u32 | (15u32 << 16);
        send_admin_sqe_cdw11(&mut client, 0x05, 0x0501, 0, cdw10_cq, 0x0000_0001);
        let _ = read_pdu(&mut client).unwrap();
        let cdw10_sq = 1u32 | (15u32 << 16);
        send_admin_sqe_cdw11(&mut client, 0x01, 0x0502, 0, cdw10_sq, 0x0001_0001);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_io(&mut client, 1);
        let _ = read_pdu(&mut client).unwrap();

        // IO Read with PSDT=01 in cdw0 bits 15:14
        let mut sqe = [0u8; 64];
        let cdw0: u32 = 0x02 | (0b01u32 << 14); // opc=READ, PSDT=01
        sqe[0..4].copy_from_slice(&cdw0.to_le_bytes());
        sqe[2..4].copy_from_slice(&0x0AA0u16.to_le_bytes()); // CID 覆盖
        sqe[4..8].copy_from_slice(&1u32.to_le_bytes()); // nsid
        sqe[48..52].copy_from_slice(&0u32.to_le_bytes()); // cdw12 NLB-1=0
        let hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(&mut client, &hdr, &sqe, &[]).unwrap();

        let p = read_pdu(&mut client).unwrap();
        assert_eq!(p.header.pdu_type, pdu_type::C2H_DATA);
        assert!(p.data.iter().all(|&b| b == 0xCD));
        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(resp.header.pdu_type, pdu_type::RSP);
        let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
        assert_eq!(sc, 0, "PSDT=01 host 视角应透明走 success");
        h.join().unwrap().unwrap();
    }

    // ─── Phase V5c — IO Write (nlb=1 / 512B / R2T+H2CData 闭环) ──────

    /// 发一条 IO Write SQE：opc=0x01, cid, nsid, cdw10/11 = SLBA, cdw12 = NLB-1
    fn send_io_write(client: &mut TcpStream, cid: u16, nsid: u32, slba: u64, nlb: u32) {
        let mut sqe = [0u8; 64];
        sqe[0] = 0x01;
        sqe[2..4].copy_from_slice(&cid.to_le_bytes());
        sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
        sqe[40..44].copy_from_slice(&((slba & 0xffff_ffff) as u32).to_le_bytes()); // cdw10
        sqe[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes()); // cdw11
        sqe[48..52].copy_from_slice(&(nlb - 1).to_le_bytes()); // cdw12
        let hdr = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        };
        write_pdu(client, &hdr, &sqe, &[]).unwrap();
    }

    /// **V5c-1** — IO Write nlb=1 完整闭环：server 发 R2T → client 回 H2CData
    /// → server post_cqe → CapsuleResp success。后置：reopen backing 验
    /// LBA 0 内容 = client pattern。
    #[test]
    fn v5c_io_write_nlb1_round_trip() {
        let (mut client, server) = tcp_pair();
        // **V5-P9 fix** — backing tempfile 必须保活到 reopen verify 结束；
        // 之前 `std::mem::forget(f)` 永久 leak。现在显式绑 `_backing` 让
        // 测试 fn 结束 drop 时 unlink 干净。controller 已持 fd 不受影响。
        let f = tempfile::NamedTempFile::new().expect("tempfile");
        f.as_file().set_len(1024 * 1024).expect("set_len");
        let backing_path = f.path().to_str().expect("utf8").to_string();
        let backing_path_for_verify = backing_path.clone();
        let _backing = f; // 显式声明 lifetime 持有到 fn 末
        let controller =
            NvmeController::open(&[backing_path], 0x1414, 0, &[]).expect("open controller");
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            for _ in 0..5 {
                sess.pump_one()?;
            }
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        let cdw10_cq = 1u32 | (15u32 << 16);
        send_admin_sqe_cdw11(&mut client, 0x05, 0x0501, 0, cdw10_cq, 0x0000_0001);
        let _ = read_pdu(&mut client).unwrap();
        let cdw10_sq = 1u32 | (15u32 << 16);
        send_admin_sqe_cdw11(&mut client, 0x01, 0x0502, 0, cdw10_sq, 0x0001_0001);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_io(&mut client, 1);
        let _ = read_pdu(&mut client).unwrap();

        // IO Write nsid=1 SLBA=0 nlb=1
        send_io_write(&mut client, 0x0C00, 1, 0, 1);

        // 期望先收 R2T(ttag, offset=0, length=512)
        let p = read_pdu(&mut client).unwrap();
        assert_eq!(p.header.pdu_type, pdu_type::R2T);
        let r: crate::pdu::R2tPsh = crate::pdu::decode_psh(&p.psh).unwrap();
        let ttag = r.ttag;
        let length = r.r2t_length;
        assert_eq!(length, 512, "1 LBA Write at LBADS=9 = 512 byte R2T");
        assert!(ttag != 0);

        // client 回 H2CData
        let pattern = vec![0xE5u8; 512];
        send_h2cdata_at(&mut client, 0x0C00, ttag, 0, &pattern);

        // 收 CapsuleResp success
        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(resp.header.pdu_type, pdu_type::RSP);
        let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
        assert_eq!(sc, 0, "IO Write 应 success");
        h.join().unwrap().unwrap();

        // 后置：reopen backing 验 LBA 0 = pattern
        let mut readback = vec![0u8; 512];
        use std::io::Read as _;
        let mut bf = std::fs::File::open(&backing_path_for_verify).unwrap();
        bf.read_exact(&mut readback).unwrap();
        assert!(
            readback.iter().all(|&b| b == 0xE5),
            "backing file LBA 0 应被 IO Write 改写为 0xE5"
        );
    }

    /// **V5c-2** — IO Write nlb=2 → reject SC=0x18，不发 R2T。
    #[test]
    fn v5c_io_write_nlb2_rejected() {
        let (mut client, h, _backing) = v5a_full_setup_qid1(5);
        send_io_write(&mut client, 0x0D11, 1, 0, 2);
        let resp = read_pdu(&mut client).unwrap();
        assert_eq!(
            resp.header.pdu_type,
            pdu_type::RSP,
            "nlb>1 应直接 reject 不发 R2T"
        );
        let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
        assert_eq!(sc, 0x18, "应回 SGL_DATA_LENGTH_INVALID");
        h.join().unwrap().unwrap();
    }

    /// **V5c-3** — Write→Read 持久化一致性：同一 sector 先 Write 0xC3 再 Read
    /// → C2HData 内容 = 0xC3。验数据持久化 + admin/IO 状态切换。
    #[test]
    fn v5c_io_write_then_read_roundtrip_data_integrity() {
        let (mut client, server) = tcp_pair();
        let f = tempfile::NamedTempFile::new().expect("tempfile");
        f.as_file().set_len(1024 * 1024).expect("set_len");
        let path = f.path().to_str().expect("utf8").to_string();
        let _backing = f; // **V5-P9 fix** — 保活到 fn 末，drop 时 unlink 干净
        let controller = NvmeController::open(&[path], 0x1414, 0, &[]).expect("open controller");
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut sess = V2Session::accept_and_handshake(server, controller)?;
            for _ in 0..6 {
                // 4 setup + Write + Read
                sess.pump_one()?;
            }
            Ok(())
        });
        send_icreq(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_admin(&mut client);
        let _ = read_pdu(&mut client).unwrap();
        let cdw10_cq = 1u32 | (15u32 << 16);
        send_admin_sqe_cdw11(&mut client, 0x05, 0x0501, 0, cdw10_cq, 0x0000_0001);
        let _ = read_pdu(&mut client).unwrap();
        let cdw10_sq = 1u32 | (15u32 << 16);
        send_admin_sqe_cdw11(&mut client, 0x01, 0x0502, 0, cdw10_sq, 0x0001_0001);
        let _ = read_pdu(&mut client).unwrap();
        send_connect_io(&mut client, 1);
        let _ = read_pdu(&mut client).unwrap();

        // Write 0xC3 pattern
        send_io_write(&mut client, 0x0E22, 1, 0, 1);
        let p = read_pdu(&mut client).unwrap();
        assert_eq!(p.header.pdu_type, pdu_type::R2T);
        let r: crate::pdu::R2tPsh = crate::pdu::decode_psh(&p.psh).unwrap();
        let ttag = r.ttag;
        send_h2cdata_at(&mut client, 0x0E22, ttag, 0, &vec![0xC3u8; 512]);
        let resp = read_pdu(&mut client).unwrap();
        let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
        assert_eq!(sc, 0);

        // Read 同 LBA
        send_io_read(&mut client, 0x0E33, 1, 0, 1);
        let p = read_pdu(&mut client).unwrap();
        assert_eq!(p.header.pdu_type, pdu_type::C2H_DATA);
        assert_eq!(p.data.len(), 512);
        assert!(
            p.data.iter().all(|&b| b == 0xC3),
            "Read 应拿到 Write 写入的 0xC3"
        );
        let resp = read_pdu(&mut client).unwrap();
        let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
        assert_eq!(sc, 0);

        h.join().unwrap().unwrap();
    }
}
