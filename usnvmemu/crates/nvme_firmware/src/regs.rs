// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe 1.4 Controller 寄存器 + state machine。
//!
//! 仅实现 Windows `nvme.sys` 真正 touch 的子集：
//! - CAP / VS / INTMS / INTMC / CC / CSTS / AQA / ASQ / ACQ
//! - SQ0/CQ0 doorbell + SQ1/CQ1 doorbell（1 IO queue pair）
//!
//! 不实现：CMBLOC/CMBSZ（无 CMB）/ BPINFO（无 boot partition）。
//! CC.SHN shutdown notification + CSTS.SHST 已实现（spec § 3.1.4.5，2026-06-10）：
//! CC.SHN=01/10 → flush 所有 NS → CSTS.SHST=complete。

#![allow(dead_code)]

/// BAR0 总大小：必须够装 controller regs (0..0x1000) + doorbell strip
/// （0x1000..0x2000）+ MSI-X table (0x2000..) + MSI-X PBA (0x3000..)。
///
/// **真 QEMU 11 vfio-user guest e2e 修复（2026-06-10）**：guest 的 nvme 驱动要求
/// 至少一个 MSI-X 向量；MSI-X capability 的 table/PBA 必须落在某个 BAR 内（QEMU
/// `vfio_setup_msix` 从 config-space MSI-X cap 解析 table BIR+offset / PBA
/// BIR+offset，并在该 BAR 上**自行 overlay** MSI-X table 内存区——server 不服务
/// table 访问，仅 PBA 读被 forward 给 server）。把 table 放 BAR0 0x2000（独立页，
/// 不与 doorbell strip 0x1000 重叠）、PBA 放 0x3000，故 BAR0 升到 16 KiB（仍是
/// 2 的幂，PCIe BAR 合规）。
pub const BAR0_SIZE: u64 = 16 * 1024;

/// MSI-X table 在 BAR0 内的 byte offset（独立 4 KiB 页，避开 reg/doorbell）。
/// 8 字节对齐（spec 要求）；QEMU overlay 自己的 table region 于此，controller
/// 不服务该区读写。
pub const MSIX_TABLE_BAR0_OFFSET: u64 = 0x2000;

/// MSI-X PBA 在 BAR0 内的 byte offset（再独立一页）。8 字节对齐。QEMU 把 PBA
/// 读 forward 给 server（写丢弃）；controller 的 mmio_read 对 ≥0x1000 的非
/// doorbell 区返 0 = 无 pending（我们直接经 eventfd fire 中断，不靠 PBA 轮询）。
pub const MSIX_PBA_BAR0_OFFSET: u64 = 0x3000;

/// MSI-X capability ID（PCI spec / `PCI_CAP_ID_MSIX`）。
pub const MSIX_CAP_ID: u8 = 0x11;

/// CC.MPS=0 → memory page size = 4 KiB（NVMe 默认）。
pub const NVME_PAGE_SHIFT: u32 = 12;
pub const NVME_PAGE_SIZE: u64 = 1 << NVME_PAGE_SHIFT;

/// MDTS = 5 → 单 cmd 最大 transfer = 2^5 = 32 page = 128 KiB（Phase E
/// 完成 PRP list 后启用）。NVMe 2.0c § 5.17.2.2 MDTS 是 log2 of max
/// data transfer in MPSMIN units。Windows nvme.sys 会按此拆大 IO 为
/// 多 cmd。
///
/// **C1① MDTS 适用矩阵（spec § 5.17.2.2 + § 8.x）—— 全面审计**：
/// MDTS 只约束**在 host 内存与 NVM 间真传数据**的命令；其传输大小对
/// extended-LBA（内联 metadata）**计入 metadata**（separate-buffer 才排除）。
///   - 受 MDTS 约束（已强制）：READ / WRITE（三档 PRP 路径都查）；READ/WRITE 的
///     metadata 格式按 block_bytes(data+meta) 计（C1① 修正，旧版漏算 meta）。
///     COMPARE 也查 MDTS，但仅支持 plain NS（PI/meta NS 直接 INVALID_FIELD），
///     故其 MDTS = N×sector_bytes，无 extended-LBA metadata 计入路径。
///   - ZONE_APPEND：理论受 MDTS（ZASL=0 → 走 MDTS），但教学版单-PRP 限制
///     (≤1 page=4 KiB) 已把它钉在远低于 MDTS 处 → 不会越界，无需额外 gate。
///   - **不受 MDTS**：FLUSH / WRITE_ZEROES / WRITE_UNCORRECTABLE（无 host 数据
///     传输，只动 LBA 范围）、DSM（传 range list 非 LBA 数据）、COPY（设备内拷贝，
///     受 MCL/MSRC/MSSRL 而非 MDTS）、VERIFY（不向 host 传数据）、所有 admin 命令
///     （MDTS 仅 IO 命令；admin 有各自隐含上限）、reservation/zone-mgmt（控制面）。
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
    Bpinfo = 0x40, // 4 bytes — RO，bits 14:0 = BPSZ (boot partition size in 128 KiB)，bit 15..23 reserved, bits 25:24 = BRS (Read Status)，bits 31:26 = ABPID
    /// **Phase Q5** — Boot Partition Read Select (spec § 3.1.16)
    Bprsel = 0x44, // 4 bytes — RW: BPRSZ + BPROF + BPID 选 active boot partition 读
    /// **Phase Q5** — Boot Partition Memory Buffer Location (spec § 3.1.17)
    Bpmbl = 0x48, // 8 bytes — RW: 64-bit guest memory address driver 提供给 controller 写 boot partition content
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
    // **Shutdown Notification (CC.SHN, bits 15:14)** — spec § 3.1.4.5。
    pub const SHN_SHIFT: u32 = 14;
    pub const SHN_MASK: u32 = 0b11 << SHN_SHIFT;
    pub const SHN_NORMAL: u32 = 0; // 00 = not shutting down（field value）
    pub const SHN_NORMAL_SHUTDOWN: u32 = 0b01; // 01 = normal shutdown（field value）
    pub const SHN_ABRUPT_SHUTDOWN: u32 = 0b10; // 10 = abrupt shutdown（field value）
    pub const IOSQES_SHIFT: u32 = 16;
    pub const IOCQES_SHIFT: u32 = 20;
}

/// CSTS (Controller Status) bit layout。
pub mod csts {
    pub const RDY: u32 = 1 << 0;
    pub const CFS: u32 = 1 << 1; // controller fatal status
    // **Shutdown Status (CSTS.SHST, bits 3:2)** — spec § 3.1.4.5（含位移）。
    pub const SHST_MASK: u32 = 0b11 << 2;
    pub const SHST_NORMAL: u32 = 0; // 00 = normal operation
    pub const SHST_OCCURRING: u32 = 0b01 << 2; // 01 = shutdown processing occurring
    pub const SHST_COMPLETE: u32 = 0b10 << 2; // 10 = shutdown processing complete
}

/// CAP (Controller Capabilities) — 64 位 RO，启动期一次构造。
///
/// `max_qe` = 队列深度（单 SQ/CQ 最大 entry 数），∈ [1, 65536]。MQES 字段是
/// **0-based**（存 `max_qe - 1`，≤ 0xFFFF），spec **不要求** 2 的幂。
pub fn build_cap(max_qe: u32) -> u64 {
    debug_assert!((1..=65536).contains(&max_qe), "max_qe 须 ∈ [1, 65536]");
    // Queue size minus 1 (MQES) bits 15:0 — 0-based，最大 65535（= 65536 entries）。
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
