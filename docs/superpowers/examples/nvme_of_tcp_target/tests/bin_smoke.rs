// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V5d bin smoke test** — spawn `nvme_of_tcp_target` 子进程绑
//! ephemeral port → TCP 客户端连 → 跑 ICReq/Connect handshake → close。
//!
//! 不在 CI 中跑真 Linux nvme-cli interop（需 root + kernel modules）；
//! 本 smoke 仅验 bin entry 启动 + accept + 完成 handshake。

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

#[test]
fn bin_smoke_handshake_then_disconnect() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(1024 * 1024).unwrap();
    f.flush().unwrap();
    let path = f.path().to_path_buf();

    let (mut child, port) = spawn_target(&path);

    let mut sock = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    sock.set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    // 发 ICReq
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

    // 收 ICResp
    let pdu = read_pdu(&mut sock).expect("read ICResp");
    let rt = pdu.header.pdu_type;
    assert_eq!(rt, pdu_type::ICRESP, "expected ICRESP");

    // 干净 close socket，让 server thread bail
    drop(sock);

    // wait child；smoke 不验 exit code（V5d 长跑 binary 没 SIGINT 处理）
    // 直接 kill child 让 cargo test 不挂住
    let _ = child.kill();
    let _ = child.wait();
}
