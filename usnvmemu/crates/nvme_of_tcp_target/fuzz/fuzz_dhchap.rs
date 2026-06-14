// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **NVMe-oF TCP DH-HMAC-CHAP auth-message parse fuzz** —— 打
//! `nvme_of_tcp_target::dhchap::{parse_negotiate, parse_reply}`（in-band auth 协议的 attacker-控
//! wire 解析）。见 `docs/plans/2026-06-14-pdu-framing-fuzz.md` §7。
//!
//! 攻击面:in-band DH-HMAC-CHAP 认证里,**malicious initiator 发 AUTH_Negotiate / Reply 消息**——
//! attacker 控整段 auth wire 字节。两个解析器:
//!   - `parse_negotiate`:含**手写长度算术 + descriptor 迭代**(attacker 控 napd/halen/dhlen,
//!     `off + 4 + halen + dhlen` 切片 + 8 字节对齐 padding 推进)——典型 length-arithmetic 攻击面。
//!   - `parse_reply`:hl/length 校验后取 HMAC response。
//! auth 是 security-sensitive 面:解析在校验密码学之前发生,parse 崩 = 未认证 DoS。
//!
//! **驱动**:两者皆 `pub fn(&[u8]) -> Result`,纯 attacker 字节、无 harness/refactor。同一 fuzzer
//! 字节喂两者(各自按 `data[1]` msg_id 路由,非匹配的早 error;coverage-guided 学会两种 prefix)。
//!
//! **oracle(observable-only + ASan)**:对**任意**字节只能返 `Ok` 或 `Err`,**绝不 panic/abort/
//! OOB/hang**。不加语义自洽断言(避自证陷阱)。

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use xtask_fuzz::fuzz_target;

fuzz_target!(|data: &[u8]| {
    xtask_fuzz::init_tracing_if_repro();
    // 同一字节流喂两个 attacker-wire 解析器;各按 data[1] msg_id 路由,结果丢弃(observable oracle
    // 只要求不 panic/OOB/hang)。
    let _ = nvme_of_tcp_target::dhchap::parse_negotiate(data);
    let _ = nvme_of_tcp_target::dhchap::parse_reply(data);
});
