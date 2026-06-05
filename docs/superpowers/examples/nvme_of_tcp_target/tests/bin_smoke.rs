// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V5d bin smoke test** — spawn `nvme_of_tcp_target` 子进程绑
//! ephemeral port → TCP 客户端连 → 跑完整 handshake + Connect admin +
//! 读 controller property + graceful SIGTERM → 验子进程 exit code 0。
//!
//! 不在 CI 中跑真 Linux nvme-cli interop（需 root + kernel modules）；
//! 本 smoke 用 in-process 客户端覆盖 bin entry → accept → handshake →
//! Connect → Property Get → graceful shutdown 整条链路。

use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::{read_pdu, write_pdu};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;
use zerocopy::IntoBytes;

/// 找一个空闲 TCP 端口（bind 0 让 OS 分配，绑定后立即 drop）。
fn alloc_ephemeral_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// spawn bin 子进程，返还 (Child, port)。
fn spawn_target(backing_path: &std::path::Path) -> (Child, u16) {
    let port = alloc_ephemeral_port();
    let listen = format!("127.0.0.1:{port}");
    let bin = std::env::var("CARGO_BIN_EXE_nvme_of_tcp_target")
        .expect("CARGO_BIN_EXE_nvme_of_tcp_target env not set");
    let child = Command::new(bin)
        .arg("--listen")
        .arg(&listen)
        .arg("--backing-file")
        .arg(backing_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn target");
    // 简单 sleep 等 listener up；不可用"poll connect"因为成功的探测会
    // 占用 backing-file Mutex (V5d R-8)，后续真 test conn 会被拒。
    thread::sleep(Duration::from_millis(300));
    (child, port)
}

/// **V5d-fix-2 (review M-3)** — 子进程 graceful shutdown：发 SIGTERM →
/// 等最多 5s exit → 检 exit code = 0；失败 force kill 并 panic。
#[cfg(unix)]
fn shutdown_and_check(mut child: Child) {
    use std::os::unix::process::ExitStatusExt as _;
    // SIGTERM 让 ctrlc handler 触发 graceful shutdown
    let pid = child.id() as i32;
    // 无 unsafe / 无额外 dep：用 `kill` 命令发送 SIGTERM
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return;
                }
                // signal-killed 是 ctrlc handler 没装上 fallback 路径；接受 0 exit
                if let Some(sig) = status.signal() {
                    panic!("child exited by signal {sig}, expected graceful exit");
                }
                panic!("child exited with status {status:?}, expected success");
            }
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("child did not exit within 5s of SIGTERM");
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
}

#[cfg(not(unix))]
fn shutdown_and_check(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// **V5d-fix-2 (review M-3 + V5-P3)** — 完整 bin smoke：
/// ICReq → Connect admin → Property Get (CAP) → graceful shutdown。
/// 覆盖 V5d 整条 admin path 直到 Fabric Connect/Property 路径。
#[test]
fn bin_smoke_full_admin_path_then_graceful_shutdown() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(1024 * 1024).unwrap();
    f.flush().unwrap();
    let path = f.path().to_path_buf();

    let (child, port) = spawn_target(&path);

    let mut sock = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    sock.set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // === 1. ICReq → ICResp ===
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
    write_pdu(&mut sock, &hdr, psh.as_bytes(), &[]).expect("write ICReq");
    let pdu = read_pdu(&mut sock).expect("read ICResp");
    assert_eq!(pdu.header.pdu_type, pdu_type::ICRESP, "expected ICRESP");

    // === 2. Fabric Connect qid=0 ===
    let mut sqe = [0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&0x0001u16.to_le_bytes()); // CID
    sqe[4] = fctype::CONNECT;
    let connect_fields = ConnectFabricFields {
        recfmt: 0,
        qid: 0,
        sqsize: 31,
        cattr: 0,
        rsvd1: 0,
        kato: 0,
        rsvd2: [0u8; 12],
    };
    sqe[40..64].copy_from_slice(connect_fields.as_bytes());
    let cd = ConnectData::default();
    let cmd_hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 72,
        plen: 72 + 1024,
    };
    write_pdu(&mut sock, &cmd_hdr, &sqe, cd.as_bytes()).expect("write Connect");
    let pdu = read_pdu(&mut sock).expect("read Connect resp");
    assert_eq!(pdu.header.pdu_type, pdu_type::RSP, "Connect → RSP");
    let status = u16::from_le_bytes(pdu.psh[14..16].try_into().unwrap());
    let sc = ((status >> 1) & 0xff) as u8;
    assert_eq!(sc, 0, "Connect admin 应 success, got sc={sc:#x}");

    // === 3. Property Get CAP (offset 0, size 1 = 8B) ===
    let mut sqe = [0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&0x0002u16.to_le_bytes()); // CID
    sqe[4] = fctype::PROPERTY_GET;
    let prop_fields = fabric::PropertyFabricFields {
        attrib: 1, // 8B
        rsvd1: [0u8; 3],
        ofst: fabric::property_offset::CAP,
        value: 0,
        rsvd2: [0u8; 8],
    };
    sqe[40..64].copy_from_slice(prop_fields.as_bytes());
    let cmd_hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    write_pdu(&mut sock, &cmd_hdr, &sqe, &[]).expect("write Property Get");
    let pdu = read_pdu(&mut sock).expect("read Property Get resp");
    assert_eq!(pdu.header.pdu_type, pdu_type::RSP);
    let status = u16::from_le_bytes(pdu.psh[14..16].try_into().unwrap());
    let sc = ((status >> 1) & 0xff) as u8;
    assert_eq!(sc, 0, "Property Get CAP 应 success");
    let cap_lo = u32::from_le_bytes(pdu.psh[0..4].try_into().unwrap());
    assert_ne!(cap_lo, 0, "CAP DW0 应非零");

    // 干净 close socket，让 server thread 走 peer-closed 路径
    drop(sock);

    // === 4. graceful shutdown ===
    shutdown_and_check(child);
}
