// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe SQE/CQE layouts + Admin/IO command opcodes + Identify payloads。
//!
//! 全部 little-endian。`zerocopy::FromBytes` 让我们从 `Vec<u8>` 直接
//! cast，避免手写 byte-shuffling。

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// 结构尺寸编译期断言（NVMe spec 固定）。
const _: () = assert!(core::mem::size_of::<Sqe>() == 64, "Sqe must be 64 bytes");
const _: () = assert!(core::mem::size_of::<Cqe>() == 16, "Cqe must be 16 bytes");
const _: () = assert!(
    core::mem::size_of::<IdentifyController>() == 4096,
    "IdentifyController must be 4096 bytes"
);
const _: () = assert!(
    core::mem::size_of::<IdentifyNamespace>() == 4096,
    "IdentifyNamespace must be 4096 bytes"
);

/// Admin command opcodes (NVMe spec 1.4 § 5)。
#[allow(dead_code)] // 完整 opcode table 留供未来扩展
pub mod admin_opc {
    pub const DELETE_IO_SQ: u8 = 0x00;
    pub const CREATE_IO_SQ: u8 = 0x01;
    pub const GET_LOG_PAGE: u8 = 0x02;
    pub const DELETE_IO_CQ: u8 = 0x04;
    pub const CREATE_IO_CQ: u8 = 0x05;
    pub const IDENTIFY: u8 = 0x06;
    pub const ABORT: u8 = 0x08;
    pub const SET_FEATURES: u8 = 0x09;
    pub const GET_FEATURES: u8 = 0x0a;
    pub const ASYNC_EVENT_REQUEST: u8 = 0x0c;
    pub const KEEP_ALIVE: u8 = 0x18;
}

/// NVM (IO) command opcodes (NVMe spec 1.4 NVM § 6)。
pub mod nvm_opc {
    pub const FLUSH: u8 = 0x00;
    pub const WRITE: u8 = 0x01;
    pub const READ: u8 = 0x02;
}

/// CQE.SC (Status Code) — Generic Command Status (NVMe spec 1.4 § 4.6.1.2.1).
#[allow(dead_code)]
pub mod sc {
    pub const SUCCESS: u8 = 0x00;
    pub const INVALID_OPCODE: u8 = 0x01;
    pub const INVALID_FIELD: u8 = 0x02;
    pub const DATA_TRANSFER_ERROR: u8 = 0x04;
    pub const INTERNAL_ERROR: u8 = 0x06;
    /// LBA Out of Range (NVM CSD)
    pub const LBA_OUT_OF_RANGE: u8 = 0x80;
}

/// Submission Queue Entry — 64 bytes 固定。
///
/// 仅 cast 用；字段含义需结合 opcode 解读。
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct Sqe {
    /// CDW0: bits 7:0 = opcode, bits 9:8 = FUSE, bits 15:14 = PSDT,
    /// bits 31:16 = CID (Command Identifier)。
    pub cdw0: u32,
    pub nsid: u32,
    pub cdw2: u32,
    pub cdw3: u32,
    pub mptr: u64,
    pub prp1: u64,
    pub prp2: u64,
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}

impl Sqe {
    pub fn opcode(&self) -> u8 {
        (self.cdw0 & 0xff) as u8
    }
    pub fn cid(&self) -> u16 {
        (self.cdw0 >> 16) as u16
    }
}

/// Completion Queue Entry — 16 bytes 固定。
///
/// SQID/SQ_HEAD/CID 共 32 位 (DW2)；DW3 含 phase bit + status code。
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Default)]
pub struct Cqe {
    pub cdw0: u32,
    pub cdw1: u32,
    /// bits 15:0 = SQ head pointer (next SQ entry host will fetch)
    /// bits 31:16 = SQ identifier
    pub dw2: u32,
    /// bits 15:0 = command identifier
    /// bit 16 = phase tag (P)
    /// bits 31:17 = status field (SF) — SC/SCT/CRD/M/DNR
    pub dw3: u32,
}

impl Cqe {
    /// 构造一个 SUCCESS CQE（最常见路径）。
    pub fn success(cid: u16, sq_id: u16, sq_head: u16, phase: u8) -> Self {
        let dw2 = ((sq_id as u32) << 16) | sq_head as u32;
        let dw3 = (cid as u32) | ((phase as u32 & 1) << 16); // SC=0 = success
        Cqe { cdw0: 0, cdw1: 0, dw2, dw3 }
    }

    /// 构造一个 ERROR CQE，sc=status code, sct=status code type。
    pub fn error(cid: u16, sq_id: u16, sq_head: u16, phase: u8, sc: u8, sct: u8) -> Self {
        let dw2 = ((sq_id as u32) << 16) | sq_head as u32;
        // SF layout: bits 17..=24 SC, bits 25..=27 SCT, bit 28 CRD, bit 29 M (more), bit 30 DNR
        let sf = ((sc as u32) << 1) | ((sct as u32 & 0x7) << 9);
        let dw3 = (cid as u32) | ((phase as u32 & 1) << 16) | (sf << 16);
        Cqe { cdw0: 0, cdw1: 0, dw2, dw3 }
    }
}

/// Identify Controller data structure (NVMe spec 1.4 § 5.15.2.2)
/// 共 4096 字节。这里只填 Windows nvme.sys enumeration 必需字段；
/// 其余清零。
#[repr(C, packed)]
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct IdentifyController {
    pub vid: u16,        // 0
    pub ssvid: u16,      // 2
    pub sn: [u8; 20],    // 4
    pub mn: [u8; 40],    // 24
    pub fr: [u8; 8],     // 64
    pub rab: u8,         // 72
    pub ieee: [u8; 3],   // 73
    pub cmic: u8,        // 76
    pub mdts: u8,        // 77
    pub cntlid: u16,     // 78
    pub ver: u32,        // 80
    pub _resv1: [u8; 256 - 84], // pad to 256
    // Admin command set attributes
    pub oacs: u16,       // 256
    pub acl: u8,         // 258
    pub aerl: u8,        // 259
    pub frmw: u8,        // 260
    pub lpa: u8,         // 261
    pub elpe: u8,        // 262
    pub npss: u8,        // 263
    pub avscc: u8,       // 264
    pub apsta: u8,       // 265
    pub _resv2: [u8; 512 - 266],
    // NVM command set attributes
    pub sqes: u8,        // 512
    pub cqes: u8,        // 513
    pub maxcmd: u16,     // 514
    pub nn: u32,         // 516
    pub oncs: u16,       // 520
    pub _resv3: [u8; 4096 - 522],
}

impl IdentifyController {
    /// 构造 Windows nvme.sys 可成功 enumerate 的最小 Identify Controller。
    pub fn build(vid: u16, ssvid: u16) -> Self {        let mut this: Self = zerocopy::FromZeros::new_zeroed();
        this.vid = vid;
        this.ssvid = ssvid;
        // Serial Number "PCIE-REMOTE-USRSPACE" 左对齐 ASCII，padding = ' '。
        let sn = b"PCIE-REMOTE-USRSPACE";
        this.sn[..sn.len()].copy_from_slice(sn);
        for b in &mut this.sn[sn.len()..] {
            *b = b' ';
        }
        // Model Number
        let mn = b"OpenHCL Userspace NVMe v1               ";
        this.mn.copy_from_slice(mn);
        // Firmware Revision
        let fr = b"v1.0    ";
        this.fr.copy_from_slice(fr);
        this.mdts = super::regs::MDTS_PAGES_LOG2;
        this.cntlid = 1;
        this.ver = super::regs::VS_NVME_1_4;
        // SQES/CQES：bits 3:0 min, 7:4 max。NVMe spec 固定 SQE=64=2^6 / CQE=16=2^4。
        this.sqes = 0x66; // min=2^6=64, max=2^6=64
        this.cqes = 0x44; // min=2^4=16, max=2^4=16
        this.maxcmd = 64;
        this.nn = 1; // 1 namespace
        this.oncs = 0; // optional commands: none
        // ACL / AERL / NPSS 用合理默认
        this.acl = 3;
        this.aerl = 3;
        this
    }
}

/// Identify Namespace data structure (NVMe spec 1.4 § 5.15.2.1) 4 KiB。
#[repr(C, packed)]
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct IdentifyNamespace {
    pub nsze: u64,    // 0  Namespace Size (total LBAs)
    pub ncap: u64,    // 8  Namespace Capacity
    pub nuse: u64,    // 16 Namespace Utilization
    pub nsfeat: u8,   // 24
    pub nlbaf: u8,    // 25 number of LBA formats supported (0 = 1 format)
    pub flbas: u8,    // 26 formatted LBA size index
    pub mc: u8,       // 27 metadata caps
    pub dpc: u8,      // 28
    pub dps: u8,      // 29
    pub nmic: u8,     // 30
    pub rescap: u8,   // 31
    pub fpi: u8,      // 32
    pub dlfeat: u8,   // 33
    pub nawun: u16,   // 34
    pub nawupf: u16,  // 36
    pub nacwu: u16,   // 38
    pub _resv1: [u8; 128 - 40],
    /// LBAF[16]：每个 4 字节。LBAF[0].lbads = log2(sector size)。
    /// 整个数组共 64 字节 (4*16)。
    pub lbaf: [u32; 16],
    pub _resv2: [u8; 4096 - 128 - 64],
}

impl IdentifyNamespace {
    /// 构造一个总容量 = `total_lba * 512` 的 namespace（512B sectors）。
    pub fn build(total_lba: u64) -> Self {
        let mut this: Self = zerocopy::FromZeros::new_zeroed();
        this.nsze = total_lba;
        this.ncap = total_lba;
        // NUSE = 0：spec § 5.15.2.1 此字段为"现实际已使用 LBA 数"，
        // 我们的 backing file 上没追踪 sparse 使用，所以保守 = 0。
        // 之前误填 total_lba 会让 Windows 误以为整盘已满 → Initialize-Disk
        // 拒绝（return code 40004 "no media"）。
        this.nuse = 0;
        this.flbas = 0; // use LBAF[0]
        this.nlbaf = 0; // 1 format
        // LBAF[0]: MS=0, LBADS=9 (2^9 = 512), RP=0
        // word layout: bits 0:15 MS, bits 16:23 LBADS, bits 24:25 RP
        this.lbaf[0] = 9u32 << 16;
        // NSFEAT bit 0 = THINP (thin provisioning) 让 driver 接受 NUSE=0
        // 与 NCAP 不等。
        this.nsfeat = 0x01;
        this
    }
}
