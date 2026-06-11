// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! client 端线消息收发。
//!
//! - W1/W2：纯 `UnixStream` read/write（无 fd）——握手 + region wire。
//! - W3：`write_message_with_fds` 经 SCM_RIGHTS 发 fd（DMA_MAP）——**全 safe nix
//!   `sendmsg`+`ScmRights`，无 unsafe**（收 fd 才需 `from_raw_fd` unsafe，client 只发
//!   不收，DMA_MAP reply 无 fd）。保持本 crate `#![deny(unsafe_code)]` 干净。
//!
//! 字节布局与 server 端 `vfio_user_transport::framing` 一致（同一份
//! `vfio_user_wire::proto` 编解码），仅收发原语不同（client connect）。

use anyhow::Context as _;
use nix::sys::socket::ControlMessage;
use nix::sys::socket::MsgFlags;
use nix::sys::socket::sendmsg;
use std::io::IoSlice;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;
use vfio_user_wire::framing::WireMessage;
use vfio_user_wire::proto::HEADER_LEN;
use vfio_user_wire::proto::Header;
use vfio_user_wire::proto::decode_header;
use zerocopy::IntoBytes; // Header::as_bytes() 需要（写死，非"若报错再加"）

/// 写一帧（header + payload，无 fd）到 stream。
pub(crate) fn write_message(
    stream: &mut UnixStream,
    header: &Header,
    payload: &[u8],
) -> anyhow::Result<()> {
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.extend_from_slice(header.as_bytes());
    buf.extend_from_slice(payload);
    stream.write_all(&buf).context("write_message: write_all")?;
    stream.flush().context("write_message: flush")?;
    Ok(())
}

/// 读一帧（header + payload，无 fd）。先读定长 header，据 `msg_size` 读 payload。
pub(crate) fn read_message(stream: &mut UnixStream) -> anyhow::Result<WireMessage> {
    let mut hdr_buf = [0u8; HEADER_LEN];
    stream
        .read_exact(&mut hdr_buf)
        .context("read_message: read header")?;
    let header = decode_header(&hdr_buf).context("read_message: decode_header")?;
    // **packed 字段先 copy 到本地**（Header 是 #[repr(C,packed)]，deny(unsafe_code)
    // 下不能直接借用/读字段，E0793）。decode_header 已校验 msg_size ∈ [HEADER_LEN,
    // MAX_MSG_SIZE]，checked_sub 是双保险。
    let msg_size = header.msg_size as usize;
    let payload_len = msg_size
        .checked_sub(HEADER_LEN)
        .context("read_message: msg_size < HEADER_LEN")?;
    let mut payload = vec![0u8; payload_len];
    stream
        .read_exact(&mut payload)
        .context("read_message: read payload")?;
    Ok(WireMessage { header, payload })
}

/// 写一帧（header + payload）并经 SCM_RIGHTS 附带 `fds` 到 stream。
///
/// 用 `nix::sendmsg` + `ControlMessage::ScmRights`——**全 safe nix API，无 unsafe**
/// （收 fd 才需 `OwnedFd::from_raw_fd` unsafe；本函数只发不收）。fd 必须随首字节
/// 发出（vfio-user spec：所有 fd 跟首字节走），故单次 sendmsg 带 header+payload+cmsg。
/// short-write 分支照搬 server `vfio_user_transport::framing::write_message`。
pub(crate) fn write_message_with_fds(
    stream: &mut UnixStream,
    header: &Header,
    payload: &[u8],
    fds: &[RawFd],
) -> anyhow::Result<()> {
    let hdr_bytes = header.as_bytes();
    let iov = [IoSlice::new(hdr_bytes), IoSlice::new(payload)];
    let cmsgs: Vec<ControlMessage<'_>> = if fds.is_empty() {
        Vec::new()
    } else {
        vec![ControlMessage::ScmRights(fds)]
    };
    // 首次 sendmsg 带 fd；fd 必须与首字节一并传递。
    let first = sendmsg::<()>(stream.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
        .context("write_message_with_fds: sendmsg")?;
    if first < hdr_bytes.len() {
        // header 没写完 — 先补 header 尾，再写 payload。
        stream
            .write_all(&hdr_bytes[first..])
            .context("write header tail after short sendmsg")?;
        stream
            .write_all(payload)
            .context("write full payload after short header sendmsg")?;
    } else {
        let pay_off = first - hdr_bytes.len();
        if pay_off < payload.len() {
            stream
                .write_all(&payload[pay_off..])
                .context("write payload tail after short sendmsg")?;
        }
    }
    stream.flush().context("write_message_with_fds: flush")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfio_user_wire::proto::Command;

    /// header + payload roundtrip（socketpair，无 fd）。
    #[test]
    fn roundtrip_header_and_payload() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let payload = b"hello-vfio-user".to_vec();
        let hdr = Header::command(7, Command::Version, payload.len() as u32);
        write_message(&mut a, &hdr, &payload).unwrap();
        let got = read_message(&mut b).unwrap();
        let msg_id = got.header.msg_id; // packed 字段先 copy
        assert_eq!(msg_id, 7);
        assert_eq!(got.payload, payload);
    }

    /// 空 payload roundtrip。
    #[test]
    fn roundtrip_header_only() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let hdr = Header::command(1, Command::Version, 0);
        write_message(&mut a, &hdr, &[]).unwrap();
        let got = read_message(&mut b).unwrap();
        let msg_id = got.header.msg_id; // packed 字段先 copy
        assert_eq!(msg_id, 1);
        assert!(got.payload.is_empty());
    }

    /// W3: write_message_with_fds 发 header+payload+1fd；对端用纯 read_message
    /// 读回 header+payload（ancillary fd 被纯 read 忽略，字节流不受影响）。fd 真正
    /// 传达由 loopback_dma 端到端证（server mmap 成功）。
    #[test]
    fn write_with_fds_sends_header_and_payload() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let devnull = std::fs::File::open("/dev/null").unwrap();
        let payload = b"dma-map-payload".to_vec();
        let hdr = Header::command(3, Command::DmaMap, payload.len() as u32);
        write_message_with_fds(&mut a, &hdr, &payload, &[devnull.as_raw_fd()]).unwrap();
        let got = read_message(&mut b).unwrap();
        let msg_id = got.header.msg_id; // packed 字段先 copy
        assert_eq!(msg_id, 3);
        assert_eq!(got.payload, payload);
    }
}
