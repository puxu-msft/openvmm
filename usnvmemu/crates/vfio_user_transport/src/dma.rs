// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase U4** — DMA_MAP/UNMAP 表 + 服务端发起的 DMA_READ/DMA_WRITE。
//!
//! ## 设计要点
//!
//! - **两条 DMA 路径**：① **zero-copy（Phase W）** —— DMA_MAP 带 memfd 时
//!   `mmap` 共享内存，`dma_read/write` 本地 memcpy，无 wire round-trip；
//!   ② **message-mediated（教学/回退路径）** —— 不带 fd 或 mmap 失败时，把
//!   region 元数据存表，`dma_read/write` 走 DMA_READ/DMA_WRITE S→C 消息 *阻塞*
//!   round-trip。spec 允许 server 忽略 fd 走消息路径，故二者都合规。
//!
//! - **Token = msg_id**：vfio-user 的 server-initiated msg_id 由 server 自选
//!   且与 client 方向独立。我们把 `Transport::dma_read/write` 返的 token 直接
//!   用 server-initiated msg_id（顶位 `0x8000` 起始便于日志区分）。
//!
//! - **同步阻塞**：`dma_read` 内部 `write request → read reply` 完成后才返
//!   token。[`VfioUserSession`](crate::VfioUserSession)（它自身 `impl Transport`）
//!   在 `dma_read`/`dma_write` 里调本模块的 `dma_read_sync` / `dma_write_sync`
//!   完成 wire 往返，把 `(token, data)` 推 `pending_completions`，再由 `pump`
//!   主循环投给 [`PcieDevice`] 的 `on_dma_complete`。本模块只提供 sync 原语，
//!   不直接持 PcieDevice callback。
//!
//! - **bound 校验**：DMA_MAP 表查 (addr, len) 是否落在某个映射区间；不在则返
//!   `EFAULT`（vfio-user 标准行为）。
//!
//! - **fd 处理（Phase W mmap 零拷贝）**：DMA_MAP 带 memfd 时，按 region 权限
//!   `mmap`，把该 region 的 backing 升级为 `DmaBacking::Mmap`，`dma_read/write`
//!   走零拷贝 memcpy 不走 wire；mmap 失败或不带 fd 时保持 `DmaBacking::Message`
//!   退回 wire 路径（功能不变）。映射用 MAP_SHARED 后独立于 fd 存续，fd 用完即
//!   close，munmap 随映射 Drop。
//!
//! ## TODO（spec follow-up，未来扩展前必读）
//!
//! 当前 `dma_read_sync`/`dma_write_sync` 把整段 (addr, len) 放进**单条**
//! DMA_READ/DMA_WRITE 消息。`DMA_READ/WRITE` 方向是 server→client，**接收方是
//! client**，故单条消息的数据量受 **client 广告的** `max_data_xfer_size` 约束
//! （per-receiver 语义，见 [`crate::handshake::SERVER_MAX_DATA_XFER_SIZE`] 的反方向）。
//! 教学 NVMe 当前单次 DMA 远小于 1 MiB（PRP 粒度），未触限；但**将来若要发
//! 超过 client `max_data_xfer_size` 的 DMA，必须先 parse `Negotiated.client_caps_json`
//! 里的 `max_data_xfer_size` 并据此分片**（类似 NVMe-oF 的 MAXH2CDATA 切片），
//! 否则超 client 接收上限。`VfioUserSession.negotiated` 字段正是这个未来钩子。

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
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use zerocopy::IntoBytes;

/// fd-backed 零拷贝 DMA 映射：`DMA_MAP` 带 memfd 时，按 region 权限 mmap，
/// `dma_read`/`dma_write` 直接 memcpy 不走 wire round-trip。
///
/// readable-only region → `Ro`（PROT_READ）；writeable region → `Rw`
/// （PROT_READ|WRITE，兼容读）。client 不带 fd 的 region 用
/// [`DmaBacking::Message`] 而非本类型。
#[derive(Debug)]
pub(crate) enum DmaMmap {
    /// 只读映射（region 仅 readable）。
    Ro(memmap2::Mmap),
    /// 读写映射（region writeable；读也走它）。
    Rw(memmap2::MmapMut),
}

impl DmaMmap {
    /// 映射字节（读路径；RO/RW 都可读）。
    fn as_bytes(&self) -> &[u8] {
        match self {
            DmaMmap::Ro(m) => m,
            DmaMmap::Rw(m) => m,
        }
    }
    /// 可写字节切片；RO 映射返 `None`（写须走 message 路径 / 报错）。
    fn as_bytes_mut(&mut self) -> Option<&mut [u8]> {
        match self {
            DmaMmap::Rw(m) => Some(&mut m[..]),
            DmaMmap::Ro(_) => None,
        }
    }
}

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

/// 一段已映射 DMA region 的**访问后端（DmaBacking）**：决定 `dma_read`/`dma_write`
/// 如何兑现对该 region 的访问。
///
/// 这是"region 如何被访问"的概念，**在 [`crate::Transport`] 之下一层**：
/// `Transport` 是 device 朝外的原语（`dma_read`/`dma_write`/`fire_interrupt`，
/// 一个 backend 一个）；`DmaBacking` 则逐 region 选定，决定一次 `dma_read` /
/// `dma_write` 落到本地内存还是落到 wire。
///
/// - [`DmaBacking::Mmap`]：DMA_MAP 带可映射 memfd → 本地 memcpy 零拷贝。
///   **QEMU 接管时的正常路径**（QEMU 给的 guest RAM 总是 memfd-backed）。
/// - [`DmaBacking::Message`]：无 fd / mmap 失败 → 走 wire DMA_READ/WRITE 往返。
///   **冷 fallback**：仅 non-mmappable 内存才走，QEMU 场景基本不触发。
#[derive(Debug)]
pub(crate) enum DmaBacking {
    /// 零拷贝 mmap（QEMU 正常路径）。
    Mmap(DmaMmap),
    /// wire message-mediated（DMA_READ/WRITE 往返，冷 fallback）。
    Message,
}

/// `DmaBacking` 的种类标签：不含资源句柄，`Copy`，供 metrics / 测试断言
/// "这条 region 是零拷贝还是 wire"，把"wire 是冷路径"变成结构上可查。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingKind {
    /// 零拷贝 mmap。
    Mmap,
    /// wire message-mediated。
    Message,
}

/// 一条 DMA 表项：region 元数据（`Copy` 真相源）+ 其访问后端 [`DmaBacking`]。
///
/// 把此前的两张并行 `BTreeMap`（`regions` + `mmaps`）合一，使"每个 region
/// 恰有一个 backing"成为类型级不变量，消除并行表失步的一类 bug（remove/clear
/// 漏删 mmap、mmap 孤儿项等）。
#[derive(Debug)]
struct RegionEntry {
    region: DmaRegion,
    backing: DmaBacking,
}

/// DMA 映射表：addr → `RegionEntry`（region 元数据 + 访问后端）。`BTreeMap`
/// 按 addr 排序，便于 `unmap_all` 时有序遍历。`region` 字段是权限/边界校验的
/// 真相源；`backing` 决定零拷贝 mmap 还是 wire message。
///
/// **不 derive Clone**：`backing` 内的 `memmap2::MmapMut` 非 Clone，且映射
/// 独占 fd 资源不应被复制。
#[derive(Debug, Default)]
pub struct DmaTable {
    regions: BTreeMap<u64, RegionEntry>,
}

impl DmaTable {
    /// 加一条 region（初始 backing = `DmaBacking::Message`，带 fd 时由
    /// `attach_mmap` 升级为零拷贝）；同 addr 已存在 →
    /// `Exists`；与已有 region 有任意 *字节级* 重叠（不同 addr）→ `Overlap`。
    pub fn insert(&mut self, r: DmaRegion) -> Result<(), DmaError> {
        if self.regions.contains_key(&r.addr) {
            return Err(DmaError::Exists);
        }
        // O(log n) 查左右邻居 + 校验是否相交。
        let r_end = r.addr.saturating_add(r.size);
        if let Some((_, prev)) = self.regions.range(..r.addr).next_back() {
            let p_end = prev.region.addr.saturating_add(prev.region.size);
            if p_end > r.addr {
                return Err(DmaError::Overlap);
            }
        }
        if let Some((_, next)) = self.regions.range(r.addr..).next()
            && r_end > next.region.addr
        {
            return Err(DmaError::Overlap);
        }
        self.regions.insert(
            r.addr,
            RegionEntry {
                region: r,
                backing: DmaBacking::Message,
            },
        );
        Ok(())
    }
    /// 移除 (addr, size) 必须精确匹配一条已存在 region；否则 EINVAL。
    /// backing（含 mmap）随表项 Drop 一并释放（munmap）。
    pub fn remove_exact(&mut self, addr: u64, size: u64) -> Result<(), DmaError> {
        match self.regions.get(&addr) {
            Some(e) if e.region.size == size => {
                self.regions.remove(&addr);
                Ok(())
            }
            _ => Err(DmaError::NotFound),
        }
    }
    /// 撤销所有 region（DMA_UNMAP flags=UNMAP_ALL）+ 所有 backing。
    pub fn clear(&mut self) {
        self.regions.clear();
    }
    /// **Phase W (mmap 零拷贝)** — 把已入表 region 的 backing 升级为零拷贝
    /// [`DmaBacking::Mmap`]。caller（`handle_dma_map`）在 region `insert` 成功
    /// 后、mmap fd 成功时调；此时 addr 必已存在（insert 在前）。addr 不存在
    /// 时静默 no-op（不再制造孤儿 mmap 项）。
    pub(crate) fn attach_mmap(&mut self, addr: u64, mmap: DmaMmap) {
        // **契约**：caller 必须先 `insert(region)` 再 attach（handle_dma_map 即如此）。
        // debug 构建下把这个隐式前提变成会炸的断言——若将来新 caller 违反顺序，
        // 立即暴露，而非静默走 no-op 让该 region 退回 wire（"wire 是冷路径需可查"）。
        debug_assert!(
            self.regions.contains_key(&addr),
            "attach_mmap({addr:#x}) before insert — region 必须先入表"
        );
        if let Some(e) = self.regions.get_mut(&addr) {
            e.backing = DmaBacking::Mmap(mmap);
        }
    }
    /// 内部：查包含 `[gpa, gpa+len)` 的表项（不可变借）。
    fn entry_for(&self, gpa: u64, len: u64) -> Option<&RegionEntry> {
        self.regions
            .range(..=gpa)
            .next_back()
            .map(|(_, e)| e)
            .filter(|e| e.region.contains(gpa, len))
    }
    /// 内部：查包含 `[gpa, gpa+len)` 的表项（可变借）。
    fn entry_for_mut(&mut self, gpa: u64, len: u64) -> Option<&mut RegionEntry> {
        self.regions
            .range_mut(..=gpa)
            .next_back()
            .map(|(_, e)| e)
            .filter(|e| e.region.contains(gpa, len))
    }
    /// **Phase W (mmap 零拷贝)** — 零拷贝读：若 `[gpa, gpa+len)` 落在某
    /// [`Mmap`](DmaBacking::Mmap)-backed 的 readable region 内，返回其字节副本
    /// （无 wire round-trip）；否则 `None`（`Message` backing / 不可读 / 越界 →
    /// caller 退回 message-mediated 路径）。
    pub(crate) fn mmap_read(&self, gpa: u64, len: u32) -> Option<Vec<u8>> {
        let e = self.entry_for(gpa, len as u64)?;
        if !e.region.readable {
            return None;
        }
        let DmaBacking::Mmap(m) = &e.backing else {
            return None;
        };
        let off = (gpa - e.region.addr) as usize;
        let end = off.checked_add(len as usize)?;
        let bytes = m.as_bytes();
        // **review H-2** — 防御校验以**真实映射长度** `bytes.len()` 为准，不依赖
        // "region.size == mmap 长度" 这个等式；保证 Rust slice 层永不越界。
        if end > bytes.len() {
            return None;
        }
        Some(bytes[off..end].to_vec())
    }
    /// **Phase W (mmap 零拷贝)** — 零拷贝写：若 `[gpa, gpa+len)` 落在某
    /// [`Mmap`](DmaBacking::Mmap)(RW)-backed 的 writeable region 内，原地写入并
    /// 返 `Some(())`（无 wire round-trip）；否则 `None`（无 mmap / RO 映射 /
    /// `Message` backing / 越界 → caller 退回 message 路径）。
    pub(crate) fn mmap_write(&mut self, gpa: u64, data: &[u8]) -> Option<()> {
        let e = self.entry_for_mut(gpa, data.len() as u64)?;
        if !e.region.writeable {
            return None;
        }
        let off = (gpa - e.region.addr) as usize;
        let DmaBacking::Mmap(m) = &mut e.backing else {
            return None;
        };
        let dst = m.as_bytes_mut()?;
        let end = off.checked_add(data.len())?;
        if end > dst.len() {
            return None;
        }
        dst[off..end].copy_from_slice(data);
        Some(())
    }
    /// 查找包含 `[gpa, gpa+len)` 的 region 元数据；返 readable/writeable 用于权限校验。
    pub fn find(&self, gpa: u64, len: u64) -> Option<&DmaRegion> {
        self.entry_for(gpa, len).map(|e| &e.region)
    }
    /// 包含 `gpa` 的 region 的访问后端种类（`Mmap`=零拷贝 / `Message`=wire）；
    /// 无此 region → `None`。供 metrics / 测试断言"这条 region 走哪条路径"。
    pub fn backing_kind(&self, gpa: u64) -> Option<BackingKind> {
        let e = self.entry_for(gpa, 1)?;
        Some(match e.backing {
            DmaBacking::Mmap(_) => BackingKind::Mmap,
            DmaBacking::Message => BackingKind::Message,
        })
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
/// **Phase W (mmap 零拷贝)** — 若 `msg.fds` 带 memfd，按 region 权限 mmap 进
/// `table.mmaps`，后续 `dma_read`/`dma_write` 走零拷贝 memcpy；mmap 失败或不带
/// fd 时退回 message-mediated 路径（功能不受影响）。取走的 fd 用完即 drop（close）；
/// 多余 fd 随 `msg.fds` Drop 自动 close，无 leak。
pub fn handle_dma_map(
    stream: &mut UnixStream,
    table: &mut DmaTable,
    msg_id: u16,
    msg: &mut Message,
    no_reply: bool,
) -> anyhow::Result<()> {
    let want = core::mem::size_of::<DmaMapPayload>();
    if msg.payload.len() != want {
        send_err(
            stream,
            msg_id,
            Command::DmaMap,
            libc::EINVAL as u32,
            no_reply,
        )?;
        return Ok(());
    }
    let pl: DmaMapPayload = match decode_payload(&msg.payload) {
        Ok(p) => p,
        Err(_) => {
            send_err(
                stream,
                msg_id,
                Command::DmaMap,
                libc::EINVAL as u32,
                no_reply,
            )?;
            return Ok(());
        }
    };
    // **review M3** — 拒 size=0、flags=0、addr+size 溢出。
    let perms =
        pl.flags & (crate::proto::dma_map_flags::READABLE | crate::proto::dma_map_flags::WRITEABLE);
    if pl.size == 0 || perms == 0 {
        send_err(
            stream,
            msg_id,
            Command::DmaMap,
            libc::EINVAL as u32,
            no_reply,
        )?;
        return Ok(());
    }
    if pl.addr.checked_add(pl.size).is_none() {
        send_err(
            stream,
            msg_id,
            Command::DmaMap,
            libc::EINVAL as u32,
            no_reply,
        )?;
        return Ok(());
    }
    let region = DmaRegion {
        addr: pl.addr,
        size: pl.size,
        readable: pl.flags & crate::proto::dma_map_flags::READABLE != 0,
        writeable: pl.flags & crate::proto::dma_map_flags::WRITEABLE != 0,
    };
    if let Err(e) = table.insert(region) {
        send_err(stream, msg_id, Command::DmaMap, e.errno(), no_reply)?;
        return Ok(());
    }
    let r_addr = region.addr;
    let r_size = region.size;
    let r_rd = region.readable;
    let r_wr = region.writeable;
    // **Phase W (mmap 零拷贝)** — region 入表成功后，若带 fd 则 mmap。取第一个
    // fd（vfio-user DMA_MAP 单 region 单 fd）；mmap 失败只记 warn + 退回 message
    // 路径，不让 MAP 整体失败（功能正确性不依赖 mmap）。
    let mut zero_copy = false;
    if !msg.fds.is_empty() {
        let fd = msg.fds.remove(0);
        match map_dma_fd(&fd, pl.offset, pl.size, region.writeable) {
            Ok(mmap) => {
                table.attach_mmap(region.addr, mmap);
                zero_copy = true;
            }
            Err(e) => {
                tracing::warn!(
                    addr = format_args!("{r_addr:#x}"),
                    error = %e,
                    "DMA_MAP mmap 失败，退回 message-mediated"
                );
            }
        }
        // fd 在此 drop（close）；mmap(MAP_SHARED) 后映射独立于 fd 存续。
    }
    tracing::debug!(
        addr = format_args!("{r_addr:#x}"),
        size = r_size,
        readable = r_rd,
        writeable = r_wr,
        zero_copy,
        "DMA_MAP added"
    );
    // OK reply：spec 说回 header only。NO_REPLY（posted MAP）则不发。
    if no_reply {
        return Ok(());
    }
    let hdr = Header::reply_ok(msg_id, Command::DmaMap, 0);
    write_message(stream, &hdr, &[], &[]).context("write DMA_MAP reply")
}

/// **Phase W (mmap 零拷贝)** — 按 `[offset, offset+size)` mmap 客户端 memfd。
/// writeable region → RW 映射（兼容读写）；只读 region → RO 映射。
///
/// **review C-1（CRITICAL 修复）**：mmap 前必须 `fstat` fd 校验
/// `offset + size ≤ 文件真实大小`。`mmap(2)` 只要求 offset 页对齐，**不**校验
/// 区间是否在文件内；映射超出真实页的区间会成功返回，但 memcpy 触碰即 **SIGBUS**
/// 杀进程。client 声明的 `size` 不可信（恶意 client 发大 size + 小 memfd 即可
/// DoS），故真正的 oracle 是 `fstat().st_size`，不是 client 的声明。校验失败 →
/// `Err` 退回 message-mediated 路径，绝不 mmap 超出真实文件的区间。
fn map_dma_fd(
    fd: &std::os::fd::OwnedFd,
    offset: u64,
    size: u64,
    writeable: bool,
) -> std::io::Result<DmaMmap> {
    use std::os::fd::AsFd as _;
    // **review C-1** — 用 fd 真实大小（独立 oracle）校验，而非 client 声明的 size。
    let st = nix::sys::stat::fstat(fd.as_fd())
        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
    let file_len = u64::try_from(st.st_size).unwrap_or(0);
    let end = offset
        .checked_add(size)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "offset+size 溢出"))?;
    // **W5a real-VM 修复** — fstat `st_size` 上界（防 SIGBUS）仅对**普通文件**
    // （memfd）有意义。真 OpenHCL guest RAM fd 是**字符设备** /dev/mshv_vtl_low
    // （st_size=0），其 mmap 有效性由驱动的 GPA-range mmap handler 决定，非 st_size。
    // 对字符/块设备施此上界会误拒（offset+size > 0）→ 零拷贝失效（W3 仅用 memfd 测，
    // 漏了真目标）。故仅普通文件校验上界；设备 fd 跳过，由 mmap+驱动 enforce。
    use nix::sys::stat::SFlag;
    let is_regular = SFlag::from_bits_truncate(st.st_mode) & SFlag::S_IFMT == SFlag::S_IFREG;
    if is_regular && end > file_len {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("DMA_MAP offset+size {end} 超出 fd 真实大小 {file_len}（防 SIGBUS）"),
        ));
    }
    let mut opts = memmap2::MmapOptions::new();
    opts.offset(offset).len(size as usize);
    let raw = fd.as_raw_fd();
    if writeable {
        #[allow(unsafe_code)]
        // SAFETY: `fd` 是 client 经 SCM_RIGHTS 传来的 memfd（共享 guest RAM）。
        // 不变量与外部依赖（已诚实列出，非自家 self-consistent 断言）：
        // 1. **有真实页 backing**：上面 `fstat` 已校验 `offset+size ≤ st_size`，
        //    故映射区间全程有文件页 backing，普通访问不会 SIGBUS（修 review C-1）。
        //    ——对**字符设备**（mshv_vtl_low，st_size=0）此上界跳过（W5a 修），backing
        //    由设备驱动的 GPA-range mmap handler 保证（映射真 guest 物理页，POC-6 验）；
        //    越界 GPA 由驱动 mmap 失败 enforce，非 fstat。
        // 2. **fd 生命周期**：mmap 用 MAP_SHARED，映射独立于 fd 存续；caller 在
        //    map 后 drop fd 是安全的。munmap 随 `MmapMut` Drop。
        // 3. **slice 边界**：`mmap_read/write` 以真实映射长度 `bytes.len()`（非
        //    client 声明 size）做 `off+len` 校验，故 Rust slice 层不会越界。
        // 4. **外部依赖（教学版接受、生产须加固）**：若 client 在映射存续期间
        //    `ftruncate` 缩小 memfd，超出新大小的页会失去 backing → SIGBUS（fstat
        //    是单次 TOCTOU，挡不住事后 shrink）。生产应要求 client 对 memfd 加
        //    `F_SEAL_SHRINK` 后再信任；本教学版信任 QEMU 不 shrink 已映射的 DMA 区。
        // 5. **并发**：guest 可并发改这段共享内存——这是 DMA 的固有语义。纯 `u8`
        //    memcpy 无 typed 解释，撕裂的字节值合法 → 无 Rust 内存模型 UB；逻辑
        //    一致性（不在 DMA in-flight 时改 buffer）由 guest/DMA 协议保证，非本层职责。
        let m = unsafe { opts.map_mut(raw) }?;
        Ok(DmaMmap::Rw(m))
    } else {
        #[allow(unsafe_code)]
        // SAFETY: 同 RW 分支的不变量 1-5（只读映射，只读不写）。关键同样是：上面
        // `fstat` 校验保证有真实页 backing（修 C-1；字符设备如 mshv_vtl_low st_size=0
        // 跳过上界，backing 由驱动 mmap handler 保证，W5a 修）；`mmap_read` 以
        // `bytes.len()` 校验 slice 边界；F_SEAL_SHRINK 外部依赖同上；u8 读无 UB。
        let m = unsafe { opts.map(raw) }?;
        Ok(DmaMmap::Ro(m))
    }
}

/// 处理 DMA_UNMAP cmd。
pub fn handle_dma_unmap(
    stream: &mut UnixStream,
    table: &mut DmaTable,
    msg_id: u16,
    msg: &Message,
    no_reply: bool,
) -> anyhow::Result<()> {
    let want = core::mem::size_of::<DmaUnmapPayload>();
    if msg.payload.len() != want {
        send_err(
            stream,
            msg_id,
            Command::DmaUnmap,
            libc::EINVAL as u32,
            no_reply,
        )?;
        return Ok(());
    }
    let pl: DmaUnmapPayload = match decode_payload(&msg.payload) {
        Ok(p) => p,
        Err(_) => {
            send_err(
                stream,
                msg_id,
                Command::DmaUnmap,
                libc::EINVAL as u32,
                no_reply,
            )?;
            return Ok(());
        }
    };
    if pl.flags & crate::proto::dma_unmap_flags::UNMAP_ALL != 0 {
        table.clear();
        tracing::debug!("DMA_UNMAP all regions");
    } else if let Err(e) = table.remove_exact(pl.addr, pl.size) {
        send_err(stream, msg_id, Command::DmaUnmap, e.errno(), no_reply)?;
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
    // NO_REPLY（posted UNMAP）则不发。
    if no_reply {
        return Ok(());
    }
    let hdr = Header::reply_ok(msg_id, Command::DmaUnmap, pl.as_bytes().len() as u32);
    write_message(stream, &hdr, pl.as_bytes(), &[]).context("write DMA_UNMAP reply")
}

/// **Phase W (vfio-spec head-of-line 修复)** — 等待一条 server-initiated 请求
/// 的 reply，期间把**非本次 reply** 的入站帧 defer 到 `inbound_queue`。
///
/// vfio-user spec：两方向 msg_id 独立、无严格请求/应答顺序，client 完全可能在
/// 我们等 DMA reply 时插一条 REGION_READ 等 inbound command。此前 `dma_*_sync`
/// 盲读下一帧当 reply → 校验失败 → 连接挂（真 QEMU 并发 blocker）。现在按
/// msg_id 精确匹配本次 reply；其余帧 defer，等当前 device 回调栈 unwind 后由
/// `VfioUserSession::pump_one` 顶部安全 dispatch（避免 device 重入）。
///
/// server-initiated msg_id 唯一标识本次请求（顶位 0x8000，与 client msg_id 不撞），
/// 故 `msg_id == expected` 即本次 reply。
///
/// **DoS 注记**：恶意 peer 持续插帧会让 `inbound_queue` 无界增长（教学版未设
/// 上限）；真生产应加 backpressure / 队列上限，留 future hardening。
pub(crate) fn read_reply_deferring_inbound(
    stream: &mut UnixStream,
    expected_msg_id: u16,
    inbound_queue: &mut std::collections::VecDeque<crate::framing::Message>,
) -> anyhow::Result<crate::framing::Message> {
    loop {
        let frame = read_message(stream).context("read while waiting server-request reply")?;
        // packed header 字段先 copy 到局部，避免 unaligned ref。
        let frame_id = frame.header.msg_id;
        // **review H-1** — 必须**同时**是 REPLY flag 且 msg_id 匹配。vfio-user 两
        // 方向 msg_id 独立、spec **不保留**顶位 0x8000；client 的 inbound command
        // 完全可能 msg_id 撞上我们 server-initiated id。只认 msg_id 会把这条
        // command 误当 reply 吞掉 → command 丢失 + 连接挂（与本 fix 要根除的
        // head-of-line bug 同类）。reply flag 才是协议语义判据。
        if frame_id == expected_msg_id && frame.header.flags().is_reply() {
            return Ok(frame);
        }
        let frame_cmd = frame.header.cmd;
        tracing::debug!(
            deferred_id = frame_id,
            deferred_cmd = frame_cmd,
            awaiting = expected_msg_id,
            "DMA wait: defer interleaved inbound frame (head-of-line 修复)"
        );
        inbound_queue.push_back(frame);
    }
}

/// 服务端发起的同步 DMA_READ：发 request → 等 reply。
///
/// 返 `(msg_id, data)`：`msg_id` 就是我们用过的 wire id（=token，写给
/// caller 当 DMA 完成 token）；`data` 是 guest mem 字节。
///
/// **review H1/H2** — vfio-user spec 两方向 msg_id 独立、不要求严格请求/应答
/// 顺序；client 完全可能在我们等 reply 时插一条 REGION_READ。本函数通过
/// `read_reply_deferring_inbound` 拿 reply：匹配本次 `msg_id` 的帧才返回，
/// 其余入站帧 defer 到 `inbound_queue`，由 session pump 后续处理 —— 多路复用
/// 已在此满足，无需上层再套 wait-for-reply 循环。
///
/// **review H2** — 校验 `reply.header.msg_id == msg_id` + `cmd == DmaRead`，
/// 并把 `msg_id` 返给 caller，token 由它直接持有不依赖 `wrapping_sub(1)`。
pub fn dma_read_sync(
    stream: &mut UnixStream,
    table: &DmaTable,
    next_server_msg_id: &mut u16,
    inbound_queue: &mut std::collections::VecDeque<crate::framing::Message>,
    gpa: u64,
    len: u32,
) -> anyhow::Result<(u16, Vec<u8>)> {
    let region = *table
        .find(gpa, len as u64)
        .ok_or_else(|| anyhow::anyhow!("DMA_READ {gpa:#x}+{len} not in any DMA region"))?;
    if !region.readable {
        anyhow::bail!("DMA_READ region @{gpa:#x} not readable");
    }
    // **Phase W (mmap 零拷贝)** — 命中带 mmap 的 region → 本地 memcpy，无 wire
    // round-trip。仍分配 token（server msg_id）保持返回契约一致（caller 用它
    // 当 DMA 完成 token close pending_ios）。
    if let Some(data) = table.mmap_read(gpa, len) {
        let msg_id = alloc_server_msg_id(next_server_msg_id);
        return Ok((msg_id, data));
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
    let reply = read_reply_deferring_inbound(stream, msg_id, inbound_queue)?;
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

/// 服务端发起的同步 DMA_WRITE：命中 mmap → 零拷贝写；否则发 request + data →
/// 等 echo reply。返 msg_id。`table` 取 `&mut`（mmap 写需可变）。
pub fn dma_write_sync(
    stream: &mut UnixStream,
    table: &mut DmaTable,
    next_server_msg_id: &mut u16,
    inbound_queue: &mut std::collections::VecDeque<crate::framing::Message>,
    gpa: u64,
    data: &[u8],
) -> anyhow::Result<u16> {
    let len = data.len() as u64;
    let region = *table
        .find(gpa, len)
        .ok_or_else(|| anyhow::anyhow!("DMA_WRITE {gpa:#x}+{len} not in any DMA region"))?;
    if !region.writeable {
        anyhow::bail!("DMA_WRITE region @{gpa:#x} not writeable");
    }
    // **Phase W (mmap 零拷贝)** — 命中带 RW mmap 的 region → 本地 memcpy，无 wire。
    if table.mmap_write(gpa, data).is_some() {
        return Ok(alloc_server_msg_id(next_server_msg_id));
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
    let reply = read_reply_deferring_inbound(stream, msg_id, inbound_queue)?;
    validate_dma_reply(&reply, msg_id, Command::DmaWrite)?;
    Ok(msg_id)
}

/// **review H2** — 公共 DMA reply 校验：msg_id + cmd + error flag。
///
/// 提取为 pub(crate) 让 `dma_read_sync` / `dma_write_sync` 复用同一套校验逻辑。
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
///
/// **LOW-2（DBBUF 负载放大说明）**：firmware 广告 OACS Doorbell Buffer Config 后，
/// 每次 SQ/CQ 提交 controller 都可能多发一组 shadow-doorbell DMA（读 shadow + 写
/// event_idx），这把经本 transport 的 DMA-op 速率成倍放大 → `0x8000..=0xFFFF`
/// （32768 值）这段 server msg_id 空间在更高速率下成为**承载关键**。仍然安全：任一
/// 时刻 outstanding 的 server-initiated 请求数极小（DMA 同步往返、逐条完成），值在
/// 绕回复用前早已周转过千万次，不会与在飞 id 撞。但若未来把 DMA 改成大批量并发在飞，
/// 需重新核对此空间容量。
pub(crate) fn alloc_server_msg_id(next: &mut u16) -> u16 {
    let v = *next;
    // wraparound 后跳回 0x8000；保顶位 1。
    *next = if v == u16::MAX { 0x8000 } else { v + 1 };
    v
}

/// 与 [`crate::handshake::send_err`] 同等的本模块内联 helper（避免循环 import）。
///
/// `no_reply` = true（posted 请求）时静默不发任何 reply（含 error），符合 vfio-user
/// NO_REPLY 语义（QEMU 没把该 msg_id 入 pending，多发会让它失步）。
fn send_err(
    stream: &mut UnixStream,
    msg_id: u16,
    cmd: Command,
    errno: u32,
    no_reply: bool,
) -> anyhow::Result<()> {
    if no_reply {
        return Ok(());
    }
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
            let mut msg1 = read_message(&mut server)?;
            handle_dma_map(
                &mut server,
                &mut table,
                msg1.header.msg_id,
                &mut msg1,
                false,
            )?;
            let msg2 = read_message(&mut server)?;
            handle_dma_unmap(&mut server, &mut table, msg2.header.msg_id, &msg2, false)?;
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
            let mut queue = std::collections::VecDeque::new();
            dma_read_sync(&mut server, &table, &mut next, &mut queue, 0x1100, 8)
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

    /// **Phase W (head-of-line 修复)** — client 在我们等 DMA reply 时插一条
    /// REGION_READ：DMA reply 仍按 msg_id 正确返回（不被误判），interleaved 帧
    /// defer 到 queue 待 pump_one 后续处理。此前盲读会把 REGION_READ 当 DMA
    /// reply 校验失败 → 连接挂（真 QEMU 并发 blocker）。
    #[test]
    fn dma_read_sync_defers_interleaved_inbound() {
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
        let h = thread::spawn(move || {
            let mut next = 0x8000u16;
            let mut queue = std::collections::VecDeque::new();
            let r = dma_read_sync(&mut server, &table, &mut next, &mut queue, 0x1100, 8);
            (r, queue)
        });
        // client 读 DMA_READ 请求
        let req = read_message(&mut client).unwrap();
        let req_id = req.header.msg_id;
        // 先插一条 interleaved inbound（client msg_id=0x42 的 REGION_READ）
        let inter = Header::command(0x42, Command::RegionRead, 0);
        fw_write(&mut client, &inter, &[], &[]).unwrap();
        // 再发真 DMA_READ reply（msg_id = 请求的 server-initiated id）
        let mut reply_pl = Vec::new();
        reply_pl.extend_from_slice(
            DmaRwHdrPayload {
                addr: 0x1100,
                count: 8,
            }
            .as_bytes(),
        );
        reply_pl.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let rhdr = Header::reply_ok(req_id, Command::DmaRead, reply_pl.len() as u32);
        fw_write(&mut client, &rhdr, &reply_pl, &[]).unwrap();
        let (r, queue) = h.join().unwrap();
        let (msg_id, data) = r.unwrap();
        assert_eq!(msg_id, 0x8000);
        assert_eq!(
            data,
            vec![1, 2, 3, 4, 5, 6, 7, 8],
            "DMA reply 正确返回，未被 interleaved 帧干扰"
        );
        // interleaved REGION_READ 被 defer，等 pump_one 后续 dispatch
        assert_eq!(queue.len(), 1, "interleaved 帧应 defer 到 queue");
        let q_id = queue[0].header.msg_id;
        let q_cmd = queue[0].header.cmd;
        assert_eq!(q_id, 0x42);
        assert_eq!(q_cmd, Command::RegionRead as u16);
    }

    /// **review H-1** — inbound COMMAND 的 msg_id 即使**撞上**我们 server-initiated
    /// id（0x8000），也必须 defer（它非 reply flag），不被误当本次 DMA reply。
    /// 此前只认 msg_id 会把它吞掉 → command 丢失 + 连接挂（与 head-of-line 同类）。
    #[test]
    fn dma_reply_match_requires_reply_flag_not_just_id() {
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
        let h = thread::spawn(move || {
            let mut next = 0x8000u16;
            let mut queue = std::collections::VecDeque::new();
            let r = dma_read_sync(&mut server, &table, &mut next, &mut queue, 0x1100, 8);
            (r, queue)
        });
        let req = read_message(&mut client).unwrap();
        let req_id = req.header.msg_id; // = 0x8000
        // 插一条 COMMAND，msg_id **故意 == req_id**（撞顶位），但是 command 非 reply。
        let collide = Header::command(req_id, Command::RegionRead, 0);
        fw_write(&mut client, &collide, &[], &[]).unwrap();
        // 真 reply（msg_id == req_id，reply flag）。
        let mut reply_pl = Vec::new();
        reply_pl.extend_from_slice(
            DmaRwHdrPayload {
                addr: 0x1100,
                count: 8,
            }
            .as_bytes(),
        );
        reply_pl.extend_from_slice(&[9u8; 8]);
        let rhdr = Header::reply_ok(req_id, Command::DmaRead, reply_pl.len() as u32);
        fw_write(&mut client, &rhdr, &reply_pl, &[]).unwrap();
        let (r, queue) = h.join().unwrap();
        let (_id, data) = r.unwrap();
        assert_eq!(data, vec![9u8; 8], "撞 id 的 command 没被误当 reply");
        assert_eq!(
            queue.len(),
            1,
            "撞 id 的 command 仍被 defer（因非 reply flag）"
        );
        let q_cmd = queue[0].header.cmd;
        assert_eq!(q_cmd, Command::RegionRead as u16);
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
        let mut queue = std::collections::VecDeque::new();
        let r = dma_read_sync(&mut server, &table, &mut next, &mut queue, 0x9999, 8);
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
            let mut queue = std::collections::VecDeque::new();
            dma_write_sync(
                &mut server,
                &mut table,
                &mut next,
                &mut queue,
                0x2200,
                &[1, 2, 3, 4],
            )
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
        let mut queue = std::collections::VecDeque::new();
        let r = dma_write_sync(
            &mut server,
            &mut table,
            &mut next,
            &mut queue,
            0x1100,
            &[1, 2, 3],
        );
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
                let mut msg = read_message(&mut server)?;
                handle_dma_map(&mut server, &mut table, msg.header.msg_id, &mut msg, false)?;
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
            let mut m1 = read_message(&mut server)?;
            handle_dma_map(&mut server, &mut table, m1.header.msg_id, &mut m1, false)?;
            let mut m2 = read_message(&mut server)?;
            handle_dma_map(&mut server, &mut table, m2.header.msg_id, &mut m2, false)?;
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

    // ── Phase W: mmap 零拷贝 DMA ──────────────────────────────────────────

    /// 建一个含 `bytes` 的 memfd（DMA_MAP fd 的测试替身，模拟 client 共享 RAM）。
    fn memfd_with(bytes: &[u8]) -> std::os::fd::OwnedFd {
        use std::io::Write as _;
        let fd = nix::sys::memfd::memfd_create(c"dma-zc-test", nix::sys::memfd::MFdFlags::empty())
            .expect("memfd_create");
        let mut f = std::fs::File::from(fd);
        f.write_all(bytes).expect("write memfd");
        f.flush().ok();
        std::os::fd::OwnedFd::from(f)
    }

    /// 构造一条带 fd 的 DMA_MAP Message（绕过 socket，直接喂 handle_dma_map）。
    fn dma_map_msg(addr: u64, size: u64, flags: u32, fd: std::os::fd::OwnedFd) -> Message {
        let pl = DmaMapPayload {
            argsz: core::mem::size_of::<DmaMapPayload>() as u32,
            flags,
            offset: 0,
            addr,
            size,
        };
        Message {
            wire: crate::framing::WireMessage { header: Header::command(1, Command::DmaMap, pl.as_bytes().len() as u32), payload: pl.as_bytes().to_vec() },
            fds: vec![fd],
        }
    }

    /// 带 fd 的 DMA_MAP → mmap 附加；`dma_read` 走零拷贝读出 memfd 内容（无 wire）。
    #[test]
    fn dma_map_with_fd_enables_zero_copy_read() {
        let (mut server, _client) = pair();
        let mut table = DmaTable::default();
        let bytes = b"zero-copy-dma-payload-0123456789";
        let mut msg = dma_map_msg(
            0x4000,
            bytes.len() as u64,
            dma_map_flags::READABLE | dma_map_flags::WRITEABLE,
            memfd_with(bytes),
        );
        handle_dma_map(&mut server, &mut table, 1, &mut msg, false).unwrap();
        assert!(msg.fds.is_empty(), "fd 应被取走");
        // mmap_read 命中 → 返 memfd 内容。
        assert_eq!(
            table.mmap_read(0x4000, bytes.len() as u32).as_deref(),
            Some(&bytes[..])
        );
        // 子区间偏移读也正确。
        assert_eq!(table.mmap_read(0x4005, 4).as_deref(), Some(&bytes[5..9]));
        // dma_read_sync 命中 mmap：无需 server 端 reply 即返（_client 没发任何东西）。
        let mut next = 0x8000u16;
        let mut q = std::collections::VecDeque::new();
        let (tok, data) = dma_read_sync(&mut server, &table, &mut next, &mut q, 0x4000, 8).unwrap();
        assert_eq!(tok, 0x8000);
        assert_eq!(data, &bytes[..8]);
    }

    /// 带 RW fd 的 DMA_MAP → `dma_write` 零拷贝写进 memfd，回读可见。
    #[test]
    fn dma_map_with_fd_enables_zero_copy_write() {
        let (mut server, _client) = pair();
        let mut table = DmaTable::default();
        let fd = memfd_with(&[0u8; 64]);
        let mut msg = dma_map_msg(
            0x5000,
            64,
            dma_map_flags::READABLE | dma_map_flags::WRITEABLE,
            fd,
        );
        handle_dma_map(&mut server, &mut table, 1, &mut msg, false).unwrap();
        let mut next = 0x8000u16;
        let mut q = std::collections::VecDeque::new();
        let payload = b"written-via-mmap";
        let tok =
            dma_write_sync(&mut server, &mut table, &mut next, &mut q, 0x5010, payload).unwrap();
        assert_eq!(tok, 0x8000);
        // 回读 mmap 见写入值。
        assert_eq!(
            table.mmap_read(0x5010, payload.len() as u32).as_deref(),
            Some(&payload[..])
        );
    }

    /// 不带 fd 的 DMA_MAP → 不附加 mmap；mmap_read 返 None（退回 message 路径）。
    #[test]
    fn dma_map_without_fd_no_mmap_falls_back() {
        let (mut server, mut client) = pair();
        let mut table = DmaTable::default();
        let h = thread::spawn(move || -> anyhow::Result<DmaTable> {
            let mut m = read_message(&mut server)?;
            handle_dma_map(&mut server, &mut table, m.header.msg_id, &mut m, false)?;
            Ok(table)
        });
        let pl = DmaMapPayload {
            argsz: 32,
            flags: dma_map_flags::READABLE | dma_map_flags::WRITEABLE,
            offset: 0,
            addr: 0x6000,
            size: 0x1000,
        };
        let hdr = Header::command(1, Command::DmaMap, pl.as_bytes().len() as u32);
        fw_write(&mut client, &hdr, pl.as_bytes(), &[]).unwrap();
        let reply = read_message(&mut client).unwrap();
        assert!(!reply.header.flags().is_error(), "无 fd 的 MAP 仍应成功");
        let table = h.join().unwrap().unwrap();
        assert!(
            table.mmap_read(0x6000, 8).is_none(),
            "无 fd → 无 mmap，应退回 message 路径"
        );
    }

    /// 只读 region 的 mmap：`mmap_write` 返 None（写须走 message / 报错），读 OK。
    #[test]
    fn dma_map_readonly_fd_blocks_zero_copy_write() {
        let (mut server, _client) = pair();
        let mut table = DmaTable::default();
        let bytes = b"read-only-region";
        let mut msg = dma_map_msg(
            0x7000,
            bytes.len() as u64,
            dma_map_flags::READABLE,
            memfd_with(bytes),
        );
        handle_dma_map(&mut server, &mut table, 1, &mut msg, false).unwrap();
        // 读零拷贝 OK。
        assert_eq!(
            table.mmap_read(0x7000, bytes.len() as u32).as_deref(),
            Some(&bytes[..])
        );
        // 写零拷贝被拒（RO 映射）→ None。
        assert!(table.mmap_write(0x7000, b"xxxx").is_none());
    }

    /// DMA_UNMAP 精确移除后，mmap 一并丢弃（zero-copy 读返 None）。
    #[test]
    fn dma_unmap_drops_mmap() {
        let (mut server, _client) = pair();
        let mut table = DmaTable::default();
        let bytes = b"unmap-drops-mmap";
        let mut msg = dma_map_msg(
            0x8800,
            bytes.len() as u64,
            dma_map_flags::READABLE | dma_map_flags::WRITEABLE,
            memfd_with(bytes),
        );
        handle_dma_map(&mut server, &mut table, 1, &mut msg, false).unwrap();
        assert!(table.mmap_read(0x8800, 4).is_some());
        table.remove_exact(0x8800, bytes.len() as u64).unwrap();
        assert!(table.mmap_read(0x8800, 4).is_none(), "unmap 后 mmap 应丢弃");
    }

    /// **review C-1（CRITICAL 回归）** — client 声明 size 大于 memfd 真实大小：
    /// mmap **不得**附加（否则 memcpy 触发 SIGBUS 杀进程）；DMA_MAP 本身仍成功，
    /// 退回 message 路径（`mmap_read` 返 None）。本测试若回退失效会让进程崩溃，
    /// 故它同时是"绝不 SIGBUS"的活体守卫。
    #[test]
    fn dma_map_fd_smaller_than_declared_size_falls_back_no_sigbus() {
        let (mut server, _client) = pair();
        let mut table = DmaTable::default();
        // memfd 只有 4096 字节，但 DMA_MAP 声明 8192。
        let fd = memfd_with(&[0xABu8; 4096]);
        let mut msg = dma_map_msg(
            0x9000,
            8192,
            dma_map_flags::READABLE | dma_map_flags::WRITEABLE,
            fd,
        );
        handle_dma_map(&mut server, &mut table, 1, &mut msg, false).unwrap();
        // fstat 校验拒绝 mmap（offset+size > 真实大小）→ 无 mmap 附加。
        assert!(
            table.mmap_read(0x9000, 8).is_none(),
            "fd 小于声明 size 时必须退回 message 路径，绝不 mmap（防 SIGBUS）"
        );
    }

    /// **review L-2** — `pl.offset` 透传：DMA_MAP 带页对齐 offset，mmap 映射 fd 内
    /// `[offset, offset+size)`，zero-copy 读出该段内容。
    #[test]
    fn dma_map_honors_fd_offset() {
        let (mut server, _client) = pair();
        let mut table = DmaTable::default();
        // memfd 总长 4096+64；marker 写在 offset 4096 处。
        let mut content = vec![0u8; 4096 + 64];
        let marker: &[u8] = b"at-page-1-offset-marker-012345678";
        content[4096..4096 + marker.len()].copy_from_slice(marker);
        let fd = memfd_with(&content);
        let pl = DmaMapPayload {
            argsz: core::mem::size_of::<DmaMapPayload>() as u32,
            flags: dma_map_flags::READABLE,
            offset: 4096, // 页对齐
            addr: 0xA000,
            size: 64,
        };
        let mut msg = Message {
            wire: crate::framing::WireMessage { header: Header::command(1, Command::DmaMap, pl.as_bytes().len() as u32), payload: pl.as_bytes().to_vec() },
            fds: vec![fd],
        };
        handle_dma_map(&mut server, &mut table, 1, &mut msg, false).unwrap();
        // gpa 0xA000 → fd offset 4096，读出 marker。
        assert_eq!(
            table.mmap_read(0xA000, marker.len() as u32).as_deref(),
            Some(marker)
        );
    }

    // ── DmaBacking 概念：backing_kind 可观测 + 单表不失步 ──────────────────

    /// `backing_kind` 反映 region 的访问后端：不带 fd → `Message`（wire）；
    /// `attach_mmap` 后 → `Mmap`（零拷贝）；未映射 gpa → `None`。
    #[test]
    fn backing_kind_reflects_message_then_mmap() {
        let mut t = DmaTable::default();
        t.insert(DmaRegion {
            addr: 0x1000,
            size: 0x1000,
            readable: true,
            writeable: true,
        })
        .unwrap();
        // 初始 backing = Message（wire fallback），region 内任意 gpa 都报 Message。
        assert_eq!(t.backing_kind(0x1000), Some(BackingKind::Message));
        assert_eq!(t.backing_kind(0x1800), Some(BackingKind::Message));
        // region 外 → None。
        assert_eq!(t.backing_kind(0x9999), None);

        // 附加 mmap → 升级为零拷贝 Mmap backing。
        let fd = memfd_with(&[0u8; 0x1000]);
        let mmap = map_dma_fd(&fd, 0, 0x1000, true).unwrap();
        t.attach_mmap(0x1000, mmap);
        assert_eq!(t.backing_kind(0x1000), Some(BackingKind::Mmap));
        assert_eq!(t.backing_kind(0x1fff), Some(BackingKind::Mmap));
    }

    /// **单表不失步**：`remove_exact` 一次性删 region + 其 backing（含 mmap）；
    /// 旧的两张并行表设计下，"漏删 mmaps" 会留下孤儿映射 —— 合一后类型上
    /// 不可能发生。本测试锁住该不变量。
    #[test]
    fn remove_exact_drops_region_and_backing_atomically() {
        let mut t = DmaTable::default();
        t.insert(DmaRegion {
            addr: 0x2000,
            size: 0x1000,
            readable: true,
            writeable: true,
        })
        .unwrap();
        let fd = memfd_with(&[7u8; 0x1000]);
        t.attach_mmap(0x2000, map_dma_fd(&fd, 0, 0x1000, true).unwrap());
        assert_eq!(t.backing_kind(0x2000), Some(BackingKind::Mmap));
        assert!(t.mmap_read(0x2000, 4).is_some());

        t.remove_exact(0x2000, 0x1000).unwrap();
        // region 与 backing 同时消失：无孤儿 mmap 残留。
        assert_eq!(t.backing_kind(0x2000), None);
        assert!(t.find(0x2000, 4).is_none());
        assert!(t.mmap_read(0x2000, 4).is_none(), "remove 后不应有孤儿 mmap");
    }

    /// `clear`（UNMAP_ALL）丢弃所有 region 及其 backing。
    #[test]
    fn clear_drops_all_backings() {
        let mut t = DmaTable::default();
        t.insert(DmaRegion {
            addr: 0x3000,
            size: 0x1000,
            readable: true,
            writeable: true,
        })
        .unwrap();
        t.attach_mmap(
            0x3000,
            map_dma_fd(&memfd_with(&[1u8; 0x1000]), 0, 0x1000, true).unwrap(),
        );
        assert_eq!(t.backing_kind(0x3000), Some(BackingKind::Mmap));
        t.clear();
        assert!(t.is_empty());
        assert_eq!(t.backing_kind(0x3000), None);
    }
}
