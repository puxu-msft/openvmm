// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V4a** — H2CData reassembler。
//!
//! controller 发 R2T(cccid, ttag, offset, length) 后，host 用 H2CData PDU 回
//! 数据。R2T 与 H2CData 的关系（spec NVMe-TCP 1.0a § 4.4.4 / § 4.4.5）：
//!
//! - host 可以一条 H2CData 覆盖整个 R2T，也可以拆成多条
//! - 每条 H2CData 必须带 `ttag = R2T.ttag`、`cccid = R2T.cccid`
//! - `data_offset` 必须从 R2T 范围 `[r2t_offset, r2t_offset + r2t_length)` 起始；
//!   多条 PDU 必须连续无空洞、按递增 offset
//! - 累计 `data_length` 之和必须 = `r2t_length`
//! - 最后一条 PDU 必须置 `DATA_LAST` flag
//!
//! V4 范围内一次只跟 1 个 R2T；如果 host 乱序、错 ttag、超出范围、或最后段
//! 没置 DATA_LAST，返 [`AcceptOutcome::Error`] 让 caller 发 C2HTermReq 后断。

use crate::framing::Pdu;
use crate::pdu::{DataPsh, flags, pdu_type, term_fes};
use zerocopy::FromBytes;

/// `accept_pdu` 的三种结果。
///
/// **V4a-polish (review M-5)** — `#[must_use]` 防 caller `let _ = ...;` 把
/// 协议错静默吞掉；Error 必须触发 session 终止。
#[must_use = "AcceptOutcome must be inspected; Error means session must be terminated"]
#[derive(Debug)]
pub enum AcceptOutcome {
    /// 收到合法 H2CData，但还差更多；continue 读下一条。
    Continue,
    /// 收齐全部 `r2t_length` 字节；返还重组好的 buffer。
    Done(Vec<u8>),
    /// 协议错；caller 应发 C2HTermReq 后断。`fes` 是 spec § 5.2 fatal error
    /// status code。
    Error {
        /// fatal error status code (term_fes::*)
        fes: u16,
        /// 调试 hint
        reason: &'static str,
    },
}

/// 单 R2T 的 H2CData 重组器。
pub struct H2cReassembler {
    expected_cccid: u16,
    expected_ttag: u16,
    /// **V4c**：cmd 级累计 offset 基址。host 端 Linux nvme-tcp driver 填
    /// `psh.data_offset = req->data_sent`（spec § 8.2.5），即 cmd 内累计；
    /// reassembler 收到的 `data_offset` 应 == `base_offset + received`。
    /// V4b 单 R2T 时 `base_offset = 0`；V4c 多 R2T 时每片 `base_offset =
    /// 该 R2T 的 r2t_offset`。
    base_offset: u32,
    expected_total: u32,
    /// 已接收字节数。
    received: u32,
    /// 拼装好的数据；预分配到 `expected_total`。
    buf: Vec<u8>,
    /// 是否已收到 DATA_LAST 标记。
    seen_last: bool,
}

impl H2cReassembler {
    /// 起一个新 reassembler，对应即将 emit 的 R2T(cccid, ttag, length)。
    /// `base_offset` 是 cmd 内累计 offset（V4b 单 R2T = 0；V4c 多 R2T 用
    /// R2T 的 `r2t_offset`），用于与 host 填的 `psh.data_offset` 对齐。
    ///
    /// **V4a-polish (review M-6)** — `length == 0` 是 spec violation
    /// (R2T length 必须 > 0)，直接 panic 防止上层 controller 路径 bug
    /// 给 Linux nvme-tcp host 发出非法 R2T。
    ///
    /// **V4a-polish (review M-2 partial / V4b TODO)** — 当前 `Vec::with_capacity(length)`
    /// 不 cap；V4b 接 controller 后必须在更高层 cap 到 MAXH2CDATA × N
    /// 防 host 端造 u32::MAX 触发 4 GiB 分配 OOM。
    ///
    /// **V4a-polish (review M-4)** — `Done` 返还 buf 后此 reassembler 进入
    /// terminal state，调用者必须丢弃；再调 `accept_pdu` 行为未定义
    /// （目前 `received == expected_total` 不变，新 PDU 会被 OUT_OF_RANGE
    /// 拒，但语义上不应依赖）。
    pub fn new(cccid: u16, ttag: u16, length: u32) -> Self {
        Self::with_base_offset(cccid, ttag, 0, length)
    }

    /// **V4c (review H-1)** — 带 `base_offset` 的构造器。
    /// 多 R2T 场景下每片 reassembler 用本片的 `r2t_offset` 当 base，
    /// 与 host 端 `psh.data_offset = req->data_sent` 累计语义对齐。
    pub fn with_base_offset(cccid: u16, ttag: u16, base_offset: u32, length: u32) -> Self {
        assert!(length > 0, "R2T length must be > 0 (spec §8.2.4)");
        Self {
            expected_cccid: cccid,
            expected_ttag: ttag,
            base_offset,
            expected_total: length,
            received: 0,
            buf: Vec::with_capacity(length as usize),
            seen_last: false,
        }
    }

    /// 喂一条新进 PDU 到 reassembler。
    pub fn accept_pdu(&mut self, pdu: &Pdu) -> AcceptOutcome {
        if pdu.header.pdu_type != pdu_type::H2C_DATA {
            return AcceptOutcome::Error {
                fes: term_fes::PDU_SEQ_ERR,
                reason: "non-H2CData PDU while expecting H2CData",
            };
        }
        let psh = match DataPsh::read_from_bytes(&pdu.psh) {
            Ok(p) => p,
            Err(_) => {
                return AcceptOutcome::Error {
                    fes: term_fes::INVALID_PDU_HDR,
                    reason: "H2CData PSH parse failed",
                };
            }
        };
        // 复制 packed 字段到 locals
        let cccid = psh.cccid;
        let ttag = psh.ttag_or_rsvd;
        let off = psh.data_offset;
        let len = psh.data_length;

        if cccid != self.expected_cccid {
            return AcceptOutcome::Error {
                fes: term_fes::PDU_SEQ_ERR,
                reason: "H2CData cccid mismatch",
            };
        }
        if ttag != self.expected_ttag {
            return AcceptOutcome::Error {
                fes: term_fes::PDU_SEQ_ERR,
                reason: "H2CData ttag mismatch",
            };
        }
        // 必须严格按递增 offset 拼接，无空洞。
        // **V4c (review H-1)** — host 端 `psh.data_offset` 是 cmd 内累计；
        // 我们期望 `data_offset == base_offset + received`。
        let expected_off = self.base_offset.saturating_add(self.received);
        if off != expected_off {
            return AcceptOutcome::Error {
                fes: term_fes::DATA_OUT_OF_RANGE,
                reason: "H2CData data_offset out of order / has gap / wrong base",
            };
        }
        if len as usize != pdu.data.len() {
            return AcceptOutcome::Error {
                fes: term_fes::INVALID_PDU_HDR,
                reason: "H2CData PSH data_length != actual data bytes",
            };
        }
        let new_received = self.received.saturating_add(len);
        if new_received > self.expected_total {
            return AcceptOutcome::Error {
                fes: term_fes::DATA_OUT_OF_RANGE,
                reason: "H2CData accumulated > R2T length",
            };
        }
        // OK，吸入
        self.buf.extend_from_slice(&pdu.data);
        self.received = new_received;
        let is_last = pdu.header.flags & flags::DATA_LAST != 0;
        if is_last {
            self.seen_last = true;
        }

        // 完成判定
        if self.received == self.expected_total {
            if !self.seen_last {
                return AcceptOutcome::Error {
                    fes: term_fes::PDU_SEQ_ERR,
                    reason: "H2CData total reached but last PDU not DATA_LAST",
                };
            }
            return AcceptOutcome::Done(std::mem::take(&mut self.buf));
        }
        // 未完
        if self.seen_last {
            // 标 LAST 但字节没到齐
            return AcceptOutcome::Error {
                fes: term_fes::PDU_SEQ_ERR,
                reason: "H2CData DATA_LAST set but bytes short",
            };
        }
        AcceptOutcome::Continue
    }

    /// 当前已收字节数（debug 用）。
    pub fn received(&self) -> u32 {
        self.received
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{read_pdu, write_pdu};
    use crate::pdu::{CommonHdr, DataPsh, flags, pdu_type};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use zerocopy::IntoBytes;

    fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let t = thread::spawn(move || listener.accept().unwrap().0);
        let client = TcpStream::connect(addr).unwrap();
        let server = t.join().unwrap();
        (client, server)
    }

    fn h2c_pdu(
        cccid: u16,
        ttag: u16,
        offset: u32,
        data: &[u8],
        last: bool,
    ) -> (CommonHdr, DataPsh, Vec<u8>) {
        let plen = 24 + data.len() as u32;
        let hdr = CommonHdr {
            pdu_type: pdu_type::H2C_DATA,
            flags: if last { flags::DATA_LAST } else { 0 },
            hlen: 24,
            pdo: 24,
            plen,
        };
        let psh = DataPsh {
            cccid,
            ttag_or_rsvd: ttag,
            data_offset: offset,
            data_length: data.len() as u32,
            rsvd: [0u8; 4],
        };
        (hdr, psh, data.to_vec())
    }

    /// host 一条 H2CData 覆盖整个 R2T。
    #[test]
    fn h2c_reassembler_single_pdu_complete() {
        let (mut client, mut server) = tcp_pair();
        let mut r = H2cReassembler::new(0x42, 7, 4096);
        let data = vec![0xABu8; 4096];
        let (hdr, psh, payload) = h2c_pdu(0x42, 7, 0, &data, true);
        let _t = thread::spawn(move || {
            write_pdu(&mut client, &hdr, psh.as_bytes(), &payload).unwrap();
        });
        let p = read_pdu(&mut server).unwrap();
        let outcome = r.accept_pdu(&p);
        match outcome {
            AcceptOutcome::Done(bytes) => {
                assert_eq!(bytes.len(), 4096);
                assert!(bytes.iter().all(|&b| b == 0xAB));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// host 分 4 段 16 KiB（合计 64 KiB），最后段带 DATA_LAST。
    #[test]
    fn h2c_reassembler_multi_pdu_with_offsets() {
        let total: u32 = 64 * 1024;
        let chunk: u32 = 16 * 1024;
        let mut r = H2cReassembler::new(0x55, 3, total);
        for i in 0..4 {
            let off = i * chunk;
            let last = i == 3;
            let data: Vec<u8> = (0..chunk).map(|n| (n + off) as u8).collect();
            let (mut client, mut server) = tcp_pair();
            let (hdr, psh, payload) = h2c_pdu(0x55, 3, off, &data, last);
            let _t = thread::spawn(move || {
                write_pdu(&mut client, &hdr, psh.as_bytes(), &payload).unwrap();
            });
            let p = read_pdu(&mut server).unwrap();
            let outcome = r.accept_pdu(&p);
            if last {
                match outcome {
                    AcceptOutcome::Done(bytes) => assert_eq!(bytes.len(), total as usize),
                    other => panic!("expected Done, got {other:?}"),
                }
            } else {
                assert!(
                    matches!(outcome, AcceptOutcome::Continue),
                    "expected Continue, got {outcome:?}"
                );
            }
        }
    }

    /// ttag 错配 → PDU_SEQ_ERR。
    #[test]
    fn h2c_reassembler_rejects_ttag_mismatch() {
        let (mut client, mut server) = tcp_pair();
        let mut r = H2cReassembler::new(0x42, 7, 1024);
        let (hdr, psh, payload) = h2c_pdu(0x42, 9 /* wrong */, 0, &vec![0u8; 1024], true);
        let _t = thread::spawn(move || {
            write_pdu(&mut client, &hdr, psh.as_bytes(), &payload).unwrap();
        });
        let p = read_pdu(&mut server).unwrap();
        match r.accept_pdu(&p) {
            AcceptOutcome::Error { fes, .. } => assert_eq!(fes, term_fes::PDU_SEQ_ERR),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// offset 错位 → DATA_OUT_OF_RANGE。
    #[test]
    fn h2c_reassembler_rejects_gap() {
        let (mut client, mut server) = tcp_pair();
        let mut r = H2cReassembler::new(0x42, 7, 2048);
        let (hdr, psh, payload) =
            h2c_pdu(0x42, 7, 512 /* should be 0 */, &vec![0u8; 512], false);
        let _t = thread::spawn(move || {
            write_pdu(&mut client, &hdr, psh.as_bytes(), &payload).unwrap();
        });
        let p = read_pdu(&mut server).unwrap();
        match r.accept_pdu(&p) {
            AcceptOutcome::Error { fes, .. } => assert_eq!(fes, term_fes::DATA_OUT_OF_RANGE),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// 字节总数到齐但最后一条没置 DATA_LAST → PDU_SEQ_ERR。
    #[test]
    fn h2c_reassembler_requires_last_flag() {
        let (mut client, mut server) = tcp_pair();
        let mut r = H2cReassembler::new(0x42, 7, 1024);
        let (hdr, psh, payload) = h2c_pdu(0x42, 7, 0, &vec![0u8; 1024], false /* no LAST */);
        let _t = thread::spawn(move || {
            write_pdu(&mut client, &hdr, psh.as_bytes(), &payload).unwrap();
        });
        let p = read_pdu(&mut server).unwrap();
        match r.accept_pdu(&p) {
            AcceptOutcome::Error { fes, .. } => assert_eq!(fes, term_fes::PDU_SEQ_ERR),
            other => panic!("expected Error (no LAST), got {other:?}"),
        }
    }

    /// 非 H2C_DATA pdu_type → PDU_SEQ_ERR。
    #[test]
    fn h2c_reassembler_rejects_non_h2cdata_pdu_type() {
        let (mut client, mut server) = tcp_pair();
        let mut r = H2cReassembler::new(0x42, 7, 1024);
        // 用 C2H_DATA pdu_type 替代（host 不应发，但我们要拒）
        let hdr = CommonHdr {
            pdu_type: pdu_type::C2H_DATA,
            flags: flags::DATA_LAST,
            hlen: 24,
            pdo: 24,
            plen: 24 + 1024,
        };
        let psh = DataPsh {
            cccid: 0x42,
            ttag_or_rsvd: 7,
            data_offset: 0,
            data_length: 1024,
            rsvd: [0u8; 4],
        };
        let payload = vec![0u8; 1024];
        let _t = thread::spawn(move || {
            write_pdu(&mut client, &hdr, psh.as_bytes(), &payload).unwrap();
        });
        let p = read_pdu(&mut server).unwrap();
        match r.accept_pdu(&p) {
            AcceptOutcome::Error { fes, .. } => assert_eq!(fes, term_fes::PDU_SEQ_ERR),
            other => panic!("expected Error (wrong pdu_type), got {other:?}"),
        }
    }

    /// **V4c (review H-1 regression)** — `base_offset != 0` 时按
    /// "host 端 psh.data_offset == base_offset + received" 接受。
    /// 模拟多 R2T 第 2 段：base=65536，单 PDU 4 byte，psh.data_offset=65536。
    #[test]
    fn h2c_reassembler_with_base_offset_accepts_cumulative() {
        let (mut client, mut server) = tcp_pair();
        let mut r = H2cReassembler::with_base_offset(0xF9, 7, 65536, 4);
        let (hdr, psh, payload) = h2c_pdu(0xF9, 7, 65536, &[1, 2, 3, 4], true);
        let _t = thread::spawn(move || {
            write_pdu(&mut client, &hdr, psh.as_bytes(), &payload).unwrap();
        });
        let p = read_pdu(&mut server).unwrap();
        match r.accept_pdu(&p) {
            AcceptOutcome::Done(b) => assert_eq!(b, vec![1, 2, 3, 4]),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// **V4c (review H-1)** — base_offset 与 psh.data_offset 不匹配时拒。
    /// 模拟 host bug：第 2 段本应 data_offset=65536 却填 0。
    #[test]
    fn h2c_reassembler_with_base_offset_rejects_zero_offset() {
        let (mut client, mut server) = tcp_pair();
        let mut r = H2cReassembler::with_base_offset(0xF9, 7, 65536, 4);
        let (hdr, psh, payload) = h2c_pdu(0xF9, 7, 0, &[1, 2, 3, 4], true);
        let _t = thread::spawn(move || {
            write_pdu(&mut client, &hdr, psh.as_bytes(), &payload).unwrap();
        });
        let p = read_pdu(&mut server).unwrap();
        match r.accept_pdu(&p) {
            AcceptOutcome::Error { fes, .. } => assert_eq!(fes, term_fes::DATA_OUT_OF_RANGE),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
