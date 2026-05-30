// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe SQE/CQE layouts + Admin/IO command opcodes + Identify payloads。
//!
//! # Phase A 注：渐进式复用 nvme_spec crate
//!
//! 仓库内 [`nvme_spec`] crate 提供完整 NVMe Base 2.0c + NVM CS 1.0c 定义
//! （200+ 字段的 IdentifyController、35 个 admin opcode、12 个 NVM opcode、
//! 完整 Cap/Cc/Csts bitfield 等）。我们的本 controller 此前手写了一个最
//! 小子集 — 容易踩字段错位 / spec 升级跟不上的 bug。
//!
//! Phase A 选择**渐进**而非一次性大重构：本模块保留原 Sqe/Cqe（与 wire
//! 协议直接 cast 简单）+ 原 admin_opc/nvm_opc/sc 常量（旧 dispatch 还在
//! 用），但**重写 `IdentifyController::build` / `IdentifyNamespace::build`
//! 内部使用 nvme_spec 的完整结构填充**——这样：
//!
//! 1. 200+ Identify 字段全 spec-correct，driver 看到正确 byte offset
//! 2. 通过 [`SpecAdminOpcode`] / [`SpecNvmOpcode`] re-export，可看到
//!    NVMe 2.0 完整 opcode 表（教学价值）
//! 3. Phase C/D 实现具体 opcode 时直接 match `SpecAdminOpcode::FORMAT_NVM`
//!    等 spec 名字，不会手写错。
//!
//! 全部 little-endian。`zerocopy::FromBytes` 让我们从 `Vec<u8>` 直接
//! cast，避免手写 byte-shuffling。

/// nvme_spec NVMe 2.0c 完整 opcode 表 — re-export 让本 crate 内一处
/// 看到所有可能的命令名字。我们目前只实现一部分，其它对应 admin_opc /
/// nvm_opc 中的占位常量 + dispatch_admin/io 中"unsupported"分支。
#[allow(unused_imports)] pub use nvme_spec::AdminOpcode as SpecAdminOpcode;
pub use nvme_spec::IdentifyController as SpecIdentifyController;
pub use nvme_spec::nvm::IdentifyNamespace as SpecIdentifyNamespace;
#[allow(unused_imports)] pub use nvme_spec::nvm::NvmOpcode as SpecNvmOpcode;

use zerocopy::FromBytes;
use zerocopy::FromZeros;
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
    pub fuses: u16,      // 522
    pub fna: u8,         // 524
    /// Volatile Write Cache (offset 525) — bit 0 = "present"。
    /// 设 1 让 driver 主动发 NVM FLUSH (opcode 0x00) 拿持久化承诺，
    /// 我们的 backing file 默认 write-back，靠 FLUSH 触发 sync_all。
    pub vwc: u8,         // 525
    pub _resv3: [u8; 4096 - 526],
}

impl IdentifyController {
    /// 构造 Windows nvme.sys 可成功 enumerate 的最小 Identify Controller。
    #[allow(dead_code)]
    /// **Deprecated** — Phase A 后用 build_v2_bytes (\n    /// nvme_spec 完整 200+ 字段)。保留方法是为了 spec
    /// 学习对照（hand-written vs spec-corrected）。
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
        // VWC bit 0 = 1 → 通告 volatile write cache，driver 主动发 FLUSH。
        // 配合 NVM FLUSH handler 在 controller.rs 调 sync_all() 拿持久化。
        this.vwc = 0x01;
        // ACL / AERL / NPSS 用合理默认
        this.acl = 3;
        this.aerl = 3;
        this
    }

    /// **Phase A 新增**：用 nvme_spec 完整 200+ 字段构造 IdentifyController
    /// 并 serialize 成 4 KiB 字节。教学版本：取代手写最小子集，driver 看
    /// 到 spec-correct byte layout（含 RTD3R/RTD3E/OAES/CTRATT/CNTRLTYPE/
    /// FGUID/HMPRE/SANICAP/ANATT/SUBNQN/IOCCSZ/SGLS 等之前我们 padding
    /// 字段的位置）。
    pub fn build_v2_bytes(vid: u16, ssvid: u16) -> Vec<u8> {

        let mut id = SpecIdentifyController::new_zeroed();
        id.vid = vid;
        id.ssvid = ssvid;
        id.sn = ascii_padded::<20>(b"PCIE-REMOTE-USRSPACE");
        id.mn = ascii_padded::<40>(b"OpenHCL Userspace NVMe v2.0");
        id.fr = ascii_padded::<8>(b"v2.0    ");
        // MDTS = 2 → max transfer = 2^2 * MPSMIN(4 KiB) = 16 KiB；
        // 与现 dual-PRP (≤ 8 KiB) 兼容（driver 自觉拆 ≤ MDTS）；
        // Phase E PRP list 完成后可放宽。
        id.mdts = 2;
        id.cntlid = 1;
        id.ver = NVME_VERSION_2_0;
        id.cntrltype = nvme_spec::ControllerType::IO_CONTROLLER;
        id.acl = 3;
        id.aerl = 3;
        id.frmw = nvme_spec::FirmwareUpdates::new()
            .with_ffsro(true)
            .with_nofs(1);
        // OACS 现阶段全 0（Phase C 实现 Format/FW/Self-Test/Sanitize 等
        // admin opcode 后逐位翻 true）。
        id.oacs = nvme_spec::OptionalAdminCommandSupport::new();
        // SQES/CQES：NVMe spec 固定 SQE=64B (2^6) / CQE=16B (2^4)。
        id.sqes = nvme_spec::QueueEntrySize::new().with_min(6).with_max(6);
        id.cqes = nvme_spec::QueueEntrySize::new().with_min(4).with_max(4);
        id.maxcmd = 64;
        id.nn = 1;
        // ONCS 现阶段全 0（Phase D 实现 DSM/Compare/WriteZeroes/Verify/
        // Reservations 后逐位翻 true）。
        id.oncs = nvme_spec::Oncs::new();
        // VWC.bit0 = present → driver 主动发 NVM FLUSH (opc 0x00) 拿持久化
        // 承诺；我们 FLUSH handler 调 sync_all() 落盘。
        id.vwc = nvme_spec::VolatileWriteCache::new().with_present(true);
        id.as_bytes().to_vec()
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
    #[allow(dead_code)]
    /// **Deprecated** — Phase A 后用 build_v2_bytes。
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

impl IdentifyNamespace {
    /// **Phase A 新增**：用 nvme_spec 完整字段构造 IdentifyNamespace + serialize 4 KiB。
    /// 之前手写版本只 ~8 字段（nsze/ncap/nuse/nsfeat/nlbaf/flbas/lbaf[0]/_resv），
    /// 现在通过 nvme_spec::nvm::IdentifyNamespace 拿到完整 60+ 字段（含 mssrl/mcl/
    /// msrc/anagrpid/nvmsetid/endgid/eui64 等 spec 后续版本字段）。
    pub fn build_v2_bytes(total_lba: u64) -> Vec<u8> {
        let mut ns = SpecIdentifyNamespace::new_zeroed();
        ns.nsze = total_lba;
        ns.ncap = total_lba;
        ns.nuse = 0;
        ns.nsfeat = 0x01.into(); // THINP
        ns.flbas = 0.into();
        ns.nlbaf = 0;
        ns.lbaf[0] = nvme_spec::nvm::Lbaf::new()
            .with_ms(0)
            .with_lbads(9)  // 2^9 = 512 byte sector
            .with_rp(0);
        ns.as_bytes().to_vec()
    }
}

/// NVMe Base 2.0c 版本号常量（用于 IdentifyController.ver + VS register）。
pub const NVME_VERSION_2_0: u32 = 0x0002_0000;

/// 把 src 字节复制成 AsciiString<N>，右侧空格填充。
///
/// NVMe spec 要求 SN/MN/FR 等字段是 left-justified ASCII with trailing
/// spaces；`storage_string::AsciiString::from(arr)` 接受 `[u8; N]`，
/// 这里 wrap 一下方便填可变长字面常量。
fn ascii_padded<const N: usize>(src: &[u8]) -> storage_string::AsciiString<N> {
    let mut arr = [b' '; N];
    let copy_len = src.len().min(N);
    arr[..copy_len].copy_from_slice(&src[..copy_len]);
    storage_string::AsciiString::<N>::from(arr)
}
