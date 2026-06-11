// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 全双工 vfio-user client 通道（**W6b**）：把一个已握手的 [`crate::VfioUserClient`]
//! 拆成独立的写半 [`VfioUserWriter`] + 读半 [`VfioUserReader`]，供 underhill device
//! worker 在 `select!` 里**并发**收发——发 MMIO 请求帧的同时收 reply 帧，多帧
//! in-flight 由 worker 自己按 `msg_id` 匹配（msg_id 分配权移交调用方）。
//!
//! 为什么需要它（架构 BLOCKING B-1）：串行 [`crate::VfioUserClient`] 每个方法是「写一帧
//! → 立即同一调用里读 reply」（一把 `Mutex<PolledSocket>`）。若 worker 这样 `await`，那
//! firmware 一慢就会卡死整个 `select!` 循环，连 shutdown 都处理不了——真死锁风险。本通道
//! 经 [`pal_async::socket::PolledSocket::split`] 拆读写半（共享 socket 但不跨内核调用持
//! 锁），send/recv 真正并发。
//!
//! 本模块纯 safe（cmsg/unsafe 全在 [`crate::async_socket`]）。
#![deny(unsafe_code)]

use crate::async_socket::ReaderHalf;
use crate::async_socket::WriterHalf;
use anyhow::Context as _;
use std::io::IoSlice;
use std::os::fd::BorrowedFd;
use vfio_user_wire::framing::WireMessage;
use vfio_user_wire::proto::Command;
use vfio_user_wire::proto::HEADER_LEN;
use vfio_user_wire::proto::Header;
use vfio_user_wire::proto::RegionAccessPayload;
use vfio_user_wire::proto::decode_header;
use zerocopy::IntoBytes;

/// 全双工写半：发 command 帧（**不等 reply**）。worker 持此发 MMIO 请求。
pub struct VfioUserWriter {
    write: WriterHalf,
}

/// 全双工读半：读 reply 帧（worker 按 `msg_id` 自行匹配 in-flight 表）。
pub struct VfioUserReader {
    read: ReaderHalf,
}

impl VfioUserWriter {
    /// 内部构造（仅 [`crate::VfioUserClient::into_channel`] 调用）。
    pub(crate) fn new(write: WriterHalf) -> Self {
        Self { write }
    }

    /// 发一帧 command（不等 reply）。`fds` 空 slice = 无 fd（经 SCM_RIGHTS 随首字节发）。
    /// `msg_id` 由调用方（worker）分配并自行记入 in-flight 表。
    pub async fn send_command(
        &mut self,
        msg_id: u16,
        cmd: Command,
        payload: &[u8],
        fds: &[BorrowedFd<'_>],
    ) -> anyhow::Result<()> {
        let hdr = Header::command(msg_id, cmd, payload.len() as u32);
        let hdr_bytes = hdr.as_bytes();
        let iov = [IoSlice::new(hdr_bytes), IoSlice::new(payload)];
        self.write
            .send_with_fds(&iov, fds)
            .await
            .context("send_command: send_with_fds")?;
        Ok(())
    }

    /// 便捷：REGION_READ 帧（worker MMIO read 用）。payload = `RegionAccessPayload`(16B)。
    pub async fn send_region_read(
        &mut self,
        msg_id: u16,
        region: u32,
        offset: u64,
        count: u32,
    ) -> anyhow::Result<()> {
        let req = RegionAccessPayload {
            offset,
            region,
            count,
        };
        self.send_command(msg_id, Command::RegionRead, req.as_bytes(), &[])
            .await
    }

    /// 便捷：REGION_WRITE 帧（worker MMIO write fire-and-forget 用）。
    /// payload = `RegionAccessPayload`(16B) ++ `data`（与 `client.rs::region_write` 一致）。
    pub async fn send_region_write(
        &mut self,
        msg_id: u16,
        region: u32,
        offset: u64,
        data: &[u8],
    ) -> anyhow::Result<()> {
        let req = RegionAccessPayload {
            offset,
            region,
            count: data.len() as u32,
        };
        let mut payload =
            Vec::with_capacity(core::mem::size_of::<RegionAccessPayload>() + data.len());
        payload.extend_from_slice(req.as_bytes());
        payload.extend_from_slice(data);
        self.send_command(msg_id, Command::RegionWrite, &payload, &[])
            .await
    }
}

impl VfioUserReader {
    /// 内部构造（仅 [`crate::VfioUserClient::into_channel`] 调用）。
    pub(crate) fn new(read: ReaderHalf) -> Self {
        Self { read }
    }

    /// 读一帧 reply（worker 按 `header.msg_id` 自行匹配 in-flight）。镜像
    /// `framing::read_message`：先读定长 header，据 `msg_size` 读 payload。
    pub async fn recv_reply(&mut self) -> anyhow::Result<WireMessage> {
        let mut hdr_buf = [0u8; HEADER_LEN];
        self.read
            .recv_exact(&mut hdr_buf)
            .await
            .context("recv_reply: read header")?;
        let header = decode_header(&hdr_buf).context("recv_reply: decode_header")?;
        // **packed 字段先 copy 到本地**（Header #[repr(C,packed)] + deny(unsafe_code)
        // 下不能直接读字段，E0793）。decode_header 已校验 msg_size 范围；checked_sub 双保险。
        let msg_size = header.msg_size as usize;
        let payload_len = msg_size
            .checked_sub(HEADER_LEN)
            .context("recv_reply: msg_size < HEADER_LEN")?;
        let mut payload = vec![0u8; payload_len];
        self.read
            .recv_exact(&mut payload)
            .await
            .context("recv_reply: read payload")?;
        Ok(WireMessage { header, payload })
    }
}
