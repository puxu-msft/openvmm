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
    /// **Phase Q6/CMB-P1a** — CMB Memory Space Control (spec § 3.1.24)
    Cmbmsc = 0x50, // 8 bytes — RW: CRE(bit0) + CMSE(bit1) + CBA(bits63:12)
    /// **Phase Q6/CMB-P1a** — CMB Status (spec § 3.1.25)
    Cmbsts = 0x58, // 4 bytes — RO: CBAI(bit0) Controller Base Address Invalid
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

/// CAP (Controller Capabilities) 中本实现会用到的位（64-bit RO）。
pub mod cap {
    /// bit 57 = CMBS (Controller Memory Buffer Supported)，spec § 3.1.1。
    pub const CMBS: u64 = 1 << 57;
}

/// CMBSZ (CMB Size, offset 0x3C, RO) 位布局 — spec § 3.1.14（NVMe 2.0）。
///
/// CMB 总大小 = SZ × (CMB size unit)，size unit 由 SZU 编码（4 KiB × 16^SZU）。
/// 低位的 SQS/CQS/LISTS/RDS/WDS 声明"CMB 可承载哪些数据类型"。**字段次序严格按
/// spec 字节布局**（项目纪律 spec-aligned-field-order）。
pub mod cmbsz {
    // NVMe Base Spec § 3.1.14 (CMBSZ, offset 0x3C) 字节布局 —— 字段绝对位置必须
    // 与 spec 一致，否则真 host 驱动按 spec 解码会读错能力位（教训：曾把数据类型位
    // 整体上移 4 位、SZU 错放 3:0，真 Linux nvme 据 SQS=0 拒绝把 SQ 放进 CMB；
    // in-process 测试因两端共用错误常量而自洽通过、未暴露——见 self-consistent trap）。
    /// bit 0 = SQS (Submission Queue Support)：CMB 可放 SQ。
    pub const SQS: u32 = 1 << 0;
    /// bit 1 = CQS (Completion Queue Support)：CMB 可放 CQ。
    pub const CQS: u32 = 1 << 1;
    /// bit 2 = LISTS (PRP/SGL List Support)：CMB 可放 PRP list / SGL segment。
    pub const LISTS: u32 = 1 << 2;
    /// bit 3 = RDS (Read Data Support)：CMB 可作 read 数据源。
    pub const RDS: u32 = 1 << 3;
    /// bit 4 = WDS (Write Data Support)：CMB 可作 write 数据宿。
    pub const WDS: u32 = 1 << 4;
    // bits 7:5 reserved。
    /// bits 11:8 = SZU (Size Units)：0=4 KiB, 1=64 KiB, 2=1 MiB, 3=16 MiB …（每 +1 乘 16）。
    pub const SZU_SHIFT: u32 = 8;
    pub const SZU_MASK: u32 = 0xf << SZU_SHIFT;
    /// bits 31:12 = SZ (Size)：CMB 大小（以 SZU 编码的 size unit 为单位）。
    pub const SZ_SHIFT: u32 = 12;
    pub const SZ_MASK: u32 = 0xf_ffff << SZ_SHIFT;

    /// SZU 编码 → 每个 size unit 的字节数（4 KiB × 16^szu）。
    pub fn unit_bytes(szu: u32) -> u64 {
        (4 * 1024u64) * 16u64.pow(szu)
    }
}

/// CMBLOC (CMB Location, offset 0x38, RO) 位布局 — spec § 3.1.13（NVMe 2.0）。
///
/// 告诉 driver CMB 落在哪个 BAR（BIR）以及在该 BAR 内的偏移（OFST，以 size unit
/// 为粒度）。本教学实现 CMB 用独立 BAR、OFST=0。
pub mod cmbloc {
    /// bits 2:0 = BIR (Base Indicator Register)：CMB 所在 BAR 索引。
    pub const BIR_SHIFT: u32 = 0;
    pub const BIR_MASK: u32 = 0x7 << BIR_SHIFT;
    // bits 11:3 在 NVMe 2.0 含 CQMMS/CQPDS 等子字段（教学版不广告，留 0）。
    /// bits 31:12 = OFST (Offset)：CMB 在 BAR 内的偏移（CMBSZ.SZU 为粒度）。
    pub const OFST_SHIFT: u32 = 12;
    pub const OFST_MASK: u32 = 0xf_ffff << OFST_SHIFT;
}

/// CMBMSC (CMB Memory Space Control, offset 0x50, RW, 64-bit) 位布局 —
/// spec § 3.1.24（NVMe 2.0）。driver 经它**启用** CMB 并编程 Controller Base
/// Address（CBA = CMB 在 guest 地址空间的基址）。
pub mod cmbmsc {
    /// bit 0 = CRE (Capabilities Registers Enabled)：使能 CMBSZ/CMBLOC 反映真值。
    /// spec 要求 **CRE 先于 CMSE**。
    pub const CRE: u64 = 1 << 0;
    /// bit 1 = CMSE (CMB Memory Space Enable)：使能 CMB 内存空间（CBA 生效，CMB
    /// 可被访问）。置位前 CRE 必须已置（否则非法，见 mmio.rs 校验）。
    pub const CMSE: u64 = 1 << 1;
    // bits 11:2 reserved。
    /// bits 63:12 = CBA (Controller Base Address)：CMB 在 guest 地址空间的基址
    /// （4 KiB 对齐，低 12 位隐含 0）。
    pub const CBA_SHIFT: u64 = 12;
    pub const CBA_MASK: u64 = !0xfffu64; // bits 63:12
}

/// CMBSTS (CMB Status, offset 0x58, RO) 位布局 — spec § 3.1.25（NVMe 2.0）。
pub mod cmbsts {
    /// bit 0 = CBAI (Controller Base Address Invalid)：CMSE 置位时 CBA 非法 →
    /// controller 置此位拒绝启用。
    pub const CBAI: u32 = 1 << 0;
}

/// CAP (Controller Capabilities) — 64 位 RO，启动期一次构造。
///
/// `max_qe` = 队列深度（单 SQ/CQ 最大 entry 数），∈ [1, 65536]。MQES 字段是
/// **0-based**（存 `max_qe - 1`，≤ 0xFFFF），spec **不要求** 2 的幂。
///
/// `cmb_supported` = 是否广告 CMB（CAP.CMBS bit57，spec § 3.1.1）。**当且仅当**
/// CMB 启用配置（`--cmb-*` 非 off / `CmbState` 存在）时置位；否则保持 0（与无 CMB
/// 现状一致）。CMBS 只声明"设备支持 CMB"，CMB 的实际可用还需 driver 编程 CMBMSC。
pub fn build_cap(max_qe: u32, cmb_supported: bool) -> u64 {
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
    // bit 56 = PMRS (Persistent Memory Region Supported), 0 = no PMR.
    // **bit 57 = CMBS (Controller Memory Buffer Supported)** — spec § 3.1.1。
    let cmbs = if cmb_supported { cap::CMBS } else { 0 };
    mqes_minus_1 | cqr | to | dstrd | css | cmbs
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

#[cfg(test)]
mod cmb_reg_tests {
    use super::*;

    /// CAP.CMBS（bit57）当且仅当 cmb_supported 时置位；其余 CAP 字段不受影响。
    #[test]
    fn build_cap_sets_cmbs_only_when_supported() {
        let no_cmb = build_cap(128, false);
        let with_cmb = build_cap(128, true);
        assert_eq!(no_cmb & cap::CMBS, 0, "无 CMB 配置不应置 CAP.CMBS");
        assert_ne!(with_cmb & cap::CMBS, 0, "CMB 启用应置 CAP.CMBS（bit57）");
        // CMBS 是唯一差异：清掉 bit57 后两者相等（MQES/CQR/TO/CSS 等不变）。
        assert_eq!(
            no_cmb,
            with_cmb & !cap::CMBS,
            "CMBS 之外的 CAP 字段不应因 cmb_supported 改变"
        );
        // bit57 的绝对位置锚定（spec § 3.1.1）。
        assert_eq!(cap::CMBS, 1u64 << 57);
    }

    /// CMBSZ 编码：数据类型位 SQS/CQS/LISTS/RDS/WDS 在 bits 4:0、SZU 在 bits 11:8、
    /// SZ 在 bits 31:12（NVMe Base Spec § 3.1.14）。绝对位置锚定，防字段次序漂移。
    #[test]
    fn cmbsz_field_offsets_match_spec() {
        use cmbsz::*;
        // 位绝对位置（与 spec 字节布局对齐；同步交叉核对 Linux nvme.h / QEMU hw/nvme）。
        assert_eq!(SQS, 1 << 0);
        assert_eq!(CQS, 1 << 1);
        assert_eq!(LISTS, 1 << 2);
        assert_eq!(RDS, 1 << 3);
        assert_eq!(WDS, 1 << 4);
        assert_eq!(SZU_SHIFT, 8);
        assert_eq!(SZU_MASK, 0xf << 8);
        assert_eq!(SZ_SHIFT, 12);
        assert_eq!(SZ_MASK, 0xf_ffff << 12);
        // 组装一个典型 CMBSZ：SZU=0（4 KiB unit）+ SZ=512（→ 2 MiB）+ RDS+WDS+SQS+CQS+LISTS。
        let szu = 0u32;
        let sz = 512u32; // 512 × 4 KiB = 2 MiB
        let v = (szu << SZU_SHIFT) | (sz << SZ_SHIFT) | RDS | WDS | SQS | CQS | LISTS;
        assert_eq!((v & SZU_MASK) >> SZU_SHIFT, 0);
        assert_eq!((v & SZ_MASK) >> SZ_SHIFT, 512);
        assert_ne!(v & SQS, 0, "SQS 必须置位（真驱动据此决定 cmb_use_sqes）");
        assert_ne!(v & RDS, 0);
        assert_ne!(v & WDS, 0);
        // 整字回归锚：spec 布局下 2 MiB 全能力 CMB == 0x0020_001f（曾误为 0x0020_01f0）。
        assert_eq!(v, 0x0020_001f);
    }

    /// SZU 编码 → size unit 字节数（4 KiB × 16^szu）。
    #[test]
    fn cmbsz_unit_bytes_encoding() {
        assert_eq!(cmbsz::unit_bytes(0), 4 * 1024); // 4 KiB
        assert_eq!(cmbsz::unit_bytes(1), 64 * 1024); // 64 KiB
        assert_eq!(cmbsz::unit_bytes(2), 1024 * 1024); // 1 MiB
        assert_eq!(cmbsz::unit_bytes(3), 16 * 1024 * 1024); // 16 MiB
    }

    /// CMBLOC：BIR 在 bits 2:0、OFST 在 bits 31:12（spec § 3.1.13）。
    #[test]
    fn cmbloc_field_offsets_match_spec() {
        use cmbloc::*;
        assert_eq!(BIR_MASK, 0x7);
        assert_eq!(OFST_SHIFT, 12);
        // BIR=2（独立 BAR2）、OFST=0。
        let v = (2u32 << BIR_SHIFT) | (0u32 << OFST_SHIFT);
        assert_eq!((v & BIR_MASK) >> BIR_SHIFT, 2);
        assert_eq!((v & OFST_MASK) >> OFST_SHIFT, 0);
    }

    /// CMBMSC：CRE bit0、CMSE bit1、CBA bits 63:12（spec § 3.1.24）。
    #[test]
    fn cmbmsc_field_offsets_match_spec() {
        use cmbmsc::*;
        assert_eq!(CRE, 1);
        assert_eq!(CMSE, 1 << 1);
        assert_eq!(CBA_SHIFT, 12);
        assert_eq!(CBA_MASK, !0xfffu64);
        // 组装 CMBMSC：CBA=0x8000_0000、CMSE=1、CRE=1。
        let cba = 0x8000_0000u64;
        let v = (cba & CBA_MASK) | CMSE | CRE;
        assert_ne!(v & CRE, 0);
        assert_ne!(v & CMSE, 0);
        assert_eq!(v & CBA_MASK, cba, "CBA 4 KiB 对齐字段回读一致");
    }

    /// CMBSTS.CBAI bit0（spec § 3.1.25）。
    #[test]
    fn cmbsts_cbai_bit() {
        assert_eq!(cmbsts::CBAI, 1);
    }
}
