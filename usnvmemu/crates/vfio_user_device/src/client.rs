// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! vfio-user client：connect AF_UNIX + VERSION 握手（**async**，W6a）。
//!
//! 本模块纯 safe（cmsg/unsafe 全在 [`crate::async_socket`]）。
#![deny(unsafe_code)]

use crate::async_socket::AsyncSocket;
use crate::async_socket::split_full_duplex;
use crate::channel::VfioUserReader;
use crate::channel::VfioUserWriter;
use crate::framing::read_message;
use crate::framing::write_message;
use anyhow::Context as _;
use anyhow::anyhow;
use pal_async::driver::Driver;
use pal_async::socket::PolledSocket;
use std::os::fd::BorrowedFd;
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
use vfio_user_wire::proto::DmaMapPayload;
use vfio_user_wire::proto::DmaUnmapPayload;
use vfio_user_wire::proto::Header;
use vfio_user_wire::proto::IrqInfoPayload;
use vfio_user_wire::proto::IrqSetPayload;
use vfio_user_wire::proto::PROTOCOL_MAJOR;
use vfio_user_wire::proto::PROTOCOL_MINOR;
use vfio_user_wire::proto::RegionAccessPayload;
use vfio_user_wire::proto::RegionInfoPayload;
use vfio_user_wire::proto::VersionPayload;
use vfio_user_wire::proto::decode_payload;
use vfio_user_wire::proto::dma_unmap_flags;
use vfio_user_wire::proto::irq_set;
use zerocopy::IntoBytes;

/// vfio-user client 连接。持一条 async [`AsyncSocket`] + msg_id 计数器。
pub struct VfioUserClient {
    sock: AsyncSocket,
    /// 下一个请求的 msg_id（W1 握手用 1，W2 起每请求递增；reply 须 echo 同 id）。
    next_msg_id: u16,
}

impl VfioUserClient {
    /// connect 到 server 的 AF_UNIX socket 路径（async）。
    pub async fn connect(
        driver: &(impl ?Sized + Driver),
        path: impl AsRef<Path>,
    ) -> anyhow::Result<Self> {
        let ps = PolledSocket::connect_unix(driver, path.as_ref())
            .await
            .with_context(|| format!("connect vfio-user socket {:?}", path.as_ref()))?;
        Ok(Self {
            sock: AsyncSocket::new(ps),
            next_msg_id: 1,
        })
    }

    /// 从已建立的 `UnixStream` 构造（loopback 单测 / 已 connect 的场景）。包成
    /// `PolledSocket`（需 `driver`）。
    pub fn from_stream(
        driver: &(impl ?Sized + Driver),
        stream: UnixStream,
    ) -> anyhow::Result<Self> {
        let ps = PolledSocket::new(driver, stream).context("wrap UnixStream in PolledSocket")?;
        Ok(Self {
            sock: AsyncSocket::new(ps),
            next_msg_id: 1,
        })
    }

    /// 取下一个 msg_id 并自增（wrapping，避免溢出 panic）。
    fn alloc_msg_id(&mut self) -> u16 {
        let id = self.next_msg_id;
        self.next_msg_id = self.next_msg_id.wrapping_add(1);
        id
    }

    /// 消费一个**已握手**的 client，拆出全双工读写半（W6b worker 用）。msg_id 分配
    /// 移到调用方（worker 持 `next_msg_id` 计数器），故 `next_msg_id` 在此丢弃。
    ///
    /// 拆分后 [`VfioUserWriter`]/[`VfioUserReader`] 共享同一 socket 但各自 `poll_io`、
    /// **不跨内核调用持锁**——worker 可在 `select!` 里并发发请求 + 收 reply（串行
    /// `VfioUserClient` 的「写后立即读」做不到，见 [`crate::channel`] 模块文档）。
    pub fn into_channel(self) -> (VfioUserWriter, VfioUserReader) {
        let polled = self.sock.into_polled();
        let (writer, reader) = split_full_duplex(polled);
        (VfioUserWriter::new(writer), VfioUserReader::new(reader))
    }

    /// 执行 VERSION 握手：发 client VERSION command → 收 server reply → 验证协商。
    ///
    /// client 提议 `(PROTOCOL_MAJOR, PROTOCOL_MINOR)`；server 回
    /// `(PROTOCOL_MAJOR, min(client_minor, server_minor))`；client 验
    /// `reply.major == 提议` 且 `reply.minor <= 提议`（见
    /// [`verify_server_version_reply`]）。
    pub async fn handshake(&mut self) -> anyhow::Result<NegotiatedClient> {
        // 1. 发 VERSION command。
        let payload = build_version_command_payload(
            PROTOCOL_MAJOR,
            PROTOCOL_MINOR,
            CLIENT_CAPS_JSON.as_bytes(),
        );
        let msg_id = self.alloc_msg_id();
        let hdr = Header::command(msg_id, Command::Version, payload.len() as u32);
        write_message(&self.sock, &hdr, &payload, &[])
            .await
            .context("send VERSION command")?;

        // 2. 收 server reply。
        let reply = read_message(&self.sock)
            .await
            .context("recv VERSION reply")?;

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
            return Err(anyhow!(
                "{cmd:?} reply 是 error（error_no={reply_error_no}）"
            ));
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
    async fn request<T: zerocopy::IntoBytes + zerocopy::Immutable>(
        &mut self,
        cmd: Command,
        req: &T,
    ) -> anyhow::Result<WireMessage> {
        let msg_id = self.alloc_msg_id();
        let payload = req.as_bytes();
        let hdr = Header::command(msg_id, cmd, payload.len() as u32);
        write_message(&self.sock, &hdr, payload, &[])
            .await
            .with_context(|| format!("send {cmd:?}"))?;
        let reply = read_message(&self.sock)
            .await
            .with_context(|| format!("recv {cmd:?} reply"))?;
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
        decode_payload(&reply.payload[..want])
            .with_context(|| format!("decode {cmd:?} reply payload"))
    }

    /// DEVICE_GET_INFO：查 region 数 / IRQ 数 / flags。
    pub async fn get_device_info(&mut self) -> anyhow::Result<DeviceInfoPayload> {
        let req = DeviceInfoPayload {
            argsz: core::mem::size_of::<DeviceInfoPayload>() as u32,
            ..Default::default()
        };
        let reply = self.request(Command::DeviceGetInfo, &req).await?;
        Self::decode_reply_payload(&reply, Command::DeviceGetInfo)
    }

    /// DEVICE_GET_REGION_INFO：查某 region 的 size / flags。
    pub async fn get_region_info(&mut self, index: u32) -> anyhow::Result<RegionInfoPayload> {
        let req = RegionInfoPayload {
            argsz: core::mem::size_of::<RegionInfoPayload>() as u32,
            index,
            ..Default::default()
        };
        let reply = self.request(Command::DeviceGetRegionInfo, &req).await?;
        Self::decode_reply_payload(&reply, Command::DeviceGetRegionInfo)
    }

    /// DEVICE_GET_IRQ_INFO：查某 IRQ type 的向量数 / flags。
    pub async fn get_irq_info(&mut self, index: u32) -> anyhow::Result<IrqInfoPayload> {
        let req = IrqInfoPayload {
            argsz: core::mem::size_of::<IrqInfoPayload>() as u32,
            index,
            ..Default::default()
        };
        let reply = self.request(Command::DeviceGetIrqInfo, &req).await?;
        Self::decode_reply_payload(&reply, Command::DeviceGetIrqInfo)
    }

    /// REGION_READ：读 region[index] 的 [offset, offset+count) 字节。
    ///
    /// reply = `RegionAccessPayload echo(16B) + count 字节数据`；校验 echo 的
    /// region/offset/count 与请求一致，返回数据段。
    pub async fn region_read(
        &mut self,
        region: u32,
        offset: u64,
        count: u32,
    ) -> anyhow::Result<Vec<u8>> {
        let req = RegionAccessPayload {
            offset,
            region,
            count,
        };
        let reply = self.request(Command::RegionRead, &req).await?;
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
    pub async fn region_write(
        &mut self,
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
        let msg_id = self.alloc_msg_id();
        let hdr = Header::command(msg_id, Command::RegionWrite, payload.len() as u32);
        write_message(&self.sock, &hdr, &payload, &[])
            .await
            .context("send REGION_WRITE")?;
        let reply = read_message(&self.sock)
            .await
            .context("recv REGION_WRITE reply")?;
        Self::expect_reply(&reply, msg_id, Command::RegionWrite)?;
        Ok(())
    }

    /// DEVICE_RESET：空 payload，触发 server 端 device reset（FLR）。
    pub async fn reset(&mut self) -> anyhow::Result<()> {
        let msg_id = self.alloc_msg_id();
        let hdr = Header::command(msg_id, Command::DeviceReset, 0);
        write_message(&self.sock, &hdr, &[], &[])
            .await
            .context("send DEVICE_RESET")?;
        let reply = read_message(&self.sock)
            .await
            .context("recv DEVICE_RESET reply")?;
        Self::expect_reply(&reply, msg_id, Command::DeviceReset)?;
        Ok(())
    }

    // ── W3: DMA_MAP / DMA_UNMAP fd-passing 零拷贝 ──

    /// DMA_MAP：把 `fd` 指向的内存（从 `fd_offset` 起 `size` 字节）映射到 IOVA
    /// `[gpa, gpa+size)`，server mmap 它做零拷贝 DMA。`flags` = READABLE|WRITEABLE
    /// （`vfio_user_wire::proto::dma_map_flags`）。
    ///
    /// fd 经 SCM_RIGHTS 单独传（vfio-user：单 region 单 fd）。reply 是 header-only OK。
    pub async fn dma_map(
        &mut self,
        gpa: u64,
        size: u64,
        flags: u32,
        fd: BorrowedFd<'_>,
        fd_offset: u64,
    ) -> anyhow::Result<()> {
        let req = DmaMapPayload {
            argsz: core::mem::size_of::<DmaMapPayload>() as u32,
            flags,
            offset: fd_offset,
            addr: gpa,
            size,
        };
        let msg_id = self.alloc_msg_id();
        let hdr = Header::command(
            msg_id,
            Command::DmaMap,
            core::mem::size_of::<DmaMapPayload>() as u32,
        );
        write_message(&self.sock, &hdr, req.as_bytes(), &[fd])
            .await
            .context("send DMA_MAP")?;
        let reply = read_message(&self.sock)
            .await
            .context("recv DMA_MAP reply")?;
        Self::expect_reply(&reply, msg_id, Command::DmaMap)?;
        Ok(())
    }

    /// DMA_UNMAP：精确撤销 `[gpa, gpa+size)` 的映射（含其 mmap）。
    pub async fn dma_unmap(&mut self, gpa: u64, size: u64) -> anyhow::Result<()> {
        let req = DmaUnmapPayload {
            argsz: core::mem::size_of::<DmaUnmapPayload>() as u32,
            flags: 0,
            addr: gpa,
            size,
        };
        let _reply = self.request(Command::DmaUnmap, &req).await?;
        Ok(())
    }

    /// DMA_UNMAP（UNMAP_ALL flag）：撤销所有映射。
    pub async fn dma_unmap_all(&mut self) -> anyhow::Result<()> {
        let req = DmaUnmapPayload {
            argsz: core::mem::size_of::<DmaUnmapPayload>() as u32,
            flags: dma_unmap_flags::UNMAP_ALL,
            addr: 0,
            size: 0,
        };
        let _reply = self.request(Command::DmaUnmap, &req).await?;
        Ok(())
    }

    // ── W4: MSI-X SET_IRQS eventfd ──

    /// SET_IRQS assign：把 `eventfds` 配给 MSI-X 向量 `[start, start+len)`，server
    /// 后续 fire 该向量时写对应 eventfd（8 字节 u64+=1）。多 fd 经 SCM_RIGHTS。
    ///
    /// `index` 通常 `pci_irq::MSIX`。reply 是 header-only OK。
    pub async fn set_irqs(
        &mut self,
        index: u32,
        start: u32,
        eventfds: &[BorrowedFd<'_>],
    ) -> anyhow::Result<()> {
        let req = IrqSetPayload {
            argsz: core::mem::size_of::<IrqSetPayload>() as u32,
            flags: irq_set::DATA_EVENTFD | irq_set::ACTION_TRIGGER,
            index,
            start,
            count: eventfds.len() as u32,
        };
        self.send_set_irqs(&req, eventfds).await
    }

    /// SET_IRQS deassign：清 MSI-X 向量 `[start, start+count)` 的 eventfd（发 0 fd，
    /// = vfio-user 全 `-1` 编码 / 掩码）。
    pub async fn set_irqs_deassign(
        &mut self,
        index: u32,
        start: u32,
        count: u32,
    ) -> anyhow::Result<()> {
        let req = IrqSetPayload {
            argsz: core::mem::size_of::<IrqSetPayload>() as u32,
            flags: irq_set::DATA_EVENTFD | irq_set::ACTION_TRIGGER,
            index,
            start,
            count,
        };
        self.send_set_irqs(&req, &[]).await
    }

    /// SET_IRQS clear：DATA_NONE + count=0，清整个 IRQ 向量数组。
    pub async fn set_irqs_clear(&mut self, index: u32) -> anyhow::Result<()> {
        let req = IrqSetPayload {
            argsz: core::mem::size_of::<IrqSetPayload>() as u32,
            flags: irq_set::DATA_NONE | irq_set::ACTION_TRIGGER,
            index,
            start: 0,
            count: 0,
        };
        self.send_set_irqs(&req, &[]).await
    }

    /// SET_IRQS 公共发送：payload = IrqSetPayload（无 trailing data），fd 经 SCM_RIGHTS。
    async fn send_set_irqs(
        &mut self,
        req: &IrqSetPayload,
        fds: &[BorrowedFd<'_>],
    ) -> anyhow::Result<()> {
        let msg_id = self.alloc_msg_id();
        let hdr = Header::command(
            msg_id,
            Command::DeviceSetIrqs,
            core::mem::size_of::<IrqSetPayload>() as u32,
        );
        write_message(&self.sock, &hdr, req.as_bytes(), fds)
            .await
            .context("send SET_IRQS")?;
        let reply = read_message(&self.sock)
            .await
            .context("recv SET_IRQS reply")?;
        Self::expect_reply(&reply, msg_id, Command::DeviceSetIrqs)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::DefaultPool;
    use std::thread;

    /// server 回 error reply（flags F_ERROR）→ client handshake 报错。server thread
    /// 仍用**同步** server 端原语（独立线程），client async。
    #[test]
    fn handshake_rejects_error_reply() {
        let (server_end, client_end) = UnixStream::pair().unwrap();
        // server thread：同步读 client VERSION command（丢弃）+ 回 error reply。
        // 用 server 侧 vfio_user_transport::framing 同步原语，避免 client async 依赖。
        let srv = thread::spawn(move || {
            use vfio_user_transport::framing as srv_framing;
            let mut s = server_end;
            let _ = srv_framing::read_message(&mut s).unwrap();
            let hdr = Header::reply_err(1, Command::Version, 71); // EPROTO
            // server write_message 是 4 参数（末尾 fds: &[RawFd]）；空 fds。
            srv_framing::write_message(&mut s, &hdr, &[], &[]).unwrap();
        });
        DefaultPool::run_with(async |driver| {
            let mut client = VfioUserClient::from_stream(&driver, client_end).unwrap();
            let err = client.handshake().await.unwrap_err();
            assert!(
                format!("{err:#}").contains("error"),
                "应报 error reply：{err:#}"
            );
        });
        srv.join().unwrap();
    }
}
