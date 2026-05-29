// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Transport selection + port validation (spec §3.2)。

use anyhow::Context as _;
use anyhow::anyhow;

/// vsock 端口黑名单（v3.1 修正数值）。
pub const PORT_BLACKLIST: &[u32] = &[
    1,      // VSOCK_CONTROL_PORT (openhcl/diag_proto)
    2,      // VSOCK_DATA_PORT
    3,      // VNC default
    4,      // gdbstub default
    0x1337, // PIPETTE_VSOCK_PORT
];

/// 校验 vsock 端口：拒绝黑名单 + 与 vnc/gdbstub 端口冲突。
pub fn check_vsock_port(
    port: u32,
    vnc_port: Option<u32>,
    gdbstub_port: Option<u32>,
) -> anyhow::Result<()> {
    if PORT_BLACKLIST.contains(&port) {
        return Err(anyhow!("vsock port {port} is in the well-known blacklist"));
    }
    if let Some(p) = vnc_port
        && p == port
    {
        return Err(anyhow!("vsock port {port} conflicts with vnc_port"));
    }
    if let Some(p) = gdbstub_port
        && p == port
    {
        return Err(anyhow!("vsock port {port} conflicts with gdbstub_port"));
    }
    Ok(())
}

/// 仅允许 127.0.0.1 / ::1（spec §3.2 OpenVMM 端 TCP loopback enforce）。
pub fn check_tcp_loopback(addr: &str) -> anyhow::Result<()> {
    let sa: std::net::SocketAddr = addr
        .parse()
        .with_context(|| format!("invalid socket addr: {addr}"))?;
    if !sa.ip().is_loopback() {
        return Err(anyhow!(
            "TCP addr {addr} must be loopback (127.0.0.1 / ::1)"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blacklist_rejects_well_known() {
        for &p in PORT_BLACKLIST {
            assert!(check_vsock_port(p, None, None).is_err());
        }
    }

    #[test]
    fn allows_high_port() {
        assert!(check_vsock_port(50000, None, None).is_ok());
    }

    #[test]
    fn rejects_collision_with_vnc() {
        assert!(check_vsock_port(50000, Some(50000), None).is_err());
    }

    #[test]
    fn rejects_collision_with_gdbstub() {
        assert!(check_vsock_port(50000, None, Some(50000)).is_err());
    }

    #[test]
    fn tcp_loopback_only() {
        assert!(check_tcp_loopback("127.0.0.1:48914").is_ok());
        assert!(check_tcp_loopback("0.0.0.0:48914").is_err());
        assert!(check_tcp_loopback("[::1]:48914").is_ok());
        assert!(check_tcp_loopback("bad").is_err());
    }
}
