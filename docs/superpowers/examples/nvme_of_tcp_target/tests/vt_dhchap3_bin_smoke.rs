// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-dhchap-3 bin smoke 测试** — `--host-secret` CLI 解析。
//!
//! 不在子进程内跑真 AUTH wire 流量 (AUTH_SEND/RECV PDU dispatch 留
//! V-followup-dhchap-3-wire)；本测试只覆盖 CLI 入口：
//!
//! 1. `vt_dhchap3_bin_fails_on_bad_secret_hex` — `--host-secret nqn=NOT_HEX`
//!    → exit != 0
//! 2. `vt_dhchap3_bin_fails_on_short_secret` — secret < 32B → exit != 0
//! 3. `vt_dhchap3_bin_fails_on_missing_equals` — `--host-secret onlykey` 缺 =
//!    → exit != 0
//! 4. `vt_dhchap3_bin_starts_with_valid_secret` — 合法 NQN + 64-hex-char
//!    secret → bin 启动成功 + SIGTERM exit 0

use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

fn alloc_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

fn bin() -> String {
    std::env::var("CARGO_BIN_EXE_nvme_of_tcp_target")
        .expect("CARGO_BIN_EXE_nvme_of_tcp_target env not set")
}

fn make_backing() -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(1024 * 1024).unwrap();
    f
}

#[test]
fn vt_dhchap3_bin_fails_on_bad_secret_hex() {
    let backing = make_backing();
    let port = alloc_port();
    let out = Command::new(bin())
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--backing-file")
        .arg(backing.path())
        .arg("--host-secret")
        .arg("nqn.h=NOT_HEX!!")
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .expect("spawn");
    assert!(
        !out.status.success(),
        "bad hex secret 应 exit != 0; got {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--host-secret") || stderr.contains("hex"),
        "stderr 应解释 hex 解码失败, got: {stderr}"
    );
}

#[test]
fn vt_dhchap3_bin_fails_on_short_secret() {
    let backing = make_backing();
    let port = alloc_port();
    // 16-byte secret (32 hex chars) < 32-byte 最小
    let short_hex = "ab".repeat(16);
    let out = Command::new(bin())
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--backing-file")
        .arg(backing.path())
        .arg("--host-secret")
        .arg(format!("nqn.h={short_hex}"))
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .expect("spawn");
    assert!(!out.status.success(), "short secret 应 exit != 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("< 最小") || stderr.contains("32"),
        "stderr 应解释长度过短, got: {stderr}"
    );
}

#[test]
fn vt_dhchap3_bin_fails_on_missing_equals() {
    let backing = make_backing();
    let port = alloc_port();
    let out = Command::new(bin())
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--backing-file")
        .arg(backing.path())
        .arg("--host-secret")
        .arg("no_equal_sign")
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .expect("spawn");
    assert!(!out.status.success(), "缺 = 应 exit != 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("HOST_NQN") || stderr.contains("="),
        "stderr 应解释格式, got: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn vt_dhchap3_bin_starts_with_valid_secret() {
    let backing = make_backing();
    let port = alloc_port();
    let valid_hex = "ab".repeat(32); // 64 hex chars = 32 bytes
    let mut child = Command::new(bin())
        .env("RUST_LOG", "info")
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--backing-file")
        .arg(backing.path())
        .arg("--host-secret")
        .arg(format!("nqn.legit-host={valid_hex}"))
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn");
    thread::sleep(Duration::from_millis(500));
    let probe = std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_secs(2),
    );
    assert!(probe.is_ok(), "bin 应正常起 listener; got {probe:?}");
    drop(probe);
    // SIGTERM graceful
    let pid = child.id() as i32;
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .output();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                assert!(st.success(), "bin 应 graceful exit 0, got {st:?}");
                break;
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("bin did not exit within 5s after SIGTERM");
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("waitpid: {e}"),
        }
    }
}
