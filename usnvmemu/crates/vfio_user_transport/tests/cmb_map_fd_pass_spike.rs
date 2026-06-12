// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **CMB-P4 承重 spike** — 验证 map 模式唯一真承重点：**带 fd 的 GET_REGION_INFO
//! reply + 双端共享同一 memfd mmap**。
//!
//! map 模式数据流（server→client 方向，对称于 W3 的 client→server DMA_MAP）：
//! 1. server 建一个 memfd，写 marker；
//! 2. server 把该 memfd 经 SCM_RIGHTS 附在 `GET_REGION_INFO` reply 里（+FLAG_MMAP）；
//! 3. client 收 reply 解出 fd → `mmap(MAP_SHARED)` → 读到 marker（**证 server→client 可见**）；
//! 4. client 改 mmap → server 经自己的 mmap 看到（**证 client→server 可见**，零拷贝双向）。
//!
//! 这条机制绿了，map 模式的完整实现才成立；红了就停下报告（别硬建）。
//!
//! 注：本 spike 两端都用 server 端 `framing::{read_message,write_message}`（in-process
//! socketpair）。它专测**fd-pass + 共享 mmap 内核机制**，不测 client crate 的 async 路径
//! （那在完整 e2e 里测）。

use std::io::Write as _;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use vfio_user_transport::Command;
use vfio_user_transport::Header;
use vfio_user_transport::framing::read_message;
use vfio_user_transport::framing::write_message;
use vfio_user_transport::proto::RegionInfoPayload;
use vfio_user_transport::proto::decode_payload;
use vfio_user_transport::proto::region_flags;
use zerocopy::IntoBytes;

/// 建一个 `len` 字节的 memfd（全零），返回 OwnedFd。模拟 server 自持的 CMB backing。
fn make_memfd(len: usize) -> std::os::fd::OwnedFd {
    let fd = nix::sys::memfd::memfd_create(c"cmb-spike", nix::sys::memfd::MFdFlags::empty())
        .expect("memfd_create");
    let f = std::fs::File::from(fd);
    f.set_len(len as u64).expect("ftruncate memfd");
    std::os::fd::OwnedFd::from(f)
}

/// 把一个 fd 经 `MAP_SHARED` 映射成读写视图（spike 用，min 复刻 dma::map_dma_fd）。
fn mmap_shared(fd: std::os::fd::BorrowedFd<'_>, len: usize) -> memmap2::MmapMut {
    let mut opts = memmap2::MmapOptions::new();
    opts.len(len);
    // SAFETY: spike 内 fd 是本测试刚建的 memfd，已 set_len(len)，故 [0,len) 全程有页
    // backing（无 SIGBUS）；MAP_SHARED 后映射独立于 fd 存续。本测试单线程顺序访问，
    // 仅 u8 memcpy 无 typed UB。
    unsafe { opts.map_mut(fd.as_raw_fd()) }.expect("mmap MAP_SHARED")
}

const CMB_LEN: usize = 4096;
const MARKER_OFF: usize = 0x40;
const SERVER_MARKER: u64 = 0xDEAD_BEEF_CAFE_BABE;
const CLIENT_MARKER: u64 = 0x0102_0304_0506_0708;

/// **spike 主体** — 端到端验证 fd-pass region reply + 共享 mmap 双向可见。
#[test]
fn cmb_map_region_info_fd_pass_shared_mmap() {
    let (mut server, mut client) = UnixStream::pair().unwrap();

    // ── server 侧 ──────────────────────────────────────────────────────────
    // ① 建 memfd backing，server 经自己的 mmap 写 marker（marker 入共享页）。
    let server_fd = make_memfd(CMB_LEN);
    let mut server_view = mmap_shared(server_fd.as_fd(), CMB_LEN);
    server_view[MARKER_OFF..MARKER_OFF + 8].copy_from_slice(&SERVER_MARKER.to_le_bytes());
    server_view.flush().ok();

    // ② server 在另一线程发 GET_REGION_INFO reply（+FLAG_MMAP）并经 SCM_RIGHTS 附 fd。
    let server_fd_raw = server_fd.as_raw_fd();
    let srv = std::thread::spawn(move || {
        let pl = RegionInfoPayload {
            argsz: core::mem::size_of::<RegionInfoPayload>() as u32,
            flags: region_flags::READ | region_flags::WRITE | region_flags::MMAP,
            index: 2,
            cap_offset: 0,
            size: CMB_LEN as u64,
            offset: 0,
        };
        let hdr = Header::reply_ok(1, Command::DeviceGetRegionInfo, pl.as_bytes().len() as u32);
        // **承重点**：reply 带 fd（现 session.rs 的 reply() helper 不带 fd，故完整实现
        // 需扩展或直调 write_message(.., &[fd])——此处正是验证该底层能力可用）。
        write_message(&mut server, &hdr, pl.as_bytes(), &[server_fd_raw]).expect("reply with fd");
        // 把 server_view + fd move 进线程，等 client 改完后回读（join 前不 drop）。
        // 用一个 channel 等 client 通知"已写回"。
        server
    });

    // ── client 侧 ──────────────────────────────────────────────────────────
    // ③ client 收 reply，解出 FLAG_MMAP + fd。
    let reply = read_message(&mut client).expect("client read reply");
    let info: RegionInfoPayload = decode_payload(&reply.payload).expect("decode region info");
    // packed 字段先 copy 到本地（#[repr(C,packed)]，E0793）。
    let (info_flags, info_size) = (info.flags, info.size);
    assert_eq!(
        info_flags & region_flags::MMAP,
        region_flags::MMAP,
        "reply 应置 FLAG_MMAP"
    );
    assert_eq!(
        reply.fds.len(),
        1,
        "reply 应附带恰好 1 个 region fd（SCM_RIGHTS）"
    );
    assert_eq!(info_size, CMB_LEN as u64);

    let client_fd = &reply.fds[0];
    let mut client_view = mmap_shared(client_fd.as_fd(), CMB_LEN);

    // ④ client 经 mmap 读到 server 写的 marker（**server→client 零拷贝可见**）。
    let seen = u64::from_le_bytes(client_view[MARKER_OFF..MARKER_OFF + 8].try_into().unwrap());
    assert_eq!(
        seen, SERVER_MARKER,
        "client mmap 应读到 server 写入的 marker（共享页）"
    );

    // ⑤ client 改 marker（写回共享页）。
    client_view[MARKER_OFF..MARKER_OFF + 8].copy_from_slice(&CLIENT_MARKER.to_le_bytes());
    client_view.flush().ok();

    // ── server 回读 ────────────────────────────────────────────────────────
    let mut server = srv.join().unwrap();
    // server 重新经自己的 mmap 读（同一物理页），应看到 client 写的 marker。
    let server_view2 = mmap_shared(server_fd.as_fd(), CMB_LEN);
    let seen_by_server =
        u64::from_le_bytes(server_view2[MARKER_OFF..MARKER_OFF + 8].try_into().unwrap());
    assert_eq!(
        seen_by_server, CLIENT_MARKER,
        "server 经自己的 mmap 应看到 client 写入的 marker（**client→server 零拷贝可见**）"
    );

    // 收尾：drop server stream（避免未用告警）。
    let _ = server.write(&[]);
}
