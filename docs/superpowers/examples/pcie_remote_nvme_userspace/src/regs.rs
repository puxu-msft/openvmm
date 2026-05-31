// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe 1.4 Controller 寄存器 + state machine。
//!
//! 仅实现 Windows `nvme.sys` 真正 touch 的子集：
//! - CAP / VS / INTMS / INTMC / CC / CSTS / AQA / ASQ / ACQ
//! - SQ0/CQ0 doorbell + SQ1/CQ1 doorbell（1 IO queue pair）
//!
//! 不实现：CMBLOC/CMBSZ（无 CMB）/ BPINFO（无 boot partition）/ CC.SHN
//! shutdown notification 的多级（直接走 CSTS.RDY=0）。

#![allow(dead_code)]

/// BAR0 总大小：必须够装 controller regs (0..0x1000) + doorbell strip
/// （0x1000..），8 KiB 是最小合规且 round 上 PCIe BAR power-of-2 边界值。
pub const BAR0_SIZE: u64 = 8 * 1024;

/// CC.MPS=0 → memory page size = 4 KiB（NVMe 默认）。
pub const NVME_PAGE_SHIFT: u32 = 12;
pub const NVME_PAGE_SIZE: u64 = 1 << NVME_PAGE_SHIFT;

/// MDTS = 5 → 单 cmd 最大 transfer = 2^5 = 32 page = 128 KiB（Phase E
/// 完成 PRP list 后启用）。NVMe 2.0c § 5.17.2.2 MDTS 是 log2 of max
/// data transfer in MPSMIN units。Windows nvme.sys 会按此拆大 IO 为
/// 多 cmd。
pub const MDTS_PAGES_LOG2: u8 = 5;
pub const MDTS_MAX_BYTES: u64 = NVME_PAGE_SIZE << MDTS_PAGES_LOG2 as u64;

/// Admin / IO SQE / CQE 大小（NVMe spec 1.4 固定）。
pub const SQE_BYTES: u64 = 64;
pub const CQE_BYTES: u64 = 16;

/// Controller 寄存器 byte offset（NVMe spec 1.4 § 3.1）。
#[repr(u64)]
pub enum Reg {
    Cap = 0x00,   // 8 bytes
    Vs = 0x08,    // 4 bytes
    Intms = 0x0c, // 4 bytes
    Intmc = 0x10, // 4 bytes
    Cc = 0x14,    // 4 bytes
    /// 0x18 reserved
    Csts = 0x1c, // 4 bytes
    /// 0x20 NSSR
    Aqa = 0x24, // 4 bytes
    Asq = 0x28,   // 8 bytes
    Acq = 0x30,   // 8 bytes
    /// **Phase L3** — Controller Memory Buffer Location (spec § 3.1.13)
    Cmbloc = 0x38, // 4 bytes — RO, 0 = no CMB
    /// **Phase L3** — CMB Size (spec § 3.1.14)
    Cmbsz = 0x3c, // 4 bytes — RO, 0 = no CMB
    /// **Phase L3** — Boot Partition Information (spec § 3.1.15)
    Bpinfo = 0x40, // 4 bytes — RO, 0 = no boot partition
    /// **Phase L3** — Persistent Memory Region Capabilities (spec § 3.1.27)
    Pmrcap = 0xe00, // 4 bytes — RO, 0 = no PMR
    /// **Phase L3** — PMR Control
    Pmrctl = 0xe04, // 4 bytes — RW (writes ignored when PMR=0)
    /// **Phase L3** — PMR Status
    Pmrsts = 0xe08, // 4 bytes — RO, 0 = no error
}

/// CC (Controller Configuration) bit layout。
pub mod cc {
    pub const EN: u32 = 1 << 0;
    pub const CSS_NVM: u32 = 0; // bits 6:4, 000 = NVM command set
    pub const MPS_SHIFT: u32 = 7;
    pub const MPS_MASK: u32 = 0xf << MPS_SHIFT; // bits 10:7
    pub const AMS_RR: u32 = 0; // bits 13:11, Round Robin
    pub const SHN_NORMAL: u32 = 0; // bits 15:14, 00 = not shutting down
    pub const IOSQES_SHIFT: u32 = 16;
    pub const IOCQES_SHIFT: u32 = 20;
}

/// CSTS (Controller Status) bit layout。
pub mod csts {
    pub const RDY: u32 = 1 << 0;
    pub const CFS: u32 = 1 << 1; // controller fatal status
    pub const SHST_NORMAL: u32 = 0; // bits 3:2
}

/// CAP (Controller Capabilities) — 64 位 RO，启动期一次构造。
pub fn build_cap(max_qe: u16) -> u64 {
    // Queue size minus 1 (MQES) bits 15:0 — 最大 SQ/CQ 大小，单位 entry。
    // 这里 max_qe 已是 "max entry count"，存储用 max_qe-1。
    let mqes_minus_1 = (max_qe - 1) as u64;
    // bit 16 = CQR (Contiguous Queues Required); 强制 1 让 driver 只用
    // 一段连续物理内存（也就是 PRP1 指 entry[0]），简化 host 实现。
    let cqr = 1u64 << 16;
    // bits 18:17 = AMS (Arbitration Mechanism Supported), 00 = RR only.
    // bits 23:19 = reserved.
    // bits 31:24 = TO (Timeout，单位 500ms)，写 10 = 5s ready timeout。
    let to = 10u64 << 24;
    // bits 35:32 = DSTRD (Doorbell stride)，0 = 4 bytes。
    let dstrd = 0u64;
    // bit 36 = NSSRS, 0 = no NVM subsystem reset.
    // bits 44:37 = CSS (Command Set Supported), bit 37 (CSS_NVM) = 1.
    let css = 1u64 << 37;
    // bits 47:45 reserved.
    // bits 51:48 = MPSMIN, 0 = 4 KiB.
    // bits 55:52 = MPSMAX, 0 = 4 KiB.
    mqes_minus_1 | cqr | to | dstrd | css
}

/// VS (Version) — NVMe 1.4 = 0x00010400。
pub const VS_NVME_1_4: u32 = 0x0001_0400;

/// 内部 controller 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlState {
    /// 启动后 / CC.EN=0：所有 IO/admin 操作拒绝。
    Disabled,
    /// CC.EN 刚 0→1：admin queue 已初始化，accept admin commands。
    Ready,
}

/// 单个 Submission Queue 的运行时状态。
#[derive(Debug, Clone)]
pub struct SubmissionQueue {
    /// guest 物理地址（PRP1 形式，连续）。
    pub base_gpa: u64,
    /// 槽位数（spec entry count = AQA.ASQS+1 / Create IO SQ size+1）。
    pub size: u32,
    /// host 已 fetch 到下个槽（== 下一个 fetch 的 entry index）。
    pub head: u32,
    /// 最近收到的 doorbell tail（driver 写到 SQyTDBL）。
    pub tail: u32,
    /// 关联 CQ ID（admin SQ → CQ0；IO SQ 由 Create IO SQ 指定）。
    pub cq_id: u16,
}

/// 单个 Completion Queue 的运行时状态。
#[derive(Debug, Clone)]
pub struct CompletionQueue {
    pub base_gpa: u64,
    pub size: u32,
    /// host 下一个 post 的 slot（写完后递增 + flip phase）。
    pub tail: u32,
    /// 当前 phase tag（CQE.P bit）。每绕一圈翻转。
    pub phase: u8,
    /// driver 最近写入的 head doorbell（host 用来感知 driver 已处理多少）。
    pub head: u32,
    /// MSI-X vector index（Create IO CQ 时 driver 指定；admin CQ = 0）。
    pub interrupt_vector: u16,
    /// IV 中断是否使能（Create IO CQ 的 IEN bit）。
    pub interrupt_enabled: bool,
    /// **Phase M1b** — 自上次 fire interrupt 起累积的未通知 CQE 数；当
    /// 达到 controller-wide AGGR_THR + 1 时立即 fire；否则 tick 检 time。
    pub pending_completions: u32,
    /// 上次 fire interrupt 的时刻（None = 从未 fire 或刚 fire）。
    pub last_fire: Option<std::time::Instant>,
}
