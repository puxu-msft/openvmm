// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **§22 fuzz POC（承重假设验证，非正式 target）** —— 验证整个 §22-fuzz 方案最关键、
//! 最不确定的一条承重假设：**libfuzzer 能否 catch「触碰未-backing mmap 页」的 SIGBUS？**
//!
//! 背景（LESSON §22，CRITICAL）：`vfio_user_transport::dma::map_dma_fd` 把 client 声明的
//! `size` 当 mmap 长度；`mmap(2)` 不校验 `offset+size ≤ fd 真实大小`，映射超出真实页的区间
//! **成功**返回，但 memcpy 触碰无 backing 的页 → 内核投 **SIGBUS 杀整个进程**。§22 的修是
//! mmap 前 `fstat` 取真实大小校验。要 fuzz 这条线、用"SIGBUS 不发生"当 oracle，**前提**是
//! libfuzzer 能把 SIGBUS 当 crash 捕获报告——否则进程被静默杀死、fuzz 无从判定。
//!
//! 本 POC **确定性触发** §22 场景（不依赖 fuzzer 找输入）：memfd ftruncate 到 1 页 →
//! mmap 2 页（第 2 页无 backing）→ 写第 2 页 → 必 SIGBUS。**验收**：cargo-fuzz 跑本 target
//! 应在第 1 个输入即报 `deadly signal` + 写 crash artifact（= libfuzzer 捕获了 SIGBUS）。
//! 若进程静默死、libfuzzer 不报 → §22 用 SIGBUS-as-oracle 不可行，须改"决策 oracle"
//! （比对 map_dma_fd 的 accept/reject 与独立 fstat 判据，不触碰页）。
//!
//! 同时顺验：① 本 fuzz crate（依赖 vfio_user_transport）在 stable 可 build；② harness 能
//! 构造 memfd + mmap（§22 真 target 的基础动作）。

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use std::os::fd::AsRawFd;
use xtask_fuzz::fuzz_target;

const PAGE: usize = 4096;

/// 确定性复现 §22 SIGBUS：memfd 1 页 backing + mmap 2 页 + 触碰第 2 页（无 backing）。
fn trigger_sigbus() {
    // 1) memfd_create + ftruncate 到 1 页（只第 1 页有真实 backing）。
    let fd = nix::sys::memfd::memfd_create(c"sigbus-poc", nix::sys::memfd::MFdFlags::empty())
        .expect("memfd_create");
    nix::unistd::ftruncate(&fd, PAGE as i64).expect("ftruncate 1 page");

    // 2) mmap **2 页**（第 2 页 offset 4096..8192 超出 1 页 backing，正是 §22 的"撒谎 size"）。
    let mut opts = memmap2::MmapOptions::new();
    opts.len(2 * PAGE);
    // SAFETY: POC 故意构造 §22 的危险映射（2 页 map / 1 页 backing）以触发 SIGBUS,
    // 验证 libfuzzer 捕获能力。这正是 map_dma_fd 的 fstat 修要防的场景。
    #[allow(unsafe_code)]
    let mut m = unsafe { opts.map_mut(fd.as_raw_fd()) }.expect("mmap 2 pages over 1-page memfd");

    // 3) 写第 1 页（有 backing，安全）确认映射本身可用。
    m[0] = 0xAB;
    // 4) 写第 2 页（offset 4096，无 backing）→ 内核投 SIGBUS。
    //    若 libfuzzer 装了 SIGBUS handler → 报 deadly signal + crash artifact。
    m[PAGE] = 0xCD;

    // 不应到达（上一行已 SIGBUS）。
    std::hint::black_box(&m);
}

fuzz_target!(|_input: &[u8]| {
    xtask_fuzz::init_tracing_if_repro();
    // 确定性：忽略输入，每次都触发 §22 SIGBUS 场景。
    trigger_sigbus();
});
