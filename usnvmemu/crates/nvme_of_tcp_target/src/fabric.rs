// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V2** — Fabric Command (NVMe opcode 0x7F)：Connect / Property
//! Get / Property Set + Connect Data 1024-byte struct。
//!
//! 参考：`docs/superpowers/specs/2026-06-04-nvme-tcp-wire-reference.md` § 7。
//!
//! Property Get/Set 用 NVMe BAR0 reg 偏移（CAP=0x00 / VS=0x08 / CC=0x14 /
//! CSTS=0x1C / NSSR=0x20），让 session 用 `controller.mmio_read(bar=0, ofst, size)`
//! / `mmio_write(...)` 直接复用 NvmeController 已实现的 BAR0 dispatch
//! （zero NVMe code change）。

use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// NVMe Fabric opcode：所有 fabric 命令的 SQE.opc 字段。
pub const NVME_OPC_FABRIC: u8 = 0x7F;

/// SQE 偏移 4 byte = fctype，区分 fabric 子命令。
pub mod fctype {
    /// Property Set。
    pub const PROPERTY_SET: u8 = 0x00;
    /// Connect — 建队列 + 绑 NQN/HOSTID + 分配 CNTLID。
    pub const CONNECT: u8 = 0x01;
    /// Property Get。
    pub const PROPERTY_GET: u8 = 0x04;
    /// Auth Send（V-followup）。
    pub const AUTH_SEND: u8 = 0x05;
    /// Auth Receive（V-followup）。
    pub const AUTH_RECV: u8 = 0x06;
    /// Disconnect — V8。
    pub const DISCONNECT: u8 = 0x08;
}

/// Connect Data 大小：1024 byte（修正：之前 plan 写的 1792 是错的）。
pub const CONNECT_DATA_SIZE: usize = 1024;

/// **review L3** — 教学版单 controller 固定 CNTLID。V8 多 controller 时
/// 改为 `next_cntlid: AtomicU16` 分配器。
pub const TEACHING_CNTLID: u16 = 1;

/// 1024-byte Connect Data 块。
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
pub struct ConnectData {
    /// HostID UUID raw bytes。
    pub hostid: [u8; 16],
    /// CNTLID：0xFFFF 动态、0xFFFE static-any、其它指定。
    pub cntlid: u16,
    /// 保留 238 byte。
    pub rsvd1: [u8; 238],
    /// Subsystem NQN，ASCII zero-padded（无 NUL 终止）。
    pub subnqn: [u8; 256],
    /// Host NQN，ASCII zero-padded。
    pub hostnqn: [u8; 256],
    /// 保留 256 byte。
    pub rsvd2: [u8; 256],
}

impl Default for ConnectData {
    fn default() -> Self {
        Self {
            hostid: [0u8; 16],
            cntlid: 0,
            rsvd1: [0u8; 238],
            subnqn: [0u8; 256],
            hostnqn: [0u8; 256],
            rsvd2: [0u8; 256],
        }
    }
}

impl ConnectData {
    /// 取 subsystem NQN 字符串（trim trailing 0/space）。
    pub fn subnqn_str(&self) -> &str {
        nqn_to_str(&self.subnqn)
    }
    /// 取 host NQN 字符串（trim trailing 0/space）。
    pub fn hostnqn_str(&self) -> &str {
        nqn_to_str(&self.hostnqn)
    }
}

fn nqn_to_str(b: &[u8; 256]) -> &str {
    let end = b
        .iter()
        .position(|&c| c == 0 || c == b' ')
        .unwrap_or(b.len());
    core::str::from_utf8(&b[..end]).unwrap_or("")
}

/// Connect SQE 的 fabric-specific 部分（SQE byte 40..64 = 24 byte）。
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
pub struct ConnectFabricFields {
    /// Record Format；must = 0。
    pub recfmt: u16,
    /// Queue ID（0 = admin）。
    pub qid: u16,
    /// Submission Queue Size，0-based。
    pub sqsize: u16,
    /// Connect attributes（bit 0 = priority class）。
    pub cattr: u8,
    /// 保留。
    pub rsvd1: u8,
    /// Keep-Alive Timeout (ms)，admin queue only。
    pub kato: u32,
    /// 保留 12 byte。
    pub rsvd2: [u8; 12],
}

/// Property Get/Set SQE 的 fabric-specific 部分（SQE byte 40..64 = 24 byte）。
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
pub struct PropertyFabricFields {
    /// 大小位（bits 0..2，0 = 4B，1 = 8B）。
    pub attrib: u8,
    /// 保留。
    pub rsvd1: [u8; 3],
    /// controller property offset (CAP/VS/CC/CSTS/NSSR)。
    pub ofst: u32,
    /// Set 时写值，Get 时不用（0）。
    pub value: u64,
    /// 保留 8 byte。
    pub rsvd2: [u8; 8],
}

/// **Phase V8d** — Disconnect SQE 的 fabric-specific 部分（SQE byte 40..64 = 24 byte）。
///
/// Spec § 3.5 Figure 26 layout：byte 40..42 = RECFMT（must = 0），byte 42..64 = 保留 22B。
/// 比 Connect 简单很多 — Disconnect 拆整条 association，不传 qid（spec 强制
/// 走 admin queue，且 controller 端拆所有该 association 的 IO queue）。
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
pub struct DisconnectFabricFields {
    /// Record Format；must = 0。
    pub recfmt: u16,
    /// 保留 22 byte。
    pub rsvd: [u8; 22],
}

/// **Phase V8d** — 解 Disconnect SQE 24-byte fabric section。校验 `recfmt = 0`
/// 否则返 `Err` 让 caller 回 SC=0x80 INVALID_CONNECT_FORMAT（spec § 3.5 status）。
pub fn decode_disconnect_fields(sqe_bytes: &[u8]) -> Result<DisconnectFabricFields, FabricError> {
    if sqe_bytes.len() != 64 {
        return Err(FabricError::SqeLen {
            got: sqe_bytes.len(),
        });
    }
    let fields = DisconnectFabricFields::read_from_bytes(&sqe_bytes[40..64])
        .map_err(|_| FabricError::MalformedDisconnect)?;
    // spec § 3.5：RECFMT 必须 = 0，否则 SC=0x80 INVALID_CONNECT_FORMAT
    let recfmt = fields.recfmt;
    if recfmt != 0 {
        return Err(FabricError::InvalidDisconnectRecfmt(recfmt));
    }
    Ok(fields)
}

/// NVMe controller property offset（与 BAR0 reg offset 一致）。
pub mod property_offset {
    /// Controller Capabilities (8 byte)。
    pub const CAP: u32 = 0x00;
    /// Version (4 byte)。
    pub const VS: u32 = 0x08;
    /// Controller Configuration (4 byte，RW)。
    pub const CC: u32 = 0x14;
    /// Controller Status (4 byte)。
    pub const CSTS: u32 = 0x1C;
    /// NVM Subsystem Reset (4 byte, WO)。
    pub const NSSR: u32 = 0x20;
}

/// Fabric command 解析错误。
#[derive(Debug, Error)]
pub enum FabricError {
    /// SQE 长度不对（必须 64）。
    #[error("SQE length wrong: got {got}, want 64")]
    SqeLen {
        /// 实际 byte 数。
        got: usize,
    },
    /// opcode 非 0x7F。
    #[error("not a fabric command: opcode={0:#x}, want 0x7F")]
    NotFabric(u8),
    /// fctype 未知。
    #[error("unknown fctype {0:#x}")]
    UnknownFctype(u8),
    /// Connect Data 长度不对（必须 1024）。
    #[error("Connect Data length wrong: got {got}, want 1024")]
    ConnectDataLen {
        /// 实际 byte 数。
        got: usize,
    },
    /// Property attrib 不支持的 size（仅 0=4B / 1=8B 合法）。
    #[error("Property attrib size {0} unsupported (only 0=4B, 1=8B)")]
    InvalidPropertySize(u8),
    /// **V8d** — Disconnect 的 RECFMT 字段不为 0（spec § 3.5 要求 = 0）。
    #[error("Disconnect RECFMT {0:#x} unsupported (must be 0)")]
    InvalidDisconnectRecfmt(u16),
    /// **V8d reviewer H-2** — Disconnect SQE byte slice 24B 解码失败（理论
    /// 不该出现，因为长度已校验；保留专属变体让日志不漂到"unknown fctype"）。
    #[error("Disconnect fabric section decode failed")]
    MalformedDisconnect,
}

/// 抽 SQE byte 4 = fctype。
pub fn sqe_fctype(sqe: &[u8]) -> Result<u8, FabricError> {
    if sqe.len() != 64 {
        return Err(FabricError::SqeLen { got: sqe.len() });
    }
    if sqe[0] != NVME_OPC_FABRIC {
        return Err(FabricError::NotFabric(sqe[0]));
    }
    Ok(sqe[4])
}

/// 从 SQE byte 40..64 解出 [`ConnectFabricFields`]。
pub fn decode_connect_fields(sqe: &[u8]) -> Result<ConnectFabricFields, FabricError> {
    if sqe.len() != 64 {
        return Err(FabricError::SqeLen { got: sqe.len() });
    }
    ConnectFabricFields::read_from_bytes(&sqe[40..64])
        .map_err(|_| FabricError::SqeLen { got: sqe.len() })
}

/// 从 SQE byte 40..64 解出 [`PropertyFabricFields`]。
pub fn decode_property_fields(sqe: &[u8]) -> Result<PropertyFabricFields, FabricError> {
    if sqe.len() != 64 {
        return Err(FabricError::SqeLen { got: sqe.len() });
    }
    PropertyFabricFields::read_from_bytes(&sqe[40..64])
        .map_err(|_| FabricError::SqeLen { got: sqe.len() })
}

/// Property attrib.size 取 byte 数：0 → 4，1 → 8；其它返 Err。
pub fn property_size_bytes(attrib: u8) -> Result<u32, FabricError> {
    match attrib & 0x7 {
        0 => Ok(4),
        1 => Ok(8),
        s => Err(FabricError::InvalidPropertySize(s)),
    }
}

// ─── Fabric SC（Connect 失败 CQE 状态码）──────────────────────────────

/// Common fabric SC（CQE.status bits 1..8）。
pub mod fabric_sc {
    /// 不兼容的 record/data 格式。
    pub const INCOMPATIBLE_FORMAT: u8 = 0x80;
    /// Controller 正忙。
    pub const CONTROLLER_BUSY: u8 = 0x81;
    /// Connect 参数非法。
    pub const CONNECT_INVALID_PARAM: u8 = 0x82;
    /// 客户端应重启 discovery。
    pub const RESTART_DISCOVERY: u8 = 0x83;
    /// 主机未授权（hostnqn 不在白名单）。
    pub const CONNECT_INVALID_HOST: u8 = 0x84;
    /// Log 应重启。
    pub const LOG_RESTART_DISCOVERY: u8 = 0x90;
    /// 需要认证（V-followup）。
    pub const AUTH_REQUIRED: u8 = 0x91;
}

// **承重前提锚定** — IATTR.bit0=1（data）⟹ 下方 IPO 相对 Connect data payload 起点；
// 这里依赖 `ConnectData` struct 首字节（`hostid`）即 data payload byte 0，故 `offset_of!`
// 即 data-relative 偏移。const-assert 锚死该前提，防字段重排破坏语义。
const _: () = assert!(core::mem::offset_of!(ConnectData, hostid) == 0);

/// **static controller model（2026-06-14）** — Connect 因非法 CNTLID 被拒时填入
/// CapsuleResp CQE result DW0 的值（spec § 3.3 Connect response / wire-reference
/// 「失败：DW0 bits0..15 = IPO（dword 单位），bits16..23 = IATTR（bit0：0=command
/// 1=data）」）。
///
/// CNTLID 在 Connect **data** payload offset 16（dword 4）；IATTR.bit0=1（在 data）→
/// `(4) | (1<<16)` = `0x0001_0004`。仅在 SCT=0x01（Command Specific Status）下有意义
/// ——见 [`crate::async_session`] / [`crate::session`] 的 err helper（fabrics SC 用 SCT=0x01）。
pub const CONNECT_CNTLID_INVALID_RESULT_DW0: u32 =
    (core::mem::offset_of!(ConnectData, cntlid) as u32 / 4) // IPO（dword）= 16/4 = 4
        | (1u32 << 16); // IATTR bit0 = data → 0x0001_0004

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_match_spec() {
        assert_eq!(core::mem::size_of::<ConnectData>(), CONNECT_DATA_SIZE);
        assert_eq!(core::mem::size_of::<ConnectFabricFields>(), 24);
        assert_eq!(core::mem::size_of::<PropertyFabricFields>(), 24);
    }

    #[test]
    fn connect_data_field_offsets() {
        let c = ConnectData {
            hostid: [0xAA; 16],
            cntlid: 0x1234,
            rsvd1: [0u8; 238],
            subnqn: [b's'; 256],
            hostnqn: [b'h'; 256],
            rsvd2: [0u8; 256],
        };
        let bytes = c.as_bytes();
        assert_eq!(bytes[0], 0xAA);
        assert_eq!(bytes[15], 0xAA);
        assert_eq!(&bytes[16..18], &[0x34, 0x12]); // cntlid LE
        assert_eq!(bytes[256], b's');
        assert_eq!(bytes[256 + 255], b's');
        assert_eq!(bytes[512], b'h');
    }

    /// NQN string trim trailing zeros + spaces。
    #[test]
    fn nqn_str_trims_trailing_zeros() {
        let mut c = ConnectData::default();
        let nqn = b"nqn.2026-06.io.openhcl:nvme.userspace";
        c.subnqn[..nqn.len()].copy_from_slice(nqn);
        let s = c.subnqn_str();
        assert_eq!(s, "nqn.2026-06.io.openhcl:nvme.userspace");
    }

    #[test]
    fn sqe_fctype_paths() {
        let mut sqe = [0u8; 64];
        sqe[0] = NVME_OPC_FABRIC;
        sqe[4] = fctype::CONNECT;
        assert_eq!(sqe_fctype(&sqe).unwrap(), fctype::CONNECT);

        sqe[0] = 0x06; // Identify
        let r = sqe_fctype(&sqe);
        assert!(matches!(r, Err(FabricError::NotFabric(0x06))));

        let short = [0u8; 32];
        let r = sqe_fctype(&short);
        assert!(matches!(r, Err(FabricError::SqeLen { got: 32 })));
    }

    #[test]
    fn decode_connect_fields_roundtrip() {
        let mut sqe = [0u8; 64];
        sqe[0] = NVME_OPC_FABRIC;
        sqe[4] = fctype::CONNECT;
        let f = ConnectFabricFields {
            recfmt: 0,
            qid: 3,
            sqsize: 31,
            cattr: 0,
            rsvd1: 0,
            kato: 60000,
            rsvd2: [0u8; 12],
        };
        sqe[40..64].copy_from_slice(f.as_bytes());
        let got = decode_connect_fields(&sqe).unwrap();
        let qid = got.qid;
        let sqsize = got.sqsize;
        let kato = got.kato;
        assert_eq!(qid, 3);
        assert_eq!(sqsize, 31);
        assert_eq!(kato, 60000);
    }

    #[test]
    fn decode_property_fields_roundtrip() {
        let mut sqe = [0u8; 64];
        sqe[0] = NVME_OPC_FABRIC;
        sqe[4] = fctype::PROPERTY_SET;
        let f = PropertyFabricFields {
            attrib: 1, // 8B
            rsvd1: [0u8; 3],
            ofst: property_offset::CC,
            value: 0x460001, // CC.EN=1 + IOSQES/IOCQES default
            rsvd2: [0u8; 8],
        };
        sqe[40..64].copy_from_slice(f.as_bytes());
        let got = decode_property_fields(&sqe).unwrap();
        let attrib = got.attrib;
        let ofst = got.ofst;
        let value = got.value;
        assert_eq!(attrib, 1);
        assert_eq!(ofst, property_offset::CC);
        assert_eq!(value, 0x460001);
    }

    #[test]
    fn property_size_bytes_paths() {
        assert_eq!(property_size_bytes(0).unwrap(), 4);
        assert_eq!(property_size_bytes(1).unwrap(), 8);
        assert!(matches!(
            property_size_bytes(2),
            Err(FabricError::InvalidPropertySize(2))
        ));
        // attrib 高位被忽略
        assert_eq!(property_size_bytes(0b1111_1000).unwrap(), 4);
    }
}
