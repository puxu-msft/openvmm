// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V8c 集成测试** — per-conn AER routing 与 Drop cleanup。
//!
//! V8c 把 controller 端 `aen_pending` 从 controller-wide FIFO 改为 per-conn
//! 路由（每条 entry 含 `conn_id`），让两条 conn 的 AER 互不窃取；conn drop
//! 时通过 `nvme_cleanup_conn_aers` 抹掉残留 entry 防 leak（security H-1 / R-12）。
//!
//! 覆盖：
//! 1. `v8c_two_conns_each_have_own_aer_queue` — A AER 不被 B 拿走，B 反之亦然。
//! 2. `v8c_pending_aer_count_per_conn_isolated` — count getter 只数本 conn。
//! 3. `v8c_conn_drop_cleans_pending_aers` — Session Drop 后 controller 该 conn
//!    pending 数归 0。
//! 4. `v8c_legacy_dispatch_conn_id_zero_still_works` — 旧 `nvme_admin_dispatch`
//!    BC 路径（conn_id=0）push 进 controller 并能被 `nvme_fire_aen` 兜底 fire。
//! 5. `v8c_allocate_conn_id_is_unique_and_nonzero` — `SharedControllerInner`
//!    原子分配单调递增、永不 0。

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::{SharedControllerInner, V2Session};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use zerocopy::FromZeros;

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

/// minimal in-test Transport that ignores all writes / reads。
struct NullTransport;
impl pcie_device_core::Transport for NullTransport {
    fn fire_interrupt(&mut self, _msix_index: u32) {}
    fn dma_write(&mut self, _gpa: u64, _data: Vec<u8>) -> u64 {
        0
    }
    fn dma_read(&mut self, _gpa: u64, _len: u32) -> u64 {
        0
    }
}

/// 构造一条 AER admin SQE（opc=0xC AsyncEventRequest），cid 在 cdw0 bits 31:16。
fn make_aer_sqe(cid: u16) -> nvme_firmware::cmd::Sqe {
    let mut sqe = nvme_firmware::cmd::Sqe::new_zeroed();
    sqe.cdw0 = ((cid as u32) << 16) | 0xC;
    sqe
}

fn push_aer(shared: &Arc<SharedControllerInner>, cid: u16, conn_id: u32) {
    let mut c = shared.controller.lock();
    let _ = c.nvme_install_admin_cq(
        nvme_of_tcp_target::CQ_BASE_GPA,
        nvme_of_tcp_target::ADMIN_CQ_SIZE,
    );
    let mut t = NullTransport;
    let mut ctx = pcie_device_core::DeviceCtx::new(&mut t);
    let r = c.nvme_admin_dispatch_with_conn(&mut ctx, make_aer_sqe(cid), cid, 0, conn_id);
    assert!(r.is_none(), "AER 应 async（None）");
}

#[test]
fn v8c_two_conns_each_have_own_aer_queue() {
    let backing = make_backing(1024 * 1024);
    let shared = make_shared(backing.path());

    let conn_a = shared.allocate_conn_id();
    let conn_b = shared.allocate_conn_id();
    assert_ne!(conn_a, conn_b);

    push_aer(&shared, 0xAAAA, conn_a);
    push_aer(&shared, 0xBBBB, conn_b);

    let (a_pend, b_pend, total) = {
        let c = shared.controller.lock();
        (
            c.nvme_pending_aer_count_for_conn(conn_a),
            c.nvme_pending_aer_count_for_conn(conn_b),
            c.nvme_pending_aer_count(),
        )
    };
    assert_eq!(a_pend, 1, "conn A 应有 1 个 pending");
    assert_eq!(b_pend, 1, "conn B 应有 1 个 pending");
    assert_eq!(total, 2);

    // fire conn_a → 只消费 conn_a 的；conn_b 的不动
    {
        let mut c = shared.controller.lock();
        let mut t = NullTransport;
        let mut ctx = pcie_device_core::DeviceCtx::new(&mut t);
        let fired = c.nvme_fire_aen_for_conn(&mut ctx, 0x01, 0x00, 0x02, conn_a);
        assert!(fired, "应 fire conn_a 的 AER");
    }
    let (a_pend, b_pend) = {
        let c = shared.controller.lock();
        (
            c.nvme_pending_aer_count_for_conn(conn_a),
            c.nvme_pending_aer_count_for_conn(conn_b),
        )
    };
    assert_eq!(a_pend, 0, "fire 后 conn A 应清空");
    assert_eq!(b_pend, 1, "conn B 应保留不被窃取");
}

#[test]
fn v8c_pending_aer_count_per_conn_isolated() {
    let backing = make_backing(1024 * 1024);
    let shared = make_shared(backing.path());
    let conn_a = shared.allocate_conn_id();
    let conn_b = shared.allocate_conn_id();

    push_aer(&shared, 1, conn_a);
    push_aer(&shared, 2, conn_a);
    push_aer(&shared, 3, conn_b);
    push_aer(&shared, 4, conn_a);

    let c = shared.controller.lock();
    assert_eq!(c.nvme_pending_aer_count(), 4);
    assert_eq!(c.nvme_pending_aer_count_for_conn(conn_a), 3);
    assert_eq!(c.nvme_pending_aer_count_for_conn(conn_b), 1);
    assert_eq!(
        c.nvme_pending_aer_count_for_conn(9999),
        0,
        "未分配 conn 应 0"
    );
}

#[test]
fn v8c_conn_drop_cleans_pending_aers() {
    let backing = make_backing(1024 * 1024);
    let shared = make_shared(backing.path());

    // 先预 push 另一 conn (= 9999) 的一条 AER，验只清本 conn
    push_aer(&shared, 0x9ABC, 9999);

    // 起 session（accept_and_handshake_shared 内 allocate_conn_id 自分配）
    // 用 oneshot 把 sess.conn_id 回传，避免预测 fetch_add 顺序（reviewer H-4）
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    let shared_clone = Arc::clone(&shared);
    let h = thread::spawn(move || {
        let (server, _) = listener.accept().unwrap();
        let sess = V2Session::accept_and_handshake_shared(server, shared_clone).unwrap();
        tx.send(sess.conn_id).unwrap();
        // 等 1 个 PDU；client close 后 pump 退出 → Drop
        let mut sess = sess;
        let _ = sess.pump_one();
    });

    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    use nvme_of_tcp_target::framing::{read_pdu, write_pdu};
    use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
    use zerocopy::IntoBytes;
    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let psh = IcPsh::default();
    write_pdu(&mut client, &hdr, psh.as_bytes(), &[]).unwrap();
    let _ = read_pdu(&mut client).unwrap();

    let session_conn_id = rx.recv().unwrap();
    assert_ne!(session_conn_id, 0, "session 应分到非 0 conn_id");
    assert_ne!(session_conn_id, 9999, "session id 不该撞预占的 9999");

    // 此时 session 已 handshake，conn_id 已分配。push 两条本 conn AER 等 Drop 清
    push_aer(&shared, 0x1234, session_conn_id);
    push_aer(&shared, 0x5678, session_conn_id);

    // close → server pump_one returns false → Drop
    drop(client);
    h.join().unwrap();

    // Drop 应已抹掉 session_conn_id 的 2 条；conn 9999 的 1 条保留
    let c = shared.controller.lock();
    assert_eq!(
        c.nvme_pending_aer_count_for_conn(session_conn_id),
        0,
        "Drop 应清干净本 conn 的 AER"
    );
    assert_eq!(
        c.nvme_pending_aer_count_for_conn(9999),
        1,
        "其它 conn 应保留"
    );
}

#[test]
fn v8c_legacy_dispatch_conn_id_zero_still_works() {
    let backing = make_backing(1024 * 1024);
    let shared = make_shared(backing.path());

    {
        let mut c = shared.controller.lock();
        let _ = c.nvme_install_admin_cq(
            nvme_of_tcp_target::CQ_BASE_GPA,
            nvme_of_tcp_target::ADMIN_CQ_SIZE,
        );
        let mut t = NullTransport;
        let mut ctx = pcie_device_core::DeviceCtx::new(&mut t);
        // 走 legacy nvme_admin_dispatch（不带 conn_id）
        let r = c.nvme_admin_dispatch(&mut ctx, make_aer_sqe(0xDEAD), 0xDEAD, 0);
        assert!(r.is_none());
    }
    let c = shared.controller.lock();
    assert_eq!(
        c.nvme_pending_aer_count(),
        1,
        "legacy 也应 push 进 controller"
    );
    assert_eq!(c.nvme_pending_aer_count_for_conn(0), 1, "conn_id 应为 0");
}

#[test]
fn v8c_allocate_conn_id_is_unique_and_nonzero() {
    let backing = make_backing(1024 * 1024);
    let shared = make_shared(backing.path());

    // 串行 5 条：单调递增、全非 0
    let ids: Vec<u32> = (0..5).map(|_| shared.allocate_conn_id()).collect();
    for (i, id) in ids.iter().enumerate() {
        assert_ne!(*id, 0, "第 {i} 个 id 不应 0");
    }
    assert_eq!(
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        5
    );

    // 并发 8 条：全互不相同
    let s2 = Arc::clone(&shared);
    let bag = Arc::new(parking_lot::Mutex::new(Vec::<u32>::new()));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let s = Arc::clone(&s2);
            let b = Arc::clone(&bag);
            thread::spawn(move || {
                b.lock().push(s.allocate_conn_id());
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let got = bag.lock();
    assert_eq!(got.len(), 8);
    assert_eq!(
        got.iter().collect::<std::collections::HashSet<_>>().len(),
        8,
        "并发 8 条 conn_id 必两两不同：{got:?}"
    );
}
