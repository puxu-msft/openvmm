// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **NVMe-oF TCP H2CData reassembler fuzz** —— 打
//! `nvme_of_tcp_target::h2c_reassembler::H2cReassembler::accept_pdu`（host→controller data 重组
//! 状态机）。见 `docs/plans/2026-06-14-pdu-framing-fuzz.md` §7。
//!
//! 攻击面:controller 发 R2T(cccid,ttag,offset,length) 后,**host(attacker)用一串 H2CData PDU 回
//! 数据**,每条带 fuzzer 控的 (cccid, ttag, data_offset, data_length, DATA_LAST) + data 字节。
//! accept_pdu 校验 ttag/cccid 匹配、offset 严格递增无空洞(== base+received)、data_length==实际字节、
//! 累计 <= R2T length、末段 DATA_LAST。这是 **offset/length chasing 状态机**(类 SGL/PRP 链)——
//! attacker 控 offset/length 的指针追逐攻击面。
//!
//! **驱动**:纯公共 API(Pdu/DataPsh pub 字段、with_base_offset/accept_pdu pub),无需 refactor。
//! fuzzer 控 reassembler 参数 + 一串 H2CData PDU;harness 构造 framing::Pdu(手搓 16B DataPsh wire
//! 布局,免 zerocopy 依赖)喂 accept_pdu。`use_contiguous` 让 fuzzer 选「合法递增 offset」(到达
//! 累积/Done 深态)或「raw fuzzer offset」(error 路径)——加速 coverage 探深。
//!
//! **oracle(observable-only + ASan)**:accept_pdu 对**任意** PDU 序列只能返 Continue/Done/Error,
//! **绝不 panic/abort(OOM)/hang**。length 限幅 [1,65535] 聚焦 attacker-data 路径(避开 constructor
//! `with_capacity(length)` OOM / `assert(length>0)`——那是 controller-side R2T length、受 MAXH2CDATA
//! 界,非 attacker-unbounded)。

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use nvme_of_tcp_target::framing::Pdu;
use nvme_of_tcp_target::h2c_reassembler::{AcceptOutcome, H2cReassembler};
use nvme_of_tcp_target::pdu::{CommonHdr, flags, pdu_type};
use xtask_fuzz::fuzz_target;

/// 单条 fuzzer H2CData PDU 的控制面。
#[derive(Arbitrary, Debug)]
struct FuzzPdu {
    cccid: u16,
    ttag: u16,
    /// true → data_offset = base + 已接受字节(合法递增,探累积/Done 深态);false → 用 raw_offset。
    use_contiguous: bool,
    raw_offset: u32,
    /// 本 PDU 的 data 字节(限幅,见 fuzz_target)。data_length 默认 = data.len()。
    data: Vec<u8>,
    /// true → 故意让 PSH data_length != 实际 data.len()(测该校验)。
    mismatch_len: bool,
    mismatch_len_val: u32,
    /// 置 DATA_LAST flag。
    data_last: bool,
    /// 偶尔喂非-H2CData type(测 type 校验)。
    wrong_type: bool,
}

#[derive(Arbitrary, Debug)]
struct ReassemblerInput {
    cccid: u16,
    ttag: u16,
    base_offset: u32,
    /// R2T length;限幅 [1,65536] 避开 constructor with_capacity OOM / assert(0)。
    length_minus_1: u16,
    pdus: Vec<FuzzPdu>,
}

/// 手搓 16-byte DataPsh wire 布局(repr(C,packed): cccid u16 + ttag u16 + offset u32 + length u32
/// + rsvd[4]),免给 fuzz crate 加 zerocopy 依赖。
fn datapsh_bytes(cccid: u16, ttag: u16, offset: u32, length: u32) -> Vec<u8> {
    let mut psh = Vec::with_capacity(16);
    psh.extend_from_slice(&cccid.to_le_bytes());
    psh.extend_from_slice(&ttag.to_le_bytes());
    psh.extend_from_slice(&offset.to_le_bytes());
    psh.extend_from_slice(&length.to_le_bytes());
    psh.extend_from_slice(&[0u8; 4]); // rsvd
    psh
}

const MAX_PDUS: usize = 256;
const MAX_DATA: usize = 4096;

fn run(input: ReassemblerInput) {
    let length = (input.length_minus_1 as u32) + 1; // [1, 65536]
    let mut r = H2cReassembler::with_base_offset(input.cccid, input.ttag, input.base_offset, length);

    // harness 跟踪「已接受字节」(== reassembler.received),仅用于 contiguous 模式生成合法 offset
    // (输入生成,非 oracle)。on Continue 才累加。
    let mut accepted = 0u32;
    for fp in input.pdus.into_iter().take(MAX_PDUS) {
        let mut data = fp.data;
        data.truncate(MAX_DATA);
        let data_len = if fp.mismatch_len {
            fp.mismatch_len_val
        } else {
            data.len() as u32
        };
        let offset = if fp.use_contiguous {
            input.base_offset.saturating_add(accepted)
        } else {
            fp.raw_offset
        };
        let ptype = if fp.wrong_type {
            pdu_type::RSP
        } else {
            pdu_type::H2C_DATA
        };
        let pdu = Pdu {
            header: CommonHdr {
                pdu_type: ptype,
                flags: if fp.data_last { flags::DATA_LAST } else { 0 },
                hlen: 0, // accept_pdu 不校验 framing-level hlen/pdo/plen
                pdo: 0,
                plen: 0,
            },
            psh: datapsh_bytes(fp.cccid, fp.ttag, offset, data_len),
            data: data.clone(),
        };
        match r.accept_pdu(&pdu) {
            AcceptOutcome::Continue => {
                // 接受了 → reassembler.received += data.len();同步 harness 计数。
                accepted = accepted.saturating_add(data.len() as u32);
            }
            AcceptOutcome::Done(_) | AcceptOutcome::Error { .. } => break,
        }
    }
}

fuzz_target!(|input: ReassemblerInput| {
    xtask_fuzz::init_tracing_if_repro();
    run(input);
});
