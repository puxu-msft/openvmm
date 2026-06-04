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
use crate::fabric::FabricError;
use crate::fabric::fabric_sc;
use crate::fabric::fctype;
use crate::fabric::property_offset;
use crate::framing::FramingError;
use crate::framing::Pdu;
use crate::framing::read_pdu;
use crate::framing::write_pdu;
use crate::pdu::CH_LEN;
use crate::pdu::CommonHdr;
use crate::pdu::DataPsh;
use crate::pdu::IcPsh;
use crate::pdu::TermPsh;
use crate::pdu::flags;
use crate::pdu::pdu_type;
use crate::pdu::term_fes;
use anyhow::Context as _;
use pcie_remote_nvme_userspace::NvmeController;
use std::net::TcpStream;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

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
        controller: NvmeController,
    ) -> anyhow::Result<Self> {
        let negotiated = ic_handshake(&mut stream).context("ICReq/ICResp handshake")?;
        Ok(Self {
            stream,
            controller,
            negotiated,
            cntlid: 1, // 单 controller 教学版固定 1
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
            // 非 fabric — 真 NVMe 命令；V3+ 实现 Identify / SET_FEATURES 等。
            tracing::warn!(opc, cid, "V2 不处理非 fabric NVMe cmd: opc={opc:#x}");
            self.send_capsule_resp_err(cid, 0x01) // INVALID_OPCODE
        }
    }

    fn handle_connect(&mut self, cid: u16, _sqe: &[u8], data: &[u8]) -> anyhow::Result<()> {
        if data.len() != fabric::CONNECT_DATA_SIZE {
            return self.send_capsule_resp_err(cid, fabric_sc::CONNECT_INVALID_PARAM);
        }
        let cd = match ConnectData::read_from_bytes(data) {
            Ok(c) => c,
            Err(_) => {
                return self.send_capsule_resp_err(cid, fabric_sc::CONNECT_INVALID_PARAM);
            }
        };
        let fields = match fabric::decode_connect_fields(_sqe) {
            Ok(f) => f,
            Err(_) => {
                return self.send_capsule_resp_err(cid, fabric_sc::CONNECT_INVALID_PARAM);
            }
        };
        let qid = fields.qid;
        let kato = fields.kato;
        let subnqn = cd.subnqn_str().to_string();
        let hostnqn = cd.hostnqn_str().to_string();
        tracing::info!(
            qid,
            kato,
            subnqn = subnqn.as_str(),
            hostnqn = hostnqn.as_str(),
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
        if !valid_property_offset(pf.ofst) {
            return self.send_capsule_resp_err(cid, 0x02);
        }
        let value = self.controller.mmio_read_impl(0, pf.ofst as u64, size);
        // 4B 路径 → CQE.result DW0 = value（低 32 位）；8B → V2 简化也只
        // 把低 32 位放 DW0；高 32 位 spec 用 DW1（CQE.result u32 字段之上
        // 的 reserved DW），V8 完善。
        self.send_capsule_resp_ok(cid, value as u32)
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
        if !valid_property_offset(pf.ofst) {
            return self.send_capsule_resp_err(cid, 0x02);
        }
        // 调 controller mmio_write — mmio_write_impl 需要 DeviceCtx；构
        // 一个本地 NoopTransport stub（CC.EN=1 等 reg 写入不会触发
        // dma/interrupt，等 driver 发 SQE 才动）。
        let mut t = pcie_vfio_user_sdk::NoopTransport;
        let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut t);
        self.controller
            .mmio_write_impl(&mut ctx, 0, pf.ofst as u64, size, pf.value);
        self.send_capsule_resp_ok(cid, 0)
    }

    /// 写一条 CapsuleResp CQE（16 byte）回 client。`status_sf` = SF 字段
    /// （bits 1..15 of status；phase bit 我们填 0 — NVMe-oF spec 不用 phase）。
    fn send_capsule_resp_ok(&mut self, cid: u16, result_dw0: u32) -> anyhow::Result<()> {
        let mut cqe = [0u8; 16];
        cqe[0..4].copy_from_slice(&result_dw0.to_le_bytes());
        // bytes 4..8 reserved；bytes 8..10 sq_head（V2 不真追踪 SQ head，填 0）；
        // bytes 10..12 sq_id（V2 admin queue = 0）；bytes 12..14 cmd id；
        // bytes 14..16 status = 0 (success)。
        cqe[12..14].copy_from_slice(&cid.to_le_bytes());
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
        // status word: bits 1..8 = SC, bit 0 = phase (= 0 NVMe-oF)
        let status: u16 = (sc as u16) << 1;
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

fn valid_property_offset(ofst: u32) -> bool {
    matches!(
        ofst,
        property_offset::CAP
            | property_offset::VS
            | property_offset::CC
            | property_offset::CSTS
            | property_offset::NSSR
    )
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

// 让 IcPsh / DataPsh / CH_LEN / FabricError 出现在测试外部 use 链，避免
// dead-code warning。
const _LINK: usize =
    CH_LEN + core::mem::size_of::<DataPsh>() + core::mem::size_of::<crate::pdu::R2tPsh>();

#[allow(dead_code)]
fn _link_fabric_error(_e: FabricError) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fabric::ConnectFabricFields;
    use crate::fabric::PropertyFabricFields;
    use crate::fabric::fctype;
    use crate::fabric::property_offset;
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
    fn make_test_controller() -> NvmeController {
        let path = std::env::temp_dir().join(format!(
            "nvme_oftcp_v2_{}_{:?}.img",
            std::process::id(),
            std::thread::current().id()
        ));
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(1024 * 1024).unwrap();
        drop(f);
        NvmeController::open(&[path.to_str().unwrap().to_string()], 0x1414, 0, &[]).unwrap()
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
        let controller = make_test_controller();
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
        let controller = make_test_controller();
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
}
