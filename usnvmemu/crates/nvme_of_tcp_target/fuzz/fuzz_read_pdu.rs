// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **NVMe-oF TCP PDU framing fuzz** —— 打 `nvme_of_tcp_target::framing::read_pdu`（第三条
//! transport 的 wire 解析金矿，0 fuzz 覆盖）。见 `docs/plans/2026-06-14-pdu-framing-fuzz.md`。
//!
//! read_pdu 对 **attacker 控的 wire 字节流**做 CommonHdr → PSH(hlen-CH_LEN) → HDGST → PDO-pad →
//! data(plen-pdo-ddgst) → DDGST 的一串**长度/偏移算术**——典型 host-controlled parser 攻击面。
//!
//! **驱动**：read_pdu 已泛型化 `<R: std::io::Read>`（behavior-preserving，TcpStream: Read），故
//! 直接用 `std::io::Cursor<&[u8]>` 喂 fuzzer 字节驱**真** read_pdu（非抄一份）。Cursor 短读 →
//! `read_exact` `UnexpectedEof` → `PeerClosed` Err（不 hang）。
//!
//! **oracle（observable-only + ASan）**：read_pdu 对**任意字节**只能返 `Ok(Pdu)` 或 `Err`，**绝不
//! panic / abort(OOM) / hang**。libfuzzer+ASan 兜 abort/OOM。**不**加"plen/hlen 自洽"不变量（那用
//! 被测同套假设自证=陷阱，且把 fuzz 从找崩溃偏成验语义）。
//!
//! 长度算术安全靠**三个独立保护**（见 plan §6 表）：psh_len←`hlen≥CH_LEN` 校验（本轮修）、
//! data_len←`:123` guard、pdo←`:101` guard。fuzz 绿 = 三者都在；又挖出长度 crash = 某 guard 回归。

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use std::io::Cursor;
use xtask_fuzz::fuzz_target;

fuzz_target!(|data: &[u8]| {
    xtask_fuzz::init_tracing_if_repro();
    // Cursor 喂整个 fuzzer 字节流驱真 read_pdu（泛型 <R:Read>）。结果丢弃——observable oracle
    // 只要求不 panic/abort/hang，不校验内容。
    let mut cur = Cursor::new(data);
    let _ = nvme_of_tcp_target::framing::read_pdu(&mut cur);
});
