// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V1** — NVMe TCP PDU 数据结构 + 编解码 helper。
//!
//! 完整 wire 参考：`docs/superpowers/specs/2026-06-04-nvme-tcp-wire-reference.md`。
//!
//! 所有 on-wire struct 用 `#[repr(C, packed)]` + zerocopy `FromBytes/IntoBytes`；
//! 字段都是 LE on x86_64。BE 主机本 crate compile error（与
//! pcie_vfio_user_sdk 一致）。

use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[cfg(target_endian = "big")]
compile_error!(
    "nvme_of_tcp_target 假设 host-endian = little-endian；NVMe TCP wire 是 LE，BE 主机请加 byte-swap 路径"
);

/// CommonHdr 长度。
pub const CH_LEN: usize = 8;

// ─── PDU type 常量 ──────────────────────────────────────────────────────

/// PDU 类型 byte 取值（NVMe TCP Transport Spec § 3.4）。
pub mod pdu_type {
    /// Initialize Connection Request。
    pub const ICREQ: u8 = 0x00;
    /// Initialize Connection Response。
    pub const ICRESP: u8 = 0x01;
    /// Host → Controller Terminate Request。
    pub const H2C_TERM: u8 = 0x02;
    /// Controller → Host Terminate Request。
    pub const C2H_TERM: u8 = 0x03;
    /// Capsule Command (SQE)。
    pub const CMD: u8 = 0x04;
    /// Capsule Response (CQE)。
    pub const RSP: u8 = 0x05;
    /// Host → Controller Data。
    pub const H2C_DATA: u8 = 0x06;
    /// Controller → Host Data。
    pub const C2H_DATA: u8 = 0x07;
    /// Ready-to-Transfer。
    pub const R2T: u8 = 0x09;
}

// ─── flags 位 ────────────────────────────────────────────────────────────

/// PDU.flags 各 bit。
pub mod flags {
    /// Header digest present (CRC32C 4 byte 跟在 PSH 后)。
    pub const HDGST: u8 = 1 << 0;
    /// Data digest present (CRC32C 4 byte 在 PDU 尾)。
    pub const DDGST: u8 = 1 << 1;
    /// Last data PDU of transfer (Data PDU only)。
    pub const DATA_LAST: u8 = 1 << 2;
    /// C2HData 兼当 CQE — controller 不再发 CapsuleResp。
    pub const DATA_SUCCESS: u8 = 1 << 3;
}

// ─── CommonHdr ──────────────────────────────────────────────────────────

/// 8-byte 通用 PDU header。
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
pub struct CommonHdr {
    /// 见 [`pdu_type`]。
    pub pdu_type: u8,
    /// 见 [`flags`]。
    pub flags: u8,
    /// CH+PSH 长度（**不含** HDGST trailer）。
    pub hlen: u8,
    /// 数据起始 byte 偏移（从 PDU 起算）。
    pub pdo: u8,
    /// 总 PDU 字节数（含 CH+PSH+HDGST+pad+data+DDGST）。
    pub plen: u32,
}

impl CommonHdr {
    /// 校验 plen ≥ hlen + (HDGST 4 byte if set)。
    pub fn has_hdgst(&self) -> bool {
        self.flags & flags::HDGST != 0
    }
    /// 是否有 data digest。
    pub fn has_ddgst(&self) -> bool {
        self.flags & flags::DDGST != 0
    }
    /// 是否标 last data PDU。
    pub fn is_data_last(&self) -> bool {
        self.flags & flags::DATA_LAST != 0
    }
    /// 是否标 C2HData 兼 CQE。
    pub fn is_data_success(&self) -> bool {
        self.flags & flags::DATA_SUCCESS != 0
    }
}

// ─── 各 PDU PSH struct ──────────────────────────────────────────────────

/// ICReq / ICResp 共用 PSH（120 byte，配合 8 byte CH 共 128 byte）。
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
pub struct IcPsh {
    /// PDU Format Version。当前 = 0。
    pub pfv: u16,
    /// host/controller PDU data alignment（0-based dword count，max 31）。
    pub hpda_or_cpda: u8,
    /// digest 启用位（bit0 HDGST，bit1 DDGST，与 [`flags`] 一致）。
    pub digest: u8,
    /// ICReq: maxr2t（host 最大未决 R2T 数 - 1）；ICResp: maxh2cdata（H2C 单 PDU 最大 byte）。
    pub maxr2t_or_maxh2cdata: u32,
    /// 保留字节。
    pub rsvd: [u8; 112],
}

/// Default 给 ICReq/ICResp 0 填充用，本 struct 必须支持。
impl Default for IcPsh {
    fn default() -> Self {
        Self {
            pfv: 0,
            hpda_or_cpda: 0,
            digest: 0,
            maxr2t_or_maxh2cdata: 0,
            rsvd: [0u8; 112],
        }
    }
}

/// TermReq 共用 PSH。
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
pub struct TermPsh {
    /// Fatal Error Status。
    pub fes: u16,
    /// Fatal Error Info。
    pub fei: [u8; 4],
    /// 保留。
    pub rsvd: [u8; 10],
}

/// Fatal Error Status (TermReq.fes) 取值。
pub mod term_fes {
    /// 无效 PDU header。
    pub const INVALID_PDU_HDR: u16 = 0x01;
    /// PDU 顺序错。
    pub const PDU_SEQ_ERR: u16 = 0x02;
    /// HDGST 校验失败。
    pub const HDR_DIGEST_ERR: u16 = 0x03;
    /// Data 超 R2T 范围。
    pub const DATA_OUT_OF_RANGE: u16 = 0x04;
    /// R2T 数 / data PDU 数超限。
    pub const R2T_LIMIT_EXCEEDED: u16 = 0x05;
    /// 不支持的参数。
    pub const UNSUPPORTED_PARAM: u16 = 0x06;
}

/// R2T PSH (16 byte，配 8 B CH 共 24 B)。
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
pub struct R2tPsh {
    /// 对应 H2C cmd 的 command_id。
    pub cccid: u16,
    /// controller 选的 transfer tag（host 之后 H2CData 必须 echo）。
    pub ttag: u16,
    /// cmd buffer 内 byte offset。
    pub r2t_offset: u32,
    /// 准许 host 现在发的字节数。
    pub r2t_length: u32,
    /// 保留。
    pub rsvd: [u8; 4],
}

/// H2CData / C2HData 共用 PSH（16 byte，配 8 B CH 共 24 B）。
///
/// `ttag_or_rsvd`：H2CData 必须 = R2T.ttag；C2HData 时此字段保留 0。
#[derive(Debug, Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
pub struct DataPsh {
    /// NVMe command_id。
    pub cccid: u16,
    /// H2CData = ttag (回 R2T)；C2HData = rsvd。
    pub ttag_or_rsvd: u16,
    /// 数据在 cmd buffer 内 byte offset。
    pub data_offset: u32,
    /// 本 PDU 携带的字节数。
    pub data_length: u32,
    /// 保留。
    pub rsvd: [u8; 4],
}

// ─── 错误类型 ──────────────────────────────────────────────────────────

/// PDU 编解码错误。
#[derive(Debug, Error)]
pub enum PduError {
    /// 短包：buf.len() < CH_LEN。
    #[error("buffer too short for CommonHdr: got {got}, need {CH_LEN}")]
    BufferShort {
        /// 实际可用字节数。
        got: usize,
    },
    /// plen 字段 < hlen — 非法 PDU。
    #[error("plen ({plen}) < hlen ({hlen})")]
    PlenLessThanHlen {
        /// 报告的 plen。
        plen: u32,
        /// 报告的 hlen。
        hlen: u8,
    },
    /// plen 超 cap（DoS 防护）。
    #[error("plen ({plen}) > MAX_PDU_SIZE ({MAX_PDU_SIZE})")]
    PduTooLarge {
        /// 报告的 plen。
        plen: u32,
    },
    /// PDU type 未知。
    #[error("unknown PDU type {0:#x}")]
    UnknownPduType(u8),
    /// payload 长度与期望 struct 大小不匹配。
    #[error("PSH length mismatch: got {got}, want {want}")]
    PshLen {
        /// 实际字节数。
        got: usize,
        /// 期望字节数。
        want: usize,
    },
    /// HDGST CRC 校验失败。
    #[error("HDGST CRC32C mismatch")]
    HdgstMismatch,
    /// DDGST CRC 校验失败。
    #[error("DDGST CRC32C mismatch")]
    DdgstMismatch,
}

/// 单条 PDU 最大字节数：1 MiB。NVMe TCP spec 没硬上限，但实际 maxh2cdata
/// 通常 ≤ 64 KiB；留 16× headroom，超出视 hostile peer。
pub const MAX_PDU_SIZE: usize = 1024 * 1024;

// ─── 编解码 helper ──────────────────────────────────────────────────────

/// 从 `buf` 头部解 [`CommonHdr`]；caller 再按 `pdu_type + hlen` 决定怎么继续解 PSH。
///
/// 严格校验：
/// 1. `buf.len() >= CH_LEN`
/// 2. `plen >= hlen` 且 `plen <= MAX_PDU_SIZE`
/// 3. PDU type ∈ 已定义值
pub fn decode_common_hdr(buf: &[u8]) -> Result<CommonHdr, PduError> {
    if buf.len() < CH_LEN {
        return Err(PduError::BufferShort { got: buf.len() });
    }
    let hdr = CommonHdr::read_from_bytes(&buf[..CH_LEN])
        .map_err(|_| PduError::BufferShort { got: buf.len() })?;
    let plen = hdr.plen;
    let hlen = hdr.hlen;
    if (plen as usize) < hlen as usize {
        return Err(PduError::PlenLessThanHlen { plen, hlen });
    }
    if (plen as usize) > MAX_PDU_SIZE {
        return Err(PduError::PduTooLarge { plen });
    }
    let t = hdr.pdu_type;
    if !matches!(
        t,
        pdu_type::ICREQ
            | pdu_type::ICRESP
            | pdu_type::H2C_TERM
            | pdu_type::C2H_TERM
            | pdu_type::CMD
            | pdu_type::RSP
            | pdu_type::H2C_DATA
            | pdu_type::C2H_DATA
            | pdu_type::R2T
    ) {
        return Err(PduError::UnknownPduType(t));
    }
    Ok(hdr)
}

/// 严格按精确长度解一个 packed PSH 结构。
pub fn decode_psh<T: FromBytes + KnownLayout + Immutable + Copy>(
    psh: &[u8],
) -> Result<T, PduError> {
    let want = core::mem::size_of::<T>();
    if psh.len() != want {
        return Err(PduError::PshLen {
            got: psh.len(),
            want,
        });
    }
    T::read_from_bytes(psh).map_err(|_| PduError::PshLen {
        got: psh.len(),
        want,
    })
}

/// 把 CommonHdr + PSH 字节 + 可选 data 拼成完整 wire bytes，附 HDGST/DDGST
/// CRC32C 视 `hdr.flags` 而定。pad 区为 0 填充。
///
/// **重要**：caller 必须已经 *正确填好* `hdr.hlen`、`hdr.pdo`、`hdr.plen`，
/// 这是 spec invariant；本函数仅做计算 / digest 写入。
pub fn encode_pdu(hdr: &CommonHdr, psh: &[u8], data: &[u8], out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(hdr.as_bytes());
    out.extend_from_slice(psh);
    if hdr.has_hdgst() {
        let hdgst = crate::digest::crc32c_le_bytes(&out[start..start + hdr.hlen as usize]);
        out.extend_from_slice(&hdgst);
    }
    // pad 到 pdo（若 pdo > 当前已写字节数）。仅 Data / Cmd PDU 有 data；
    // 其它 PDU pdo 通常 = 0 或等于 hlen [+4]。
    let written = out.len() - start;
    if hdr.pdo as usize > written {
        let pad = hdr.pdo as usize - written;
        out.extend(std::iter::repeat_n(0u8, pad));
    }
    out.extend_from_slice(data);
    if hdr.has_ddgst() {
        let ddgst = crate::digest::crc32c_le_bytes(data);
        out.extend_from_slice(&ddgst);
    }
}

// ─── 单测 ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_hdr_size_is_8() {
        assert_eq!(core::mem::size_of::<CommonHdr>(), CH_LEN);
    }

    #[test]
    fn psh_sizes() {
        assert_eq!(core::mem::size_of::<IcPsh>(), 120); // 2+1+1+4+112
        assert_eq!(core::mem::size_of::<TermPsh>(), 16); // 2+4+10
        assert_eq!(core::mem::size_of::<R2tPsh>(), 16); // 2+2+4+4+4
        assert_eq!(core::mem::size_of::<DataPsh>(), 16);
    }

    /// CommonHdr LE 字节序（spec §3.4）。
    #[test]
    fn common_hdr_le_layout() {
        let h = CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: flags::HDGST,
            hlen: 72,
            pdo: 76,
            plen: 0x12345678,
        };
        let bytes = h.as_bytes();
        assert_eq!(bytes[0], 0x04);
        assert_eq!(bytes[1], 0x01);
        assert_eq!(bytes[2], 72);
        assert_eq!(bytes[3], 76);
        assert_eq!(&bytes[4..8], &[0x78, 0x56, 0x34, 0x12]); // LE
    }

    /// flags accessor 全套语义。
    #[test]
    fn flags_helpers() {
        let h = CommonHdr {
            flags: flags::HDGST | flags::DDGST | flags::DATA_LAST | flags::DATA_SUCCESS,
            ..Default::default()
        };
        assert!(h.has_hdgst());
        assert!(h.has_ddgst());
        assert!(h.is_data_last());
        assert!(h.is_data_success());
        let h0 = CommonHdr::default();
        assert!(!h0.has_hdgst());
        assert!(!h0.has_ddgst());
        assert!(!h0.is_data_last());
        assert!(!h0.is_data_success());
    }

    /// decode_common_hdr：合法 + 全部 error 路径。
    #[test]
    fn decode_common_hdr_paths() {
        let good = CommonHdr {
            pdu_type: pdu_type::ICREQ,
            flags: 0,
            hlen: 128,
            pdo: 0,
            plen: 128,
        };
        let bytes = good.as_bytes().to_vec();
        let parsed = decode_common_hdr(&bytes).unwrap();
        let plen = parsed.plen;
        assert_eq!(plen, 128);

        // 短包
        assert!(matches!(
            decode_common_hdr(&[0u8; 4]),
            Err(PduError::BufferShort { got: 4 })
        ));

        // plen < hlen
        let bad = CommonHdr {
            pdu_type: pdu_type::ICREQ,
            flags: 0,
            hlen: 128,
            pdo: 0,
            plen: 100,
        };
        let r = decode_common_hdr(bad.as_bytes());
        assert!(matches!(r, Err(PduError::PlenLessThanHlen { .. })));

        // plen > MAX
        let too_big = CommonHdr {
            pdu_type: pdu_type::ICREQ,
            flags: 0,
            hlen: 128,
            pdo: 0,
            plen: (MAX_PDU_SIZE as u32) + 1,
        };
        let r = decode_common_hdr(too_big.as_bytes());
        assert!(matches!(r, Err(PduError::PduTooLarge { .. })));

        // 未知 type
        let bad_t = CommonHdr {
            pdu_type: 0xFE,
            flags: 0,
            hlen: 8,
            pdo: 0,
            plen: 8,
        };
        let r = decode_common_hdr(bad_t.as_bytes());
        assert!(matches!(r, Err(PduError::UnknownPduType(0xFE))));
    }

    /// encode/decode ICReq 完整一帧（无 digest），byte-exact roundtrip。
    #[test]
    fn encode_decode_icreq_no_digest() {
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
        let mut buf = Vec::new();
        encode_pdu(&hdr, psh.as_bytes(), &[], &mut buf);
        assert_eq!(buf.len(), 128);
        let parsed = decode_common_hdr(&buf).unwrap();
        let plen = parsed.plen;
        let hlen = parsed.hlen;
        assert_eq!(plen, 128);
        assert_eq!(hlen, 128);
        let p: IcPsh = decode_psh(&buf[CH_LEN..CH_LEN + 120]).unwrap();
        let m = p.maxr2t_or_maxh2cdata;
        assert_eq!(m, 7);
    }

    /// HDGST present：encode 后 byte[hlen..hlen+4] 是 CRC32C(byte[..hlen])。
    #[test]
    fn encode_with_hdgst_writes_correct_crc() {
        let hdr = CommonHdr {
            pdu_type: pdu_type::ICREQ,
            flags: flags::HDGST,
            hlen: 128,
            pdo: 0,
            plen: 128 + 4,
        };
        let psh = IcPsh::default();
        let mut buf = Vec::new();
        encode_pdu(&hdr, psh.as_bytes(), &[], &mut buf);
        assert_eq!(buf.len(), 132);
        let expected = crate::digest::crc32c_le_bytes(&buf[..128]);
        assert_eq!(&buf[128..132], &expected);
    }

    /// DDGST present + data：encode 后末 4B = CRC32C(data)。
    #[test]
    fn encode_with_ddgst_writes_correct_crc() {
        let data = b"hello world";
        let hdr = CommonHdr {
            pdu_type: pdu_type::C2H_DATA,
            flags: flags::DDGST | flags::DATA_LAST,
            hlen: 24,
            pdo: 24,
            plen: 24 + data.len() as u32 + 4,
        };
        let psh = DataPsh::default();
        let mut buf = Vec::new();
        encode_pdu(&hdr, psh.as_bytes(), data, &mut buf);
        assert_eq!(buf.len(), 24 + data.len() + 4);
        let tail = &buf[buf.len() - 4..];
        let expected = crate::digest::crc32c_le_bytes(data);
        assert_eq!(tail, &expected);
    }

    /// pdo > hlen+HDGST → pad 字节为 0。
    #[test]
    fn encode_pads_to_pdo() {
        // 例：hlen=24, HDGST off, pdo=32（强制 8-byte align）。
        let hdr = CommonHdr {
            pdu_type: pdu_type::C2H_DATA,
            flags: 0,
            hlen: 24,
            pdo: 32,
            plen: 32 + 4,
        };
        let psh = DataPsh::default();
        let data = b"ABCD";
        let mut buf = Vec::new();
        encode_pdu(&hdr, psh.as_bytes(), data, &mut buf);
        assert_eq!(buf.len(), 32 + 4);
        // pad 区 byte 24..32 = 0
        assert_eq!(&buf[24..32], &[0u8; 8]);
        // data 段
        assert_eq!(&buf[32..36], data);
    }
}
