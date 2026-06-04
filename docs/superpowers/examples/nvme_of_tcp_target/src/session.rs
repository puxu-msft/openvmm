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
use crate::pdu::CommonHdr;
use crate::pdu::DataPsh;
use crate::pdu::IcPsh;
use crate::pdu::TermPsh;
use crate::pdu::flags;
use crate::pdu::pdu_type;
use crate::pdu::term_fes;
use crate::tcp_transport::TcpAdminTransport;
use anyhow::Context as _;
use pcie_remote_nvme_userspace::NvmeController;
use pcie_remote_nvme_userspace::cmd::Sqe;
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
            // **Phase V3** — 真 NVMe 命令（Identify / Get Log Page /
            // Set Features 等）：派发到 NvmeController.nvme_admin_dispatch。
            self.handle_admin_cmd(cid, sqe)
        }
    }

    /// **Phase V3** — 把 CapsuleCmd 里的 NVMe SQE 派发到 controller，
    /// captured 出来的 dma_write 转 NVMe-oF wire（C2HData PDU + CapsuleResp）。
    ///
    /// 策略（**review M2 / L1 / L2** 后统一）：
    /// 1. SQE.prp1 改写为 [`PRP1_SENTINEL`]，controller 把"数据写回 host
    ///    内存"的 dma_write 都打到该哨值 gpa，session 后续识别为 C2HData
    ///    payload；CQE bytes 走 [`CQ_BASE_GPA`] 上的 16B write。
    /// 2. 同步 vs 异步路径统一走 controller post_cqe：
    ///    - 同步：dispatch 返 `Some(cqe)` → 立刻调 `nvme_post_cqe(cqe)`，
    ///      让 controller 产 1 条 CQE dma_write 进 captured。
    ///    - 异步：dispatch 返 `None` → captured 仅含 data writes。逐 token
    ///      调 `nvme_admin_complete_dma(ok=true)`，controller 内部走 post_cqe
    ///      产 1 条 CQE write。
    ///
    ///    两条路径出口都保证 captured = [data writes...] + [1 条 CQE write]。
    /// 3. drain captured 严格按 "data 先 / CQE 后" 顺序拼 C2HData + CapsuleResp；
    ///    任何顺序/计数违例 → `anyhow::bail!`（review L1 / L2，宁可断也别截断 wire）。
    fn handle_admin_cmd(&mut self, cid: u16, sqe_bytes: &[u8]) -> anyhow::Result<()> {
        let mut sqe =
            Sqe::read_from_bytes(sqe_bytes).map_err(|_| anyhow::anyhow!("SQE 不是 64 byte"))?;
        // 用哨值替换 client 给的 prp1，让 controller dma_write 到我们能识别的 gpa
        sqe.prp1 = PRP1_SENTINEL;
        sqe.prp2 = 0;
        let opc = (sqe.cdw0 & 0xff) as u8;
        tracing::debug!(opc, cid, "V3 admin dispatch");

        let mut tcp_t = TcpAdminTransport::default();

        // ─── Phase 1：dispatch ─────────────────────────────────────────
        let immediate_cqe = {
            let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
            self.controller.nvme_admin_dispatch(&mut ctx, sqe, cid, 0)
        };

        // ─── Phase 2：统一两条路径，最终 captured 都含 1 条 CQE write ───
        if let Some(cqe) = immediate_cqe {
            // **review M2** — 同步路径也走 post_cqe，让 controller 内部
            // CQ tail/phase 推进逻辑生效；保持与异步路径单一出口。
            let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
            self.controller.nvme_post_cqe(&mut ctx, cqe);
        } else {
            // 异步路径：dispatch 阶段 captured 必只含 data writes。
            // 先快照 token 列表，再逐个投 ok=true completion。
            // **review L2** — controller 协议规定 data dma_write 先于
            // CQE，dispatch 阶段不应出 CQE write；若出，是 controller path
            // 出 bug，bail 比静默丢更安全。
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

        // ─── Phase 3：drain captured → data_payload + cqe_bytes ───────
        let mut data_payload = Vec::new();
        let mut cqe_bytes: Option<Vec<u8>> = None;
        while let Some(w) = tcp_t.pop_write() {
            if w.gpa >= CQ_BASE_GPA {
                if cqe_bytes.is_some() {
                    anyhow::bail!(
                        "V3 invariant violation: multiple CQE writes captured for single admin cmd"
                    );
                }
                if w.data.len() != 16 {
                    anyhow::bail!("captured CQE write len={} != 16", w.data.len());
                }
                cqe_bytes = Some(w.data);
            } else {
                // **review L1** — data write 必须先于 CQE write。反序意味
                // controller 路径出错；bail 而非 warn-drop，避免发出截断/错序的 wire。
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
            .ok_or_else(|| anyhow::anyhow!("V3: controller did not produce CQE for admin cmd"))?;

        // ─── Phase 4：emit C2HData (若有 data) + CapsuleResp ────────────
        if !data_payload.is_empty() {
            self.send_c2h_data(cid, &data_payload)?;
        }
        self.write_capsule_resp_bytes(&cqe_bytes)
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
        } else {
            // IO queue Connect — V8 真处理；V2 仅 ack 让 nvme-cli 初步发现
            // 后再 disconnect 不卡。
            if !self.admin_connected {
                return self.send_capsule_resp_err(cid, fabric_sc::CONNECT_INVALID_PARAM);
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
    let our_maxh2cdata: u32 = 64 * 1024;
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
}
