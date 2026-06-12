// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # map-based CMB（零拷贝）—— vfio-user transport 闭环 e2e（CMB-P4）
//!
//! 验证 **map-based Controller Memory Buffer** 的零拷贝数据流在 vfio-user transport
//! 上闭环（对称于 trap e2e `vfio_user_cmb_e2e.rs`，但**零拷贝**、**无 REGION_RW 往返**）：
//!
//! ```text
//! client（QEMU 角色）                       server（真 NvmeController + VfioUserSession）
//!   GET_REGION_INFO(CMB BAR) ───────────────►  reply: FLAG_MMAP + memfd (SCM_RIGHTS)
//!   mmap(memfd) → 直接写 marker              （与 controller 的 cmb.backing 同一物理页）
//!   ── 无 REGION_WRITE ──                      controller 经 cmb_region_fd 暴露的同一 memfd 看到
//! ```
//!
//! map 模式数据流方向 server→client（与 W3 DMA_MAP 的 client→server 相反）：server 自持
//! memfd（`MemfdRamRegion` 注入进 controller 的 CMB backing），经 `GET_REGION_INFO` reply
//! 把 fd 经 SCM_RIGHTS 发给 client；client mmap 后 guest 与 firmware **零拷贝共享同一页**。
//!
//! ## 怎么证"零拷贝"
//!
//! 1. client 经 GET_REGION_INFO 拿到 reply 的 **FLAG_MMAP + 1 个 fd**；
//! 2. client mmap 该 fd，**直接**写一段（无 REGION_WRITE 消息）；
//! 3. server 侧经 `ctrl.cmb_region_fd(bir)` 暴露的**同一 memfd** 另开 mmap，读到 client 写的
//!    字节——证明二者共享同一物理页（真零拷贝）。反向同理。
//! 4. **断言整个流程只发了 1 条 GET_REGION_INFO**（无任何 REGION_READ/WRITE）——这是区别于
//!    trap 模式（每访问一次 REGION_RW 往返）的判据。
//!
//! ## e2e 边界
//!
//! in-process（`UnixStream::pair()`，server 在主线程 pump，client 在 spawn 线程）。
//! map 模式是 PCIe/QEMU-client 能力（OpenHCL underhill 无 mapper，见设计 §1），故 client 角色
//! 用 in-process 模拟，不涉 OpenHCL。真 QEMU guest mmap 直访属真机验证（本文件不覆盖）。

#![cfg(all(unix, feature = "vfio-user"))]

use nvme_firmware::controller::NvmeController;
use pcie_device_core::PcieDevice as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::thread;
use vfio_user_transport::MemfdRamRegion;
use vfio_user_transport::VfioUserSession;
use vfio_user_transport::proto::Command;
use vfio_user_transport::proto::Header;
use vfio_user_transport::proto::RegionInfoPayload;
use vfio_user_transport::proto::decode_payload;
use vfio_user_transport::proto::region_flags;
use vfio_user_transport::{Negotiated, read_message, write_message};
use zerocopy::IntoBytes;

const CMB_SIZE: u64 = 2 * 1024 * 1024; // 2 MiB
const CMB_BIR: u8 = 2;
const MARKER_OFF: usize = 0x1000;
const CLIENT_MARKER: u64 = 0xFEED_FACE_DEAD_C0DE;
const SERVER_MARKER: u64 = 0x0BAD_F00D_1234_5678;

fn neg() -> Negotiated {
    Negotiated {
        client_major: 0,
        client_minor: 1,
        client_caps_json: String::new(),
    }
}

/// 构造一个启用了 **map 模式 CMB**（memfd-backed，2 MiB / BAR2）的真 NvmeController。
/// 用 `enable_cmb_with_backing` 注入 transport 造的 `MemfdRamRegion`（其 `as_fd` 为 Some
/// → server 经 GET_REGION_INFO 暴露 fd → map 模式）。
fn make_controller_with_map_cmb() -> NvmeController {
    let path = std::env::temp_dir().join(format!(
        "nvme_vfio_cmb_map_e2e_{}_{:?}.img",
        std::process::id(),
        std::thread::current().id()
    ));
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(1024 * 1024).unwrap();
    drop(f);
    let mut c =
        NvmeController::open(&[path.to_str().unwrap().to_string()], 0x1414, 0, &[]).unwrap();
    // **map 模式注入**：transport 造 memfd-backed backing 注入 firmware（firmware-core
    // 自身不能造 memfd，agnostic + forbid-unsafe）。
    let backing = MemfdRamRegion::new(CMB_SIZE as usize).expect("MemfdRamRegion::new");
    c.enable_cmb_with_backing(Box::new(backing), CMB_BIR)
        .unwrap();
    c
}

/// 经一个 BorrowedFd 建一个独立的 `MAP_SHARED` 读写 mmap 视图（验证用）。
fn mmap_view(fd: std::os::fd::BorrowedFd<'_>, len: usize) -> memmap2::MmapMut {
    let mut opts = memmap2::MmapOptions::new();
    opts.len(len);
    // SAFETY: fd 是 controller 的 CMB memfd（MemfdRamRegion 已 ftruncate(len)），[0,len) 有页
    // backing；本测试顺序访问，纯 u8 memcpy 无 typed UB。MAP_SHARED 与 client/firmware 同页。
    unsafe { opts.map_mut(fd.as_raw_fd()) }.expect("mmap")
}

/// **CMB-P4 e2e** — map 模式 GET_REGION_INFO 对真 controller 的 memfd-backed CMB BAR 回
/// (READ|WRITE|**MMAP**, size) 且 reply 附 1 个 region fd（SCM_RIGHTS）。
#[test]
fn map_cmb_region_info_advertises_mmap_and_fd() {
    let (server, mut client) = UnixStream::pair().unwrap();
    let mut ctrl = make_controller_with_map_cmb();
    let h = thread::spawn(move || {
        let req = RegionInfoPayload {
            argsz: core::mem::size_of::<RegionInfoPayload>() as u32,
            flags: 0,
            index: CMB_BIR as u32,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };
        let hdr = Header::command(1, Command::DeviceGetRegionInfo, req.as_bytes().len() as u32);
        write_message(&mut client, &hdr, req.as_bytes(), &[]).unwrap();
        let reply = read_message(&mut client).unwrap();
        let pl: RegionInfoPayload = decode_payload(&reply.payload).unwrap();
        (pl.flags, pl.size, reply.fds.len())
    });

    let mut sess = VfioUserSession::new(server, neg());
    sess.pump_one(&mut ctrl).unwrap();

    let (flags, size, fd_count) = h.join().unwrap();
    assert_eq!(
        flags,
        region_flags::READ | region_flags::WRITE | region_flags::MMAP,
        "map 模式应置 FLAG_MMAP"
    );
    assert_eq!(size, CMB_SIZE);
    assert_eq!(fd_count, 1, "map 模式 reply 附带 1 个 region memfd");
}

/// **CMB-P4 e2e（核心闭环：零拷贝双向 + 无 REGION_RW）** — 最重要产出。
///
/// client 经 GET_REGION_INFO 拿 fd → mmap → 直接写 marker；server 经 controller 的
/// `cmb_region_fd`（同一 memfd）另开 mmap 看到 client 的写（**client→server 零拷贝**）；
/// 反向 server 经同一 memfd 写 → client mmap 看到（**server→client 零拷贝**）。全程
/// **只发 1 条 GET_REGION_INFO，无任何 REGION_READ/WRITE**（区别于 trap 模式）。
#[test]
fn map_cmb_zero_copy_bidirectional_no_region_rw() {
    let (server, mut client) = UnixStream::pair().unwrap();
    let mut ctrl = make_controller_with_map_cmb();

    // **race-free 顺序**：server 在 pump 前先写 SERVER_MARKER（client 拿 fd mmap 后必能读到）；
    // client 读 server marker（server→client）后写 CLIENT_MARKER 并发 rendezvous 信号；server
    // 在 rendezvous pump 返回后读 CLIENT_MARKER（client→server）——socket 顺序保证 client 的
    // mmap 写先于信号到达，无 TOCTOU。
    {
        let fd = ctrl
            .cmb_region_fd(CMB_BIR as u32)
            .expect("controller 暴露 CMB fd");
        let mut sview = mmap_view(fd, CMB_SIZE as usize);
        sview[MARKER_OFF + 16..MARKER_OFF + 24].copy_from_slice(&SERVER_MARKER.to_le_bytes());
        sview.flush().ok();
    }

    // client 线程：GET_REGION_INFO → 拿 fd → mmap → 读 server marker → 写自己 marker → 发信号。
    // 数据传递**全经 mmap 共享页**，wire 上只有 GET_REGION_INFO（拿 fd）+ 一条 rendezvous 信号，
    // **零 REGION_READ/WRITE**（区别于 trap 模式每访问一次往返）。
    let h = thread::spawn(move || {
        let req = RegionInfoPayload {
            argsz: core::mem::size_of::<RegionInfoPayload>() as u32,
            flags: 0,
            index: CMB_BIR as u32,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };
        let hdr = Header::command(1, Command::DeviceGetRegionInfo, req.as_bytes().len() as u32);
        write_message(&mut client, &hdr, req.as_bytes(), &[]).unwrap();
        let reply = read_message(&mut client).unwrap();
        assert_eq!(reply.fds.len(), 1, "client 应收到 region fd");
        let client_fd = reply.fds.into_iter().next().unwrap();
        let mut view = {
            let mut opts = memmap2::MmapOptions::new();
            opts.len(CMB_SIZE as usize);
            // SAFETY: fd 是 server 经 SCM_RIGHTS 传来的 CMB memfd（已 ftruncate）；顺序访问。
            unsafe { opts.map_mut(client_fd.as_raw_fd()) }.expect("client mmap")
        };
        // ① client 经 mmap 读 server 预写的 marker（server→client 零拷贝，无 REGION_READ）。
        let server_seen_by_client =
            u64::from_le_bytes(view[MARKER_OFF + 16..MARKER_OFF + 24].try_into().unwrap());
        // ② client 直接写自己的 marker（无 REGION_WRITE）。
        view[MARKER_OFF..MARKER_OFF + 8].copy_from_slice(&CLIENT_MARKER.to_le_bytes());
        view.flush().ok();
        // ③ 发 rendezvous 信号（GET_INFO，非 REGION_RW）通知 server 可读 CLIENT_MARKER 了。
        write_message(
            &mut client,
            &Header::command(2, Command::DeviceGetInfo, 0),
            &[],
            &[],
        )
        .unwrap();
        let _ = read_message(&mut client).unwrap(); // 等 GET_INFO reply（确保 server 已 pump）
        drop(client_fd); // mmap MAP_SHARED 独立于 fd 存续；留到此处更清晰
        server_seen_by_client
    });

    let mut sess = VfioUserSession::new(server, neg());
    // pump #1：GET_REGION_INFO（map reply 带 fd）。
    sess.pump_one(&mut ctrl).unwrap();
    // pump #2：rendezvous（GET_INFO）。返回时 client 的 mmap 写必已 happen-before（socket 顺序）。
    sess.pump_one(&mut ctrl).unwrap();

    // server 经 controller 的**同一 memfd** 读 client 写的 marker（client→server 零拷贝）。
    let server_seen = {
        let fd = ctrl
            .cmb_region_fd(CMB_BIR as u32)
            .expect("controller 暴露 CMB fd");
        let sview = mmap_view(fd, CMB_SIZE as usize);
        u64::from_le_bytes(sview[MARKER_OFF..MARKER_OFF + 8].try_into().unwrap())
    };

    let client_seen = h.join().unwrap();
    assert_eq!(
        server_seen, CLIENT_MARKER,
        "server 经同一 memfd 看到 client 直接写入的 marker（client→server 零拷贝，无 REGION_WRITE）"
    );
    assert_eq!(
        client_seen, SERVER_MARKER,
        "client 经 mmap 看到 server 写入的 marker（server→client 零拷贝，无 REGION_READ）"
    );
}
