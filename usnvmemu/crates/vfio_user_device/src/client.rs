// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! vfio-user client：connect AF_UNIX + VERSION 握手。

use crate::framing::read_message;
use crate::framing::write_message;
use anyhow::Context as _;
use anyhow::anyhow;
use std::os::unix::net::UnixStream;
use std::path::Path;
use vfio_user_wire::framing::WireMessage;
use vfio_user_wire::handshake::CLIENT_CAPS_JSON;
use vfio_user_wire::handshake::NegotiatedClient;
use vfio_user_wire::handshake::build_version_command_payload;
use vfio_user_wire::handshake::parse_caps_blob;
use vfio_user_wire::handshake::verify_server_version_reply;
use vfio_user_wire::proto::Command;
use vfio_user_wire::proto::DeviceInfoPayload;
use vfio_user_wire::proto::Header;
use vfio_user_wire::proto::IrqInfoPayload;
use vfio_user_wire::proto::PROTOCOL_MAJOR;
use vfio_user_wire::proto::PROTOCOL_MINOR;
use vfio_user_wire::proto::RegionAccessPayload;
use vfio_user_wire::proto::RegionInfoPayload;
use vfio_user_wire::proto::VersionPayload;
use vfio_user_wire::proto::decode_payload;
use zerocopy::IntoBytes;

/// vfio-user client 连接。持一条同步 `UnixStream` + msg_id 计数器。
pub struct VfioUserClient {
    stream: UnixStream,
    /// 下一个请求的 msg_id（W1 握手用 1，W2 起每请求递增；reply 须 echo 同 id）。
    next_msg_id: u16,
}

impl VfioUserClient {
    /// connect 到 server 的 AF_UNIX socket 路径。
    pub fn connect(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(path.as_ref())
            .with_context(|| format!("connect vfio-user socket {:?}", path.as_ref()))?;
        Ok(Self {
            stream,
            next_msg_id: 1,
        })
    }

    /// 从已建立的 `UnixStream` 构造（loopback 单测 / 已 connect 的场景）。
    pub fn from_stream(stream: UnixStream) -> Self {
        Self {
            stream,
            next_msg_id: 1,
        }
    }

    /// 取下一个 msg_id 并自增（wrapping，避免溢出 panic）。
    fn alloc_msg_id(&mut self) -> u16 {
        let id = self.next_msg_id;
        self.next_msg_id = self.next_msg_id.wrapping_add(1);
        id
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
        let msg_id = self.alloc_msg_id();
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

    // ── W2: region wire（GET_INFO / GET_REGION_INFO / GET_IRQ_INFO / REGION_RW / RESET）──

    /// 校验一个 reply 帧：必须是 REPLY（非 command）、非 error、cmd 匹配、msg_id echo。
    ///
    /// **packed 字段先 copy 到本地**（Header `#[repr(C,packed)]`，`deny(unsafe_code)`
    /// 下直接读字段是 E0793）。
    fn expect_reply(reply: &WireMessage, msg_id: u16, cmd: Command) -> anyhow::Result<()> {
        let flags = reply.header.flags();
        let reply_cmd = reply.header.cmd;
        let reply_error_no = reply.header.error_no;
        let reply_msg_id = reply.header.msg_id;
        if !flags.is_reply() {
            return Err(anyhow!("{cmd:?} reply 不是 REPLY 帧"));
        }
        if flags.is_error() {
            return Err(anyhow!("{cmd:?} reply 是 error（error_no={reply_error_no}）"));
        }
        if reply_cmd != cmd as u16 {
            return Err(anyhow!("{cmd:?} reply cmd 不匹配：got {reply_cmd}"));
        }
        if reply_msg_id != msg_id {
            return Err(anyhow!(
                "{cmd:?} reply msg_id 不匹配：sent {msg_id}, got {reply_msg_id}"
            ));
        }
        Ok(())
    }

    /// 发一个"payload 是单个 zerocopy struct"的请求，校验 reply 帧后返回整帧。
    fn request<T: zerocopy::IntoBytes + zerocopy::Immutable>(
        &mut self,
        cmd: Command,
        req: &T,
    ) -> anyhow::Result<WireMessage> {
        let msg_id = self.alloc_msg_id();
        let payload = req.as_bytes();
        let hdr = Header::command(msg_id, cmd, payload.len() as u32);
        write_message(&mut self.stream, &hdr, payload).with_context(|| format!("send {cmd:?}"))?;
        let reply = read_message(&mut self.stream).with_context(|| format!("recv {cmd:?} reply"))?;
        Self::expect_reply(&reply, msg_id, cmd)?;
        Ok(reply)
    }

    /// 解 reply 的前 `size_of::<T>()` 字节为 struct T。
    fn decode_reply_payload<
        T: zerocopy::FromBytes + zerocopy::KnownLayout + zerocopy::Immutable + Copy,
    >(
        reply: &WireMessage,
        cmd: Command,
    ) -> anyhow::Result<T> {
        let want = core::mem::size_of::<T>();
        if reply.payload.len() < want {
            return Err(anyhow!(
                "{cmd:?} reply payload 过短：{} < {want}",
                reply.payload.len()
            ));
        }
        decode_payload(&reply.payload[..want]).with_context(|| format!("decode {cmd:?} reply payload"))
    }

    /// DEVICE_GET_INFO：查 region 数 / IRQ 数 / flags。
    pub fn get_device_info(&mut self) -> anyhow::Result<DeviceInfoPayload> {
        let req = DeviceInfoPayload {
            argsz: core::mem::size_of::<DeviceInfoPayload>() as u32,
            ..Default::default()
        };
        let reply = self.request(Command::DeviceGetInfo, &req)?;
        Self::decode_reply_payload(&reply, Command::DeviceGetInfo)
    }

    /// DEVICE_GET_REGION_INFO：查某 region 的 size / flags。
    pub fn get_region_info(&mut self, index: u32) -> anyhow::Result<RegionInfoPayload> {
        let req = RegionInfoPayload {
            argsz: core::mem::size_of::<RegionInfoPayload>() as u32,
            index,
            ..Default::default()
        };
        let reply = self.request(Command::DeviceGetRegionInfo, &req)?;
        Self::decode_reply_payload(&reply, Command::DeviceGetRegionInfo)
    }

    /// DEVICE_GET_IRQ_INFO：查某 IRQ type 的向量数 / flags。
    pub fn get_irq_info(&mut self, index: u32) -> anyhow::Result<IrqInfoPayload> {
        let req = IrqInfoPayload {
            argsz: core::mem::size_of::<IrqInfoPayload>() as u32,
            index,
            ..Default::default()
        };
        let reply = self.request(Command::DeviceGetIrqInfo, &req)?;
        Self::decode_reply_payload(&reply, Command::DeviceGetIrqInfo)
    }

    /// REGION_READ：读 region[index] 的 [offset, offset+count) 字节。
    ///
    /// reply = `RegionAccessPayload echo(16B) + count 字节数据`；校验 echo 的
    /// region/offset/count 与请求一致，返回数据段。
    pub fn region_read(&mut self, region: u32, offset: u64, count: u32) -> anyhow::Result<Vec<u8>> {
        let req = RegionAccessPayload {
            offset,
            region,
            count,
        };
        let reply = self.request(Command::RegionRead, &req)?;
        let hdr_size = core::mem::size_of::<RegionAccessPayload>();
        let echo: RegionAccessPayload = Self::decode_reply_payload(&reply, Command::RegionRead)?;
        // packed 字段先 copy。
        let echo_region = echo.region;
        let echo_offset = echo.offset;
        let echo_count = echo.count;
        if echo_region != region || echo_offset != offset || echo_count != count {
            return Err(anyhow!(
                "REGION_READ echo 不匹配：req(region={region}, offset={offset}, count={count}) \
                 vs echo(region={echo_region}, offset={echo_offset}, count={echo_count})"
            ));
        }
        let data = reply
            .payload
            .get(hdr_size..hdr_size + count as usize)
            .ok_or_else(|| {
                anyhow!(
                    "REGION_READ reply 数据段过短：payload {} < {}+{count}",
                    reply.payload.len(),
                    hdr_size
                )
            })?;
        Ok(data.to_vec())
    }

    /// REGION_WRITE：把 `data` 写到 region[index] 的 offset 处。
    ///
    /// 请求 = `RegionAccessPayload(16B) + data`；reply = echo only（无数据）。
    pub fn region_write(&mut self, region: u32, offset: u64, data: &[u8]) -> anyhow::Result<()> {
        let req = RegionAccessPayload {
            offset,
            region,
            count: data.len() as u32,
        };
        let mut payload =
            Vec::with_capacity(core::mem::size_of::<RegionAccessPayload>() + data.len());
        payload.extend_from_slice(req.as_bytes());
        payload.extend_from_slice(data);
        let msg_id = self.alloc_msg_id();
        let hdr = Header::command(msg_id, Command::RegionWrite, payload.len() as u32);
        write_message(&mut self.stream, &hdr, &payload).context("send REGION_WRITE")?;
        let reply = read_message(&mut self.stream).context("recv REGION_WRITE reply")?;
        Self::expect_reply(&reply, msg_id, Command::RegionWrite)?;
        Ok(())
    }

    /// DEVICE_RESET：空 payload，触发 server 端 device reset（FLR）。
    pub fn reset(&mut self) -> anyhow::Result<()> {
        let msg_id = self.alloc_msg_id();
        let hdr = Header::command(msg_id, Command::DeviceReset, 0);
        write_message(&mut self.stream, &hdr, &[]).context("send DEVICE_RESET")?;
        let reply = read_message(&mut self.stream).context("recv DEVICE_RESET reply")?;
        Self::expect_reply(&reply, msg_id, Command::DeviceReset)?;
        Ok(())
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
