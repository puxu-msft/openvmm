// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VERSION 握手的 sans-IO 决策与常量.
//!
//! 抽自 `vfio_user_transport::handshake` (W0-5, 2026-06-11). 本模块只含
//! **无 IO** 的协商决策:
//!
//! - [`SERVER_MAX_DATA_XFER_SIZE`] / [`SERVER_CAPS_JSON`] —— server 端常量
//! - [`Negotiated`] —— 协商结果数据结构
//! - [`negotiate_minor`] —— pure fn: `min(client_minor, PROTOCOL_MINOR)`
//! - [`parse_caps_blob`] —— NUL-terminated bytes → UTF-8 String
//! - [`build_version_reply_payload`] —— 构造 VERSION reply 字节 (不含 IO)
//!
//! UnixStream 收发 (`server_handshake`) 留在 `vfio_user_transport`.

use crate::proto::PROTOCOL_MAJOR;
use crate::proto::PROTOCOL_MINOR;
use crate::proto::ProtoError;
use crate::proto::VersionPayload;
use zerocopy::IntoBytes;

/// 本 server 广告的 `max_data_xfer_size` (字节).
///
/// **vfio-spec 语义**: `max_data_xfer_size` 是 per-receiver 的——发送方不得超过
/// 接收方广告的值. `REGION_READ/WRITE` 是 client→server (接收方 = 本 server),
/// 故服务端用本常量作为 REGION 访问 `count` 的上限. client 广告的值只对反方向
/// (server→client 的 `DMA_READ/WRITE`) 有意义.
///
/// 1 MiB = spec 默认, 且 ≤ QEMU 64 MiB 上限. 必须与 [`SERVER_CAPS_JSON`] 里
/// 广告的数值一致 (`server_caps_advertises_declared_max_xfer` drift gate 测试钉死).
pub const SERVER_MAX_DATA_XFER_SIZE: usize = 1_048_576;

/// 本 server 默认 advertise 的 capabilities JSON.
///
/// **review (真 QEMU 11 oracle)** — `max_msg_fds` 须 ≤ 客户端可接受上限. QEMU
/// v11.0.1 的 `VFIO_USER_MAX_MAX_FDS = 16`, server 广告 > 16 会被判 "malformed
/// max_msg_fds" 握手失败. 广告 8 (= QEMU 默认 `VFIO_USER_DEF_MAX_FDS`) 对所有
/// client 安全; NVMe 单 `SET_IRQS` 只需 msix_count(≤4) 个 fd / `DMA_MAP` 1 个,
/// 8 足够.
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

/// 握手后协商出的双方 capabilities 摘要.
#[derive(Debug, Clone)]
pub struct Negotiated {
    /// client 上报的 protocol major (与 [`PROTOCOL_MAJOR`] 必须相等).
    pub client_major: u16,
    /// client 上报的 protocol minor. spec 允许 server.minor ≤ client.minor.
    pub client_minor: u16,
    /// client 上报的 capabilities JSON 原文 (已 UTF-8 解码 / NUL 去尾).
    pub client_caps_json: String,
}

/// **vfio-spec 协商规则**: server reply 的 version 须 **≤** client 提议.
///
/// 回比 client 高的 minor 会被 QEMU 判 "incompatible server version" 断开.
/// 此前硬编码 `PROTOCOL_MINOR(=1)`, QEMU 11 提议 minor=0 → 回 1 > 0 → 握手失败.
#[inline]
pub fn negotiate_minor(client_minor: u16) -> u16 {
    client_minor.min(PROTOCOL_MINOR)
}

/// 把 VERSION JSON 字节段 (可能带 trailing NUL + padding) 转 String.
///
/// 找首个 NUL; spec 说 NUL-terminated.
pub fn parse_caps_blob(raw: &[u8]) -> Result<String, ProtoError> {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let s =
        std::str::from_utf8(&raw[..end]).map_err(|e| ProtoError::BadJson(format!("UTF-8: {e}")))?;
    Ok(s.to_string())
}

/// 构造 VERSION reply 的 payload 字节: `VersionPayload(4) + caps_json + NUL`.
///
/// 返回 `Vec<u8>`, caller 负责包 Header 并写到 UnixStream.
pub fn build_version_reply_payload(client_minor: u16, server_caps: &[u8]) -> Vec<u8> {
    let payload_len = 4 + server_caps.len() + 1;
    let mut reply_payload = Vec::with_capacity(payload_len);
    let ver_reply = VersionPayload {
        major: PROTOCOL_MAJOR,
        minor: negotiate_minor(client_minor),
    };
    reply_payload.extend_from_slice(ver_reply.as_bytes());
    reply_payload.extend_from_slice(server_caps);
    reply_payload.push(0); // NUL
    reply_payload
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **drift gate** —— `SERVER_MAX_DATA_XFER_SIZE` 须与 `SERVER_CAPS_JSON`
    /// 里广告的数值一致; 任一改了忘改另一个, REGION 上限就和广告脱节.
    #[test]
    fn server_caps_advertises_declared_max_xfer() {
        let needle = format!("\"max_data_xfer_size\":{SERVER_MAX_DATA_XFER_SIZE}");
        assert!(
            SERVER_CAPS_JSON.contains(&needle),
            "SERVER_CAPS_JSON 须广告与 SERVER_MAX_DATA_XFER_SIZE 一致的值: {needle}"
        );
        assert_eq!(
            SERVER_CAPS_JSON.matches("max_data_xfer_size").count(),
            1,
            "max_data_xfer_size 字段须恰好出现一次"
        );
    }

    /// **协商规则锚**: `negotiate_minor` 严格 ≤ client_minor 且 ≤ PROTOCOL_MINOR.
    #[test]
    fn negotiate_minor_returns_min_of_client_and_server() {
        // client < server (PROTOCOL_MINOR ≥ 0 由 u16 类型保证)
        assert_eq!(negotiate_minor(0), 0);
        // client == server
        assert_eq!(negotiate_minor(PROTOCOL_MINOR), PROTOCOL_MINOR);
        // client > server
        assert_eq!(
            negotiate_minor(PROTOCOL_MINOR.saturating_add(5)),
            PROTOCOL_MINOR
        );
    }

    /// reply payload 字节布局锚: 4 bytes VersionPayload + caps bytes + 1 NUL.
    #[test]
    fn build_version_reply_payload_has_expected_layout() {
        let caps = b"{\"capabilities\":{}}";
        let payload = build_version_reply_payload(0, caps);
        assert_eq!(payload.len(), 4 + caps.len() + 1);
        // 末字节是 NUL
        assert_eq!(*payload.last().unwrap(), 0);
        // 头 4 字节是 VersionPayload: major (u16 le) + minor (u16 le)
        assert_eq!(&payload[0..2], PROTOCOL_MAJOR.to_le_bytes());
        // minor = min(0, PROTOCOL_MINOR) = 0
        assert_eq!(&payload[2..4], 0u16.to_le_bytes());
        // 中间是 caps bytes
        assert_eq!(&payload[4..4 + caps.len()], caps);
    }

    /// parse_caps_blob 切首个 NUL 并 UTF-8 解码.
    #[test]
    fn parse_caps_blob_strips_nul_and_padding() {
        let raw = b"{\"x\":1}\0\0\0";
        assert_eq!(parse_caps_blob(raw).unwrap(), "{\"x\":1}");
    }

    /// parse_caps_blob 无 NUL 视全段为 JSON.
    #[test]
    fn parse_caps_blob_no_nul_uses_full_slice() {
        let raw = b"{\"x\":1}";
        assert_eq!(parse_caps_blob(raw).unwrap(), "{\"x\":1}");
    }

    /// parse_caps_blob 拒绝非 UTF-8.
    #[test]
    fn parse_caps_blob_rejects_invalid_utf8() {
        let raw = &[0xff, 0xfe, 0xfd, 0x00];
        assert!(matches!(parse_caps_blob(raw), Err(ProtoError::BadJson(_))));
    }
}
