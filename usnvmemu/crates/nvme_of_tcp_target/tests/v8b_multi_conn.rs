// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V8b 集成测试** — `Arc<Mutex<NvmeController>>` 多 conn 共享语义验证。
//!
//! V8b 拆掉 V5d R-8 per-backing Mutex；改为 bin startup 一次 `NvmeController::open`
//! → `Arc::new(parking_lot::Mutex::new(...))` → 每条 conn `Arc::clone` 共享。
//! 本文件覆盖五条 plan §6 要求的回归：
//!
//! 1. `v8b_two_conns_share_controller_admin_then_io` — 串行两条 conn 各自完成
//!    Connect+Create IO CQ/SQ+IO Read，验明 controller 状态在 conn 之间 carry over。
//! 2. `v8b_two_conns_serial_io_read_carry_over_state` — V8b 阶段串行验跨 conn
//!    IO Read 状态不污染；真并发同 qid IO 留 V8c（per-conn qid 隔离）。
//! 3. `v8b_two_conns_serial_io_write_then_read_persistence` — Conn A Write 后
//!    退出，Conn B 起来 Read 同 LBA 验跨 conn 一致性 + backing 持久化。
//! 4. `v8b_admin_cq_install_idempotent` — 两条 conn 各调一次 install 不 race / 不
//!    overwrite。
//! 5. `v8b_per_backing_mutex_removed` — V5d R-8 拆除 smoke：bin 子进程同时 accept
//!    两条 conn 各自跑完 ICReq/Connect admin，不再触发 backing Mutex contention。

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::{read_pdu, write_pdu};
use nvme_of_tcp_target::pdu::{CommonHdr, DataPsh, IcPsh, R2tPsh, pdu_type};
use nvme_of_tcp_target::{SharedController, SharedControllerInner, V2Session};
use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use zerocopy::{FromBytes, IntoBytes};

// ─── helpers (整合简化 src/session.rs 内 #[cfg(test)] helper 关键路径) ──

fn alloc_port_and_listener() -> (TcpListener, u16) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    (l, p)
}

fn make_backing(size: u64, pattern: Option<u8>) -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(size).unwrap();
    if let Some(p) = pattern {
        let buf = vec![p; 4096];
        let mut h = std::fs::OpenOptions::new()
            .write(true)
            .open(f.path())
            .unwrap();
        h.write_all(&buf).unwrap();
    }
    f
}

fn make_shared_controller(paths: &[&std::path::Path]) -> SharedController {
    let strs: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let c = NvmeController::open(&strs, 0x1414, 0, &[]).unwrap();
    Arc::new(SharedControllerInner::new(c))
}

fn send_icreq(s: &mut TcpStream) {
    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let psh = IcPsh {
        pfv: 0,
        hpda_or_cpda: 0,
        digest: 0,
        maxr2t_or_maxh2cdata: 7,
        rsvd: [0u8; 112],
    };
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

fn send_connect_io(s: &mut TcpStream, cid: u16, qid: u16) {
    let mut sqe = [0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::CONNECT;
    let f = ConnectFabricFields {
        recfmt: 0,
        qid,
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
}

/// admin SQE (无 capsule data) cdw11/cdw10 简单设置
fn send_admin_sqe_cdw11(s: &mut TcpStream, opc: u8, cid: u16, nsid: u32, cdw10: u32, cdw11: u32) {
    let mut sqe = [0u8; 64];
    sqe[0] = opc;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
    sqe[40..44].copy_from_slice(&cdw10.to_le_bytes());
    sqe[44..48].copy_from_slice(&cdw11.to_le_bytes());
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    write_pdu(s, &hdr, &sqe, &[]).unwrap();
}

fn send_io_read(s: &mut TcpStream, cid: u16, nsid: u32, slba: u64, nlb: u32) {
    let mut sqe = [0u8; 64];
    sqe[0] = 0x02;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
    sqe[40..44].copy_from_slice(&((slba & 0xffff_ffff) as u32).to_le_bytes());
    sqe[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
    sqe[48..52].copy_from_slice(&(nlb - 1).to_le_bytes());
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    write_pdu(s, &hdr, &sqe, &[]).unwrap();
}

fn send_io_write(s: &mut TcpStream, cid: u16, nsid: u32, slba: u64, nlb: u32) {
    let mut sqe = [0u8; 64];
    sqe[0] = 0x01;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
    sqe[40..44].copy_from_slice(&((slba & 0xffff_ffff) as u32).to_le_bytes());
    sqe[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
    sqe[48..52].copy_from_slice(&(nlb - 1).to_le_bytes());
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    write_pdu(s, &hdr, &sqe, &[]).unwrap();
}

fn sc_of(psh: &[u8; 16]) -> u8 {
    ((u16::from_le_bytes(psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8
}

/// 在线程内跑一条 V8b session：spawn server side, return client TcpStream + JoinHandle。
fn spawn_session(shared: SharedController, pumps: usize) -> (TcpStream, thread::JoinHandle<()>) {
    let (listener, port) = alloc_port_and_listener();
    let h = thread::spawn(move || {
        let (server, _peer) = listener.accept().unwrap();
        let mut sess = V2Session::accept_and_handshake_shared(server, shared).unwrap();
        for _ in 0..pumps {
            // pump_one 阻塞，遇 peer close 会返 Ok(false)；本 fixture 把 panic 抛到 thread。
            let cont = sess.pump_one().unwrap();
            if !cont {
                break;
            }
        }
    });
    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    (client, h)
}

/// 完整跑一条 conn 的 setup（ICReq → Connect admin → Create IO CQ/SQ → Connect qid=1）。
/// 假设 server thread 已 spawn 配足 pump 数（≥4）。
fn run_full_setup(client: &mut TcpStream, cid_base: u16) {
    send_icreq(client);
    let _ = read_pdu(client).unwrap();
    send_connect_admin(client, cid_base);
    let r = read_pdu(client).unwrap();
    assert_eq!(sc_of(&r.psh.try_into().unwrap()), 0, "Connect admin");
    let cdw10_cq = 1u32 | (15u32 << 16);
    send_admin_sqe_cdw11(client, 0x05, cid_base + 1, 0, cdw10_cq, 0x0000_0001);
    let r = read_pdu(client).unwrap();
    assert_eq!(sc_of(&r.psh.try_into().unwrap()), 0, "Create IO CQ");
    let cdw10_sq = 1u32 | (15u32 << 16);
    let cdw11_sq = 0x0001_0001u32;
    send_admin_sqe_cdw11(client, 0x01, cid_base + 2, 0, cdw10_sq, cdw11_sq);
    let r = read_pdu(client).unwrap();
    assert_eq!(sc_of(&r.psh.try_into().unwrap()), 0, "Create IO SQ");
    send_connect_io(client, cid_base + 3, 1);
    let r = read_pdu(client).unwrap();
    assert_eq!(sc_of(&r.psh.try_into().unwrap()), 0, "Connect qid=1");
}

// ─── Tests ───────────────────────────────────────────────────────────

/// V8b-1：串行两条 conn share controller — 第一条做完 setup 后退出，第二条
/// 还能复用 admin CQ/IO queue（idempotent install）继续 setup + IO Read。
#[test]
fn v8b_two_conns_share_controller_admin_then_io() {
    let backing = make_backing(1024 * 1024, Some(0xAB));
    let shared = make_shared_controller(&[backing.path()]);

    // === Conn A：完整 setup + 1 个 IO Read ===
    // pump 数：1 ICReq+Connect admin(2) + Create IO CQ(1) + Create IO SQ(1) + Connect qid=1(1)
    // + IO Read(1) = 6
    let (mut a, ha) = spawn_session(Arc::clone(&shared), 6);
    run_full_setup(&mut a, 0x0100);
    send_io_read(&mut a, 0x0110, 1, 0, 1);
    let p = read_pdu(&mut a).unwrap();
    assert_eq!(p.header.pdu_type, pdu_type::C2H_DATA);
    assert!(p.data.iter().all(|&b| b == 0xAB), "conn A 读 0xAB");
    let r = read_pdu(&mut a).unwrap();
    assert_eq!(sc_of(&r.psh.try_into().unwrap()), 0);
    drop(a);
    ha.join().unwrap();

    // === Conn B：同一 shared controller，run setup 应再次 success（idempotent）===
    let (mut b, hb) = spawn_session(Arc::clone(&shared), 6);
    run_full_setup(&mut b, 0x0200);
    send_io_read(&mut b, 0x0210, 1, 0, 1);
    let p = read_pdu(&mut b).unwrap();
    assert_eq!(p.header.pdu_type, pdu_type::C2H_DATA);
    assert!(p.data.iter().all(|&b| b == 0xAB), "conn B 读同 pattern");
    let r = read_pdu(&mut b).unwrap();
    assert_eq!(sc_of(&r.psh.try_into().unwrap()), 0);
    drop(b);
    hb.join().unwrap();
}

/// V8b-2：两条 conn **串行** IO Read 验 controller 状态在 conn 之间不污染。
///
/// **V8c 待办**：真正"两条 conn 同时跑 IO"需要 per-conn qid 命名空间隔离
/// （两条 conn 各自 Create IO SQ qid=1 时第二次会撞到 controller 的 same-qid
/// install 状态）。本 V8b 阶段聚焦在 admin path 与单条 conn IO path 的 shared
/// controller 正确性；并发同 qid IO 留给 V8c 解决。
#[test]
fn v8b_two_conns_serial_io_read_carry_over_state() {
    let backing = make_backing(1024 * 1024, Some(0x5A));
    let shared = make_shared_controller(&[backing.path()]);

    let n_loops = 8usize;
    let pumps = 5 + n_loops;

    // === Conn A 单独跑 n_loops 次 IO Read ===
    let (mut a, ha) = spawn_session(Arc::clone(&shared), pumps);
    run_full_setup(&mut a, 0x0300);
    for i in 0..n_loops {
        send_io_read(&mut a, 0x0310 + i as u16, 1, 0, 1);
        let p = read_pdu(&mut a).unwrap();
        assert_eq!(p.header.pdu_type, pdu_type::C2H_DATA);
        assert_eq!(p.data.len(), 512);
        assert!(
            p.data.iter().all(|&v| v == 0x5A),
            "conn A 第 {i} 次 read 应全 0x5A，得 {:?}",
            &p.data[..16]
        );
        let r = read_pdu(&mut a).unwrap();
        assert_eq!(sc_of(&r.psh.try_into().unwrap()), 0);
    }
    drop(a);
    ha.join().unwrap();

    // === Conn B 复用 shared controller 再跑 n_loops 次 IO Read ===
    let (mut b, hb) = spawn_session(Arc::clone(&shared), pumps);
    run_full_setup(&mut b, 0x0400);
    for i in 0..n_loops {
        send_io_read(&mut b, 0x0410 + i as u16, 1, 0, 1);
        let p = read_pdu(&mut b).unwrap();
        assert_eq!(p.header.pdu_type, pdu_type::C2H_DATA);
        assert_eq!(p.data.len(), 512);
        assert!(
            p.data.iter().all(|&v| v == 0x5A),
            "conn B 第 {i} 次 read 应全 0x5A，得 {:?}",
            &p.data[..16]
        );
        let r = read_pdu(&mut b).unwrap();
        assert_eq!(sc_of(&r.psh.try_into().unwrap()), 0);
    }
    drop(b);
    hb.join().unwrap();
}

/// V8b-3：Conn A 在 LBA 100 写 0xC7 后退出；Conn B 起来 read 同 LBA 应拿到 0xC7。
/// 验跨 conn 共享 controller 的 backing-file 持久化语义。
///
/// **V8c 待办**：与 V8b-2 同理，并发同 qid IO 需要 per-conn qid 隔离；本 test
/// 串行跑 A→B 验持久化即可，并发场景留给 V8c。
#[test]
fn v8b_two_conns_serial_io_write_then_read_persistence() {
    let backing = make_backing(1024 * 1024, Some(0x00));
    let shared = make_shared_controller(&[backing.path()]);

    // === Conn A: setup 5 + IO Write (R2T + RSP = 2 pump) = 7 ===
    let (mut a, ha) = spawn_session(Arc::clone(&shared), 7);
    run_full_setup(&mut a, 0x0500);

    // Conn A: IO Write LBA=100 nlb=1，pattern 0xC7
    send_io_write(&mut a, 0x0510, 1, 100, 1);
    // 读 R2T，提取 ttag
    let r2t = read_pdu(&mut a).unwrap();
    assert_eq!(r2t.header.pdu_type, pdu_type::R2T, "应先收 R2T");
    let r2t_psh = R2tPsh::read_from_bytes(&r2t.psh[..16]).expect("R2tPsh decode");
    let ttag = r2t_psh.ttag;
    // 构造 H2CData：hdr=8B + DataPsh=16B = 24B；plen=24+512
    let payload = vec![0xC7u8; 512];
    let data_psh = DataPsh {
        cccid: 0x0510,
        ttag_or_rsvd: ttag,
        data_offset: 0,
        data_length: 512,
        rsvd: [0u8; 4],
    };
    let hdr = CommonHdr {
        pdu_type: pdu_type::H2C_DATA,
        flags: 0x04, // LAST_PDU
        hlen: 24,
        pdo: 24,
        plen: 24 + 512,
    };
    write_pdu(&mut a, &hdr, data_psh.as_bytes(), &payload).unwrap();

    // Write RSP
    let r = read_pdu(&mut a).unwrap();
    assert_eq!(r.header.pdu_type, pdu_type::RSP);
    assert_eq!(sc_of(&r.psh.try_into().unwrap()), 0, "Write 应 success");

    drop(a);
    ha.join().unwrap();

    // === Conn B: 同一 shared controller 起来 read 同 LBA ===
    // setup 5 + IO Read (C2HData + RSP = 2 pump) = 7
    let (mut b, hb) = spawn_session(Arc::clone(&shared), 7);
    run_full_setup(&mut b, 0x0600);
    send_io_read(&mut b, 0x0610, 1, 100, 1);
    let p = read_pdu(&mut b).unwrap();
    assert_eq!(p.header.pdu_type, pdu_type::C2H_DATA);
    assert!(
        p.data.iter().all(|&v| v == 0xC7),
        "conn B 应读到 A 写入的 0xC7"
    );
    let r = read_pdu(&mut b).unwrap();
    assert_eq!(sc_of(&r.psh.try_into().unwrap()), 0);

    drop(b);
    hb.join().unwrap();
}

/// V8b-4：两条 conn 各自 handshake 自动调 `nvme_install_admin_cq`；同参数应
/// idempotent 无副作用（plan R-2）；不同参数应返 `MismatchedParams`（reviewer C-2）。
#[test]
fn v8b_admin_cq_install_idempotent() {
    let backing = make_backing(1024 * 1024, Some(0x00));
    let path = backing.path().to_str().unwrap().to_string();
    let mut c = NvmeController::open(&[path], 0x1414, 0, &[]).unwrap();

    assert!(!c.nvme_has_admin_cq(), "初始无 admin CQ");
    c.nvme_install_admin_cq(0xFFFF_0000, 64)
        .expect("first install");
    assert!(c.nvme_has_admin_cq());

    // 同参数再调 → Ok(()) silent no-op
    c.nvme_install_admin_cq(0xFFFF_0000, 64)
        .expect("idempotent same-params");
    assert!(c.nvme_has_admin_cq());

    // 不同参数再调 → Err，且原值未被覆盖
    let err = c
        .nvme_install_admin_cq(0xDEAD_0000, 128)
        .expect_err("mismatched params 应 hard-fail");
    assert!(
        format!("{err:?}").contains("MismatchedParams"),
        "应是 MismatchedParams 变体，得 {err:?}"
    );
    assert!(c.nvme_has_admin_cq(), "原 CQ 仍在（拒覆盖）");
}

/// V8b-6 (reviewer C-1)：`SharedControllerInner::allocate_token_slab` 在多次
/// 调用 / 多线程并发调用时返回的 base 互不相交（slab 大小见 `TOKEN_SLAB_SIZE`）。
/// 这是多 conn 共享 controller 时跨 conn `PendingIo` 不撞 key 的根本保证。
#[test]
fn v8b_token_slab_allocation_is_disjoint() {
    use nvme_of_tcp_target::TOKEN_SLAB_SIZE;

    let backing = make_backing(1024 * 1024, None);
    let shared = make_shared_controller(&[backing.path()]);

    // 串行 3 条：base 必递增且 step = TOKEN_SLAB_SIZE
    let b0 = shared.allocate_token_slab();
    let b1 = shared.allocate_token_slab();
    let b2 = shared.allocate_token_slab();
    assert_eq!(b1.wrapping_sub(b0), TOKEN_SLAB_SIZE);
    assert_eq!(b2.wrapping_sub(b1), TOKEN_SLAB_SIZE);

    // 并发 4 条：所有 base 必互不相同；总和 = 4 个连续 slab
    let shared2 = Arc::clone(&shared);
    let shared3 = Arc::clone(&shared);
    let shared4 = Arc::clone(&shared);
    let shared5 = Arc::clone(&shared);
    let t1 = thread::spawn(move || shared2.allocate_token_slab());
    let t2 = thread::spawn(move || shared3.allocate_token_slab());
    let t3 = thread::spawn(move || shared4.allocate_token_slab());
    let t4 = thread::spawn(move || shared5.allocate_token_slab());
    let mut bases = [
        t1.join().unwrap(),
        t2.join().unwrap(),
        t3.join().unwrap(),
        t4.join().unwrap(),
    ];
    bases.sort_unstable();
    for w in bases.windows(2) {
        assert_eq!(
            w[1].wrapping_sub(w[0]),
            TOKEN_SLAB_SIZE,
            "并发 4 条 slab base 必两两相差 TOKEN_SLAB_SIZE，得 {bases:?}"
        );
    }
}

/// V8b-5：bin 子进程同时跑两条 ICReq → Connect admin → Property Get，证明
/// V5d R-8 per-backing Mutex 拆除；并发 conn 不再触发 backing 串行限制。
#[test]
fn v8b_per_backing_mutex_removed() {
    let f = make_backing(1024 * 1024, None);

    // 找空闲端口
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let listen = format!("127.0.0.1:{port}");
    let bin = std::env::var("CARGO_BIN_EXE_nvme_of_tcp_target")
        .expect("CARGO_BIN_EXE_nvme_of_tcp_target env not set");
    let mut child = std::process::Command::new(&bin)
        .arg("--listen")
        .arg(&listen)
        .arg("--backing-file")
        .arg(f.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(300));

    let do_handshake = |port: u16| {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        send_icreq(&mut s);
        let icresp = read_pdu(&mut s).unwrap();
        assert_eq!(icresp.header.pdu_type, pdu_type::ICRESP);
        send_connect_admin(&mut s, 0x1111);
        let r = read_pdu(&mut s).unwrap();
        assert_eq!(sc_of(&r.psh.try_into().unwrap()), 0, "Connect admin");
    };

    let p1 = port;
    let p2 = port;
    let t1 = thread::spawn(move || do_handshake(p1));
    let t2 = thread::spawn(move || do_handshake(p2));
    t1.join().unwrap();
    t2.join().unwrap();

    // graceful: SIGTERM
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    if child.try_wait().unwrap().is_none() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("child 未在 5s 内退出");
    }
}
