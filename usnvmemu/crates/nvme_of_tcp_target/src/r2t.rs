// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V4a** — R2T (Ready-to-Transfer) encode helper。
//!
//! controller 处理 NVM Write 等命令在 dispatch 阶段调 `ctx.dma_read(host_addr, len)`；
//! NVMe-oF TCP 没有真 DMA，session 必须翻译成"向 host 发 R2T → 等 H2CData 回数据"。
//!
//! R2T PDU 布局（spec NVMe-TCP 1.0a § 4.4.4）：
//! ```text
//! [CommonHdr 8B][R2tPsh 16B]   — 总 24 B；无 data；hlen=24；pdo=24；plen=24
//! ```
//!
//! R2T 永远不带 data → **不可置 DDGST**（DDGST 是 data digest，无 data 则 invariant
//! 违反）；HDGST 由 negotiated 决定（默认关），交给上层 `write_pdu` 处理。
//!
//! 设计：这里只产 `(CommonHdr, R2tPsh)`，由 caller 自己调 `framing::write_pdu`
//! 写到 stream；这样 V4a 不引入 IO，可在纯 unit test 里 byte-exact 比对。

use crate::pdu::{CommonHdr, R2tPsh, pdu_type};

/// 构造 R2T PDU 的 header + PSH。
///
/// - `cccid`: 对应 CapsuleCmd 的 command_id
/// - `ttag`: caller 通过 [`crate::ttag::TtagAllocator`] 分配的非零 transfer tag
/// - `offset`: 当前 R2T 在 cmd buffer 内的起始 byte offset
/// - `length`: 准许 host 现在发的字节数（≤ MAXH2CDATA）
///
/// # Panics
///
/// **V4a-polish (review M-1)** —— hard `assert!` 检查 `ttag != 0` 与
/// `length > 0`。release build 也 panic，避免静默给 Linux nvme-tcp host
/// 发非法 R2T（host 立即 reject 整条 connection 且我们无 log）。
pub fn encode_r2t(cccid: u16, ttag: u16, offset: u32, length: u32) -> (CommonHdr, R2tPsh) {
    assert!(ttag != 0, "ttag=0 is reserved (spec §3.6.3)");
    assert!(length > 0, "R2T length must be > 0 (spec §8.2.4)");
    let hdr = CommonHdr {
        pdu_type: pdu_type::R2T,
        // **R-6** — R2T 无 data，禁置 DDGST；HDGST 由 framing 写时按 negotiated 决定
        flags: 0,
        hlen: 24,
        pdo: 0, // R2T 无 data，pdo=0
        plen: 24,
    };
    let psh = R2tPsh {
        cccid,
        ttag,
        r2t_offset: offset,
        r2t_length: length,
        rsvd: [0u8; 4],
    };
    (hdr, psh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{read_pdu, write_pdu};
    use crate::pdu::pdu_type;
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use zerocopy::IntoBytes;

    /// 构造一对本机 TcpStream（client / server）。
    fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let t = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            s
        });
        let client = TcpStream::connect(addr).unwrap();
        let server = t.join().unwrap();
        (client, server)
    }

    /// R2T encode 后写到 TcpStream，读端 byte-exact 解出来字段一致。
    #[test]
    fn r2t_encode_byte_exact_per_spec() {
        let (mut client, mut server) = tcp_pair();
        let (hdr, psh) = encode_r2t(0x0042, 7, 4096, 8192);
        // 校验 hdr 各字段（CommonHdr packed，复制到 locals 避免 unaligned ref）
        let ph_type = hdr.pdu_type;
        let pf = hdr.flags;
        let pl = hdr.hlen;
        let po = hdr.pdo;
        let plen = hdr.plen;
        assert_eq!(ph_type, pdu_type::R2T);
        assert_eq!(pf, 0);
        assert_eq!(pl, 24);
        assert_eq!(po, 0);
        assert_eq!(plen, 24);

        // server 写
        let h = thread::spawn(move || {
            write_pdu(&mut server, &hdr, psh.as_bytes(), &[]).unwrap();
        });

        // client 读
        let p = read_pdu(&mut client).unwrap();
        h.join().unwrap();
        let rp_type = p.header.pdu_type;
        let rp_plen = p.header.plen;
        assert_eq!(rp_type, pdu_type::R2T);
        assert_eq!(rp_plen, 24);
        assert!(p.data.is_empty(), "R2T 不带 data");
        let r: R2tPsh = crate::pdu::decode_psh(&p.psh).unwrap();
        let cccid = r.cccid;
        let ttag = r.ttag;
        let offset = r.r2t_offset;
        let length = r.r2t_length;
        assert_eq!(cccid, 0x0042);
        assert_eq!(ttag, 7);
        assert_eq!(offset, 4096);
        assert_eq!(length, 8192);
    }

    /// R2T flags 不可带 DDGST（review R-6 invariant）。
    #[test]
    fn r2t_no_ddgst_invariant() {
        let (hdr, _) = encode_r2t(1, 1, 0, 4096);
        let flags_v = hdr.flags;
        assert_eq!(
            flags_v & crate::pdu::flags::DDGST,
            0,
            "R2T 必须 0 DDGST（spec §3.4：无 data 不可置 DDGST）"
        );
    }
}
