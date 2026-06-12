// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # trap-based CMB —— vfio-user transport 闭环 e2e（CMB-P3a）
//!
//! 验证 **trap-based Controller Memory Buffer** 的完整数据流在 vfio-user transport 上闭环：
//!
//! ```text
//! client（QEMU 角色）              server（本进程，真 NvmeController + VfioUserSession）
//!   REGION_WRITE(CMB BAR, off, data) ───────────────►  mmio_write(cmb.bir, off) → cmb.backing[off]
//!   REGION_READ (CMB BAR, off, len)  ───────────────►  mmio_read (cmb.bir, off) ← cmb.backing[off]
//! ```
//!
//! 这是 P3a 最重要的产出：用**真 `NvmeController`**（非 mock）+ **真 `VfioUserSession`**
//! 跑通 trap 模式 CMB 的 transport 全链路。
//!
//! ## e2e 边界
//!
//! 本 harness 是 **in-process**（`UnixStream::pair()`，server 在一个线程，client 在主线程），
//! **不跑真 QEMU**。它直接构造 `VfioUserSession::new(...)`，**跳过 vfio-user 握手**（与
//! transport crate 既有 session 测试同款）——焦点是 CMB BAR 的 region_info 暴露 + REGION_RW
//! 路由 + 命中真 controller backing 这条链。真 QEMU guest e2e（driver 把命令 data buffer 放
//! CMB → controller 服务）需 Windows/Hyper-V 或 QEMU vfio-user-pci，属 P3a 之外的真机验证
//! （`scripts/qemu_interop/`），本文件不覆盖。
//!
//! ## 角色：本进程是 server
//!
//! vfio-user 里 server = 设备实现方（被 QEMU 接管）。故本 harness 持真 `NvmeController`，
//! client（测试主线程）发 GET_REGION_INFO / REGION_READ / REGION_WRITE。

#![cfg(all(unix, feature = "vfio-user"))]

use nvme_firmware::controller::NvmeController;
use std::os::unix::net::UnixStream;
use std::thread;
use vfio_user_transport::VfioUserSession;
use vfio_user_transport::proto::Command;
use vfio_user_transport::proto::Header;
use vfio_user_transport::proto::RegionAccessPayload;
use vfio_user_transport::proto::RegionInfoPayload;
use vfio_user_transport::proto::decode_payload;
use vfio_user_transport::proto::region_flags;
use vfio_user_transport::{Negotiated, read_message, write_message};
use zerocopy::IntoBytes;

const CMB_SIZE: u64 = 2 * 1024 * 1024; // 2 MiB
const CMB_BIR: u8 = 2;

fn neg() -> Negotiated {
    Negotiated {
        client_major: 0,
        client_minor: 1,
        client_caps_json: String::new(),
    }
}

/// 构造一个启用了 CMB（2 MiB / BAR2）的真 NvmeController。CMB BAR 直访（REGION_RW）
/// 在 CMB advertise 后即服务（不要求 CMSE，见 firmware `is_cmb_bar` 设计注释）。
fn make_controller_with_cmb() -> NvmeController {
    let path = std::env::temp_dir().join(format!(
        "nvme_vfio_cmb_e2e_{}_{:?}.img",
        std::process::id(),
        std::thread::current().id()
    ));
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(1024 * 1024).unwrap();
    drop(f);
    let mut c =
        NvmeController::open(&[path.to_str().unwrap().to_string()], 0x1414, 0, &[]).unwrap();
    c.enable_cmb(CMB_SIZE, CMB_BIR).unwrap();
    c
}

/// **CMB-P3a e2e** — GET_REGION_INFO 对真 controller 的 CMB BAR（index=CMB_BIR）回
/// (READ|WRITE, CMB_SIZE) 且不置 FLAG_MMAP（trap 模式）。region index 从真 describe()
/// 派生（firmware describe() 在 cmb 启用时 push 一条 index=cmb.bir 的 BAR）。
#[test]
fn cmb_bar_region_info_from_real_controller() {
    let (server, mut client) = UnixStream::pair().unwrap();
    let mut ctrl = make_controller_with_cmb();
    // **NvmeController 含 `Box<dyn SharedRamRegion>` 非 Send** → controller 留在主线程
    // pump，client（只持 UnixStream，Send）放到 spawn 线程。
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
        (pl.index, pl.flags, pl.size)
    });

    let mut sess = VfioUserSession::new(server, neg());
    sess.pump_one(&mut ctrl).unwrap();

    let (idx, flags, size) = h.join().unwrap();
    assert_eq!(idx, CMB_BIR as u32);
    assert_eq!(
        flags,
        region_flags::READ | region_flags::WRITE,
        "CMB BAR = R/W"
    );
    assert_eq!(
        flags & region_flags::MMAP,
        0,
        "trap 模式不置 FLAG_MMAP（P4 才置）"
    );
    assert_eq!(size, CMB_SIZE, "size == cmb_size");
}

/// **CMB-P3a e2e（核心闭环 a）** — client 经 REGION_WRITE 写 CMB BAR → 命中真
/// controller 的 `cmb.backing`；再 REGION_READ 读回同字节。trap 全链路：vfio-user
/// 消息 ↔ 真 NvmeController mmio_read/write(cmb.bir) ↔ backing。
#[test]
fn cmb_bar_region_write_read_hits_real_backing() {
    let (server, mut client) = UnixStream::pair().unwrap();
    let mut ctrl = make_controller_with_cmb();
    // controller 留主线程 pump；client 在 spawn 线程发 WRITE + READ 并回传读到的字节。
    let h = thread::spawn(move || {
        let data: Vec<u8> = (0..16u8).map(|i| 0xA0u8.wrapping_add(i)).collect();
        let off = 0x1000u64;
        // ① REGION_WRITE 16 字节到 CMB BAR offset 0x1000。
        let wreq = RegionAccessPayload {
            offset: off,
            region: CMB_BIR as u32,
            count: data.len() as u32,
        };
        let mut wpl = Vec::new();
        wpl.extend_from_slice(wreq.as_bytes());
        wpl.extend_from_slice(&data);
        let whdr = Header::command(1, Command::RegionWrite, wpl.len() as u32);
        write_message(&mut client, &whdr, &wpl, &[]).unwrap();
        let _ = read_message(&mut client).unwrap();

        // ② REGION_READ 同 offset/len 读回。
        let rreq = RegionAccessPayload {
            offset: off,
            region: CMB_BIR as u32,
            count: data.len() as u32,
        };
        let rhdr = Header::command(2, Command::RegionRead, rreq.as_bytes().len() as u32);
        write_message(&mut client, &rhdr, rreq.as_bytes(), &[]).unwrap();
        let reply = read_message(&mut client).unwrap();
        let payload_off = core::mem::size_of::<RegionAccessPayload>();
        let body = reply.payload[payload_off..payload_off + data.len()].to_vec();
        (data, body)
    });

    let mut sess = VfioUserSession::new(server, neg());
    sess.pump_one(&mut ctrl).unwrap(); // WRITE
    sess.pump_one(&mut ctrl).unwrap(); // READ

    let (written, read_back) = h.join().unwrap();
    assert_eq!(
        read_back, written,
        "CMB BAR REGION_WRITE→READ 经真 controller backing 往返一致"
    );
}
