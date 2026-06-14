// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **§22 B wire-level DMA-map fuzz target** —— 经**真 socketpair + SCM_RIGHTS + 真 wire `Message`**
//! 驱 `vfio_user_transport::dma::handle_dma_map`（pub wire-level DMA_MAP 处理器），再驱
//! `mmap_read`/`mmap_write` 的 **region-relative 边界**。见 `docs/plans/2026-06-14-section22-mmap-fuzz.md`（§22 B 段）。
//!
//! **区别于 §22 A**（`fuzz_map_dma_fd` 经 `__fuzz_map_and_touch` **直驱** `map_dma_fd`，只守 mmap
//! 的 backing 完整性 / SIGBUS 本体）：A 跳过了整个 wire 层。本 target 补 A 漏的 3 缺口：
//!   1. **`handle_dma_map` wire 解析 + 校验**：`decode_payload` + 3 个 reject 分支（size==0 /
//!      perms==0 / `addr+size` checked_add 溢出）+ `table.insert`。
//!   2. **`checked_add` 溢出分支**（`pl.addr + pl.size`，dma.rs:386）——A 直驱下 addr 不在路径上、不可达。
//!   3. **`mmap_read`/`mmap_write` 的 region-relative 边界（SAFETY #3）**：即便 map 本身（fstat）正确，
//!      `gpa`/`len` 越过 region 内偏移仍可能 OOB——`off = gpa - region.addr` + `off+len ≤ 真实映射长度
//!      bytes.len()` 的校验是否正确，是 A 完全未覆盖的面。经 `fuzzing`-feature 的 `__fuzz_mmap_access`
//!      直驱（不走 `dma_read_sync`/`write_sync`：那俩在 mmap miss 时回退 wire 阻塞等 client → fuzz hang）。
//!
//! **驱动形状**：fuzzer 控 DMA_MAP 的 `(flags, addr, size, offset)` + memfd 真实页数 + 是否带 fd +
//! no_reply + 一串 access `(gpa=addr+delta, len, write?)`。`size` 由 `backing-offset + size_delta`
//! 导出（把采样压在 fstat accept/reject + region 长度边界，delta≤0 → mmap-backed 可达 → 行使 gap-3；
//! delta>0 → fstat reject → Message-backed）。`addr` 全 fuzz（大 addr → gap-2 溢出分支可达）。access
//! 的 `gpa=addr.wrapping_add(delta)` 把读写采样压在 region 边界附近（off-by-one 最可能藏处）。
//!
//! **单线程无阻塞**：payload 仅 32B（`DmaMapPayload`），client write 先全进 socketpair 缓冲、server
//! `read_message` 再读（数据已在缓冲、不阻塞）；`handle_dma_map` 只写小 reply（缓冲，不读 stream）；
//! `__fuzz_mmap_access` 直驱 mmap 层无 wire。每输入新建 socketpair、结束即 drop，无跨迭代堆积。
//!
//! **oracle（observable-only + ASan）**：对**任意** wire 字节/fd/access，只能正常返或 `Err`/`None`，
//! **绝不 panic / abort / OOB / SIGBUS / hang**。`mmap_read` 内 `bytes[off..end].to_vec()` 是真读——
//! 边界校验错则 Rust slice panic（libfuzzer catch）或裸 OOB（ASan catch）。**不加语义自洽断言**
//! （避自证陷阱：harness 与被测共享同一套偏移算术，断言"读到的字节对"会两边一起错）。

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use vfio_user_transport::proto::DmaMapPayload;
use vfio_user_transport::{Command, DmaTable, Header, read_message, write_message};
use xtask_fuzz::fuzz_target;
use zerocopy::IntoBytes;

const PAGE: u64 = 4096;
/// memfd 页数上限（避免 OOM；64 页 = 256 KiB 足够压边界）。
const MAX_BACKING_PAGES: u8 = 64;
/// access 列表上限（W-2 限幅，防 Arbitrary 生成超长向量）。
const MAX_ACCESSES: usize = 16;
/// 单次 mmap_write 数据上限（4 页；够压 region 边界、不炸内存）。
const MAX_WRITE_LEN: usize = 4 * PAGE as usize;

#[derive(Arbitrary, Debug)]
struct WireMapInput {
    /// `argsz`（spec 兼容字段）。当前 `handle_dma_map` 只按 `payload.len()` 校验、不读 argsz，故此字段
    /// 现为 dead——但**fuzz 打 wire 契约而非当前实现**（[[fuzz-the-contract-not-current-impl]]）：契约里它在，
    /// 就喂 fuzzer，未来若 spec 让 argsz 参与校验即自动覆盖该分支。
    argsz: u32,
    /// DMA_MAP flags（bit0=READABLE bit1=WRITEABLE + 高位 garbage）——压 perms==0 reject + 各权限组合。
    flags: u32,
    /// region IOVA（全 fuzz；大 addr → `addr+size` checked_add 溢出 reject 分支可达）。
    addr: u64,
    /// memfd ftruncate 到 N 页（真实 backing）。
    backing_pages: u8,
    /// mmap offset 页数（压到 [0, backing] 内，page-aligned）。
    offset_pages: u16,
    /// region size 相对 `backing-offset` 的偏移：size = max(0, (backing-offset) + delta)。delta≤0 →
    /// fstat 通过 → mmap-backed（行使 mmap_read/write 边界）；delta>0 → fstat reject → Message-backed。
    size_delta: i32,
    /// 是否经 SCM_RIGHTS 带 memfd（否 → Message-backed，mmap_read/write 恒 None）。
    attach_fd: bool,
    /// posted MAP（NO_REPLY）：handle_dma_map 跳过 reply 写。
    no_reply: bool,
    /// map 后的 region-relative 读写 access。
    accesses: Vec<Access>,
}

#[derive(Arbitrary, Debug)]
struct Access {
    /// gpa = addr.wrapping_add(delta as u64)：把采样压在 region 边界附近。
    delta: i32,
    /// mmap_read 长度（mmap_write 用 data.len()）。
    len: u16,
    /// Some → mmap_write（data 截到 MAX_WRITE_LEN）；None → mmap_read。
    write: Option<Vec<u8>>,
}

fn run(mut input: WireMapInput) {
    input.accesses.truncate(MAX_ACCESSES);

    let backing_pages = (input.backing_pages % (MAX_BACKING_PAGES + 1)) as u64;
    let backing_bytes = backing_pages * PAGE;
    let offset = if backing_pages == 0 {
        0
    } else {
        ((input.offset_pages as u64) % (backing_pages + 1)) * PAGE
    };
    // size 压在 fstat-accept(≤backing) / region 长度边界附近；delta>0 越过 → fstat reject。
    let size = ((backing_bytes as i64 - offset as i64) + input.size_delta as i64).max(0) as u64;
    let addr = input.addr;

    // 1) memfd + ftruncate 真实 backing（仅 attach_fd 时）。
    let memfd = if input.attach_fd {
        match nix::sys::memfd::memfd_create(c"s22b", nix::sys::memfd::MFdFlags::empty()) {
            Ok(fd) => {
                if nix::unistd::ftruncate(&fd, backing_bytes as i64).is_err() {
                    return;
                }
                Some(fd)
            }
            Err(_) => return,
        }
    } else {
        None
    };

    // 2) socketpair（client 写 wire、server 收并 handle）。
    let (mut client, mut server) = match UnixStream::pair() {
        Ok(p) => p,
        Err(_) => return,
    };

    // 3) client 写真 DMA_MAP wire（header + DmaMapPayload 字节 + 可选 memfd via SCM_RIGHTS）。
    let pl = DmaMapPayload {
        argsz: input.argsz,
        flags: input.flags,
        offset,
        addr,
        size,
    };
    let bytes = pl.as_bytes();
    let hdr = Header::command(1, Command::DmaMap, bytes.len() as u32);
    let fds: Vec<std::os::fd::RawFd> = match &memfd {
        Some(fd) => vec![fd.as_raw_fd()],
        None => vec![],
    };
    if write_message(&mut client, &hdr, bytes, &fds).is_err() {
        return;
    }

    // 4) server 收 → 真 wire 解析（含 SCM_RIGHTS fd recv）→ handle_dma_map（gap-1/gap-2）。
    let mut msg = match read_message(&mut server) {
        Ok(m) => m,
        Err(_) => return,
    };
    let mut table = DmaTable::default();
    let msg_id = msg.header.msg_id;
    let _ = vfio_user_transport::dma::handle_dma_map(
        &mut server,
        &mut table,
        msg_id,
        &mut msg,
        input.no_reply,
    );
    // memfd 在此后可 drop：mmap(MAP_SHARED) 独立于 fd 存续；server 侧 fd 已在 msg.fds（用完 drop）。
    drop(memfd);

    // 5) region-relative access（gap-3：mmap_read/write 边界）。gpa 压在 region 边界附近。
    for a in &input.accesses {
        let gpa = addr.wrapping_add(a.delta as i64 as u64);
        match &a.write {
            Some(data) => {
                let n = data.len().min(MAX_WRITE_LEN);
                vfio_user_transport::dma::__fuzz_mmap_access(&mut table, gpa, 0, Some(&data[..n]));
            }
            None => {
                vfio_user_transport::dma::__fuzz_mmap_access(&mut table, gpa, a.len as u32, None);
            }
        }
    }
}

fuzz_target!(|input: WireMapInput| {
    xtask_fuzz::init_tracing_if_repro();
    run(input);
});
