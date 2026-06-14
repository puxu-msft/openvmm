// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **NVMe-oF TCP PDU framing fuzz（async 路径）** —— 打
//! `nvme_of_tcp_target::framing::read_pdu_async`（sync `read_pdu` 的**平行手写实现**）。
//! 见 `docs/plans/2026-06-14-pdu-framing-fuzz.md` §6/§7。
//!
//! **为什么单独 fuzz async**：sync/async 共享 `decode_common_hdr`（hlen 修两路都覆盖），但
//! data/pad/digest 那串长度算术是**两份独立拷贝**（framing.rs:98-142 vs :226-268，V8e-1 的
//! M-1/M-2 guard 分别加到两边）——可独立 drift。只 fuzz sync 逮不到 async 侧的回归。
//!
//! **驱动（承重假设）**：`read_pdu_async<S: tokio::io::AsyncRead + Unpin>` 已泛型；`Cursor<&[u8]>`
//! 是 tokio AsyncRead，喂 fuzzer 字节驱**真** read_pdu_async。body 只 await `read_exact`（无 timer/
//! spawn），故 `futures::executor::block_on` 即可跑完——Cursor slice 读同步 Ready、**无需 tokio
//! runtime**，每轮快（不像 per-iter 建 tokio runtime 那样拖）。
//!
//! **oracle（observable-only + ASan）**：对任意字节只能返 `Ok(Pdu)` 或 `Err`，绝不 panic/abort/
//! hang。Cursor 短读 → `read_exact` `UnexpectedEof` → `PeerClosed` Err（不 hang）。

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use std::io::Cursor;
use xtask_fuzz::fuzz_target;

fuzz_target!(|data: &[u8]| {
    xtask_fuzz::init_tracing_if_repro();
    // Cursor<&[u8]> 是 tokio AsyncRead；block_on 跑 read_pdu_async（同步完成，无 tokio runtime）。
    let mut cur = Cursor::new(data);
    let _ = futures::executor::block_on(nvme_of_tcp_target::framing::read_pdu_async(&mut cur));
});
