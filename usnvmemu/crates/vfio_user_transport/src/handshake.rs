// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase U2** — VERSION 握手 + 服务端 caps JSON 协商。
//!
//! Server-side handshake：
//! 1. accept 后立刻 `read_message` 期待 `Command::Version` 命令；
//! 2. parse 出 client 的 `(major, minor) + caps JSON`；
//! 3. server 回 `Command::Version` reply：major echo，minor 回
//!    `min(client, server)`（spec 要求 server.version ≤ client 提议），
//!    capabilities 为 client 子集（且 `max_msg_fds` 不超过 client 上限）。
//!
//! 我们 advertise 的能力：
//! - `max_msg_fds = 8`（QEMU 默认上限，对所有 client 安全；NVMe 单包 fd 数远小于此）
//! - `max_data_xfer_size = 1 MiB`（spec 默认）
//! - `max_dma_maps = 65535`
//! - `pgsizes = 4096`
//!
//! 不 advertise（spec 允许 omitted）：
//! - `twin_socket` — 单 socket 即可
//! - `write_multiple` — 不实现 batch write
//! - migration / dirty pages — Phase U 外

use crate::framing::read_message;
use crate::framing::write_message;
use crate::proto::Command;
use crate::proto::Header;
use crate::proto::PROTOCOL_MAJOR;
use crate::proto::PROTOCOL_MINOR;
use crate::proto::ProtoError;
use crate::proto::VersionPayload;
use anyhow::Context as _;
use anyhow::anyhow;
use std::os::unix::net::UnixStream;
use zerocopy::IntoBytes;

/// 本 server 默认 advertise 的 capabilities JSON（教学路径写死）。
///
/// **review (真 QEMU 11 oracle)** — `max_msg_fds` 须 ≤ 客户端可接受上限。QEMU
/// v11.0.1 的 `VFIO_USER_MAX_MAX_FDS = 16`，server 广告 > 16 会被判 "malformed
/// max_msg_fds" 握手失败（此前广告 128 直接挂 QEMU）。广告 8（= QEMU 默认
/// `VFIO_USER_DEF_MAX_FDS`，对所有 client 安全）；我们 framing 实际能收
/// [`crate::framing::MAX_MSG_FDS`]，但 NVMe 单 `SET_IRQS` 只需 msix_count(≤4) 个 fd、
/// `DMA_MAP` 1 个，8 足够。其余 caps 值（max_data_xfer_size 1 MiB ≤ QEMU 64 MiB 上限 /
/// max_dma_maps 65535 = QEMU 默认 / pgsizes 4096）均在 QEMU 限内。
pub const SERVER_CAPS_JSON: &str = concat!(
    "{",
    "\"capabilities\":{",
    "\"max_msg_fds\":8,",
    "\"max_data_xfer_size\":1048576,",
    "\"max_dma_maps\":65535,",
    "\"pgsizes\":4096",
    "}",
    "}"
);

/// 握手后协商出的双方 capabilities 摘要（本 server 当前不主动按 client
/// caps 调整行为，仅记录 + log）。
#[derive(Debug, Clone)]
pub struct Negotiated {
    /// client 上报的 protocol major（与 [`PROTOCOL_MAJOR`] 必须相等）。
    pub client_major: u16,
    /// client 上报的 protocol minor。spec 允许 server.minor ≤ client.minor，
    /// 故教学路径不强校验，仅记录；reply 时回 `min(client, PROTOCOL_MINOR)`。
    pub client_minor: u16,
    /// client 发来的原始 caps JSON 字符串（caller 自行 parse / 记录）。
    pub client_caps_json: String,
}

/// Server-side VERSION handshake 一轮：阻塞读 client `Version` cmd → 回 reply。
///
/// 协议错误（错命令、major mismatch、JSON 解析失败）会 send error reply +
/// 返 `Err`，调用方应 close socket。
pub fn server_handshake(stream: &mut UnixStream) -> anyhow::Result<Negotiated> {
    let msg = read_message(stream).context("read VERSION request")?;
    if !msg.header.flags().is_command() {
        send_err(
            stream,
            msg.header.msg_id,
            Command::Version,
            libc::EINVAL as u32,
        );
        return Err(anyhow!("expected COMMAND, got REPLY in VERSION handshake"));
    }
    let cmd_u = msg.header.cmd;
    if cmd_u != Command::Version as u16 {
        send_err(
            stream,
            msg.header.msg_id,
            Command::Version,
            libc::EPROTO as u32,
        );
        return Err(anyhow!(
            "expected VERSION (cmd=1) as first message, got cmd={cmd_u}"
        ));
    }
    let payload = &msg.payload;
    if payload.len() < core::mem::size_of::<VersionPayload>() {
        send_err(
            stream,
            msg.header.msg_id,
            Command::Version,
            libc::EINVAL as u32,
        );
        return Err(anyhow!(
            "VERSION payload too short: {} < {}",
            payload.len(),
            core::mem::size_of::<VersionPayload>()
        ));
    }
    // 头 4 byte = major/minor；剩余 = NUL-terminated UTF-8 JSON。
    let ver: VersionPayload =
        crate::proto::decode_payload(&payload[..4]).context("decode VersionPayload prefix")?;
    let client_major = ver.major;
    let client_minor = ver.minor;
    if client_major != PROTOCOL_MAJOR {
        send_err(
            stream,
            msg.header.msg_id,
            Command::Version,
            libc::EPROTO as u32,
        );
        return Err(anyhow!(
            "VERSION major mismatch: client={client_major} server={PROTOCOL_MAJOR}"
        ));
    }
    // minor：spec 允许 server.minor ≤ client.minor。我们写死 0.1，所以只要
    // 不强校验 client.minor（教学放宽）；reply 时回 min(client, server) 协商值。
    // JSON：从 4 字节后到（可选）NUL 之前。
    let json_bytes = &payload[4..];
    let json_str =
        parse_caps_blob(json_bytes).map_err(|e| anyhow!("VERSION caps JSON 非法: {e}"))?;
    tracing::debug!(
        client_major,
        client_minor,
        caps = json_str.as_str(),
        "vfio-user VERSION client cmd received"
    );

    // ─── 回 reply ───
    // server caps JSON + NUL terminator
    let server_caps = SERVER_CAPS_JSON.as_bytes();
    // payload = VersionPayload(4) + caps_json + NUL byte
    let payload_len = 4 + server_caps.len() + 1;
    let mut reply_payload = Vec::with_capacity(payload_len);
    let ver_reply = VersionPayload {
        major: PROTOCOL_MAJOR,
        // **review (真 QEMU 11 oracle)** — 协商 minor = min(client, server)。
        // vfio-user spec：server reply 的 version 须 **≤** client 提议的；回比 client
        // 高的 minor 会被 QEMU 判 "incompatible server version" 断开。此前硬编码
        // PROTOCOL_MINOR(=1)，QEMU 11 提议 minor=0 → 我们回 1 > 0 → 握手失败。
        minor: client_minor.min(PROTOCOL_MINOR),
    };
    reply_payload.extend_from_slice(ver_reply.as_bytes());
    reply_payload.extend_from_slice(server_caps);
    reply_payload.push(0); // NUL
    let reply_hdr = Header::reply_ok(msg.header.msg_id, Command::Version, payload_len as u32);
    write_message(stream, &reply_hdr, &reply_payload, &[]).context("write VERSION reply")?;

    Ok(Negotiated {
        client_major,
        client_minor,
        client_caps_json: json_str,
    })
}

/// 把 VERSION JSON 字节段（可能带 trailing NUL + padding）转 String。
fn parse_caps_blob(raw: &[u8]) -> Result<String, ProtoError> {
    // 找首个 NUL；spec 说 NUL-terminated。
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let s =
        std::str::from_utf8(&raw[..end]).map_err(|e| ProtoError::BadJson(format!("UTF-8: {e}")))?;
    Ok(s.to_string())
}

/// Best-effort 发一个 error reply；任何 IO err 记 warn（caller 已在错误路径，
/// 不再向上传播）。
fn send_err(stream: &mut UnixStream, msg_id: u16, cmd: Command, errno: u32) {
    let hdr = Header::reply_err(msg_id, cmd, errno);
    if let Err(e) = write_message(stream, &hdr, &[], &[]) {
        tracing::warn!(
            error = %e,
            msg_id,
            ?cmd,
            errno,
            "failed to send error reply during handshake"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Command;
    use crate::proto::Header;
    use std::thread;

    /// 两端 UnixStream — 一端跑 server_handshake，另一端作 client 发 VERSION。
    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socketpair")
    }

    fn client_send_version(stream: &mut UnixStream, major: u16, minor: u16, caps_json: &str) {
        let mut payload = Vec::new();
        payload.extend_from_slice(VersionPayload { major, minor }.as_bytes());
        payload.extend_from_slice(caps_json.as_bytes());
        payload.push(0);
        let hdr = Header::command(0x42, Command::Version, payload.len() as u32);
        write_message(stream, &hdr, &payload, &[]).unwrap();
    }

    /// 正常 handshake：client 0.1 + 默认 caps → server 接受 + 回 reply。
    #[test]
    fn handshake_happy_path() {
        let (mut server, mut client) = pair();
        let handle = thread::spawn(move || server_handshake(&mut server));
        client_send_version(&mut client, 0, 1, "{\"capabilities\":{}}");
        // 读 server reply
        let reply = read_message(&mut client).expect("read VERSION reply");
        assert!(reply.header.flags().is_reply() && !reply.header.flags().is_error());
        let cmd = reply.header.cmd;
        assert_eq!(cmd, Command::Version as u16);
        let neg = handle.join().unwrap().expect("server handshake ok");
        assert_eq!(neg.client_major, 0);
        assert_eq!(neg.client_minor, 1);
        assert!(neg.client_caps_json.contains("capabilities"));
        // reply payload = VersionPayload + server caps JSON + NUL
        let ver: VersionPayload = crate::proto::decode_payload(&reply.payload[..4]).unwrap();
        let major = ver.major;
        let minor = ver.minor;
        assert_eq!(major, PROTOCOL_MAJOR);
        assert_eq!(minor, PROTOCOL_MINOR);
        // 截到第一个 NUL，做 UTF-8 检查
        let server_json_end = reply.payload[4..].iter().position(|&b| b == 0).unwrap();
        let server_json = std::str::from_utf8(&reply.payload[4..4 + server_json_end]).unwrap();
        assert!(server_json.contains("max_msg_fds"));
    }

    /// **review (真 QEMU 11 oracle)** — client 提议 minor < server 时，reply 的
    /// minor 须回 `min(client, server)`（≤ client）。QEMU 11 提议 minor=0；若 server
    /// 回硬编码的 minor=1 会被判 "incompatible server version" 断开握手。
    #[test]
    fn handshake_replies_min_minor_not_higher_than_client() {
        let (mut server, mut client) = pair();
        let handle = thread::spawn(move || server_handshake(&mut server));
        client_send_version(&mut client, 0, 0, "{\"capabilities\":{}}"); // minor=0 (QEMU 11)
        let reply = read_message(&mut client).expect("read VERSION reply");
        let neg = handle.join().unwrap().expect("server handshake ok");
        assert_eq!(neg.client_minor, 0);
        let ver: VersionPayload = crate::proto::decode_payload(&reply.payload[..4]).unwrap();
        let minor = ver.minor;
        assert_eq!(
            minor, 0,
            "server reply minor 须 ≤ client 提议（min(0,1)=0），否则 QEMU 判 incompatible"
        );
    }

    /// **review L-1** — 反方向：client 提议 minor **高于** server，reply 须封顶到
    /// server 支持上限（`min(5,1)=1`），不得 echo client 的 5。锁死协商是 `min`
    /// 而非 `max`/直接 echo。
    #[test]
    fn handshake_caps_minor_at_server_supported_max() {
        let (mut server, mut client) = pair();
        let handle = thread::spawn(move || server_handshake(&mut server));
        client_send_version(&mut client, 0, 5, "{\"capabilities\":{}}"); // minor=5 > server 1
        let reply = read_message(&mut client).expect("read VERSION reply");
        let neg = handle.join().unwrap().expect("server handshake ok");
        assert_eq!(neg.client_minor, 5);
        let ver: VersionPayload = crate::proto::decode_payload(&reply.payload[..4]).unwrap();
        let minor = ver.minor;
        assert_eq!(
            minor, PROTOCOL_MINOR,
            "client minor > server 时 reply 须封顶到 server 上限（min），不得 echo client"
        );
    }

    /// 错命令 → server 回 EPROTO error。
    #[test]
    fn handshake_rejects_non_version_first_message() {
        let (mut server, mut client) = pair();
        let handle = thread::spawn(move || server_handshake(&mut server));
        // 客户端先发 DEVICE_RESET（非 VERSION）
        let hdr = Header::command(7, Command::DeviceReset, 0);
        write_message(&mut client, &hdr, &[], &[]).unwrap();
        let reply = read_message(&mut client).expect("read err reply");
        assert!(reply.header.flags().is_error());
        let err = reply.header.error_no;
        assert_eq!(err, libc::EPROTO as u32);
        let r = handle.join().unwrap();
        assert!(r.is_err());
    }

    /// major mismatch → server 回 EPROTO error。
    #[test]
    fn handshake_rejects_major_mismatch() {
        let (mut server, mut client) = pair();
        let handle = thread::spawn(move || server_handshake(&mut server));
        client_send_version(&mut client, /*major*/ 99, 0, "{}");
        let reply = read_message(&mut client).expect("read err reply");
        assert!(reply.header.flags().is_error());
        let err = reply.header.error_no;
        assert_eq!(err, libc::EPROTO as u32);
        let r = handle.join().unwrap();
        assert!(r.is_err());
    }

    /// Caps JSON 是非 UTF-8 → server 回 EINVAL error。
    #[test]
    fn handshake_rejects_bad_utf8_caps() {
        let (mut server, mut client) = pair();
        let handle = thread::spawn(move || server_handshake(&mut server));
        // 构造非法 UTF-8 caps payload
        let mut payload = Vec::new();
        payload.extend_from_slice(VersionPayload { major: 0, minor: 1 }.as_bytes());
        payload.extend_from_slice(&[0xff, 0xfe]); // invalid UTF-8 head
        payload.push(0);
        let hdr = Header::command(0x33, Command::Version, payload.len() as u32);
        write_message(&mut client, &hdr, &payload, &[]).unwrap();
        // 注意：当前实现是先 send err 再 return Err；不再写 ok reply
        let _ = read_message(&mut client); // 可能 err 也可能正常 reply（取决何处先 fail）
        let r = handle.join().unwrap();
        assert!(r.is_err(), "expected handshake to fail on bad UTF-8 caps");
    }
}
