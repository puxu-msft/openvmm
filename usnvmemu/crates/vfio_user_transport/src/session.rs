// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase U3** — `VfioUserSession`：握手后的长连 session，负责把入站
//! command 派发到底下的 [`PcieDevice`] 实例。
//!
//! 本 phase 覆盖纯查询 / 同步 IO 命令：
//! - `DeviceGetInfo` — 回 PCI flags + num_regions=9 + num_irqs=5
//! - `DeviceGetRegionInfo` — BAR0 走 [`Regions`] 描述，其它 size=0
//! - `DeviceGetIrqInfo` — MSI-X 按 [`Regions::msix_count`]，其它 0
//! - `RegionRead` / `RegionWrite` — 直接调 [`PcieDevice::mmio_read/write`]
//! - `DeviceReset` — 调 [`PcieDevice::reset`]
//!
//! 仅 DMA_MAP/UNMAP/DMA_READ/DMA_WRITE/SET_IRQS 留给 U4/U5。

use crate::framing::FramingError;
use crate::framing::Message;
use crate::framing::read_message;
use crate::framing::write_message;
use crate::handshake::Negotiated;
use crate::proto::Command;
use crate::proto::DeviceInfoPayload;
use crate::proto::Header;
use crate::proto::IrqInfoPayload;
use crate::proto::RegionAccessPayload;
use crate::proto::RegionInfoPayload;
use crate::proto::decode_payload;
use crate::proto::device_flags;
use crate::proto::irq_info;
use crate::proto::pci_irq;
use crate::proto::pci_region;
use crate::proto::region_flags;
use anyhow::Context as _;
use pcie_device_sdk::PcieDevice;
use std::os::unix::net::UnixStream;
use zerocopy::IntoBytes;

/// 设备的 region/MSI-X 静态描述，由 [`PcieDevice`] 实现者通过
/// `describe()` 间接提供；vfio-user backend 需要它回 `GET_REGION_INFO`
/// 等查询。当前提取的最小子集：
///
/// - `bar0_size`：BAR0 字节数（NVMe controller 8 KiB）
/// - `config_size`：PCI config space 字节数（标准 PCIe 4 KiB；legacy PCI 256 B）
/// - `msix_count`：MSI-X 向量数（>0 时 GET_IRQ_INFO index=MSIX 回此值）
///
/// 用 trait 而非具体 struct，方便测试 mock。
pub trait Regions {
    /// BAR0 字节数。NVMe = 8 KiB；返 0 表示无 BAR0。
    fn bar0_size(&self) -> u64;
    /// PCI config space 字节数。标准 PCIe = 4096；返 0 跳过。
    fn config_size(&self) -> u64 {
        4096
    }
    /// MSI-X 向量数。NVMe controller 一般 = io_queues + 1。
    fn msix_count(&self) -> u32;
}

/// Server-side session：握手已完成，循环派发入站命令到 [`PcieDevice`]。
///
/// **Phase U4**：内部持 [`crate::DmaTable`] 跟踪 client 通告的 guest RAM
/// region；DMA_READ/WRITE 通过 [`crate::dma::dma_read_sync`] / `dma_write_sync`
/// 走真往返。
///
/// **Phase U5**：内部持 [`crate::IrqVectors`] 收 MSI-X eventfd；session
/// 的 `fire_interrupt(idx)` 方法（U5 后由 [`VfioUserTransport`] 调）会
/// 往该 eventfd 写 8 byte u64=1 触发 guest 中断。
///
/// **Phase U5-polish (review H1)**：DMA 走 *同步完成 + pending 队列* —
/// `dma_read` 内部完成 wire 往返后把 `(token, data)` 推 `pending_completions`，
/// `pump_one` 在 dispatch inbound 后 drain 队列调 `device.on_dma_complete`。
/// 这让 NVMe controller `pending_ios` 表能被正常 close，不会因为 SDK
/// 丢 callback 而 hang。
pub struct VfioUserSession {
    stream: UnixStream,
    #[allow(dead_code)] // U5 之后会用 negotiated caps 限速
    negotiated: Negotiated,
    /// DMA_MAP 通告的所有 guest RAM region。
    pub(crate) dma_table: crate::dma::DmaTable,
    /// MSI-X 向量 eventfd 数组。
    pub(crate) irq_vectors: crate::irq::IrqVectors,
    /// server-initiated DMA_READ/WRITE 的 msg_id 计数器（顶位 0x8000）。
    pub(crate) next_server_msg_id: u16,
    /// **review H1** — DMA 完成事件队列：(token, ok, data)。
    /// `dma_read`/`dma_write` 完成 wire round-trip 后入队；`pump_one` 在
    /// inbound 处理后 drain，调 `device.on_dma_complete(token, ok, data, ctx)`。
    pub(crate) pending_completions: std::collections::VecDeque<DmaCompletion>,
}

/// 一条等待投递给 device 的 DMA 完成事件。
#[derive(Debug)]
pub struct DmaCompletion {
    /// `dma_read`/`dma_write` 返给 device 的 token（= server-initiated msg_id）。
    pub token: u64,
    /// 成功 = true；wire error / region 校验失败 = false。
    pub ok: bool,
    /// `dma_read` 成功时 = 字节数据；其它情形 = 空 Vec。
    pub data: Vec<u8>,
}

impl VfioUserSession {
    /// 用已 handshake 的 UnixStream + Negotiated 构造 session。
    pub fn new(stream: UnixStream, negotiated: Negotiated) -> Self {
        Self {
            stream,
            negotiated,
            dma_table: crate::dma::DmaTable::default(),
            irq_vectors: crate::irq::IrqVectors::default(),
            next_server_msg_id: 0x8000,
            pending_completions: std::collections::VecDeque::new(),
        }
    }

    /// 阻塞处理一条入站消息：read → dispatch → 自动 reply。
    ///
    /// 返回值：
    /// - `Ok(true)` — 正常处理完；caller 应继续 loop（包含 *参数错误* 已
    ///   反 errno reply 但 session 保留 — driver 探测时常见）
    /// - `Ok(false)` — peer 关闭 socket；caller 应退出
    /// - `Err` — **协议层硬错误**（unknown command / wire 解析失败），caller
    ///   应 close socket
    pub fn pump_one<D: PcieDevice + Regions>(&mut self, device: &mut D) -> anyhow::Result<bool> {
        let msg = match read_message(&mut self.stream) {
            Ok(m) => m,
            Err(e) => {
                // **review H1** — 用 downcast 检测 typed PeerClosed，不再
                // 用字符串子串匹配（脆弱、误判）。
                if e.downcast_ref::<FramingError>()
                    .is_some_and(|fe| matches!(fe, FramingError::PeerClosed { .. }))
                {
                    return Ok(false);
                }
                return Err(e);
            }
        };
        let msg_id = msg.header.msg_id;
        let cmd_u = msg.header.cmd;
        let cmd = match Command::try_from(cmd_u) {
            Ok(c) => c,
            Err(_) => {
                // 未知 cmd 是 wire 层硬错：close。
                self.send_err(msg_id, Command::Version, libc::EINVAL as u32);
                return Err(anyhow::anyhow!("unknown command {cmd_u}"));
            }
        };
        self.dispatch(cmd, msg, device)?;
        // **review H1** — dispatch 完后 drain DMA 完成事件投给 device。
        // 设备 mmio_write handler 内调 ctx.dma_read() 时本 transport 同步
        // 完成 wire，把 (token, ok, data) 推 pending_completions；现在
        // 在 pump 主循环里调 on_dma_complete 让 NVMe controller `pending_ios`
        // 取走 token + 数据。
        self.drain_dma_completions(device);
        Ok(true)
    }

    /// 把 `pending_completions` 队列里的 DMA 完成事件依次投给 device。
    /// `device.on_dma_complete` 内部还可能再触发 dma_*（如 NVMe PRP list
    /// 第二阶段），所以用 `pop_front` 循环；每条事件 pop 出来 *再* 调 device，
    /// 避免重复借用 self（self 在 dma_read/write 中已被 transport 借走）。
    fn drain_dma_completions<D: PcieDevice>(&mut self, device: &mut D) {
        while let Some(c) = self.pending_completions.pop_front() {
            // 给 device 的 ctx 借 *self*：device.on_dma_complete 可能继续
            // 调 ctx.dma_*，又往 pending_completions 推新事件 — while 循环
            // 自动 drain 干净。
            let mut ctx = pcie_device_sdk::DeviceCtx::new(self);
            device.on_dma_complete(&mut ctx, c.token, c.ok, c.data);
        }
    }

    /// 内部 dispatch — 按 [`Command`] 路由到对应处理函数。
    fn dispatch<D: PcieDevice + Regions>(
        &mut self,
        cmd: Command,
        msg: Message,
        device: &mut D,
    ) -> anyhow::Result<()> {
        let id = msg.header.msg_id;
        match cmd {
            Command::DeviceGetInfo => self.handle_get_info(id, &msg, device),
            Command::DeviceGetRegionInfo => self.handle_get_region_info(id, &msg, device),
            Command::DeviceGetIrqInfo => self.handle_get_irq_info(id, &msg, device),
            Command::RegionRead => self.handle_region_read(id, &msg, device),
            Command::RegionWrite => self.handle_region_write(id, &msg, device),
            Command::DeviceReset => self.handle_reset(id, &msg, device),
            // **Phase U4** — DMA 表已接：
            Command::DmaMap => {
                crate::dma::handle_dma_map(&mut self.stream, &mut self.dma_table, id, &msg)
            }
            Command::DmaUnmap => {
                crate::dma::handle_dma_unmap(&mut self.stream, &mut self.dma_table, id, &msg)
            }
            // U5 实现：
            Command::DeviceSetIrqs => {
                let mut msg = msg;
                crate::irq::handle_set_irqs(&mut self.stream, &mut self.irq_vectors, id, &mut msg)
            }
            // 我们不实现 — 礼貌地回 ENOTSUP。
            Command::DeviceGetRegionIoFds => {
                self.send_err(id, cmd, libc::ENOTSUP as u32);
                Ok(())
            }
            // server-initiated 类型不该作为入站 cmd 出现
            Command::Version | Command::DmaRead | Command::DmaWrite => {
                tracing::warn!(?cmd, "unexpected inbound command (server-initiated kind)");
                self.send_err(id, cmd, libc::EPROTO as u32);
                Ok(())
            }
        }
    }

    fn handle_get_info<D: Regions>(
        &mut self,
        id: u16,
        _msg: &Message,
        _device: &D,
    ) -> anyhow::Result<()> {
        let pl = DeviceInfoPayload {
            argsz: core::mem::size_of::<DeviceInfoPayload>() as u32,
            flags: device_flags::PCI | device_flags::RESET,
            num_regions: pci_region::NUM_REGIONS,
            num_irqs: pci_irq::NUM_IRQS,
        };
        let hdr = Header::reply_ok(id, Command::DeviceGetInfo, pl.as_bytes().len() as u32);
        write_message(&mut self.stream, &hdr, pl.as_bytes(), &[]).context("write GET_INFO reply")
    }

    fn handle_get_region_info<D: Regions>(
        &mut self,
        id: u16,
        msg: &Message,
        device: &D,
    ) -> anyhow::Result<()> {
        // **review H2 + M1** — payload 长度校验在前；参数错走 send_err + Ok(())
        // 保持 session 不被 driver 探测 BAR1..5 等行为关闭。
        let want = core::mem::size_of::<RegionInfoPayload>();
        if msg.payload.len() != want {
            self.send_err(id, Command::DeviceGetRegionInfo, libc::EINVAL as u32);
            return Ok(());
        }
        let req: RegionInfoPayload = match decode_payload(&msg.payload) {
            Ok(r) => r,
            Err(_) => {
                self.send_err(id, Command::DeviceGetRegionInfo, libc::EINVAL as u32);
                return Ok(());
            }
        };
        let idx = req.index;
        let (flags, size) = match idx {
            x if x == pci_region::BAR0 => {
                let s = device.bar0_size();
                if s == 0 {
                    (0u32, 0u64)
                } else {
                    (region_flags::READ | region_flags::WRITE, s)
                }
            }
            x if x == pci_region::CONFIG => (
                region_flags::READ | region_flags::WRITE,
                device.config_size(),
            ),
            // BAR1..5 / ROM / VGA — 教学版无支持，size=0+flags=0。
            _ if idx < pci_region::NUM_REGIONS => (0u32, 0u64),
            _ => {
                self.send_err(id, Command::DeviceGetRegionInfo, libc::EINVAL as u32);
                return Ok(());
            }
        };
        let pl = RegionInfoPayload {
            argsz: core::mem::size_of::<RegionInfoPayload>() as u32,
            flags,
            index: idx,
            cap_offset: 0,
            size,
            offset: 0,
        };
        let hdr = Header::reply_ok(id, Command::DeviceGetRegionInfo, pl.as_bytes().len() as u32);
        write_message(&mut self.stream, &hdr, pl.as_bytes(), &[])
            .context("write GET_REGION_INFO reply")
    }

    fn handle_get_irq_info<D: Regions>(
        &mut self,
        id: u16,
        msg: &Message,
        device: &D,
    ) -> anyhow::Result<()> {
        let want = core::mem::size_of::<IrqInfoPayload>();
        if msg.payload.len() != want {
            self.send_err(id, Command::DeviceGetIrqInfo, libc::EINVAL as u32);
            return Ok(());
        }
        let req: IrqInfoPayload = match decode_payload(&msg.payload) {
            Ok(r) => r,
            Err(_) => {
                self.send_err(id, Command::DeviceGetIrqInfo, libc::EINVAL as u32);
                return Ok(());
            }
        };
        let idx = req.index;
        let (flags, count) = match idx {
            x if x == pci_irq::MSIX => (irq_info::EVENTFD, device.msix_count()),
            x if x < pci_irq::NUM_IRQS => (0u32, 0u32),
            _ => {
                self.send_err(id, Command::DeviceGetIrqInfo, libc::EINVAL as u32);
                return Ok(());
            }
        };
        let pl = IrqInfoPayload {
            argsz: core::mem::size_of::<IrqInfoPayload>() as u32,
            flags,
            index: idx,
            count,
        };
        let hdr = Header::reply_ok(id, Command::DeviceGetIrqInfo, pl.as_bytes().len() as u32);
        write_message(&mut self.stream, &hdr, pl.as_bytes(), &[])
            .context("write GET_IRQ_INFO reply")
    }

    fn handle_region_read<D: PcieDevice>(
        &mut self,
        id: u16,
        msg: &Message,
        device: &mut D,
    ) -> anyhow::Result<()> {
        // **review H2** — 解之前先校验长度，避免 slice 越界 panic。
        let want = core::mem::size_of::<RegionAccessPayload>();
        if msg.payload.len() < want {
            self.send_err(id, Command::RegionRead, libc::EINVAL as u32);
            return Ok(());
        }
        let req: RegionAccessPayload = match decode_payload(&msg.payload[..want]) {
            Ok(r) => r,
            Err(_) => {
                self.send_err(id, Command::RegionRead, libc::EINVAL as u32);
                return Ok(());
            }
        };
        let count = req.count as usize;
        let offset = req.offset;
        let bar = req.region;
        if !matches!(count, 1 | 2 | 4 | 8) {
            self.send_err(id, Command::RegionRead, libc::EINVAL as u32);
            return Ok(());
        }
        let value = device.mmio_read(bar, offset, count as u32);
        // Reply payload = RegionAccessPayload echo + value bytes.
        let mut reply_payload =
            Vec::with_capacity(core::mem::size_of::<RegionAccessPayload>() + count);
        let echo = RegionAccessPayload {
            offset,
            region: bar,
            count: count as u32,
        };
        reply_payload.extend_from_slice(echo.as_bytes());
        // value 低 count*8 位有效；按 size 写出。
        let val_bytes = value.to_le_bytes();
        reply_payload.extend_from_slice(&val_bytes[..count]);
        let hdr = Header::reply_ok(id, Command::RegionRead, reply_payload.len() as u32);
        write_message(&mut self.stream, &hdr, &reply_payload, &[])
            .context("write REGION_READ reply")
    }

    fn handle_region_write<D: PcieDevice>(
        &mut self,
        id: u16,
        msg: &Message,
        device: &mut D,
    ) -> anyhow::Result<()> {
        let req_struct_len = core::mem::size_of::<RegionAccessPayload>();
        if msg.payload.len() < req_struct_len {
            self.send_err(id, Command::RegionWrite, libc::EINVAL as u32);
            return Ok(());
        }
        let req: RegionAccessPayload = match decode_payload(&msg.payload[..req_struct_len]) {
            Ok(r) => r,
            Err(_) => {
                self.send_err(id, Command::RegionWrite, libc::EINVAL as u32);
                return Ok(());
            }
        };
        let count = req.count as usize;
        let offset = req.offset;
        let bar = req.region;
        if !matches!(count, 1 | 2 | 4 | 8) {
            self.send_err(id, Command::RegionWrite, libc::EINVAL as u32);
            return Ok(());
        }
        if msg.payload.len() != req_struct_len + count {
            self.send_err(id, Command::RegionWrite, libc::EINVAL as u32);
            return Ok(());
        }
        let mut val_bytes = [0u8; 8];
        val_bytes[..count].copy_from_slice(&msg.payload[req_struct_len..]);
        let value = u64::from_le_bytes(val_bytes);
        // 调 PcieDevice — 它通过 DeviceCtx 反向触发 DMA/中断；Phase U3 还没
        // 给 vfio-user backend 实现 Transport，先用 NoopTransport 屏蔽
        // dma/irq（U4/U5 接通后真正生效）。
        let mut t = crate::transport::NoopTransport;
        let mut ctx = pcie_device_sdk::DeviceCtx::new(&mut t);
        device.mmio_write(&mut ctx, bar, offset, count as u32, value);
        // Reply: echo struct only (no data).
        let echo = RegionAccessPayload {
            offset,
            region: bar,
            count: count as u32,
        };
        let hdr = Header::reply_ok(id, Command::RegionWrite, echo.as_bytes().len() as u32);
        write_message(&mut self.stream, &hdr, echo.as_bytes(), &[])
            .context("write REGION_WRITE reply")
    }

    fn handle_reset<D: PcieDevice>(
        &mut self,
        id: u16,
        _msg: &Message,
        device: &mut D,
    ) -> anyhow::Result<()> {
        device.reset(0); // kind=0 == FLR
        let hdr = Header::reply_ok(id, Command::DeviceReset, 0);
        write_message(&mut self.stream, &hdr, &[], &[]).context("write DEVICE_RESET reply")
    }

    fn send_err(&mut self, msg_id: u16, cmd: Command, errno: u32) {
        let hdr = Header::reply_err(msg_id, cmd, errno);
        if let Err(e) = write_message(&mut self.stream, &hdr, &[], &[]) {
            tracing::warn!(
                error = %e,
                msg_id,
                ?cmd,
                errno,
                "failed to send error reply"
            );
        }
    }
}

/// **Phase U5** — `VfioUserSession` impl `Transport`：让 PcieDevice 通过
/// `DeviceCtx` 反向触发 DMA / 中断时，走 vfio-user wire 真路径。
///
/// **设计选择**：让 session 直接 impl Transport，而非把 transport 单独
/// 拎出来对象化。原因：dma_read/write 内部要 *读 stream*（同步等 reply），
/// 那这个 stream 必须就是 session 自己的 — 没法解耦。
///
/// **同步语义** (review H1 修复)：
/// - `dma_read` 立刻 issue wire round-trip → 拿到 data；
/// - 把 `(token=msg_id, ok, data)` 推 `pending_completions` 队列；
/// - 返回 token 给 caller (device 的 mmio_write/tick handler)；
/// - `pump_one` 主循环 dispatch 完后会 drain 队列，调
///   `device.on_dma_complete(token, ok, data, ctx)` — 这条 callback
///   让 NVMe controller `pending_ios` 表正确 close。
///
/// 失败时（DMA 表查不到 region / wire IO err）：返非零 token + 把
/// `ok=false data=[]` 推队列，让 device 走 NVMe 标准 DMA-fail 清理路径。
impl pcie_device_sdk::Transport for VfioUserSession {
    fn fire_interrupt(&mut self, msix_index: u32) {
        let _ = self.irq_vectors.fire(msix_index);
    }

    fn dma_read(&mut self, gpa: u64, len: u32) -> u64 {
        match crate::dma::dma_read_sync(
            &mut self.stream,
            &self.dma_table,
            &mut self.next_server_msg_id,
            gpa,
            len,
        ) {
            Ok((msg_id, data)) => {
                let token = msg_id as u64;
                tracing::debug!(
                    token,
                    gpa = format_args!("{gpa:#x}"),
                    len,
                    bytes = data.len(),
                    "VfioUserTransport.dma_read OK; enqueue on_dma_complete"
                );
                self.pending_completions.push_back(DmaCompletion {
                    token,
                    ok: true,
                    data,
                });
                token
            }
            Err(e) => {
                tracing::warn!(error = %e, gpa = format_args!("{gpa:#x}"), len,
                    "VfioUserTransport.dma_read failed");
                // 取下一个 msg_id 作为合成 token（让 pending_ios 仍能 close）。
                let token = self.next_server_msg_id as u64;
                self.pending_completions.push_back(DmaCompletion {
                    token,
                    ok: false,
                    data: Vec::new(),
                });
                token
            }
        }
    }

    fn dma_write(&mut self, gpa: u64, data: Vec<u8>) -> u64 {
        match crate::dma::dma_write_sync(
            &mut self.stream,
            &self.dma_table,
            &mut self.next_server_msg_id,
            gpa,
            &data,
        ) {
            Ok(msg_id) => {
                let token = msg_id as u64;
                tracing::debug!(
                    token,
                    gpa = format_args!("{gpa:#x}"),
                    bytes = data.len(),
                    "VfioUserTransport.dma_write OK; enqueue on_dma_complete"
                );
                self.pending_completions.push_back(DmaCompletion {
                    token,
                    ok: true,
                    data: Vec::new(),
                });
                token
            }
            Err(e) => {
                tracing::warn!(error = %e, gpa = format_args!("{gpa:#x}"),
                    "VfioUserTransport.dma_write failed");
                let token = self.next_server_msg_id as u64;
                self.pending_completions.push_back(DmaCompletion {
                    token,
                    ok: false,
                    data: Vec::new(),
                });
                token
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HEADER_LEN;
    use crate::HeaderFlags;
    use crate::framing::write_message as fw_write;
    use pcie_device_sdk::DeviceCtx;
    use pcie_device_sdk::PcieDevice as Pde;
    use pcie_device_sdk::pcie_remote_protocol::DeviceDescribe;
    use std::os::unix::net::UnixStream;
    use std::thread;

    struct MockDev {
        bar0: Vec<u64>,
        last_reset: u32,
    }
    impl MockDev {
        fn new() -> Self {
            Self {
                bar0: vec![0u64; 1024],
                last_reset: 0xFFFF_FFFF,
            }
        }
    }
    impl Pde for MockDev {
        fn describe(&self) -> DeviceDescribe {
            DeviceDescribe::default()
        }
        fn mmio_read(&mut self, _bar: u32, offset: u64, _size: u32) -> u64 {
            *self.bar0.get((offset / 8) as usize).unwrap_or(&0)
        }
        fn mmio_write(
            &mut self,
            _ctx: &mut DeviceCtx<'_>,
            _bar: u32,
            offset: u64,
            _size: u32,
            value: u64,
        ) {
            let idx = (offset / 8) as usize;
            if idx < self.bar0.len() {
                self.bar0[idx] = value;
            }
        }
        fn reset(&mut self, kind: u32) {
            self.last_reset = kind;
        }
    }
    impl Regions for MockDev {
        fn bar0_size(&self) -> u64 {
            8192
        }
        fn msix_count(&self) -> u32 {
            8
        }
    }

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    fn neg() -> Negotiated {
        Negotiated {
            client_major: 0,
            client_minor: 1,
            client_caps_json: String::new(),
        }
    }

    /// GET_INFO 回 PCI flags + 9 regions + 5 irqs。
    #[test]
    fn get_info_reply_shape() {
        let (server, mut client) = pair();
        let n = neg();
        let mut sess = VfioUserSession::new(server, n);
        let mut dev = MockDev::new();

        let handle = thread::spawn(move || sess.pump_one(&mut dev));
        let req_pl = DeviceInfoPayload {
            argsz: core::mem::size_of::<DeviceInfoPayload>() as u32,
            flags: 0,
            num_regions: 0,
            num_irqs: 0,
        };
        let hdr = Header::command(1, Command::DeviceGetInfo, req_pl.as_bytes().len() as u32);
        fw_write(&mut client, &hdr, req_pl.as_bytes(), &[]).unwrap();
        let reply = read_message(&mut client).unwrap();
        let pl: DeviceInfoPayload = decode_payload(&reply.payload).unwrap();
        let f = pl.flags;
        let nr = pl.num_regions;
        let ni = pl.num_irqs;
        assert_eq!(f, device_flags::PCI | device_flags::RESET);
        assert_eq!(nr, pci_region::NUM_REGIONS);
        assert_eq!(ni, pci_irq::NUM_IRQS);
        assert!(handle.join().unwrap().unwrap());
    }

    /// GET_REGION_INFO BAR0 = R/W, 8 KiB；BAR3 = size=0。
    #[test]
    fn get_region_info_bar0_and_unused() {
        let (server, mut client) = pair();
        let mut sess = VfioUserSession::new(server, neg());
        let mut dev = MockDev::new();
        let _h = thread::spawn(move || {
            // 两次 pump：BAR0 + BAR3
            sess.pump_one(&mut dev).unwrap();
            sess.pump_one(&mut dev).unwrap();
        });
        for (idx, expect_flags, expect_size) in [
            (0u32, region_flags::READ | region_flags::WRITE, 8192u64),
            (3u32, 0u32, 0u64),
        ] {
            let req = RegionInfoPayload {
                argsz: core::mem::size_of::<RegionInfoPayload>() as u32,
                flags: 0,
                index: idx,
                cap_offset: 0,
                size: 0,
                offset: 0,
            };
            let hdr = Header::command(
                idx as u16,
                Command::DeviceGetRegionInfo,
                req.as_bytes().len() as u32,
            );
            fw_write(&mut client, &hdr, req.as_bytes(), &[]).unwrap();
            let reply = read_message(&mut client).unwrap();
            let pl: RegionInfoPayload = decode_payload(&reply.payload).unwrap();
            let f = pl.flags;
            let s = pl.size;
            let i = pl.index;
            assert_eq!(i, idx);
            assert_eq!(f, expect_flags);
            assert_eq!(s, expect_size);
        }
    }

    /// GET_IRQ_INFO MSIX → count=8 + EVENTFD；INTX → count=0。
    #[test]
    fn get_irq_info_msix_and_intx() {
        let (server, mut client) = pair();
        let mut sess = VfioUserSession::new(server, neg());
        let mut dev = MockDev::new();
        let _h = thread::spawn(move || {
            sess.pump_one(&mut dev).unwrap();
            sess.pump_one(&mut dev).unwrap();
        });
        for (idx, expect_flags, expect_count) in [
            (pci_irq::MSIX, irq_info::EVENTFD, 8u32),
            (pci_irq::INTX, 0u32, 0u32),
        ] {
            let req = IrqInfoPayload {
                argsz: core::mem::size_of::<IrqInfoPayload>() as u32,
                flags: 0,
                index: idx,
                count: 0,
            };
            let hdr = Header::command(
                idx as u16,
                Command::DeviceGetIrqInfo,
                req.as_bytes().len() as u32,
            );
            fw_write(&mut client, &hdr, req.as_bytes(), &[]).unwrap();
            let reply = read_message(&mut client).unwrap();
            let pl: IrqInfoPayload = decode_payload(&reply.payload).unwrap();
            let f = pl.flags;
            let c = pl.count;
            assert_eq!(f, expect_flags);
            assert_eq!(c, expect_count);
        }
    }

    /// REGION_WRITE 8 byte + REGION_READ 8 byte：写 cafebabedeadbeef 然后读回。
    #[test]
    fn region_write_then_read_roundtrip() {
        let (server, mut client) = pair();
        let mut sess = VfioUserSession::new(server, neg());
        let mut dev = MockDev::new();
        let _h = thread::spawn(move || {
            sess.pump_one(&mut dev).unwrap();
            sess.pump_one(&mut dev).unwrap();
        });
        // WRITE @ offset 0x10
        let req = RegionAccessPayload {
            offset: 0x10,
            region: 0,
            count: 8,
        };
        let mut pl = Vec::new();
        pl.extend_from_slice(req.as_bytes());
        pl.extend_from_slice(&0xCAFEBABEDEADBEEFu64.to_le_bytes());
        let hdr = Header::command(1, Command::RegionWrite, pl.len() as u32);
        fw_write(&mut client, &hdr, &pl, &[]).unwrap();
        let _ = read_message(&mut client).unwrap();

        // READ @ offset 0x10
        let req = RegionAccessPayload {
            offset: 0x10,
            region: 0,
            count: 8,
        };
        let hdr = Header::command(2, Command::RegionRead, req.as_bytes().len() as u32);
        fw_write(&mut client, &hdr, req.as_bytes(), &[]).unwrap();
        let reply = read_message(&mut client).unwrap();
        // reply payload = RegionAccessPayload echo + 8 byte value
        let echo: RegionAccessPayload =
            decode_payload(&reply.payload[..core::mem::size_of::<RegionAccessPayload>()]).unwrap();
        let cnt = echo.count;
        assert_eq!(cnt, 8);
        let val = u64::from_le_bytes(
            reply.payload[core::mem::size_of::<RegionAccessPayload>()..]
                .try_into()
                .unwrap(),
        );
        assert_eq!(val, 0xCAFEBABEDEADBEEF);
    }

    /// REGION_READ 非法 size = 3 → 服务端回 EINVAL。
    #[test]
    fn region_read_invalid_size_rejected() {
        let (server, mut client) = pair();
        let mut sess = VfioUserSession::new(server, neg());
        let mut dev = MockDev::new();
        let h = thread::spawn(move || sess.pump_one(&mut dev));
        let req = RegionAccessPayload {
            offset: 0,
            region: 0,
            count: 3,
        };
        let hdr = Header::command(7, Command::RegionRead, req.as_bytes().len() as u32);
        fw_write(&mut client, &hdr, req.as_bytes(), &[]).unwrap();
        let reply = read_message(&mut client).unwrap();
        assert!(reply.header.flags().is_error());
        let err = reply.header.error_no;
        assert_eq!(err, libc::EINVAL as u32);
        // **review M1** — 参数错（非法 size）不应 close session：返 Ok(true)。
        assert!(h.join().unwrap().unwrap());
    }

    /// DEVICE_RESET 触发 device.reset(0)。
    #[test]
    fn device_reset_calls_reset() {
        let (server, mut client) = pair();
        let mut sess = VfioUserSession::new(server, neg());
        let mut dev = MockDev::new();
        let h = thread::spawn(move || {
            sess.pump_one(&mut dev).unwrap();
            dev.last_reset
        });
        let hdr = Header::command(99, Command::DeviceReset, 0);
        fw_write(&mut client, &hdr, &[], &[]).unwrap();
        let _ = read_message(&mut client).unwrap();
        let last_reset = h.join().unwrap();
        assert_eq!(last_reset, 0);
    }

    /// Phase U5 后 SET_IRQS 真处理（DATA_NONE+count=0 → 清向量表 OK reply）。
    #[test]
    fn set_irqs_clear_returns_ok() {
        let (server, mut client) = pair();
        let mut sess = VfioUserSession::new(server, neg());
        let mut dev = MockDev::new();
        let _h = thread::spawn(move || sess.pump_one(&mut dev));
        let pl = crate::proto::IrqSetPayload {
            argsz: 20,
            flags: crate::proto::irq_set::DATA_NONE | crate::proto::irq_set::ACTION_TRIGGER,
            index: pci_irq::MSIX,
            start: 0,
            count: 0,
        };
        let hdr = Header::command(42, Command::DeviceSetIrqs, pl.as_bytes().len() as u32);
        fw_write(&mut client, &hdr, pl.as_bytes(), &[]).unwrap();
        let reply = read_message(&mut client).unwrap();
        assert!(!reply.header.flags().is_error());
    }

    /// **review L1** — GET_REGION_INFO idx >= NUM_REGIONS → EINVAL + session 存活
    #[test]
    fn region_info_out_of_range_keeps_session_alive() {
        let (server, mut client) = pair();
        let mut sess = VfioUserSession::new(server, neg());
        let mut dev = MockDev::new();
        // 两轮 pump：第一轮 idx=99 触发 EINVAL；第二轮普通 GET_INFO 仍工作
        let h = thread::spawn(move || {
            let r1 = sess.pump_one(&mut dev).unwrap();
            let r2 = sess.pump_one(&mut dev).unwrap();
            (r1, r2)
        });
        let bad = RegionInfoPayload {
            argsz: 32,
            flags: 0,
            index: 99,
            cap_offset: 0,
            size: 0,
            offset: 0,
        };
        let hdr = Header::command(1, Command::DeviceGetRegionInfo, bad.as_bytes().len() as u32);
        fw_write(&mut client, &hdr, bad.as_bytes(), &[]).unwrap();
        let r1 = read_message(&mut client).unwrap();
        assert!(r1.header.flags().is_error());
        let err1 = r1.header.error_no;
        assert_eq!(err1, libc::EINVAL as u32);

        // 第二轮：GET_INFO 正常
        let req2 = DeviceInfoPayload::default();
        let hdr2 = Header::command(2, Command::DeviceGetInfo, req2.as_bytes().len() as u32);
        fw_write(&mut client, &hdr2, req2.as_bytes(), &[]).unwrap();
        let r2 = read_message(&mut client).unwrap();
        assert!(!r2.header.flags().is_error());
        let (ok1, ok2) = h.join().unwrap();
        assert!(ok1 && ok2);
    }

    /// **review L1** — REGION_READ payload < 16 byte 不 panic，返 EINVAL + session 存活。
    #[test]
    fn region_read_short_payload_no_panic() {
        let (server, mut client) = pair();
        let mut sess = VfioUserSession::new(server, neg());
        let mut dev = MockDev::new();
        let h = thread::spawn(move || sess.pump_one(&mut dev).unwrap());
        // 故意构造非法长度 — 只发 4 byte 而非 16
        let hdr = Header::command(7, Command::RegionRead, 4);
        fw_write(&mut client, &hdr, &[0, 0, 0, 0], &[]).unwrap();
        let reply = read_message(&mut client).unwrap();
        assert!(reply.header.flags().is_error());
        let err = reply.header.error_no;
        assert_eq!(err, libc::EINVAL as u32);
        // session 没 close
        assert!(h.join().unwrap());
    }

    /// **review L1** — 未知 command 数值是 *协议硬错*：session 关闭。
    #[test]
    fn unknown_command_closes_session() {
        let (server, mut client) = pair();
        let mut sess = VfioUserSession::new(server, neg());
        let mut dev = MockDev::new();
        let h = thread::spawn(move || sess.pump_one(&mut dev));
        // cmd=99 未知
        let hdr = Header {
            msg_id: 1,
            cmd: 99,
            msg_size: HEADER_LEN as u32,
            flags: HeaderFlags::command().0,
            error_no: 0,
        };
        fw_write(&mut client, &hdr, &[], &[]).unwrap();
        // server 发完 err reply 后 return Err
        let reply = read_message(&mut client).unwrap();
        assert!(reply.header.flags().is_error());
        let r = h.join().unwrap();
        assert!(r.is_err(), "unknown command 必须让 pump_one 返 Err");
    }

    /// **review L1** — peer 关闭 → pump_one 返 Ok(false) 不 Err。
    #[test]
    fn peer_closed_returns_false_not_err() {
        let (server, client) = pair();
        let mut sess = VfioUserSession::new(server, neg());
        let mut dev = MockDev::new();
        drop(client); // peer 立即关
        let r = sess.pump_one(&mut dev);
        assert!(matches!(r, Ok(false)));
    }
}
