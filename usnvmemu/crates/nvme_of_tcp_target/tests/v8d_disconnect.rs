// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V8d 集成测试** — Disconnect (fctype=0x08) 真清 + V2Session Drop sweep。
//!
//! V8d 把 V2 stub "ack + close" 升级为 spec § 3.5/§ 7.6.1 完整路径：
//! 1. 校验 RECFMT=0；否则 SC=0x80 INVALID_CONNECT_FORMAT
//! 2. controller-side IO queue 真删（先 SQ 后 CQ）
//! 3. session.io_queues 镜像清
//! 4. 回 SC=0 success
//! 5. bail → V2Session Drop 触发 AER cleanup (V8c) + 二次 IO queue sweep
//!
//! 覆盖：
//! 1. v8d_disconnect_clears_session_io_queues —— Disconnect 后 controller cqs/sqs
//!    + session.io_queues 都空
//! 2. v8d_disconnect_invalid_recfmt_rejected —— RECFMT=1 应返 SC=0x80 + bail（H-1）
//! 3. v8d_session_drop_without_disconnect_still_cleans —— peer close 路径 Drop
//!    sweep IO queues
//! 4. v8d_disconnect_then_reconnect_qid_reusable —— 拆 qid=1 后另一 conn 重 create
//! 5. v8d_aer_hard_cap_enforced —— per-conn cap=8（H-4 cross-tenant 防御）+
//!    全局 cap=256（H-3 fire_aen O(n) DoS 防御）

#![allow(missing_docs)]

use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::{read_pdu, write_pdu};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{SharedControllerInner, V2Session};
use pcie_remote_nvme_userspace::NvmeController;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use zerocopy::{FromZeros, IntoBytes};

fn make_backing(size: u64) -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(size).unwrap();
    f
}

fn make_shared(backing: &std::path::Path) -> Arc<SharedControllerInner> {
    let c =
        NvmeController::open(&[backing.to_string_lossy().into_owned()], 0x1414, 0, &[]).unwrap();
    Arc::new(SharedControllerInner::new(c))
}

fn alloc_port() -> (TcpListener, u16) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    (l, p)
}

fn send_icreq(s: &mut TcpStream) {
    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let psh = IcPsh::default();
    write_pdu(s, &hdr, psh.as_bytes(), &[]).unwrap();
}

fn send_connect_admin(s: &mut TcpStream, cid: u16) {
    let mut sqe = [0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::CONNECT;
    let f = ConnectFabricFields {
        recfmt: 0,
        qid: 0,
        sqsize: 31,
        cattr: 0,
        rsvd1: 0,
        kato: 60_000,
        rsvd2: [0u8; 12],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let cd = ConnectData::default();
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 72,
        plen: 72 + 1024,
    };
    write_pdu(s, &hdr, &sqe, cd.as_bytes()).unwrap();
}

fn send_create_io_cq_sq_then_connect_qid1(s: &mut TcpStream, cid_base: u16) {
    // Create IO CQ qid=1, qsize-1=15, cdw11 PC=1
    let mut sqe = [0u8; 64];
    sqe[0] = 0x05;
    sqe[2..4].copy_from_slice(&(cid_base + 1).to_le_bytes());
    sqe[40..44].copy_from_slice(&(1u32 | (15u32 << 16)).to_le_bytes());
    sqe[44..48].copy_from_slice(&0x0000_0001u32.to_le_bytes());
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    write_pdu(s, &hdr, &sqe, &[]).unwrap();
    let _ = read_pdu(s).unwrap();
    // Create IO SQ qid=1, qsize-1=15, cdw11 CQ id=1 + PC=1
    let mut sqe = [0u8; 64];
    sqe[0] = 0x01;
    sqe[2..4].copy_from_slice(&(cid_base + 2).to_le_bytes());
    sqe[40..44].copy_from_slice(&(1u32 | (15u32 << 16)).to_le_bytes());
    sqe[44..48].copy_from_slice(&0x0001_0001u32.to_le_bytes());
    write_pdu(s, &hdr, &sqe, &[]).unwrap();
    let _ = read_pdu(s).unwrap();
    // Connect qid=1
    let mut sqe = [0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&(cid_base + 3).to_le_bytes());
    sqe[4] = fctype::CONNECT;
    let f = ConnectFabricFields {
        recfmt: 0,
        qid: 1,
        sqsize: 15,
        cattr: 0,
        rsvd1: 0,
        kato: 0,
        rsvd2: [0u8; 12],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let cd = ConnectData::default();
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 72,
        plen: 72 + 1024,
    };
    write_pdu(s, &hdr, &sqe, cd.as_bytes()).unwrap();
    let _ = read_pdu(s).unwrap();
}

fn send_disconnect(s: &mut TcpStream, cid: u16, recfmt: u16) {
    let mut sqe = [0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::DISCONNECT;
    let f = fabric::DisconnectFabricFields {
        recfmt,
        rsvd: [0u8; 22],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    write_pdu(s, &hdr, &sqe, &[]).unwrap();
}

fn sc_of(psh: &[u8]) -> u8 {
    ((u16::from_le_bytes(psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8
}

fn run_full_setup(c: &mut TcpStream, cid_base: u16) {
    send_icreq(c);
    let _ = read_pdu(c).unwrap();
    send_connect_admin(c, cid_base);
    let _ = read_pdu(c).unwrap();
    send_create_io_cq_sq_then_connect_qid1(c, cid_base);
}

/// 起一条 server pump_one × N 的 session worker。
fn spawn_session(
    shared: Arc<SharedControllerInner>,
    pumps: usize,
) -> (TcpStream, thread::JoinHandle<()>) {
    let (l, p) = alloc_port();
    let h = thread::spawn(move || {
        let (server, _) = l.accept().unwrap();
        let mut sess = V2Session::accept_and_handshake_shared(server, shared).unwrap();
        for _ in 0..pumps {
            // Disconnect / peer close 路径 bail；视为正常 exit。
            let cont = sess.pump_one().unwrap_or_default();
            if !cont {
                break;
            }
        }
    });
    let client = TcpStream::connect(("127.0.0.1", p)).unwrap();
    (client, h)
}

// ─── Tests ────────────────────────────────────────────────────────────

#[test]
fn v8d_disconnect_clears_session_io_queues() {
    let backing = make_backing(1024 * 1024);
    let shared = make_shared(backing.path());

    // setup 5 + Disconnect 1 = 6
    let (mut c, h) = spawn_session(Arc::clone(&shared), 6);
    run_full_setup(&mut c, 0x0100);

    // 现在 controller cqs/sqs 应含 qid=0 admin + qid=1 IO
    {
        let ctrl = shared.controller.lock();
        assert_eq!(
            ctrl.nvme_list_io_sqs(),
            vec![1],
            "Disconnect 前 IO SQ qid=1 存在"
        );
        assert_eq!(ctrl.nvme_list_io_cqs(), vec![1]);
    }

    send_disconnect(&mut c, 0x0110, /*recfmt*/ 0);
    let r = read_pdu(&mut c).unwrap();
    assert_eq!(r.header.pdu_type, pdu_type::RSP);
    assert_eq!(sc_of(&r.psh), 0, "Disconnect 应 success");
    drop(c);
    h.join().unwrap();

    // controller 端 IO SQ/CQ 应全清（admin qid=0 保留）
    let ctrl = shared.controller.lock();
    assert!(
        ctrl.nvme_list_io_sqs().is_empty(),
        "Disconnect 后 IO SQ 应空，得 {:?}",
        ctrl.nvme_list_io_sqs()
    );
    assert!(
        ctrl.nvme_list_io_cqs().is_empty(),
        "Disconnect 后 IO CQ 应空，得 {:?}",
        ctrl.nvme_list_io_cqs()
    );
    assert!(ctrl.nvme_has_admin_cq(), "admin CQ qid=0 应保留");
}

#[test]
fn v8d_disconnect_invalid_recfmt_rejected() {
    let backing = make_backing(1024 * 1024);
    let shared = make_shared(backing.path());

    // setup 5 + Disconnect bad 1 = 6
    // **V8d reviewer H-1** — RECFMT 非法后 session 立即 bail → IO queue 被
    // Drop sweep 拆掉。验：(a) wire 回 SC=0x80；(b) sweep 后 IO queue 空。
    let (mut c, h) = spawn_session(Arc::clone(&shared), 6);
    run_full_setup(&mut c, 0x0200);

    send_disconnect(&mut c, 0x0210, /*recfmt*/ 1);
    let r = read_pdu(&mut c).unwrap();
    let sc = sc_of(&r.psh);
    assert_eq!(
        sc, 0x80,
        "RECFMT=1 应回 SC=0x80 INVALID_CONNECT_FORMAT，得 {sc:#x}"
    );

    // session 应已 bail；client close 让 server thread 退出
    drop(c);
    h.join().unwrap();

    // Drop sweep 应拆 IO queue（H-1 fix 后不再"reject 留状态"）
    let ctrl = shared.controller.lock();
    assert!(
        ctrl.nvme_list_io_sqs().is_empty(),
        "H-1 fix 后 reject 也 bail → Drop sweep 应清 IO SQ"
    );
}

#[test]
fn v8d_session_drop_without_disconnect_still_cleans() {
    let backing = make_backing(1024 * 1024);
    let shared = make_shared(backing.path());

    let (mut c, h) = spawn_session(Arc::clone(&shared), 5);
    run_full_setup(&mut c, 0x0300);

    {
        let ctrl = shared.controller.lock();
        assert_eq!(ctrl.nvme_list_io_sqs(), vec![1]);
    }

    // 不发 Disconnect，直接 close → pump_one 返 false → Drop sweep
    drop(c);
    h.join().unwrap();

    let ctrl = shared.controller.lock();
    assert!(
        ctrl.nvme_list_io_sqs().is_empty(),
        "peer close 路径 Drop 应清干净 IO SQ"
    );
    assert!(ctrl.nvme_list_io_cqs().is_empty());
}

#[test]
fn v8d_disconnect_then_reconnect_qid_reusable() {
    let backing = make_backing(1024 * 1024);
    let shared = make_shared(backing.path());

    // === Conn A: setup + Disconnect ===
    let (mut a, ha) = spawn_session(Arc::clone(&shared), 6);
    run_full_setup(&mut a, 0x0400);
    send_disconnect(&mut a, 0x0410, 0);
    let _ = read_pdu(&mut a).unwrap();
    drop(a);
    ha.join().unwrap();

    {
        let ctrl = shared.controller.lock();
        assert!(
            ctrl.nvme_list_io_sqs().is_empty(),
            "A Disconnect 后 IO SQ 应空"
        );
    }

    // === Conn B: 重新 create qid=1 应 success（验 qid 复用无残留 collision）===
    let (mut b, hb) = spawn_session(Arc::clone(&shared), 5);
    run_full_setup(&mut b, 0x0500);
    let ctrl = shared.controller.lock();
    assert_eq!(
        ctrl.nvme_list_io_sqs(),
        vec![1],
        "Conn B 复用 qid=1 应 success"
    );
    drop(ctrl);
    drop(b);
    hb.join().unwrap();
}

#[test]
fn v8d_aer_hard_cap_enforced() {
    let backing = make_backing(1024 * 1024);
    let shared = make_shared(backing.path());

    // **V8d reviewer H-4** — 双层 cap：
    //   - per-conn 8（防 cross-tenant 灌满）
    //   - 全 controller 256（防 fire_aen O(n) DoS）
    // 本 test 验 per-conn cap：单 conn 第 9 条 AER 应返 SC=0x05 reject。
    {
        let mut c = shared.controller.lock();
        let _ = c.nvme_install_admin_cq(
            nvme_of_tcp_target::CQ_BASE_GPA,
            nvme_of_tcp_target::ADMIN_CQ_SIZE,
        );
    }
    struct NullTransport;
    impl pcie_remote_userspace_sdk::Transport for NullTransport {
        fn fire_interrupt(&mut self, _msix_index: u32) {}
        fn dma_write(&mut self, _gpa: u64, _data: Vec<u8>) -> u64 {
            0
        }
        fn dma_read(&mut self, _gpa: u64, _len: u32) -> u64 {
            0
        }
    }
    let make_aer_sqe = |cid: u16| -> pcie_remote_nvme_userspace::cmd::Sqe {
        let mut sqe = pcie_remote_nvme_userspace::cmd::Sqe::new_zeroed();
        sqe.cdw0 = ((cid as u32) << 16) | 0xC;
        sqe
    };

    let conn_a = 0xAAAA_AAAAu32;
    let conn_b = 0xBBBB_BBBBu32;
    {
        let mut c = shared.controller.lock();
        let mut t = NullTransport;
        let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut t);
        // conn_a 灌 8 条 AER（命中 per-conn cap=8）
        for i in 0..8 {
            let r = c.nvme_admin_dispatch_with_conn(
                &mut ctx,
                make_aer_sqe(i as u16),
                i as u16,
                0,
                conn_a,
            );
            assert!(r.is_none(), "conn_a 第 {i} 条 AER 应 queued");
        }
        // 第 9 条 conn_a AER 应返 SC=0x05
        let r = c.nvme_admin_dispatch_with_conn(&mut ctx, make_aer_sqe(9), 9, 0, conn_a);
        let cqe = r.expect("conn_a 第 9 条应 reject CQE");
        let sf = (cqe.dw3 >> 16) as u16;
        let sc = ((sf >> 1) & 0xff) as u8;
        assert_eq!(sc, 0x05, "conn_a 超 per-conn cap 应回 SC=0x05");

        // conn_b 第一条 AER 应仍能 queued（per-conn cap 隔离，验 H-4 cross-tenant 防御）
        let r = c.nvme_admin_dispatch_with_conn(&mut ctx, make_aer_sqe(0x100), 0x100, 0, conn_b);
        assert!(
            r.is_none(),
            "conn_b 应不受 conn_a 灌满影响，第 1 条仍 queued"
        );
        assert_eq!(c.nvme_pending_aer_count_for_conn(conn_a), 8);
        assert_eq!(c.nvme_pending_aer_count_for_conn(conn_b), 1);
    }
}
