// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **§22 mmap fuzz target** —— 打 `vfio_user_transport::dma::map_dma_fd`（全系统**唯一 unsafe
//! 面**、LESSON §22 的 SIGBUS DoS 本体）。见 `docs/plans/2026-06-14-section22-mmap-fuzz.md`。
//!
//! §22：`map_dma_fd` 把 client 声明的 `size` 当 mmap 长度；`mmap(2)` 不校验 `offset+size ≤ fd
//! 真实大小`，映射超出真实页的区间**成功**返回，触碰无 backing 页 → 内核 **SIGBUS 杀进程**。
//! 修（dma.rs fstat C-1）：mmap 前 `fstat` 取真实 `st_size` 校验上界（仅普通文件）。
//!
//! **驱动**（经 `fuzzing` feature 的 `__fuzz_map_and_touch` 单一 helper）：fuzzer 控
//! `(memfd 真实页数, offset, size, writeable)` → memfd+ftruncate → map（含 fstat 决策）→ **成功则
//! 逐页 volatile-touch**（helper 内，volatile 防 DCE）。
//!
//! **oracle（双判据）**：
//! 1. **no-SIGBUS（ASan 兜，主）**：helper 返 Ok 即已 touch 完每页、绝不 SIGBUS；越界被 accept 却
//!    touch SIGBUS → ASan BUS crash。只守 `map_dma_fd` 的 **backing 完整性**（§22 本体）；不守
//!    `mmap_read/write` 的 region-relative 边界（SAFETY #3，留 wire-level fuzz）。
//! 2. **决策 oracle（独立判据，与 map_dma_fd 逐分支对齐）**：harness 恒造 memfd ⟹ `is_regular`
//!    恒真（字符设备分支物理不可达）。reject ⟺ `offset+size 溢出` ∨ `offset+size > backing_bytes`。
//!    harness 用**已知 ftruncate 值**自算（独立于被测的 `fstat`）。断言 `is_ok == !reject`。
//!    `size==0` 单独短路（map_dma_fd 不挡、memmap2 可能 Err）：跳过 is_ok 断言，仅验不 panic/SIGBUS。

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use xtask_fuzz::fuzz_target;

const PAGE: u64 = 4096;
/// memfd 页数上限（避免 OOM；256 KiB 足够压边界）。
const MAX_BACKING_PAGES: u8 = 64;

#[derive(Arbitrary, Debug)]
struct MapInput {
    /// memfd ftruncate 到 N 页（真实 backing）。
    backing_pages: u8,
    /// mmap offset 页数（压到 [0, backing] 内，使 accept/reject 边界采样有意义）。
    offset_pages: u16,
    /// size 相对边界 `backing-offset` 的偏移：实际 size = max(0, boundary + delta)，把采样压到
    /// off-by-one 最可能藏的 accept/reject 边界，而非均匀撒 [0,4G)。
    size_delta: i32,
    writeable: bool,
    /// 溢出探针：true → offset 取近 u64::MAX（页对齐），触 `checked_add` 溢出 reject 分支
    /// （A 直驱下 offset 受限难自然触溢出，单列此维度可达）。
    overflow_probe: bool,
}

fn run(input: MapInput) {
    let backing_pages = (input.backing_pages % (MAX_BACKING_PAGES + 1)) as u64;
    let backing_bytes = backing_pages * PAGE;

    // 1) memfd + ftruncate 到真实 backing（memfd 恒 S_IFREG → is_regular 恒真，oracle 前提）。
    let fd = match nix::sys::memfd::memfd_create(c"s22fuzz", nix::sys::memfd::MFdFlags::empty()) {
        Ok(fd) => fd,
        Err(_) => return,
    };
    if nix::unistd::ftruncate(&fd, backing_bytes as i64).is_err() {
        return;
    }

    // 2) 算 (offset, size)。
    let (offset, size) = if input.overflow_probe {
        // 页对齐近 u64::MAX → offset+任意正 size 溢出 checked_add。
        let off = u64::MAX & !(PAGE - 1);
        (off, 1u64)
    } else {
        // offset 压在 [0, backing] 内（保证 boundary ≥ 0、边界采样密）。
        let off_pages = if backing_pages == 0 {
            0
        } else {
            (input.offset_pages as u64) % (backing_pages + 1)
        };
        let off = off_pages * PAGE;
        let boundary = backing_bytes as i64 - off as i64; // ≥ 0
        let sz = (boundary + input.size_delta as i64).max(0) as u64;
        (off, sz)
    };

    // 3) 独立决策判据（与 map_dma_fd 逐分支对齐；is_regular 恒真前提）。
    let reject = match offset.checked_add(size) {
        None => true,                      // 溢出 → map_dma_fd 返 Err
        Some(end) => end > backing_bytes,  // is_regular 恒真下的上界
    };

    // 4) 驱动 §22 核：map（含 fstat 决策）+ 成功逐页 volatile-touch（SIGBUS → ASan crash）。
    let result = vfio_user_transport::dma::__fuzz_map_and_touch(&fd, offset, size, input.writeable);

    // 5) oracle。size==0 单独短路（map_dma_fd 不挡、memmap2 len=0 可能 Err）。
    if size == 0 {
        // 仅要求"不 panic / 不 SIGBUS"——走到这里已满足（ASan 兜 SIGBUS）；不断言 is_ok。
        std::hint::black_box(&result);
        return;
    }
    assert_eq!(
        result.is_ok(),
        !reject,
        "§22 决策 oracle 失配: offset={offset:#x} size={size:#x} backing={backing_bytes:#x} \
         reject={reject} got_ok={} err={:?}",
        result.is_ok(),
        result.as_ref().err()
    );
}

fuzz_target!(|input: MapInput| {
    xtask_fuzz::init_tracing_if_repro();
    run(input);
});
