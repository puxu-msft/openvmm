// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V1** — 同步 TCP framing：阻塞从 `TcpStream` 读一帧完整 NVMe-oF
//! TCP PDU；阻塞写一帧。
//!
//! 与 pcie_vfio_user_sdk 同走 sync 模型（NVMe controller 是 parking_lot 同步），
//! 避免引入 tokio。多客户端通过 `thread::spawn` per-connection 即可。
//!
//! 完整流程：
//! 1. read 8 byte CommonHdr → decode_common_hdr
//! 2. read `(hlen - CH_LEN)` byte PSH
//! 3. 若 HDGSTF → read 4 byte HDGST + verify
//! 4. 若 plen > pdo（含 data）：先 skip `pdo - hlen - (HDGSTF?4:0)` 字节 pad，
//!    再 read 数据 `data_len = plen - pdo - (DDGSTF?4:0)`
//! 5. 若 DDGSTF → read 4 byte DDGST + verify

use crate::pdu::CH_LEN;
use crate::pdu::CommonHdr;
use crate::pdu::PduError;
use crate::pdu::decode_common_hdr;
use anyhow::Context as _;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use thiserror::Error;
use zerocopy::IntoBytes;

/// 解出的一帧 PDU：header + PSH + data。
#[derive(Debug)]
pub struct Pdu {
    /// 通用 8-byte header。
    pub header: CommonHdr,
    /// PDU-specific header bytes（hlen - 8 字节）。
    pub psh: Vec<u8>,
    /// 数据 payload（DDGST 已 strip）。
    pub data: Vec<u8>,
}

/// framing 错误。
#[derive(Debug, Error)]
pub enum FramingError {
    /// peer 关闭（EOF）。
    #[error("NVMe-oF TCP peer closed (EOF) {at}")]
    PeerClosed {
        /// "while reading header" 等。
        at: &'static str,
    },
    /// PDU 解析失败。
    #[error("PDU decode: {0}")]
    Pdu(#[from] PduError),
}

/// 阻塞读一帧完整 PDU。
pub fn read_pdu(stream: &mut TcpStream) -> anyhow::Result<Pdu> {
    // 1. read CommonHdr
    let mut hbuf = [0u8; CH_LEN];
    read_exact_or_eof(stream, &mut hbuf, "while reading CommonHdr")?;
    let header = decode_common_hdr(&hbuf).map_err(FramingError::Pdu)?;
    let hlen = header.hlen as usize;
    let plen = header.plen as usize;
    let pdo = header.pdo as usize;

    // 2. read PSH (hlen - 8)
    let psh_len = hlen - CH_LEN;
    let mut psh = vec![0u8; psh_len];
    if psh_len > 0 {
        read_exact_or_eof(stream, &mut psh, "while reading PSH")?;
    }

    // 3. HDGST 校验
    if header.has_hdgst() {
        let mut hdgst = [0u8; 4];
        read_exact_or_eof(stream, &mut hdgst, "while reading HDGST")?;
        // CRC32C 算 CH+PSH 合并字节；先拼成临时 buf
        let mut crc_input = Vec::with_capacity(hlen);
        crc_input.extend_from_slice(&hbuf);
        crc_input.extend_from_slice(&psh);
        if !crate::digest::verify_crc32c(&crc_input, hdgst) {
            return Err(FramingError::Pdu(PduError::HdgstMismatch).into());
        }
    }

    // 4. data + pad + DDGST
    let consumed = hlen + if header.has_hdgst() { 4 } else { 0 };
    let mut data = Vec::new();
    if plen > consumed {
        // 有 trailing data
        let ddgst_len = if header.has_ddgst() { 4 } else { 0 };
        // pad: pdo - consumed
        let pad_len = pdo.saturating_sub(consumed);
        if pad_len > 0 {
            let mut pad = vec![0u8; pad_len];
            read_exact_or_eof(stream, &mut pad, "while reading pad")?;
            // pad 必须 0（spec invariant；非 0 视 hostile peer，但教学路径仅 log）
            if pad.iter().any(|&b| b != 0) {
                tracing::warn!(pad_len, "non-zero pad bytes in PDU (spec says zero)");
            }
        }
        let data_off = if pdo == 0 { consumed } else { pdo };
        if plen < data_off + ddgst_len {
            return Err(FramingError::Pdu(PduError::PlenLessThanHlen {
                plen: header.plen,
                hlen: header.hlen,
            })
            .into());
        }
        let data_len = plen - data_off - ddgst_len;
        data = vec![0u8; data_len];
        if data_len > 0 {
            read_exact_or_eof(stream, &mut data, "while reading data")?;
        }
        if header.has_ddgst() {
            let mut ddgst = [0u8; 4];
            read_exact_or_eof(stream, &mut ddgst, "while reading DDGST")?;
            if !crate::digest::verify_crc32c(&data, ddgst) {
                return Err(FramingError::Pdu(PduError::DdgstMismatch).into());
            }
        }
    }
    Ok(Pdu { header, psh, data })
}

/// 阻塞写一帧完整 PDU。
///
/// caller 必须 *正确* 填好 `hdr.hlen / pdo / plen`，与 PSH/data 长度一致。
pub fn write_pdu(
    stream: &mut TcpStream,
    hdr: &CommonHdr,
    psh: &[u8],
    data: &[u8],
) -> anyhow::Result<()> {
    let mut buf = Vec::with_capacity(hdr.plen as usize);
    buf.extend_from_slice(hdr.as_bytes());
    buf.extend_from_slice(psh);
    if hdr.has_hdgst() {
        buf.extend_from_slice(&crate::digest::crc32c_le_bytes(&buf));
    }
    let written = buf.len();
    if (hdr.pdo as usize) > written {
        buf.extend(std::iter::repeat_n(0u8, hdr.pdo as usize - written));
    }
    buf.extend_from_slice(data);
    if hdr.has_ddgst() {
        buf.extend_from_slice(&crate::digest::crc32c_le_bytes(data));
    }
    stream.write_all(&buf).context("TCP write_all")?;
    Ok(())
}

fn read_exact_or_eof(
    stream: &mut TcpStream,
    buf: &mut [u8],
    at: &'static str,
) -> anyhow::Result<()> {
    match stream.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(FramingError::PeerClosed { at }.into())
        }
        Err(e) => Err(anyhow::anyhow!("TCP read_exact ({at}): {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdu::DataPsh;
    use crate::pdu::IcPsh;
    use crate::pdu::R2tPsh;
    use crate::pdu::flags;
    use crate::pdu::pdu_type;
    use std::net::TcpListener;
    use std::thread;

    /// 本地 TCP socketpair；返回 `(client_side, server_side)`。
    fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let t = thread::spawn(move || listener.accept().unwrap().0);
        let client = TcpStream::connect(addr).unwrap();
        let server = t.join().unwrap();
        (client, server)
    }

    /// ICReq 无 digest roundtrip。
    #[test]
    fn icreq_no_digest_roundtrip() {
        let (mut a, mut b) = tcp_pair();
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
            maxr2t_or_maxh2cdata: 3,
            rsvd: [0u8; 112],
        };
        let h2 = thread::spawn(move || -> anyhow::Result<Pdu> { read_pdu(&mut b) });
        write_pdu(&mut a, &hdr, psh.as_bytes(), &[]).unwrap();
        drop(a);
        let pdu = h2.join().unwrap().unwrap();
        let t = pdu.header.pdu_type;
        assert_eq!(t, pdu_type::ICREQ);
        let p: IcPsh = crate::pdu::decode_psh(&pdu.psh).unwrap();
        let m = p.maxr2t_or_maxh2cdata;
        assert_eq!(m, 3);
        assert!(pdu.data.is_empty());
    }

    /// HDGST + DDGST roundtrip：C2HData 带 12 byte 数据。
    #[test]
    fn c2hdata_with_both_digests_roundtrip() {
        let (mut a, mut b) = tcp_pair();
        let data = b"abcdef012345";
        let hdr = CommonHdr {
            pdu_type: pdu_type::C2H_DATA,
            flags: flags::HDGST | flags::DDGST | flags::DATA_LAST,
            hlen: 24,
            pdo: 24 + 4, // HDGST 占 4B
            plen: 24 + 4 + data.len() as u32 + 4,
        };
        let psh = DataPsh {
            cccid: 0x42,
            ttag_or_rsvd: 0,
            data_offset: 0,
            data_length: data.len() as u32,
            rsvd: [0u8; 4],
        };
        let h2 = thread::spawn(move || -> anyhow::Result<Pdu> { read_pdu(&mut b) });
        write_pdu(&mut a, &hdr, psh.as_bytes(), data).unwrap();
        drop(a);
        let pdu = h2.join().unwrap().unwrap();
        assert!(pdu.header.has_hdgst());
        assert!(pdu.header.has_ddgst());
        assert!(pdu.header.is_data_last());
        assert_eq!(pdu.data, data);
    }

    /// HDGST mismatch → reader 返 HdgstMismatch。
    #[test]
    fn corrupted_hdgst_detected() {
        let (mut a, mut b) = tcp_pair();
        let hdr = CommonHdr {
            pdu_type: pdu_type::ICREQ,
            flags: flags::HDGST,
            hlen: 128,
            pdo: 0,
            plen: 128 + 4,
        };
        let psh = IcPsh::default();
        // 手工拼 buf，故意改 HDGST 第一字节
        let mut buf = Vec::new();
        buf.extend_from_slice(hdr.as_bytes());
        buf.extend_from_slice(psh.as_bytes());
        let good = crate::digest::crc32c_le_bytes(&buf);
        let mut bad = good;
        bad[0] ^= 0xFF;
        buf.extend_from_slice(&bad);
        use std::io::Write as _;
        a.write_all(&buf).unwrap();
        drop(a);
        let r = read_pdu(&mut b);
        let msg = format!("{:#}", r.unwrap_err());
        assert!(msg.contains("HDGST"), "expected HDGST err, got: {msg}");
    }

    /// peer 关闭 → PeerClosed。
    #[test]
    fn eof_returns_peer_closed() {
        let (a, mut b) = tcp_pair();
        drop(a);
        let r = read_pdu(&mut b);
        let e = r.unwrap_err();
        assert!(
            e.downcast_ref::<FramingError>()
                .is_some_and(|f| matches!(f, FramingError::PeerClosed { .. })),
            "got: {e:#}"
        );
    }

    /// R2T (no digest, no data) roundtrip。
    #[test]
    fn r2t_roundtrip_no_data() {
        let (mut a, mut b) = tcp_pair();
        let hdr = CommonHdr {
            pdu_type: pdu_type::R2T,
            flags: 0,
            hlen: 24,
            pdo: 0,
            plen: 24,
        };
        let psh = R2tPsh {
            cccid: 0x55,
            ttag: 0xCAFE,
            r2t_offset: 0,
            r2t_length: 4096,
            rsvd: [0u8; 4],
        };
        let h = thread::spawn(move || -> anyhow::Result<Pdu> { read_pdu(&mut b) });
        write_pdu(&mut a, &hdr, psh.as_bytes(), &[]).unwrap();
        drop(a);
        let pdu = h.join().unwrap().unwrap();
        let parsed: R2tPsh = crate::pdu::decode_psh(&pdu.psh).unwrap();
        let tt = parsed.ttag;
        let len = parsed.r2t_length;
        assert_eq!(tt, 0xCAFE);
        assert_eq!(len, 4096);
        assert!(pdu.data.is_empty());
    }
}
