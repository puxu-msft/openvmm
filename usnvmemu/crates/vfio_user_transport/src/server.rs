// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase U3** — UNIX listener + accept loop。
//!
//! `serve_unix(path, device_factory)` 绑定 UNIX socket、accept 单个 client
//! （vfio-user spec：1 socket / 1 client），跑 handshake → pump loop。
//! peer 断开 / 协议错则 close socket + 重 accept；factory 每次产生一个
//! 新 device instance（教学 NVMe controller 重 open backing file）。

use crate::handshake::server_handshake;
use crate::session::Regions;
use crate::session::VfioUserSession;
use anyhow::Context as _;
use pcie_device_sdk::PcieDevice;
use std::os::unix::net::UnixListener;
use std::path::Path;

/// 启动 UNIX socket server，循环 accept + 跑 session 直到 caller 中断。
///
/// `device_factory` 在每次 accept 后调用一次以构造 device 实例 —— 让
/// reconnect 时设备 state 是干净的。
pub fn serve_unix<P, D, F>(socket_path: P, mut device_factory: F) -> anyhow::Result<()>
where
    P: AsRef<Path>,
    D: PcieDevice + Regions,
    F: FnMut() -> anyhow::Result<D>,
{
    let path = socket_path.as_ref();
    // 清掉旧 socket 文件（spec 没规定，但 QEMU 默认行为）。
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).with_context(|| format!("bind {path:?}"))?;
    tracing::info!(path = ?path, "vfio-user server listening");

    loop {
        let (mut stream, _addr) = listener.accept().context("accept")?;
        tracing::info!("vfio-user client connected");

        let negotiated = match server_handshake(&mut stream) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "handshake failed; closing");
                continue;
            }
        };
        tracing::info!(
            client_major = negotiated.client_major,
            client_minor = negotiated.client_minor,
            "vfio-user VERSION handshake ok"
        );

        let mut device = match device_factory() {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(error = %e, "device factory failed");
                continue;
            }
        };

        let mut session = VfioUserSession::new(stream, negotiated);
        loop {
            match session.pump_one(&mut device) {
                Ok(true) => continue,
                Ok(false) => {
                    tracing::info!("vfio-user peer closed");
                    break;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "session error; closing");
                    break;
                }
            }
        }
    }
}
