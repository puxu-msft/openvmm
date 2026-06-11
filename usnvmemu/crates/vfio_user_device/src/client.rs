// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! vfio-user client：connect AF_UNIX + VERSION 握手。

use crate::framing::read_message;
use crate::framing::write_message;
use anyhow::Context as _;
use anyhow::anyhow;
use std::os::unix::net::UnixStream;
use std::path::Path;
use vfio_user_wire::handshake::CLIENT_CAPS_JSON;
use vfio_user_wire::handshake::NegotiatedClient;
use vfio_user_wire::handshake::build_version_command_payload;
use vfio_user_wire::handshake::parse_caps_blob;
use vfio_user_wire::handshake::verify_server_version_reply;
use vfio_user_wire::proto::Command;
use vfio_user_wire::proto::Header;
use vfio_user_wire::proto::PROTOCOL_MAJOR;
use vfio_user_wire::proto::PROTOCOL_MINOR;
use vfio_user_wire::proto::VersionPayload;
use vfio_user_wire::proto::decode_payload;

/// vfio-user client 连接。W1 持一条同步 `UnixStream`，做 VERSION 握手。
pub struct VfioUserClient {
    stream: UnixStream,
}

impl VfioUserClient {
    /// connect 到 server 的 AF_UNIX socket 路径。
    pub fn connect(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(path.as_ref())
            .with_context(|| format!("connect vfio-user socket {:?}", path.as_ref()))?;
        Ok(Self { stream })
    }

    /// 从已建立的 `UnixStream` 构造（loopback 单测 / 已 connect 的场景）。
    pub fn from_stream(stream: UnixStream) -> Self {
        Self { stream }
    }

    /// 执行 VERSION 握手：发 client VERSION command → 收 server reply → 验证协商。
    ///
    /// client 提议 `(PROTOCOL_MAJOR, PROTOCOL_MINOR)`；server 回
    /// `(PROTOCOL_MAJOR, min(client_minor, server_minor))`；client 验
    /// `reply.major == 提议` 且 `reply.minor <= 提议`（见
    /// [`verify_server_version_reply`]）。
    pub fn handshake(&mut self) -> anyhow::Result<NegotiatedClient> {
        // 1. 发 VERSION command。
        let payload = build_version_command_payload(
            PROTOCOL_MAJOR,
            PROTOCOL_MINOR,
            CLIENT_CAPS_JSON.as_bytes(),
        );
        let msg_id = 1u16;
        let hdr = Header::command(msg_id, Command::Version, payload.len() as u32);
        write_message(&mut self.stream, &hdr, &payload).context("send VERSION command")?;

        // 2. 收 server reply。
        let reply = read_message(&mut self.stream).context("recv VERSION reply")?;

        // 3. reply 帧校验。**packed 字段先 copy 到本地**（Header #[repr(C,packed)]，
        //    deny(unsafe_code) 下不能直接读字段，E0793）。flags() 返回 by-value，OK。
        let flags = reply.header.flags();
        let reply_cmd = reply.header.cmd;
        let reply_error_no = reply.header.error_no;
        let reply_msg_id = reply.header.msg_id;
        if !flags.is_reply() {
            return Err(anyhow!("server VERSION reply 不是 REPLY 帧"));
        }
        if flags.is_error() {
            return Err(anyhow!(
                "server VERSION reply 是 error（error_no={reply_error_no}）"
            ));
        }
        if reply_cmd != Command::Version as u16 {
            return Err(anyhow!("server VERSION reply cmd 不匹配：got {reply_cmd}"));
        }
        if reply_msg_id != msg_id {
            return Err(anyhow!(
                "server VERSION reply msg_id 不匹配：sent {msg_id}, got {reply_msg_id}"
            ));
        }

        // 4. 解 VersionPayload + 验证协商。
        let vp_size = core::mem::size_of::<VersionPayload>();
        if reply.payload.len() < vp_size {
            return Err(anyhow!("server VERSION reply payload 过短"));
        }
        let ver: VersionPayload =
            decode_payload(&reply.payload[..vp_size]).context("decode server VersionPayload")?;
        verify_server_version_reply(PROTOCOL_MAJOR, PROTOCOL_MINOR, &ver)
            .context("verify server VERSION reply")?;

        // packed 字段先 copy。
        let server_major = ver.major;
        let server_minor = ver.minor;

        // 5. 解 server caps（NUL 截断 + UTF-8）。
        let server_caps_json =
            parse_caps_blob(&reply.payload[vp_size..]).context("parse server caps")?;

        Ok(NegotiatedClient {
            server_major,
            server_minor,
            server_caps_json,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::write_message;
    use std::thread;

    /// server 回 error reply（flags F_ERROR）→ client handshake 报错。
    #[test]
    fn handshake_rejects_error_reply() {
        let (server_end, client_end) = UnixStream::pair().unwrap();
        let srv = thread::spawn(move || {
            let mut s = server_end;
            // 先读 client 的 VERSION command（丢弃），再回 error reply。
            let _ = read_message(&mut s).unwrap();
            // EPROTO=71（Linux），避免引 libc dep。
            let hdr = Header::reply_err(1, Command::Version, 71);
            write_message(&mut s, &hdr, &[]).unwrap();
        });
        let mut client = VfioUserClient::from_stream(client_end);
        let err = client.handshake().unwrap_err();
        assert!(
            format!("{err:#}").contains("error"),
            "应报 error reply：{err:#}"
        );
        srv.join().unwrap();
    }
}
