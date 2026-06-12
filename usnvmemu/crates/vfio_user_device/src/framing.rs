// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! client 端线消息收发（**async**，W6a）。
//!
//! 基于 [`crate::async_socket::AsyncSocket`]（pal_async PolledSocket + SCM_RIGHTS）。
//! 字节布局与 server 端 `vfio_user_transport::framing` 一致（同一份
//! `vfio_user_wire::proto` 编解码），仅收发原语不同（client async）。
//!
//! 本模块纯 safe（cmsg/unsafe 全在 [`crate::async_socket`]）。
#![deny(unsafe_code)]

use crate::async_socket::AsyncSocket;
use anyhow::Context as _;
use std::io::IoSlice;
use std::os::fd::BorrowedFd;
use vfio_user_wire::framing::WireMessage;
use vfio_user_wire::proto::HEADER_LEN;
use vfio_user_wire::proto::Header;
use vfio_user_wire::proto::decode_header;
use zerocopy::IntoBytes; // Header::as_bytes() 需要。

/// 写一帧（header + payload），可经 SCM_RIGHTS 附带 `fds`（空 slice = 无 fd）。
///
/// fd 必须随首字节发出（vfio-user spec：所有 fd 跟首字节走）；[`AsyncSocket::send_with_fds`]
/// 保证 fd 只随首次 sendmsg + 短写循环补齐。统一了原 `write_message` / `write_message_with_fds`。
pub(crate) async fn write_message(
    sock: &AsyncSocket,
    header: &Header,
    payload: &[u8],
    fds: &[BorrowedFd<'_>],
) -> anyhow::Result<()> {
    let hdr_bytes = header.as_bytes();
    let iov = [IoSlice::new(hdr_bytes), IoSlice::new(payload)];
    sock.send_with_fds(&iov, fds)
        .await
        .context("write_message: send_with_fds")?;
    Ok(())
}

/// 读一帧（header + payload，无 fd）。先读定长 header，据 `msg_size` 读 payload。
pub(crate) async fn read_message(sock: &AsyncSocket) -> anyhow::Result<WireMessage> {
    let mut hdr_buf = [0u8; HEADER_LEN];
    sock.recv_exact(&mut hdr_buf)
        .await
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
    sock.recv_exact(&mut payload)
        .await
        .context("read_message: read payload")?;
    Ok(WireMessage { header, payload })
}

/// **CMB-P4 (map 模式)** — 读一帧并**捕获**随首字节到达的 SCM_RIGHTS fd（map 模式
/// `GET_REGION_INFO` reply 带 CMB region memfd）。返回 `(WireMessage, Vec<OwnedFd>)`。
///
/// fd 仅随 header（首字节）到达，故 header 段用 [`AsyncSocket::recv_exact_with_fds`]
/// 捕获；payload 段无 fd（用普通 `recv_exact`）。普通 reply 无 fd → 返回空 Vec，与
/// [`read_message`] 等价（故只在确实期待 fd 的 map 路径用本函数）。
pub(crate) async fn read_message_with_fds(
    sock: &AsyncSocket,
) -> anyhow::Result<(WireMessage, Vec<std::os::fd::OwnedFd>)> {
    let mut hdr_buf = [0u8; HEADER_LEN];
    let fds = sock
        .recv_exact_with_fds(&mut hdr_buf)
        .await
        .context("read_message_with_fds: read header + fds")?;
    let header = decode_header(&hdr_buf).context("read_message_with_fds: decode_header")?;
    let msg_size = header.msg_size as usize;
    let payload_len = msg_size
        .checked_sub(HEADER_LEN)
        .context("read_message_with_fds: msg_size < HEADER_LEN")?;
    let mut payload = vec![0u8; payload_len];
    sock.recv_exact(&mut payload)
        .await
        .context("read_message_with_fds: read payload")?;
    Ok((WireMessage { header, payload }, fds))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::async_socket::AsyncSocket;
    use pal_async::DefaultPool;
    use pal_async::socket::PolledSocket;
    use std::os::unix::net::UnixStream;
    use vfio_user_wire::proto::Command;

    /// header + payload roundtrip（两端 AsyncSocket，无 fd）。
    #[test]
    fn roundtrip_header_and_payload() {
        DefaultPool::run_with(async |driver| {
            let (a, b) = UnixStream::pair().unwrap();
            let sa = AsyncSocket::new(PolledSocket::new(&driver, a).unwrap());
            let sb = AsyncSocket::new(PolledSocket::new(&driver, b).unwrap());
            let payload = b"hello-vfio-user".to_vec();
            let hdr = Header::command(7, Command::Version, payload.len() as u32);
            write_message(&sa, &hdr, &payload, &[]).await.unwrap();
            let got = read_message(&sb).await.unwrap();
            let msg_id = got.header.msg_id; // packed 字段先 copy
            assert_eq!(msg_id, 7);
            assert_eq!(got.payload, payload);
        });
    }

    /// 空 payload roundtrip。
    #[test]
    fn roundtrip_header_only() {
        DefaultPool::run_with(async |driver| {
            let (a, b) = UnixStream::pair().unwrap();
            let sa = AsyncSocket::new(PolledSocket::new(&driver, a).unwrap());
            let sb = AsyncSocket::new(PolledSocket::new(&driver, b).unwrap());
            let hdr = Header::command(1, Command::Version, 0);
            write_message(&sa, &hdr, &[], &[]).await.unwrap();
            let got = read_message(&sb).await.unwrap();
            let msg_id = got.header.msg_id; // packed 字段先 copy
            assert_eq!(msg_id, 1);
            assert!(got.payload.is_empty());
        });
    }

    /// write_message 带 1 fd：对端收回 header+payload（fd 路径不破坏字节流；fd 真正
    /// 传达由 loopback_dma 真 server mmap 端到端证）。
    #[test]
    fn write_with_fds_sends_header_and_payload() {
        use std::os::fd::AsFd;
        DefaultPool::run_with(async |driver| {
            let (a, b) = UnixStream::pair().unwrap();
            let sa = AsyncSocket::new(PolledSocket::new(&driver, a).unwrap());
            let sb = AsyncSocket::new(PolledSocket::new(&driver, b).unwrap());
            let devnull = std::fs::File::open("/dev/null").unwrap();
            let payload = b"dma-map-payload".to_vec();
            let hdr = Header::command(3, Command::DmaMap, payload.len() as u32);
            write_message(&sa, &hdr, &payload, &[devnull.as_fd()])
                .await
                .unwrap();
            let got = read_message(&sb).await.unwrap();
            let msg_id = got.header.msg_id; // packed 字段先 copy
            assert_eq!(msg_id, 3);
            assert_eq!(got.payload, payload);
        });
    }
}
