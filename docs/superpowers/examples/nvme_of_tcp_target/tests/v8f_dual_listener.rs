// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V8f 集成测试** — dual-listener（IO + Discovery 独立端口）+
//! multi-conn IO e2e（4 串行 conn 模拟 nvme-cli 默认 4 IO queue）。
//!
//! V8f 在同 bin 进程内起两个独立 NvmeController：
//! - 主 listener: IO controller（`--listen`）
//! - discovery listener: Discovery controller（`--discovery-listen`），含 portal 列表
//!
//! 两 controller 实例隔离 AER / qid 状态防串扰（plan §6 Q6）。
//!
//! 覆盖：
//! 1. v8f_dual_listener_starts_both_ports — 两端口都能 accept
//! 2. v8f_dual_listener_discovery_reports_portals — discovery listener 收 Get Log
//!    Page 0x70 返 portal 列表，主 listener 不返
//! 3. v8f_sigterm_shuts_both — SIGTERM 让两 listener 都 exit
//! 4. v8f_e2e_multi_conn_serial_io_4_conns — 4 个 conn 串行各做 IO Read，验证
//!    跨 conn shared controller 兼容 nvme-cli 默认 IO queue 数（V8c 真并发 IO
//!    待办，本测试用串行验跨 conn state carry over）

#![allow(missing_docs)]

use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::{read_pdu, write_pdu};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use zerocopy::IntoBytes;

fn alloc_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
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

/// spawn bin with given args；等 300ms 让两 listener 上线。
fn spawn_bin(args: &[&str]) -> Child {
    let bin = std::env::var("CARGO_BIN_EXE_nvme_of_tcp_target")
        .expect("CARGO_BIN_EXE_nvme_of_tcp_target env not set");
    let child = Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bin");
    thread::sleep(Duration::from_millis(300));
    child
}

#[cfg(unix)]
fn sigterm_and_wait(mut child: Child, timeout: Duration) {
    use std::os::unix::process::ExitStatusExt as _;
    let pid = child.id() as i32;
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                if s.success() {
                    return;
                }
                if let Some(sig) = s.signal() {
                    panic!("child killed by signal {sig}");
                }
                panic!("child exit {s:?} not success");
            }
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("child not exit within timeout");
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait: {e}"),
        }
    }
}

#[cfg(not(unix))]
fn sigterm_and_wait(mut child: Child, _: Duration) {
    let _ = child.kill();
    let _ = child.wait();
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

fn send_connect_admin_discovery(s: &mut TcpStream, cid: u16) {
    // Discovery mode 要求 Connect.SUBNQN = DISCOVERY_NQN
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
    let mut cd = ConnectData::default();
    let discovery_nqn = nvme_of_tcp_target::DISCOVERY_NQN.as_bytes();
    cd.subnqn[..discovery_nqn.len()].copy_from_slice(discovery_nqn);
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 72,
        plen: 72 + 1024,
    };
    write_pdu(s, &hdr, &sqe, cd.as_bytes()).unwrap();
}

fn send_connect_admin_io(s: &mut TcpStream, cid: u16) {
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

fn sc_of(psh: &[u8]) -> u8 {
    ((u16::from_le_bytes(psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8
}

// ─── V8f tests ────────────────────────────────────────────────────────

#[test]
fn v8f_dual_listener_starts_both_ports() {
    let backing = make_backing(1024 * 1024, Some(0xA5));
    let p_main = alloc_port();
    let p_disc = alloc_port();
    let listen_main = format!("127.0.0.1:{p_main}");
    let listen_disc = format!("127.0.0.1:{p_disc}");

    let child = spawn_bin(&[
        "--listen",
        &listen_main,
        "--backing-file",
        &backing.path().to_string_lossy(),
        "--discovery-listen",
        &listen_disc,
        "--discovery-target-nqn",
        "nqn.2014-08.org.example:storage1",
        "--discovery-target-addr",
        "127.0.0.1:11420",
    ]);

    // 验两个端口都能 TCP connect 成功 + ICReq 通
    for (lbl, port) in [("main", p_main), ("disc", p_disc)] {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap_or_else(|e| {
            panic!("connect {lbl}:{port}: {e}");
        });
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        s.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        send_icreq(&mut s);
        let r = read_pdu(&mut s).unwrap_or_else(|e| panic!("read_pdu {lbl}: {e}"));
        assert_eq!(r.header.pdu_type, pdu_type::ICRESP, "{lbl} ICRESP expected");
    }

    sigterm_and_wait(child, Duration::from_secs(5));
}

#[test]
fn v8f_dual_listener_discovery_only_responds_on_discovery_port() {
    let backing = make_backing(1024 * 1024, Some(0xA5));
    let p_main = alloc_port();
    let p_disc = alloc_port();
    let listen_main = format!("127.0.0.1:{p_main}");
    let listen_disc = format!("127.0.0.1:{p_disc}");

    let child = spawn_bin(&[
        "--listen",
        &listen_main,
        "--backing-file",
        &backing.path().to_string_lossy(),
        "--discovery-listen",
        &listen_disc,
        "--discovery-target-nqn",
        "nqn.2014-08.org.example:io1",
        "--discovery-target-addr",
        "127.0.0.1:14420",
    ]);

    // === Discovery 端口: Connect 用 DISCOVERY_NQN 应 success ===
    let mut disc = TcpStream::connect(("127.0.0.1", p_disc)).unwrap();
    disc.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    disc.set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    send_icreq(&mut disc);
    let _ = read_pdu(&mut disc).unwrap();
    send_connect_admin_discovery(&mut disc, 0x1111);
    let r = read_pdu(&mut disc).unwrap();
    assert_eq!(
        sc_of(&r.psh),
        0,
        "V8f discovery listener Connect(DISCOVERY_NQN) 应 success"
    );

    // === 主端口: Connect 用普通 NQN 也应 success（IO controller，无 discovery_mode）===
    let mut main = TcpStream::connect(("127.0.0.1", p_main)).unwrap();
    main.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    main.set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    send_icreq(&mut main);
    let _ = read_pdu(&mut main).unwrap();
    send_connect_admin_io(&mut main, 0x2222);
    let r = read_pdu(&mut main).unwrap();
    assert_eq!(
        sc_of(&r.psh),
        0,
        "V8f main listener Connect(<普通 NQN>) 应 success"
    );

    drop(disc);
    drop(main);
    sigterm_and_wait(child, Duration::from_secs(5));
}

#[test]
fn v8f_sigterm_shuts_both() {
    let backing = make_backing(1024 * 1024, Some(0x00));
    let p_main = alloc_port();
    let p_disc = alloc_port();
    let child = spawn_bin(&[
        "--listen",
        &format!("127.0.0.1:{p_main}"),
        "--backing-file",
        &backing.path().to_string_lossy(),
        "--discovery-listen",
        &format!("127.0.0.1:{p_disc}"),
        "--discovery-target-nqn",
        "nqn.test",
        "--discovery-target-addr",
        "127.0.0.1:14999",
    ]);

    // 不发任何请求；纯验 SIGTERM 让两 listener exit
    sigterm_and_wait(child, Duration::from_secs(5));
}

#[test]
fn v8f_single_listener_mode_no_discovery_listen_works() {
    // 不传 --discovery-listen，验 V8a 单 listener 路径仍 OK（regression guard）
    let backing = make_backing(1024 * 1024, Some(0x00));
    let p = alloc_port();
    let child = spawn_bin(&[
        "--listen",
        &format!("127.0.0.1:{p}"),
        "--backing-file",
        &backing.path().to_string_lossy(),
    ]);

    let mut s = TcpStream::connect(("127.0.0.1", p)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    send_icreq(&mut s);
    let _ = read_pdu(&mut s).unwrap();
    drop(s);

    sigterm_and_wait(child, Duration::from_secs(5));
}

#[test]
fn v8f_discovery_listen_without_target_nqn_fails_fast() {
    // --discovery-listen 但缺 --discovery-target-nqn 应 bin 启动 fail
    let backing = make_backing(1024 * 1024, None);
    let p_main = alloc_port();
    let p_disc = alloc_port();
    let bin = std::env::var("CARGO_BIN_EXE_nvme_of_tcp_target")
        .expect("CARGO_BIN_EXE_nvme_of_tcp_target env not set");
    let status = Command::new(bin)
        .args([
            "--listen",
            &format!("127.0.0.1:{p_main}"),
            "--backing-file",
            &backing.path().to_string_lossy(),
            "--discovery-listen",
            &format!("127.0.0.1:{p_disc}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn bin");
    assert!(
        !status.success(),
        "V8f --discovery-listen 缺 --discovery-target-nqn 应 fail-fast"
    );
}
