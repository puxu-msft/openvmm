// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! client 端线消息收发（W1：纯 `UnixStream` read/write，无 fd）。
//!
//! 握手不携带 fd，故 W1 用 `Read::read_exact` / `Write::write_all` 即可，无需
//! recvmsg/SCM_RIGHTS（那是 W3 DMA_MAP fd 传递才需要）。保持本 crate
//! `#![deny(unsafe_code)]` 干净。
//!
//! 字节布局与 server 端 `vfio_user_transport::framing` 一致（同一份
//! `vfio_user_wire::proto` 编解码），仅收发原语不同（client connect / 无 fd）。

use anyhow::Context as _;
use std::io::Read;
use std::io::Write;
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
}
