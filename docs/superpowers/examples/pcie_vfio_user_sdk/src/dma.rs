// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase U4** — DMA_MAP/UNMAP 表 + 服务端发起的 DMA_READ/DMA_WRITE。
//!
//! ## 设计要点
//!
//! - **Message-mediated DMA**（教学路径）：QEMU 发来的 DMA_MAP 我们 *不*
//!   mmap 共享 memfd，而是把元数据存表；后续 controller 调 `dma_read`/
//!   `dma_write` 时通过 DMA_READ/DMA_WRITE S→C 消息 *阻塞* 走 round-trip。
//!   spec 明确允许 server 忽略 DMA_MAP 附带的 fd 强制走消息路径。
//!
//! - **Token = msg_id**：vfio-user 的 server-initiated msg_id 由 server 自选
//!   且与 client 方向独立。我们把 `Transport::dma_read/write` 返的 token 直接
//!   用 server-initiated msg_id（顶位 `0x8000` 起始便于日志区分）。
//!
//! - **同步阻塞**：`dma_read` 内部 `write request → read reply` 完成后才返
//!   token；上游 [`PcieDevice`] 拿到 token 后立刻 `on_dma_complete` 被 caller
//!   触发（U5 中由 [`VfioUserTransport`] 自己投递）。Phase U4 暂不接 PcieDevice
//!   callback；仅提供 `dma_read_sync` / `dma_write_sync` 让 session handler 用。
//!
//! - **bound 校验**：DMA_MAP 表查 (addr, len) 是否落在某个映射区间；不在则返
//!   `EFAULT`（vfio-user 标准行为）。
//!
//! - **fd 处理**：DMA_MAP 若带 fd，我们 *接收并立即 drop*（关闭），等价不 mmap。
//!   spec 允许；fd 内核会自动 close。

use crate::framing::Message;
use crate::framing::read_message;
use crate::framing::write_message;
use crate::proto::Command;
use crate::proto::DmaMapPayload;
use crate::proto::DmaRwHdrPayload;
use crate::proto::DmaUnmapPayload;
use crate::proto::Header;
use crate::proto::HeaderFlags;
use crate::proto::decode_payload;
use anyhow::Context as _;
use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use zerocopy::IntoBytes;

/// QEMU 通告的一段 guest RAM region。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaRegion {
    /// IOVA 起始（guest physical address）。
    pub addr: u64,
    /// 区间长度（字节）。
    pub size: u64,
    /// 可读？
    pub readable: bool,
    /// 可写？
    pub writeable: bool,
}

impl DmaRegion {
    /// 检查 `[gpa, gpa+len)` 是否完全落在本 region 内。
    pub fn contains(&self, gpa: u64, len: u64) -> bool {
        let end = match gpa.checked_add(len) {
            Some(e) => e,
            None => return false,
        };
        let reg_end = match self.addr.checked_add(self.size) {
            Some(e) => e,
            None => return false,
        };
        gpa >= self.addr && end <= reg_end
    }
}

/// DMA 映射表：addr → DmaRegion。BTreeMap 让按 addr 排序，便于 unmap_all 时
/// 遍历有序输出。
#[derive(Debug, Default, Clone)]
pub struct DmaTable {
    regions: BTreeMap<u64, DmaRegion>,
}

impl DmaTable {
    /// 加一条 region；同 addr 已存在 → `Exists`；与已有 region 有
    /// 任意 *字节级* 重叠（不同 addr）→ `Overlap`（**review M1** 新增校验）。
    pub fn insert(&mut self, r: DmaRegion) -> Result<(), DmaError> {
        if self.regions.contains_key(&r.addr) {
            return Err(DmaError::Exists);
        }
        // O(log n) 查左右邻居 + 校验是否相交。
        let r_end = r.addr.saturating_add(r.size);
        if let Some((_, prev)) = self.regions.range(..r.addr).next_back() {
            let p_end = prev.addr.saturating_add(prev.size);
            if p_end > r.addr {
                return Err(DmaError::Overlap);
            }
        }
        if let Some((_, next)) = self.regions.range(r.addr..).next()
            && r_end > next.addr
        {
            return Err(DmaError::Overlap);
        }
        self.regions.insert(r.addr, r);
        Ok(())
    }
    /// 移除 (addr, size) 必须精确匹配一条已存在 region；否则 EINVAL。
    pub fn remove_exact(&mut self, addr: u64, size: u64) -> Result<(), DmaError> {
        match self.regions.get(&addr) {
            Some(r) if r.size == size => {
                self.regions.remove(&addr);
                Ok(())
            }
            _ => Err(DmaError::NotFound),
        }
    }
    /// 撤销所有 region（DMA_UNMAP flags=UNMAP_ALL）。
    pub fn clear(&mut self) {
        self.regions.clear();
    }
    /// 查找包含 `[gpa, gpa+len)` 的 region；返 readable/writeable 用于权限校验。
    pub fn find(&self, gpa: u64, len: u64) -> Option<&DmaRegion> {
        // 找最大的 addr ≤ gpa。
        self.regions
            .range(..=gpa)
            .next_back()
            .map(|(_, r)| r)
            .filter(|r| r.contains(gpa, len))
    }
    /// 当前注册的 region 数（仅日志/调试）。
    pub fn len(&self) -> usize {
        self.regions.len()
    }
    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }
}

/// DMA 表错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaError {
    /// 同 addr 已存在映射（EEXIST）。
    Exists,
    /// 与现有 region 有字节级重叠（**review M1** 新增）。
    Overlap,
    /// 未找到（EINVAL）。
    NotFound,
}

impl DmaError {
    /// 对应 UNIX errno。
    pub fn errno(self) -> u32 {
        match self {
            Self::Exists | Self::Overlap => libc::EEXIST as u32,
            Self::NotFound => libc::EINVAL as u32,
        }
    }
}

/// 处理 DMA_MAP cmd：解 payload + 入表 + 回 reply（OK / 错）。
///
/// **review M4** — 本路径 *有意* 忽略 `msg.fds`：spec 允许 server 不 mmap
/// 共享 memfd，强制走 message-mediated DMA_READ/WRITE 往返。`msg.fds` 是
/// `Vec<OwnedFd>`，调用方 (session) 在 Message drop 时自动 close，无 leak。
/// 见模块 doc。
pub fn handle_dma_map(
    stream: &mut UnixStream,
    table: &mut DmaTable,
    msg_id: u16,
    msg: &Message,
) -> anyhow::Result<()> {
    let want = core::mem::size_of::<DmaMapPayload>();
    if msg.payload.len() != want {
        send_err(stream, msg_id, Command::DmaMap, libc::EINVAL as u32)?;
        return Ok(());
    }
    let pl: DmaMapPayload = match decode_payload(&msg.payload) {
        Ok(p) => p,
        Err(_) => {
            send_err(stream, msg_id, Command::DmaMap, libc::EINVAL as u32)?;
            return Ok(());
        }
    };
    // **review M3** — 拒 size=0、flags=0、addr+size 溢出。
    let perms =
        pl.flags & (crate::proto::dma_map_flags::READABLE | crate::proto::dma_map_flags::WRITEABLE);
    if pl.size == 0 || perms == 0 {
        send_err(stream, msg_id, Command::DmaMap, libc::EINVAL as u32)?;
        return Ok(());
    }
    if pl.addr.checked_add(pl.size).is_none() {
        send_err(stream, msg_id, Command::DmaMap, libc::EINVAL as u32)?;
        return Ok(());
    }
    let region = DmaRegion {
        addr: pl.addr,
        size: pl.size,
        readable: pl.flags & crate::proto::dma_map_flags::READABLE != 0,
        writeable: pl.flags & crate::proto::dma_map_flags::WRITEABLE != 0,
    };
    if let Err(e) = table.insert(region) {
        send_err(stream, msg_id, Command::DmaMap, e.errno())?;
        return Ok(());
    }
    let r_addr = region.addr;
    let r_size = region.size;
    let r_rd = region.readable;
    let r_wr = region.writeable;
    tracing::debug!(
        addr = format_args!("{r_addr:#x}"),
        size = r_size,
        readable = r_rd,
        writeable = r_wr,
        "DMA_MAP added"
    );
    // OK reply：spec 说回 header only。
    let hdr = Header::reply_ok(msg_id, Command::DmaMap, 0);
    write_message(stream, &hdr, &[], &[]).context("write DMA_MAP reply")
}

/// 处理 DMA_UNMAP cmd。
pub fn handle_dma_unmap(
    stream: &mut UnixStream,
    table: &mut DmaTable,
    msg_id: u16,
    msg: &Message,
) -> anyhow::Result<()> {
    let want = core::mem::size_of::<DmaUnmapPayload>();
    if msg.payload.len() != want {
        send_err(stream, msg_id, Command::DmaUnmap, libc::EINVAL as u32)?;
        return Ok(());
    }
    let pl: DmaUnmapPayload = match decode_payload(&msg.payload) {
        Ok(p) => p,
        Err(_) => {
            send_err(stream, msg_id, Command::DmaUnmap, libc::EINVAL as u32)?;
            return Ok(());
        }
    };
    if pl.flags & crate::proto::dma_unmap_flags::UNMAP_ALL != 0 {
        table.clear();
        tracing::debug!("DMA_UNMAP all regions");
    } else if let Err(e) = table.remove_exact(pl.addr, pl.size) {
        send_err(stream, msg_id, Command::DmaUnmap, e.errno())?;
        return Ok(());
    } else {
        let pl_addr = pl.addr;
        let pl_size = pl.size;
        tracing::debug!(
            addr = format_args!("{pl_addr:#x}"),
            size = pl_size,
            "DMA_UNMAP removed"
        );
    }
    // 回 echo（spec 说 echo struct + 可选 bitmap；我们不实 dirty bitmap）。
    let hdr = Header::reply_ok(msg_id, Command::DmaUnmap, pl.as_bytes().len() as u32);
    write_message(stream, &hdr, pl.as_bytes(), &[]).context("write DMA_UNMAP reply")
}

/// 服务端发起的同步 DMA_READ：发 request → 等 reply。
///
/// 返 `(msg_id, data)`：`msg_id` 就是我们用过的 wire id（=token，写给
/// caller 当 DMA 完成 token）；`data` 是 guest mem 字节。
///
/// **review H1/H2** — vfio-user spec 两方向 msg_id 独立、不要求严格请求/应答
/// 顺序；client 完全可能在我们等 reply 时插一条 REGION_READ。本函数本身
/// 用 [`read_message`] *直接* 拿下一帧并按 msg_id+cmd 校验；若 caller 有
/// 多路复用需求（U5 VfioUserTransport 需要在等 reply 期间继续处理 inbound
/// cmd），应改用更上层的 session 方法走 wait-for-reply 循环（U5 加）。
///
/// **review H2** — 校验 `reply.header.msg_id == msg_id` + `cmd == DmaRead`，
/// 并把 `msg_id` 返给 caller，token 由它直接持有不依赖 `wrapping_sub(1)`。
pub fn dma_read_sync(
    stream: &mut UnixStream,
    table: &DmaTable,
    next_server_msg_id: &mut u16,
    gpa: u64,
    len: u32,
) -> anyhow::Result<(u16, Vec<u8>)> {
    let region = table
        .find(gpa, len as u64)
        .ok_or_else(|| anyhow::anyhow!("DMA_READ {gpa:#x}+{len} not in any DMA region"))?;
    if !region.readable {
        anyhow::bail!("DMA_READ region @{gpa:#x} not readable");
    }
    let msg_id = alloc_server_msg_id(next_server_msg_id);
    let hdr = Header::command(
        msg_id,
        Command::DmaRead,
        core::mem::size_of::<DmaRwHdrPayload>() as u32,
    );
    let req = DmaRwHdrPayload {
        addr: gpa,
        count: len as u64,
    };
    write_message(stream, &hdr, req.as_bytes(), &[]).context("write DMA_READ request")?;
    let reply = read_message(stream).context("read DMA_READ reply")?;
    validate_dma_reply(&reply, msg_id, Command::DmaRead)?;
    let want = core::mem::size_of::<DmaRwHdrPayload>() + len as usize;
    if reply.payload.len() != want {
        anyhow::bail!(
            "DMA_READ reply payload {} != hdr({})+len({})",
            reply.payload.len(),
            core::mem::size_of::<DmaRwHdrPayload>(),
            len
        );
    }
    Ok((
        msg_id,
        reply.payload[core::mem::size_of::<DmaRwHdrPayload>()..].to_vec(),
    ))
}

/// 服务端发起的同步 DMA_WRITE：发 request + data → 等 echo reply。返 msg_id。
pub fn dma_write_sync(
    stream: &mut UnixStream,
    table: &DmaTable,
    next_server_msg_id: &mut u16,
    gpa: u64,
    data: &[u8],
) -> anyhow::Result<u16> {
    let len = data.len() as u64;
    let region = table
        .find(gpa, len)
        .ok_or_else(|| anyhow::anyhow!("DMA_WRITE {gpa:#x}+{len} not in any DMA region"))?;
    if !region.writeable {
        anyhow::bail!("DMA_WRITE region @{gpa:#x} not writeable");
    }
    let msg_id = alloc_server_msg_id(next_server_msg_id);
    let payload_len = core::mem::size_of::<DmaRwHdrPayload>() + data.len();
    let hdr = Header::command(msg_id, Command::DmaWrite, payload_len as u32);
    let mut payload = Vec::with_capacity(payload_len);
    payload.extend_from_slice(
        DmaRwHdrPayload {
            addr: gpa,
            count: len,
        }
        .as_bytes(),
    );
    payload.extend_from_slice(data);
    write_message(stream, &hdr, &payload, &[]).context("write DMA_WRITE request")?;
    let reply = read_message(stream).context("read DMA_WRITE reply")?;
    validate_dma_reply(&reply, msg_id, Command::DmaWrite)?;
    Ok(msg_id)
}

/// **review H2** — 公共 DMA reply 校验：msg_id + cmd + error flag。
///
/// 提取为 pub(crate) 让 U5 [`VfioUserTransport`] 在 multiplexed wait 循环中
/// 复用同一套校验逻辑。
pub(crate) fn validate_dma_reply(
    reply: &crate::framing::Message,
    expected_id: u16,
    expected_cmd: Command,
) -> anyhow::Result<()> {
    let r_id = reply.header.msg_id;
    let r_cmd = reply.header.cmd;
    if r_id != expected_id {
        anyhow::bail!("{expected_cmd:?} reply msg_id {r_id:#x} != expected {expected_id:#x}");
    }
    if reply.header.flags().is_error() {
        let err = reply.header.error_no;
        anyhow::bail!("{expected_cmd:?} peer error: errno={err}");
    }
    if r_cmd != expected_cmd as u16 {
        anyhow::bail!("{expected_cmd:?} reply cmd mismatch: got {r_cmd}");
    }
    Ok(())
}

/// server-initiated msg_id 分配器：顶位 `0x8000` 起始，便于日志区分；
/// `next_server_msg_id` 由 caller (VfioUserSession) 持有。
fn alloc_server_msg_id(next: &mut u16) -> u16 {
    let v = *next;
    // wraparound 后跳回 0x8000；保顶位 1。
    *next = if v == u16::MAX { 0x8000 } else { v + 1 };
    v
}

/// 与 [`crate::handshake::send_err`] 同等的本模块内联 helper（避免循环 import）。
fn send_err(stream: &mut UnixStream, msg_id: u16, cmd: Command, errno: u32) -> anyhow::Result<()> {
    let hdr = Header {
        msg_id,
        cmd: cmd as u16,
        msg_size: crate::proto::HEADER_LEN as u32,
        flags: HeaderFlags::reply_err().0,
        error_no: errno,
    };
    write_message(stream, &hdr, &[], &[]).context("write DMA err reply")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::read_message;
    use crate::framing::write_message as fw_write;
    use crate::proto::dma_map_flags;
    use crate::proto::dma_unmap_flags;
    use std::os::unix::net::UnixStream;
    use std::thread;

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    #[test]
    fn region_contains_inclusive_start_exclusive_end() {
        let r = DmaRegion {
            addr: 0x1000,
            size: 0x1000,
            readable: true,
            writeable: true,
        };
        assert!(r.contains(0x1000, 0x1000)); // 全包
        assert!(r.contains(0x1000, 1)); // 单字节
        assert!(!r.contains(0x0FFF, 1)); // 起点之前
        assert!(!r.contains(0x1000, 0x1001)); // 超尾
        assert!(!r.contains(0x2000, 1)); // 紧贴尾后 1
        assert!(!r.contains(u64::MAX, 1)); // 溢出
    }

    #[test]
    fn table_insert_remove_find_lookup() {
        let mut t = DmaTable::default();
        let a = DmaRegion {
            addr: 0x1000,
            size: 0x1000,
            readable: true,
            writeable: true,
        };
        let b = DmaRegion {
            addr: 0x3000,
            size: 0x1000,
            readable: true,
            writeable: false,
        };
        t.insert(a).unwrap();
        t.insert(b).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t.find(0x1500, 4).unwrap(), &a);
        assert_eq!(t.find(0x3500, 4).unwrap(), &b);
        assert!(t.find(0x5000, 4).is_none());
        // 重复 insert 同 addr
        assert_eq!(t.insert(a), Err(DmaError::Exists));
        // 精确 remove
        t.remove_exact(0x1000, 0x1000).unwrap();
        assert!(t.find(0x1500, 4).is_none());
        // size 错则不 remove
        assert_eq!(t.remove_exact(0x3000, 0x999), Err(DmaError::NotFound));
        assert_eq!(t.len(), 1);
        // UNMAP_ALL 等价 clear
        t.clear();
        assert!(t.is_empty());
    }

    #[test]
    fn handle_dma_map_then_unmap() {
        let (mut server, mut client) = pair();
        let mut table = DmaTable::default();
        let h = thread::spawn(move || -> anyhow::Result<DmaTable> {
            // 两轮：MAP + UNMAP
            let msg1 = read_message(&mut server)?;
            handle_dma_map(&mut server, &mut table, msg1.header.msg_id, &msg1)?;
            let msg2 = read_message(&mut server)?;
            handle_dma_unmap(&mut server, &mut table, msg2.header.msg_id, &msg2)?;
            Ok(table)
        });
        // 客户端发 DMA_MAP
        let map = DmaMapPayload {
            argsz: 32,
            flags: dma_map_flags::READABLE | dma_map_flags::WRITEABLE,
            offset: 0,
            addr: 0x1_0000,
            size: 0x1000,
        };
        let hdr = Header::command(1, Command::DmaMap, map.as_bytes().len() as u32);
        fw_write(&mut client, &hdr, map.as_bytes(), &[]).unwrap();
        let _reply = read_message(&mut client).unwrap();
        // 然后 UNMAP_ALL
        let un = DmaUnmapPayload {
            argsz: 24,
            flags: dma_unmap_flags::UNMAP_ALL,
            addr: 0,
            size: 0,
        };
        let hdr = Header::command(2, Command::DmaUnmap, un.as_bytes().len() as u32);
        fw_write(&mut client, &hdr, un.as_bytes(), &[]).unwrap();
        let _reply = read_message(&mut client).unwrap();
        let table = h.join().unwrap().unwrap();
        assert!(table.is_empty());
    }

    #[test]
    fn dma_read_sync_roundtrip() {
        let (mut server, mut client) = pair();
        let mut table = DmaTable::default();
        table
            .insert(DmaRegion {
                addr: 0x1000,
                size: 0x1000,
                readable: true,
                writeable: true,
            })
            .unwrap();
        // 启 server thread 发 DMA_READ 阻塞等
        let h = thread::spawn(move || -> anyhow::Result<(u16, Vec<u8>)> {
            let mut next = 0x8000u16;
            dma_read_sync(&mut server, &table, &mut next, 0x1100, 8)
        });
        // 客户端模拟 QEMU：read request → 发 reply 带 8 byte
        let req = read_message(&mut client).unwrap();
        {
            let cmd = req.header.cmd;
            assert_eq!(cmd, Command::DmaRead as u16);
        }
        let req_hdr: DmaRwHdrPayload = decode_payload(&req.payload).unwrap();
        let r_addr = req_hdr.addr;
        let r_cnt = req_hdr.count;
        assert_eq!(r_addr, 0x1100);
        assert_eq!(r_cnt, 8);
        // 发 reply
        let mut reply_pl = Vec::new();
        reply_pl.extend_from_slice(
            DmaRwHdrPayload {
                addr: 0x1100,
                count: 8,
            }
            .as_bytes(),
        );
        reply_pl.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22]);
        let rhdr = Header::reply_ok(req.header.msg_id, Command::DmaRead, reply_pl.len() as u32);
        fw_write(&mut client, &rhdr, &reply_pl, &[]).unwrap();
        let (msg_id, data) = h.join().unwrap().unwrap();
        assert_eq!(msg_id, 0x8000);
        assert_eq!(data, vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x11, 0x22]);
    }

    #[test]
    fn dma_read_outside_region_fails() {
        let server = pair().0;
        let mut server = server;
        let mut table = DmaTable::default();
        table
            .insert(DmaRegion {
                addr: 0x1000,
                size: 0x1000,
                readable: true,
                writeable: true,
            })
            .unwrap();
        let mut next = 0x8000u16;
        let r = dma_read_sync(&mut server, &table, &mut next, 0x9999, 8);
        assert!(r.is_err());
    }

    #[test]
    fn dma_write_sync_roundtrip() {
        let (mut server, mut client) = pair();
        let mut table = DmaTable::default();
        table
            .insert(DmaRegion {
                addr: 0x2000,
                size: 0x1000,
                readable: true,
                writeable: true,
            })
            .unwrap();
        let h = thread::spawn(move || -> anyhow::Result<u16> {
            let mut next = 0x8000u16;
            dma_write_sync(&mut server, &table, &mut next, 0x2200, &[1, 2, 3, 4])
        });
        let req = read_message(&mut client).unwrap();
        {
            let cmd = req.header.cmd;
            assert_eq!(cmd, Command::DmaWrite as u16);
        }
        let req_hdr: DmaRwHdrPayload =
            decode_payload(&req.payload[..core::mem::size_of::<DmaRwHdrPayload>()]).unwrap();
        let r_addr = req_hdr.addr;
        let r_cnt = req_hdr.count;
        assert_eq!(r_addr, 0x2200);
        assert_eq!(r_cnt, 4);
        let data_off = core::mem::size_of::<DmaRwHdrPayload>();
        assert_eq!(&req.payload[data_off..], &[1, 2, 3, 4]);
        // 回 echo
        let rhdr = Header::reply_ok(
            req.header.msg_id,
            Command::DmaWrite,
            core::mem::size_of::<DmaRwHdrPayload>() as u32,
        );
        fw_write(&mut client, &rhdr, &req.payload[..data_off], &[]).unwrap();
        let written_id = h.join().unwrap().unwrap();
        assert_eq!(written_id, 0x8000);
    }

    #[test]
    fn dma_write_to_readonly_region_rejected() {
        let server = pair().0;
        let mut server = server;
        let mut table = DmaTable::default();
        table
            .insert(DmaRegion {
                addr: 0x1000,
                size: 0x1000,
                readable: true,
                writeable: false,
            })
            .unwrap();
        let mut next = 0x8000u16;
        let r = dma_write_sync(&mut server, &table, &mut next, 0x1100, &[1, 2, 3]);
        assert!(r.is_err());
        let msg = format!("{:#}", r.unwrap_err());
        assert!(msg.contains("not writeable"), "got: {msg}");
    }

    #[test]
    fn alloc_server_msg_id_wraps_to_0x8000() {
        let mut n = u16::MAX;
        assert_eq!(alloc_server_msg_id(&mut n), u16::MAX);
        assert_eq!(n, 0x8000);
        let mut n = 0x8000u16;
        assert_eq!(alloc_server_msg_id(&mut n), 0x8000);
        assert_eq!(n, 0x8001);
    }

    /// **review M1** — `insert` 拒 *字节级* 重叠（非同 addr 也被拒）。
    #[test]
    fn table_insert_rejects_overlapping_regions() {
        let mut t = DmaTable::default();
        let a = DmaRegion {
            addr: 0x1000,
            size: 0x2000, // [0x1000, 0x3000)
            readable: true,
            writeable: true,
        };
        t.insert(a).unwrap();
        // 起点不同但与 a 末尾重叠
        let b = DmaRegion {
            addr: 0x2000,
            size: 0x1000, // [0x2000, 0x3000)
            readable: true,
            writeable: true,
        };
        assert_eq!(t.insert(b), Err(DmaError::Overlap));
        // 包含 a 整体
        let c = DmaRegion {
            addr: 0x0500,
            size: 0x3000, // [0x0500, 0x3500)
            readable: true,
            writeable: true,
        };
        assert_eq!(t.insert(c), Err(DmaError::Overlap));
        // 相邻但不重叠 — 允许
        let d = DmaRegion {
            addr: 0x3000,
            size: 0x1000, // [0x3000, 0x4000)
            readable: true,
            writeable: true,
        };
        assert!(t.insert(d).is_ok());
    }

    /// **review M3** — handle_dma_map 拒 size=0 / flags=0 / addr 溢出。
    #[test]
    fn handle_dma_map_rejects_invalid_payload() {
        for (label, pl) in [
            (
                "size=0",
                DmaMapPayload {
                    argsz: 32,
                    flags: dma_map_flags::READABLE,
                    offset: 0,
                    addr: 0x1000,
                    size: 0,
                },
            ),
            (
                "flags=0",
                DmaMapPayload {
                    argsz: 32,
                    flags: 0,
                    offset: 0,
                    addr: 0x1000,
                    size: 0x1000,
                },
            ),
            (
                "addr+size overflow",
                DmaMapPayload {
                    argsz: 32,
                    flags: dma_map_flags::READABLE,
                    offset: 0,
                    addr: u64::MAX - 0x100,
                    size: 0x1000,
                },
            ),
        ] {
            let (mut server, mut client) = pair();
            let mut table = DmaTable::default();
            let h = thread::spawn(move || -> anyhow::Result<()> {
                let msg = read_message(&mut server)?;
                handle_dma_map(&mut server, &mut table, msg.header.msg_id, &msg)?;
                Ok(())
            });
            let hdr = Header::command(1, Command::DmaMap, pl.as_bytes().len() as u32);
            fw_write(&mut client, &hdr, pl.as_bytes(), &[]).unwrap();
            let reply = read_message(&mut client).unwrap();
            assert!(reply.header.flags().is_error(), "case {label}: should err");
            let err = reply.header.error_no;
            assert_eq!(err, libc::EINVAL as u32, "case {label}: EINVAL");
            h.join().unwrap().unwrap();
        }
    }

    /// **review L2** — wire-level：第二次同 addr DMA_MAP 返 EEXIST。
    #[test]
    fn dma_map_duplicate_addr_returns_eexist() {
        let (mut server, mut client) = pair();
        let mut table = DmaTable::default();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let m1 = read_message(&mut server)?;
            handle_dma_map(&mut server, &mut table, m1.header.msg_id, &m1)?;
            let m2 = read_message(&mut server)?;
            handle_dma_map(&mut server, &mut table, m2.header.msg_id, &m2)?;
            Ok(())
        });
        let pl = DmaMapPayload {
            argsz: 32,
            flags: dma_map_flags::READABLE | dma_map_flags::WRITEABLE,
            offset: 0,
            addr: 0x4000,
            size: 0x1000,
        };
        let hdr = Header::command(10, Command::DmaMap, pl.as_bytes().len() as u32);
        fw_write(&mut client, &hdr, pl.as_bytes(), &[]).unwrap();
        let r1 = read_message(&mut client).unwrap();
        assert!(!r1.header.flags().is_error());
        // 第二次同 addr
        fw_write(&mut client, &hdr, pl.as_bytes(), &[]).unwrap();
        let r2 = read_message(&mut client).unwrap();
        assert!(r2.header.flags().is_error());
        let err = r2.header.error_no;
        assert_eq!(err, libc::EEXIST as u32);
        h.join().unwrap().unwrap();
    }
}
