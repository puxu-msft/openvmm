// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase U1** — vfio-user wire 协议数据结构 + 编解码。
//!
//! 完整 spec 参考：`docs/superpowers/specs/2026-06-04-vfio-user-wire-reference.md`。
//!
//! 设计要点：
//! - 所有 on-wire struct 用 `#[repr(C, packed)]` + zerocopy `FromBytes/IntoBytes` 实现
//!   byte-exact 序列化无 unsafe。
//! - 字节序：host-endian（x86_64/aarch64-LE 实际就是 LE）。
//! - 命令号 / flag 位严格按 libvfio-user `include/vfio-user.h` 定义。

use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// **review M3** — vfio-user wire 是 host-endian；x86_64/aarch64 LE 永远可行，
/// BE 主机我们直接 compile error，避免给读者埋"我可能是 LE 也可能是 BE"
/// 的悬念，更不会让 client 看到字段乱序。
#[cfg(target_endian = "big")]
compile_error!(
    "pcie_vfio_user_sdk 假设 host-endian = little-endian；BE 主机请改用专门的 byte-swap 路径"
);

/// 16-byte common header byte 长度。
pub const HEADER_LEN: usize = 16;

/// 单条消息最大字节数 (8 MiB)，防止 hostile peer 用 `msg_size=u32::MAX`
/// 触发 OOM。vfio-user spec 没规定上限，但实践中：
/// - REGION/DMA RW 单次 ≤ 1 MiB（`max_data_xfer_size` 默认）
/// - capability blob 一般 < 64 KiB
///
/// 8 MiB 留 8× headroom，超出直接 `ProtoError::TooLong` 拒绝。
pub const MAX_MSG_SIZE: usize = 8 * 1024 * 1024;

/// vfio-user 协议版本（写死 0.1，与当前 libvfio-user 一致）。
pub const PROTOCOL_MAJOR: u16 = 0;
/// vfio-user 协议版本 minor。
pub const PROTOCOL_MINOR: u16 = 1;

// ─── flags 位 ────────────────────────────────────────────────────────────

/// flags bits 0..3 — 消息类型 mask。
pub const F_TYPE_MASK: u32 = 0x0f;
/// flags type = COMMAND（请求方向）。
pub const F_TYPE_COMMAND: u32 = 0x00;
/// flags type = REPLY（响应方向）。
pub const F_TYPE_REPLY: u32 = 0x01;
/// flags bit 4 — command 发起方不需 reply。
pub const F_NO_REPLY: u32 = 0x10;
/// flags bit 5 — reply 表示该 cmd 失败；payload 为空，`error_no` 字段携带 UNIX errno。
pub const F_ERROR: u32 = 0x20;

// ─── 命令号 ──────────────────────────────────────────────────────────────

/// vfio-user 命令号（来自 `include/vfio-user.h`，权威）。
///
/// 注意：spec `.rst` 中的口述顺序与 header 编号 *不一致* — 以本 enum 为准。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum Command {
    /// 1: VERSION 握手（双向）。
    Version = 1,
    /// 2: DMA_MAP — client 通告 guest RAM region（C→S）。
    DmaMap = 2,
    /// 3: DMA_UNMAP — client 撤销 RAM region（C→S）。
    DmaUnmap = 3,
    /// 4: DEVICE_GET_INFO（C→S）。
    DeviceGetInfo = 4,
    /// 5: DEVICE_GET_REGION_INFO — 查询某 region 的 size/flags（C→S）。
    DeviceGetRegionInfo = 5,
    /// 6: DEVICE_GET_REGION_IO_FDS — 我们不实现（C→S）。
    DeviceGetRegionIoFds = 6,
    /// 7: DEVICE_GET_IRQ_INFO（C→S）。
    DeviceGetIrqInfo = 7,
    /// 8: DEVICE_SET_IRQS — client 配置 MSI-X eventfd 等（C→S）。
    DeviceSetIrqs = 8,
    /// 9: REGION_READ — guest MMIO 读（C→S）。
    RegionRead = 9,
    /// 10: REGION_WRITE — guest MMIO 写（C→S）。
    RegionWrite = 10,
    /// 11: DMA_READ — controller 主动从 guest mem 读（**S→C**）。
    DmaRead = 11,
    /// 12: DMA_WRITE — controller 主动写 guest mem（**S→C**）。
    DmaWrite = 12,
    /// 13: DEVICE_RESET — PCIe FLR 等（C→S）。
    DeviceReset = 13,
}

impl Command {
    /// 从 wire u16 解；未知命令返 `None`。保留是为了 caller pattern-match
    /// 友好（不用 catch error）；新代码推荐 `Command::try_from(v)?`。
    pub fn from_u16(v: u16) -> Option<Self> {
        Self::try_from(v).ok()
    }
}

impl TryFrom<u16> for Command {
    type Error = ProtoError;
    fn try_from(v: u16) -> Result<Self, Self::Error> {
        Ok(match v {
            1 => Self::Version,
            2 => Self::DmaMap,
            3 => Self::DmaUnmap,
            4 => Self::DeviceGetInfo,
            5 => Self::DeviceGetRegionInfo,
            6 => Self::DeviceGetRegionIoFds,
            7 => Self::DeviceGetIrqInfo,
            8 => Self::DeviceSetIrqs,
            9 => Self::RegionRead,
            10 => Self::RegionWrite,
            11 => Self::DmaRead,
            12 => Self::DmaWrite,
            13 => Self::DeviceReset,
            _ => return Err(ProtoError::UnknownCommand(v)),
        })
    }
}

// ─── 通用 Header ─────────────────────────────────────────────────────────

/// 16-byte 通用 message header（host-endian / LE on x86）。
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Default)]
#[repr(C, packed)]
pub struct Header {
    /// 消息 ID。两方向各自独立编号；server 自选 server-initiated 的 id。
    pub msg_id: u16,
    /// 命令号，对应 [`Command`]。
    pub cmd: u16,
    /// 总字节数（含 header）。
    pub msg_size: u32,
    /// 见 [`HeaderFlags`]。
    pub flags: u32,
    /// reply error errno；0 在 command 或成功 reply。
    pub error_no: u32,
}

/// header.flags 字段的语义包装；不入 wire 直接序列化，只做 host-side 操作便利。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderFlags(pub u32);

impl HeaderFlags {
    /// 构造 command 方向，无 NO_REPLY 无 ERROR。
    pub fn command() -> Self {
        Self(F_TYPE_COMMAND)
    }
    /// 构造 reply 方向，成功。
    pub fn reply_ok() -> Self {
        Self(F_TYPE_REPLY)
    }
    /// 构造 reply 方向，错误（client 看到 ERROR bit + error_no）。
    pub fn reply_err() -> Self {
        Self(F_TYPE_REPLY | F_ERROR)
    }
    /// 是 reply？
    pub fn is_reply(&self) -> bool {
        self.0 & F_TYPE_MASK == F_TYPE_REPLY
    }
    /// 是 command？
    pub fn is_command(&self) -> bool {
        self.0 & F_TYPE_MASK == F_TYPE_COMMAND
    }
    /// 是 error reply？
    pub fn is_error(&self) -> bool {
        self.0 & F_ERROR != 0
    }
    /// 标 NO_REPLY（仅 command 有效）。
    pub fn with_no_reply(mut self) -> Self {
        self.0 |= F_NO_REPLY;
        self
    }
    /// 取出 NO_REPLY bit。
    pub fn no_reply(&self) -> bool {
        self.0 & F_NO_REPLY != 0
    }
}

impl Header {
    /// 构造 command header。`payload_len` 是 header 之后的字节数。
    pub fn command(msg_id: u16, cmd: Command, payload_len: u32) -> Self {
        Self {
            msg_id,
            cmd: cmd as u16,
            msg_size: HEADER_LEN as u32 + payload_len,
            flags: HeaderFlags::command().0,
            error_no: 0,
        }
    }
    /// 构造成功 reply header。
    pub fn reply_ok(msg_id: u16, cmd: Command, payload_len: u32) -> Self {
        Self {
            msg_id,
            cmd: cmd as u16,
            msg_size: HEADER_LEN as u32 + payload_len,
            flags: HeaderFlags::reply_ok().0,
            error_no: 0,
        }
    }
    /// 构造错误 reply header（无 payload，errno 填）。
    pub fn reply_err(msg_id: u16, cmd: Command, errno: u32) -> Self {
        Self {
            msg_id,
            cmd: cmd as u16,
            msg_size: HEADER_LEN as u32,
            flags: HeaderFlags::reply_err().0,
            error_no: errno,
        }
    }
    /// 取 payload 字节数（msg_size - HEADER_LEN）。
    pub fn payload_len(&self) -> u32 {
        self.msg_size.saturating_sub(HEADER_LEN as u32)
    }
    /// 取 flags 的语义包装。
    pub fn flags(&self) -> HeaderFlags {
        HeaderFlags(self.flags)
    }
}

// ─── 各 command payload struct ──────────────────────────────────────────

/// VERSION (1) — major/minor + 后续 JSON capabilities blob。
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Default)]
#[repr(C, packed)]
pub struct VersionPayload {
    /// 协议主版本。当前 0。
    pub major: u16,
    /// 协议次版本。当前 1。
    pub minor: u16,
}

/// DMA_MAP (2) — client 通告一段 guest RAM region。
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Default)]
#[repr(C, packed)]
pub struct DmaMapPayload {
    /// struct 大小（spec 兼容字段）。
    pub argsz: u32,
    /// bit0 = READABLE, bit1 = WRITEABLE。
    pub flags: u32,
    /// 若附带 fd：fd 内 offset；否则 0。
    pub offset: u64,
    /// IOVA（guest physical address）。
    pub addr: u64,
    /// region 字节数。
    pub size: u64,
}

/// DMA_UNMAP (3)。
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Default)]
#[repr(C, packed)]
pub struct DmaUnmapPayload {
    /// struct 大小。
    pub argsz: u32,
    /// bit0 = GET_DIRTY_BITMAP, bit1 = UNMAP_ALL。
    pub flags: u32,
    /// 起始 IOVA。
    pub addr: u64,
    /// 字节数。
    pub size: u64,
}

/// DEVICE_GET_INFO (4) — req/reply 共用同 struct。
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Default)]
#[repr(C, packed)]
pub struct DeviceInfoPayload {
    /// struct 大小。
    pub argsz: u32,
    /// bit0=RESET, bit1=PCI。
    pub flags: u32,
    /// region 数。
    pub num_regions: u32,
    /// IRQ 数。
    pub num_irqs: u32,
}

/// DEVICE_GET_REGION_INFO (5) — req/reply 共用同 struct。
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Default)]
#[repr(C, packed)]
pub struct RegionInfoPayload {
    /// struct 大小。
    pub argsz: u32,
    /// REGION_FLAG_* (READ/WRITE/MMAP/CAPS)。
    pub flags: u32,
    /// region index（0..PCI_NUM_REGIONS）。
    pub index: u32,
    /// 第一个 capability 在本 struct 中的 byte offset；0 = 无 caps。
    pub cap_offset: u32,
    /// region 字节数。
    pub size: u64,
    /// mmap 基址 offset（FLAG_MMAP 时有效）。
    pub offset: u64,
}

/// DEVICE_GET_IRQ_INFO (7)。
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Default)]
#[repr(C, packed)]
pub struct IrqInfoPayload {
    /// struct 大小。
    pub argsz: u32,
    /// IRQ_INFO_* (EVENTFD/MASKABLE/AUTOMASKED/NORESIZE)。
    pub flags: u32,
    /// IRQ type index（0..PCI_NUM_IRQS）。
    pub index: u32,
    /// 向量数。
    pub count: u32,
}

/// DEVICE_SET_IRQS (8) — header；后续 data 形态视 flags 而定（NONE/BOOL/EVENTFD）。
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Default)]
#[repr(C, packed)]
pub struct IrqSetPayload {
    /// 总 payload 字节数（含 trailing data）。
    pub argsz: u32,
    /// IRQ_SET_DATA_* | IRQ_SET_ACTION_*。
    pub flags: u32,
    /// IRQ type（如 MSIX=2）。
    pub index: u32,
    /// 起始向量。
    pub start: u32,
    /// 向量数。
    pub count: u32,
}

/// REGION_READ (9) / REGION_WRITE (10) 共用 header。
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Default)]
#[repr(C, packed)]
pub struct RegionAccessPayload {
    /// 区域内 offset。
    pub offset: u64,
    /// region index。
    pub region: u32,
    /// 字节数。
    pub count: u32,
}

/// DMA_READ (11) / DMA_WRITE (12) 共用 header。
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Default)]
#[repr(C, packed)]
pub struct DmaRwHdrPayload {
    /// IOVA。
    pub addr: u64,
    /// 字节数。
    pub count: u64,
}

// ─── 区域 / IRQ flag 常量 ────────────────────────────────────────────────

/// region flag bits（GET_REGION_INFO.flags）。
pub mod region_flags {
    /// bit0 — 可读。
    pub const READ: u32 = 0x1;
    /// bit1 — 可写。
    pub const WRITE: u32 = 0x2;
    /// bit2 — 支持 mmap（fd 经 SCM_RIGHTS 附）。
    pub const MMAP: u32 = 0x4;
    /// bit3 — 有 capability list。
    pub const CAPS: u32 = 0x8;
}

/// PCI region index。
pub mod pci_region {
    /// BAR0。
    pub const BAR0: u32 = 0;
    /// BAR5。
    pub const BAR5: u32 = 5;
    /// expansion ROM。
    pub const ROM: u32 = 6;
    /// PCI config space。
    pub const CONFIG: u32 = 7;
    /// VGA。
    pub const VGA: u32 = 8;
    /// 总 region 数。
    pub const NUM_REGIONS: u32 = 9;
}

/// PCI IRQ type index。
pub mod pci_irq {
    /// INTx legacy。
    pub const INTX: u32 = 0;
    /// MSI。
    pub const MSI: u32 = 1;
    /// MSI-X。
    pub const MSIX: u32 = 2;
    /// 错误中断。
    pub const ERR: u32 = 3;
    /// 设备 request。
    pub const REQ: u32 = 4;
    /// 总数。
    pub const NUM_IRQS: u32 = 5;
}

/// IRQ_SET flags（DEVICE_SET_IRQS.flags）。
pub mod irq_set {
    /// 无 data。
    pub const DATA_NONE: u32 = 0x01;
    /// data = count 个 bool。
    pub const DATA_BOOL: u32 = 0x02;
    /// data 空，count 个 eventfd 经 SCM_RIGHTS。
    pub const DATA_EVENTFD: u32 = 0x04;
    /// 掩盖。
    pub const ACTION_MASK: u32 = 0x08;
    /// 取消掩盖。
    pub const ACTION_UNMASK: u32 = 0x10;
    /// (de)assign trigger / 立刻触发。
    pub const ACTION_TRIGGER: u32 = 0x20;
}

/// IRQ_INFO flags（GET_IRQ_INFO.flags）。
pub mod irq_info {
    /// 支持 eventfd 触发。
    pub const EVENTFD: u32 = 0x1;
}

/// DEVICE_INFO flags（GET_DEVICE_INFO.flags）。
pub mod device_flags {
    /// 支持 reset。
    pub const RESET: u32 = 0x1;
    /// PCI 设备。
    pub const PCI: u32 = 0x2;
}

/// DMA_MAP.flags 位（**review M1** 补）。
pub mod dma_map_flags {
    /// bit0 — region 可读。
    pub const READABLE: u32 = 0x1;
    /// bit1 — region 可写。
    pub const WRITEABLE: u32 = 0x2;
}

/// DMA_UNMAP.flags 位（**review M1** 补）。
pub mod dma_unmap_flags {
    /// bit0 — 顺便取 dirty bitmap。
    pub const GET_DIRTY_BITMAP: u32 = 0x1;
    /// bit1 — 撤销所有映射。
    pub const UNMAP_ALL: u32 = 0x2;
}

// ─── 错误类型 ──────────────────────────────────────────────────────────

/// proto 层错误。
#[derive(Debug, Error)]
pub enum ProtoError {
    /// 短包（msg_size < HEADER_LEN）或缓冲区不足。
    #[error("message too short: msg_size={msg_size}, need at least {HEADER_LEN}")]
    TooShort {
        /// 报告的 msg_size 字段值。
        msg_size: u32,
    },
    /// **review H1** — `msg_size` 超过 [`MAX_MSG_SIZE`]，可能是 hostile peer 试图触发 OOM。
    #[error("message too long: msg_size={msg_size} > MAX_MSG_SIZE={MAX_MSG_SIZE}")]
    TooLong {
        /// 报告的 msg_size 字段值。
        msg_size: u32,
    },
    /// **review H1** — `msg_size` 比可用 buffer 长度还大；caller buffer 短读。
    #[error("buffer shorter than msg_size: msg_size={msg_size}, buf_len={buf_len}")]
    BufferShort {
        /// header 报告的 msg_size。
        msg_size: u32,
        /// 实际可用 buffer 字节数。
        buf_len: u32,
    },
    /// 未知 command 值。
    #[error("unknown command: {0}")]
    UnknownCommand(u16),
    /// payload 长度与 struct 不匹配。
    #[error("payload length mismatch: got {got}, expected {want}")]
    PayloadLen {
        /// 实际收到的 payload 字节数。
        got: usize,
        /// 该 cmd 期望的字节数。
        want: usize,
    },
    /// JSON 解析失败（VERSION caps）。
    #[error("invalid JSON capabilities: {0}")]
    BadJson(String),
}

// ─── 编解码 helper ──────────────────────────────────────────────────────

/// 把 Header + payload 序列化到 `out` (extend，不清空)；返回写入字节数。
pub fn encode_msg<T: IntoBytes + Immutable>(hdr: &Header, payload: &T, out: &mut Vec<u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(hdr.as_bytes());
    out.extend_from_slice(payload.as_bytes());
    out.len() - start
}

/// 同 [`encode_msg`] 但 payload 直接是字节切片（用于 RegionWrite/DmaRead reply 等场景）。
pub fn encode_msg_bytes(hdr: &Header, payload: &[u8], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(hdr.as_bytes());
    out.extend_from_slice(payload);
    out.len() - start
}

/// 仅 header 的消息（DEVICE_RESET / 错误 reply）。
pub fn encode_header_only(hdr: &Header, out: &mut Vec<u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(hdr.as_bytes());
    out.len() - start
}

/// 从 `buf` 头部解 Header；剩余字节即 payload 待 caller 继续解析。
///
/// **review H1/H2** — 严格校验：
/// 1. `buf.len() >= HEADER_LEN`（够 header 字节数）
/// 2. `msg_size >= HEADER_LEN`（合法消息至少含 header）
/// 3. `msg_size <= MAX_MSG_SIZE`（DoS 上限）
///
/// **注意**：本函数 *不* 校验 `msg_size <= buf.len()` —— framing path 中
/// `read_message` 把 header 与 payload 分两次读，调本函数时 buf 只有 16 字节。
/// 若 caller 一次性持有整个消息字节（例如 fixture / unit test），需自行
/// 校验，可调 [`validate_msg_in_buffer`] 完成对应检查。
pub fn decode_header(buf: &[u8]) -> Result<Header, ProtoError> {
    if buf.len() < HEADER_LEN {
        return Err(ProtoError::TooShort {
            msg_size: buf.len() as u32,
        });
    }
    let hdr = Header::read_from_bytes(&buf[..HEADER_LEN]).map_err(|_| ProtoError::TooShort {
        msg_size: buf.len() as u32,
    })?;
    let msg_size = hdr.msg_size;
    if (msg_size as usize) < HEADER_LEN {
        return Err(ProtoError::TooShort { msg_size });
    }
    if (msg_size as usize) > MAX_MSG_SIZE {
        return Err(ProtoError::TooLong { msg_size });
    }
    Ok(hdr)
}

/// **review H1** — 帮 unit-test 风格 caller 校验"持有完整消息字节"：
/// `decode_header` 自身不校验 `msg_size <= buf.len()`（因为 framing path
/// 分两次读），但 fixture / 单测一次拿全消息时仍应校验，避免后续 slice panic。
pub fn validate_msg_in_buffer(hdr: &Header, buf: &[u8]) -> Result<(), ProtoError> {
    if (hdr.msg_size as usize) > buf.len() {
        return Err(ProtoError::BufferShort {
            msg_size: hdr.msg_size,
            buf_len: buf.len() as u32,
        });
    }
    Ok(())
}

/// 从 `payload` 解一个具体 struct 类型；长度必须 *精确* 匹配。
///
/// 内部 `read_from_bytes` 在长度匹配 + `#[repr(C, packed)]` + `FromBytes` 时
/// 已被证明不会失败（packed 无对齐要求，FromBytes 对任意字节都有效）；
/// 仍写一个 `unreachable!`-style 兜底转 PayloadLen，便于将来 zerocopy 升级
/// 或 struct 加 invariant 时退化为可观察错误而非 panic。
pub fn decode_payload<T: FromBytes + KnownLayout + Immutable + Copy>(
    payload: &[u8],
) -> Result<T, ProtoError> {
    let want = core::mem::size_of::<T>();
    if payload.len() != want {
        return Err(ProtoError::PayloadLen {
            got: payload.len(),
            want,
        });
    }
    T::read_from_bytes(payload).map_err(|_| ProtoError::PayloadLen {
        got: payload.len(),
        want,
    })
}

// ─── 单测 ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Header offset / size 严格 byte-exact（spec 关键不变量）。
    #[test]
    fn header_layout_is_16_bytes_le() {
        assert_eq!(core::mem::size_of::<Header>(), HEADER_LEN);
        let h = Header {
            msg_id: 0x1234,
            cmd: 0x5678,
            msg_size: 0xDEADBEEF,
            flags: 0xCAFE0000 | F_TYPE_REPLY | F_ERROR,
            error_no: 0x11223344,
        };
        let bytes = h.as_bytes();
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(&bytes[0..2], &[0x34, 0x12]); // msg_id LE
        assert_eq!(&bytes[2..4], &[0x78, 0x56]); // cmd LE
        assert_eq!(&bytes[4..8], &[0xEF, 0xBE, 0xAD, 0xDE]); // msg_size LE
        assert_eq!(&bytes[8..12], &[0x21, 0x00, 0xFE, 0xCA]); // flags LE
        assert_eq!(&bytes[12..16], &[0x44, 0x33, 0x22, 0x11]); // error_no LE
    }

    /// Command numeric values 严格按 vfio-user.h。
    #[test]
    fn command_numeric_values_match_header() {
        assert_eq!(Command::Version as u16, 1);
        assert_eq!(Command::DmaMap as u16, 2);
        assert_eq!(Command::DmaUnmap as u16, 3);
        assert_eq!(Command::DeviceGetInfo as u16, 4);
        assert_eq!(Command::DeviceGetRegionInfo as u16, 5);
        assert_eq!(Command::DeviceGetRegionIoFds as u16, 6);
        assert_eq!(Command::DeviceGetIrqInfo as u16, 7);
        assert_eq!(Command::DeviceSetIrqs as u16, 8);
        assert_eq!(Command::RegionRead as u16, 9);
        assert_eq!(Command::RegionWrite as u16, 10);
        assert_eq!(Command::DmaRead as u16, 11);
        assert_eq!(Command::DmaWrite as u16, 12);
        assert_eq!(Command::DeviceReset as u16, 13);
    }

    /// Command roundtrip + 未知值返 None。
    #[test]
    fn command_from_u16_roundtrip() {
        for cmd_u in [1u16, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13] {
            let cmd = Command::from_u16(cmd_u).expect("known cmd");
            assert_eq!(cmd as u16, cmd_u);
        }
        assert_eq!(Command::from_u16(0), None);
        assert_eq!(Command::from_u16(14), None);
        assert_eq!(Command::from_u16(0xFFFF), None);
    }

    /// HeaderFlags helper 严格反映 spec bit 含义。
    #[test]
    fn header_flags_semantics() {
        let c = HeaderFlags::command();
        assert!(c.is_command() && !c.is_reply() && !c.is_error() && !c.no_reply());

        let r = HeaderFlags::reply_ok();
        assert!(r.is_reply() && !r.is_command() && !r.is_error());

        let e = HeaderFlags::reply_err();
        assert!(e.is_reply() && e.is_error());

        let cn = HeaderFlags::command().with_no_reply();
        assert!(cn.is_command() && cn.no_reply());
    }

    /// `Header::command` 自动算 msg_size = HEADER_LEN + payload_len。
    #[test]
    fn header_command_builder_sets_msg_size() {
        let h = Header::command(0x42, Command::Version, 4);
        let msg_size = h.msg_size;
        let msg_id = h.msg_id;
        let cmd = h.cmd;
        let flags = h.flags;
        let error_no = h.error_no;
        assert_eq!(msg_size, HEADER_LEN as u32 + 4);
        assert_eq!(msg_id, 0x42);
        assert_eq!(cmd, 1);
        assert_eq!(flags, F_TYPE_COMMAND);
        assert_eq!(error_no, 0);
        assert_eq!(h.payload_len(), 4);
        assert!(h.flags().is_command());
    }

    /// 错误 reply 没有 payload，errno 透传。
    #[test]
    fn header_reply_err_carries_errno() {
        let h = Header::reply_err(7, Command::RegionRead, /* EINVAL */ 22);
        let msg_size = h.msg_size;
        let payload_len = h.payload_len();
        let error_no = h.error_no;
        assert_eq!(msg_size, HEADER_LEN as u32);
        assert_eq!(payload_len, 0);
        assert_eq!(error_no, 22);
        assert!(h.flags().is_reply() && h.flags().is_error());
    }

    /// Payload struct 大小（关键不变量，spec 验证用）。
    #[test]
    fn payload_struct_sizes() {
        assert_eq!(core::mem::size_of::<VersionPayload>(), 4);
        assert_eq!(core::mem::size_of::<DmaMapPayload>(), 32);
        assert_eq!(core::mem::size_of::<DmaUnmapPayload>(), 24);
        assert_eq!(core::mem::size_of::<DeviceInfoPayload>(), 16);
        assert_eq!(core::mem::size_of::<RegionInfoPayload>(), 32);
        assert_eq!(core::mem::size_of::<IrqInfoPayload>(), 16);
        assert_eq!(core::mem::size_of::<IrqSetPayload>(), 20);
        assert_eq!(core::mem::size_of::<RegionAccessPayload>(), 16);
        assert_eq!(core::mem::size_of::<DmaRwHdrPayload>(), 16);
    }

    /// encode → decode header roundtrip。
    #[test]
    fn encode_decode_header_only_roundtrip() {
        let h = Header::reply_err(99, Command::DeviceReset, 5);
        let mut buf = Vec::new();
        let n = encode_header_only(&h, &mut buf);
        assert_eq!(n, HEADER_LEN);
        assert_eq!(buf.len(), HEADER_LEN);
        let parsed = decode_header(&buf).unwrap();
        let msg_id = parsed.msg_id;
        let cmd = parsed.cmd;
        let error_no = parsed.error_no;
        assert_eq!(msg_id, 99);
        assert_eq!(cmd, Command::DeviceReset as u16);
        assert_eq!(error_no, 5);
        assert!(parsed.flags().is_error());
    }

    /// encode_msg + decode_header + decode_payload 全链路 roundtrip。
    #[test]
    fn encode_decode_with_payload_roundtrip() {
        let h = Header::command(0x77, Command::DeviceGetInfo, 16);
        let pl = DeviceInfoPayload {
            argsz: 16,
            flags: device_flags::PCI | device_flags::RESET,
            num_regions: pci_region::NUM_REGIONS,
            num_irqs: pci_irq::NUM_IRQS,
        };
        let mut buf = Vec::new();
        encode_msg(&h, &pl, &mut buf);
        assert_eq!(buf.len(), HEADER_LEN + 16);
        let phdr = decode_header(&buf).unwrap();
        assert_eq!(phdr.payload_len(), 16);
        let parsed: DeviceInfoPayload =
            decode_payload(&buf[HEADER_LEN..HEADER_LEN + phdr.payload_len() as usize]).unwrap();
        let parsed_flags = parsed.flags;
        let parsed_nr = parsed.num_regions;
        let parsed_ni = parsed.num_irqs;
        assert_eq!(parsed_flags, device_flags::PCI | device_flags::RESET);
        assert_eq!(parsed_nr, 9);
        assert_eq!(parsed_ni, 5);
    }

    /// decode_header 拒短包。
    #[test]
    fn decode_header_rejects_short_buf() {
        let short = [0u8; 8];
        let r = decode_header(&short);
        assert!(matches!(r, Err(ProtoError::TooShort { .. })));
    }

    /// decode_header 拒 msg_size < HEADER_LEN（内部不变量）。
    #[test]
    fn decode_header_rejects_bad_msg_size() {
        let h = Header {
            msg_id: 0,
            cmd: 1,
            msg_size: 4, // < HEADER_LEN
            flags: 0,
            error_no: 0,
        };
        let mut buf = vec![0u8; HEADER_LEN];
        buf.copy_from_slice(h.as_bytes());
        let r = decode_header(&buf);
        assert!(matches!(r, Err(ProtoError::TooShort { msg_size: 4 })));
    }

    /// decode_payload 长度不匹配返 PayloadLen。
    #[test]
    fn decode_payload_strict_length() {
        let too_long = vec![0u8; 17];
        let r: Result<DeviceInfoPayload, _> = decode_payload(&too_long);
        assert!(matches!(
            r,
            Err(ProtoError::PayloadLen { got: 17, want: 16 })
        ));
        let too_short = vec![0u8; 15];
        let r: Result<DeviceInfoPayload, _> = decode_payload(&too_short);
        assert!(matches!(
            r,
            Err(ProtoError::PayloadLen { got: 15, want: 16 })
        ));
    }

    /// **review H1** — decode_header 拒 msg_size > MAX_MSG_SIZE。
    #[test]
    fn decode_header_rejects_oversized_msg() {
        let h = Header {
            msg_id: 0,
            cmd: 1,
            msg_size: (MAX_MSG_SIZE as u32) + 1,
            flags: 0,
            error_no: 0,
        };
        let mut buf = vec![0u8; HEADER_LEN];
        buf.copy_from_slice(h.as_bytes());
        let r = decode_header(&buf);
        assert!(matches!(r, Err(ProtoError::TooLong { .. })));
    }

    /// **review H1** — `validate_msg_in_buffer` 拒 msg_size > buf.len()（合法
    /// 但 caller buffer 不足）。`decode_header` 不再做这检查，框架层 split-read
    /// 不会被误报，仍能让单测/fixture 走 explicit validate path。
    #[test]
    fn validate_msg_in_buffer_rejects_short_buffer() {
        let h = Header::command(0, Command::DeviceReset, 32); // 16 + 32 = 48
        let mut buf = vec![0u8; HEADER_LEN]; // 只 header，不含 payload
        buf.copy_from_slice(h.as_bytes());
        let hdr = decode_header(&buf).unwrap(); // 注意：decode_header 现在不再检 buf.len
        let r = validate_msg_in_buffer(&hdr, &buf);
        assert!(matches!(
            r,
            Err(ProtoError::BufferShort {
                msg_size: 48,
                buf_len: 16,
            })
        ));
        // 同等大 buffer 应通过
        let big = vec![0u8; 48];
        assert!(validate_msg_in_buffer(&hdr, &big).is_ok());
    }

    /// **review M2** — TryFrom 返 UnknownCommand 而非 None。
    #[test]
    fn command_tryfrom_error_path() {
        let r: Result<Command, _> = 0u16.try_into();
        assert!(matches!(r, Err(ProtoError::UnknownCommand(0))));
        let r: Result<Command, _> = 99u16.try_into();
        assert!(matches!(r, Err(ProtoError::UnknownCommand(99))));
        let r: Result<Command, _> = 1u16.try_into();
        assert_eq!(r.unwrap(), Command::Version);
    }

    /// **review M1** — DMA flag 常量数值。
    #[test]
    fn dma_flag_constants() {
        assert_eq!(dma_map_flags::READABLE, 0x1);
        assert_eq!(dma_map_flags::WRITEABLE, 0x2);
        assert_eq!(dma_unmap_flags::GET_DIRTY_BITMAP, 0x1);
        assert_eq!(dma_unmap_flags::UNMAP_ALL, 0x2);
    }
}
