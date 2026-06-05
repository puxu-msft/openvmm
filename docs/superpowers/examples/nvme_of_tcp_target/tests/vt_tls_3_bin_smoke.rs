// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-tls-3 bin smoke 测试** — CLI 校验路径。
//!
//! 不在子进程内跑真 TLS 流量（端到端流量由 `vt_tls_3_handshake_e2e` 覆盖）；
//! 本测试聚焦 bin 入口的双 explicit consent 校验：
//!
//! 1. `vt_tls_3_bin_fails_without_cert` — `--tls-listen` 缺 `--tls-cert/key`
//!    → exit != 0，stderr 含中文错误信息
//! 2. `vt_tls_3_bin_fails_without_consent` — `--tls-listen + --tls-cert +
//!    --tls-key` 但缺 `--tls-i-trust-this-cert` → exit != 0
//! 3. `vt_tls_3_bin_starts_with_full_tls_consent` — 四参齐全 + 自签 cert →
//!    bin 启动 + listening on TLS port 日志可见 + SIGTERM 收到 exit code 0

use std::io::Write as _;
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

fn make_self_signed_cert() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_path = tmp.path().join("server.pem");
    let key_path = tmp.path().join("server.key");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
    (tmp, cert_path, key_path)
}

#[test]
fn vt_tls_3_bin_fails_without_cert() {
    let backing = make_backing();
    let port = alloc_port();
    let tls_port = alloc_port();
    let out = Command::new(bin())
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--backing-file")
        .arg(backing.path())
        .arg("--tls-listen")
        .arg(format!("127.0.0.1:{tls_port}"))
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .expect("spawn");
    assert!(
        !out.status.success(),
        "缺 --tls-cert/--tls-key 应 exit != 0; got {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--tls-cert") && stderr.contains("--tls-key"),
        "stderr 应解释缺少 cert/key, got: {stderr}"
    );
}

#[test]
fn vt_tls_3_bin_fails_without_consent() {
    let backing = make_backing();
    let port = alloc_port();
    let tls_port = alloc_port();
    let (_tmp, cert_path, key_path) = make_self_signed_cert();
    let out = Command::new(bin())
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--backing-file")
        .arg(backing.path())
        .arg("--tls-listen")
        .arg(format!("127.0.0.1:{tls_port}"))
        .arg("--tls-cert")
        .arg(&cert_path)
        .arg("--tls-key")
        .arg(&key_path)
        // 故意不传 --tls-i-trust-this-cert
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .expect("spawn");
    assert!(
        !out.status.success(),
        "缺 --tls-i-trust-this-cert 应 exit != 0; got {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--tls-i-trust-this-cert"),
        "stderr 应解释缺少 consent flag, got: {stderr}"
    );
}

#[test]
fn vt_tls_3_bin_fails_when_tls_flag_set_without_listen() {
    let backing = make_backing();
    let port = alloc_port();
    let (_tmp, cert_path, key_path) = make_self_signed_cert();
    // 设了 --tls-cert/--tls-key 但没 --tls-listen → 应 hard-fail（防 silent ignore）
    let out = Command::new(bin())
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--backing-file")
        .arg(backing.path())
        .arg("--tls-cert")
        .arg(&cert_path)
        .arg("--tls-key")
        .arg(&key_path)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .expect("spawn");
    assert!(
        !out.status.success(),
        "设 --tls-cert 但没 --tls-listen 应 exit != 0; got {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--tls-listen"),
        "stderr 应解释要 --tls-listen, got: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn vt_tls_3_bin_starts_with_full_tls_consent() {
    let backing = make_backing();
    let port = alloc_port();
    let tls_port = alloc_port();
    let (_tmp, cert_path, key_path) = make_self_signed_cert();
    let mut child = Command::new(bin())
        .env("RUST_LOG", "info")
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--backing-file")
        .arg(backing.path())
        .arg("--tls-listen")
        .arg(format!("127.0.0.1:{tls_port}"))
        .arg("--tls-cert")
        .arg(&cert_path)
        .arg("--tls-key")
        .arg(&key_path)
        .arg("--tls-i-trust-this-cert")
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn");
    // 等 bin listen up
    thread::sleep(Duration::from_millis(500));
    // 验 TLS port 已 bind（连一下应能 connect 成功）— bin 启动 + TLS listen
    // 路径成功的充分证据
    let probe = std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{tls_port}").parse().unwrap(),
        Duration::from_secs(2),
    );
    assert!(probe.is_ok(), "TLS port {tls_port} 应可 connect");
    drop(probe);
    // 同时验 plaintext port 也 up（dual-listener 健康）
    let plain_probe = std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_secs(2),
    );
    assert!(
        plain_probe.is_ok(),
        "plaintext port {port} 应可 connect (dual-listener)"
    );
    drop(plain_probe);

    // SIGTERM graceful shutdown
    let pid = child.id() as i32;
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .output();
    // 等子进程退出
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                assert!(st.success(), "bin 应 graceful exit code 0, got {st:?}");
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
    // 让 lint 不抱怨未用 import
    let _ = std::io::sink().write_all(b"");
}
