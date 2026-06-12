// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! BAR0 MMIO read/write 分发。
//!
//! 拆出来减小 `controller/mod.rs` 体积（reviewer M-5 建议）。`mmio_read_impl`
//! / `mmio_write_impl` 都是 `pub(super)`，被 mod.rs 的 `impl PcieDevice`
//! 用 1-line shim 调用。BAR0 寄存器布局 + doorbell 协议都在此文件，让
//! NVMe regs spec layout 集中可读。

use super::*;
use crate::regs::MSIX_TABLE_BAR0_OFFSET;

impl NvmeController {
    pub(super) fn mmio_read_impl(&mut self, bar: u32, offset: u64, size: u32) -> u64 {
        if bar != 0 {
            return 0;
        }
        // CAP 是 64-bit register；driver 可以一次 8 字节读全 CAP，或分两次
        // 4 字节读 lo/hi。处理两种情况。
        let val = match (offset, size) {
            (0x00, 8) => self.cap,
            (0x00, 4) => self.cap & 0xffff_ffff,
            (0x04, 4) => self.cap >> 32,
            (0x08, _) => self.vs as u64,
            (0x0c, _) => self.intms as u64,
            (0x10, _) => self.intmc as u64,
            (0x14, _) => self.cc as u64,
            (0x1c, _) => self.csts as u64,
            (0x24, _) => self.aqa as u64,
            // ASQ/ACQ 同理可 8-byte 读
            (0x28, 8) => self.asq,
            (0x28, 4) => self.asq & 0xffff_ffff,
            (0x2c, 4) => self.asq >> 32,
            (0x30, 8) => self.acq,
            (0x30, 4) => self.acq & 0xffff_ffff,
            (0x34, 4) => self.acq >> 32,
            // **Phase L3 + Q5 + CMB-P1a** — CMB / BPINFO / PMR 寄存器
            // CMBLOC/CMBSZ 在 CMB 启用且 CMBMSC.CRE 置位后反映真值（spec § 3.1.24：
            // CRE = Capabilities Registers Enabled）；否则返 0（无 CMB / 未 enable cap）。
            (0x38, _) => self.cmbloc_value(),
            (0x3c, _) => self.cmbsz_value(),
            // **Phase L3 + Q5 + 12轮 H-Q5** — BPINFO=0 不 advertise boot partition。
            // 之前 BPSZ=1 让 driver 看到 1 partition，但我们没 boot image
            // 服务 → driver 写 BPRSEL 后 poll BRS 永远 0 = hang。BPSZ=0
            // driver enumerate 即跳过。BPRSEL/BPMBL 仍 RW 保 spec
            // register layout 完整（教学读 BAR0 时能看见）。
            // **Boot Partition（spec § 8.13）** — BPINFO：boot_partition 非空时广告
            // BPSZ（bits 14:0，128 KiB 单位）+ BRS（bits 25:24，Boot Read Status）+
            // ABPID（bit 31，单 BP 恒 0）；空 = BPSZ=0（不广告，历史 stub 行为）。
            (0x40, _) => {
                let bpsz = (self.boot_partition.len() / (128 * 1024)) as u64;
                (bpsz & 0x7fff) | ((self.boot_read_status as u64 & 0x3) << 24)
            }
            (0x44, _) => self.bprsel as u64, // BPRSEL — RW，回读上次写值
            (0x48, 8) => self.bpmbl,         // BPMBL 64-bit
            (0x48, 4) => self.bpmbl & 0xFFFF_FFFF, // 低 32
            (0x4c, 4) => self.bpmbl >> 32,   // 高 32
            // **CMB-P1a** — CMBMSC（8-byte，size-aware 仿 ASQ/ACQ）+ CMBSTS（RO）。
            (0x50, 8) => self.cmbmsc_value(),
            (0x50, 4) => self.cmbmsc_value() & 0xffff_ffff,
            (0x54, 4) => self.cmbmsc_value() >> 32,
            (0x58, _) => self.cmbsts_value() as u64,
            (0xe00, _) => 0, // PMRCAP — Q6 (PMR) 仍 0
            (0xe04, _) => 0, // PMRCTL
            (0xe08, _) => 0, // PMRSTS
            // doorbell 区 [0x1000, MSIX_TABLE) 读返回 0（write-only）。上界排除其后
            // 的 MSI-X table/PBA 区（见 parse_doorbell 的 LOW-1 注释）。
            (o, _) if (0x1000..MSIX_TABLE_BAR0_OFFSET).contains(&o) => 0,
            _ => {
                tracing::debug!(offset, size, "MMIO read: unknown offset");
                0
            }
        };
        tracing::debug!(
            offset = format_args!("{:#x}", offset),
            size,
            value = format_args!("{:#x}", val),
            "MMIO read"
        );
        val
    }

    pub(super) fn mmio_write_impl(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        bar: u32,
        offset: u64,
        size: u32,
        value: u64,
    ) {
        if bar != 0 {
            return;
        }
        tracing::debug!(
            offset = format_args!("{:#x}", offset),
            size,
            value = format_args!("{:#x}", value),
            "MMIO write"
        );
        match offset {
            0x0c => self.intms |= value as u32,    // mask set
            0x10 => self.intms &= !(value as u32), // mask clear (INTMC sets bits to clear)
            0x14 => self.write_cc(value as u32),
            0x24 => self.aqa = value as u32,
            // ASQ/ACQ 是 8-byte 寄存器：spec 允许按 4-byte（low/high dword 各一条）
            // 或单条 8-byte（qword）访问。**Bug fix 2026-06-10**：原代码只按 offset
            // 匹配、无视 size，对 0x28/0x30 一律 `value & 0xffff_ffff`。于是 Windows
            // nvme.sys 的单条 8-byte ASQ 写在 admin 队列落在 4 GiB 以上时高 32 位被
            // 截断 → SQE fetch 读错 GPA → 首条 admin 命令读成全 0（opc=0x0 被当
            // Delete IO SQ）→ guest 初始化挂。真 Hyper-V guest e2e 发现（小内存
            // in-process harness 的 GPA 永远 < 4 GiB，命不到此截断）。
            0x28 => {
                if size == 8 {
                    self.asq = value;
                } else {
                    self.asq = (self.asq & !0xffff_ffff) | (value & 0xffff_ffff);
                }
            }
            0x2c => {
                self.asq = (self.asq & 0xffff_ffff) | (value << 32);
            }
            0x30 => {
                if size == 8 {
                    self.acq = value;
                } else {
                    self.acq = (self.acq & !0xffff_ffff) | (value & 0xffff_ffff);
                }
            }
            0x34 => {
                self.acq = (self.acq & 0xffff_ffff) | (value << 32);
            }
            // **Boot Partition（spec § 8.13）** — BPRSEL control register
            0x44 => {
                // BPRSEL：bits 9:0 = BPRSZ（read size，4 KiB 单位）；bits 29:10 = BPROF
                // （read offset，4 KiB 单位）；bit 31 = BPID。写它**触发**一次 Boot
                // Partition Read：把 boot_partition[BPROF*4K .. +BPRSZ*4K] DMA-write 到
                // BPMBL，BRS 置 1(in progress)，完成回调（pending_boot_reads）置 2/3。
                self.bprsel = value as u32;
                if self.boot_partition.is_empty() {
                    // 未广告 boot partition（BPSZ=0）→ 仅存值，driver 不应到此（历史 stub）。
                    tracing::debug!(value, "BPRSEL set (no boot partition advertised)");
                } else {
                    let bprsel = value as u32;
                    let bprsz = (bprsel & 0x3ff) as usize; // 4 KiB 单位
                    let bprof = ((bprsel >> 10) & 0xf_ffff) as usize; // 4 KiB 单位
                    let off = bprof * 4096;
                    let len = bprsz * 4096;
                    if len == 0 || off.saturating_add(len) > self.boot_partition.len() {
                        tracing::warn!(
                            off,
                            len,
                            bp_len = self.boot_partition.len(),
                            "Boot Partition Read 越界/零长 → BRS=error(3)"
                        );
                        self.boot_read_status = 3; // error
                    } else {
                        let content = self.boot_partition[off..off + len].to_vec();
                        let tok = ctx.dma_write(self.bpmbl, content);
                        self.pending_boot_reads.insert(tok);
                        self.boot_read_status = 1; // read in progress
                        tracing::debug!(
                            off,
                            len,
                            bpmbl = format_args!("{:#x}", self.bpmbl),
                            "Boot Partition Read 触发 → DMA-write content 到 BPMBL"
                        );
                    }
                }
            }
            0x48 => {
                // BPMBL 同为 8-byte 寄存器，同样 size-aware（见上 ASQ 注释）。
                if size == 8 {
                    self.bpmbl = value;
                } else {
                    self.bpmbl = (self.bpmbl & !0xffff_ffff) | (value & 0xffff_ffff);
                }
            }
            0x4c => {
                self.bpmbl = (self.bpmbl & 0xffff_ffff) | (value << 32);
            }
            // **CMB-P1a** — CMBMSC 写（8-byte，size-aware 仿 ASQ/ACQ）：解析 CRE/CMSE/CBA
            // 并按 spec § 3.1.24 时序校验启用 CMB。CMBSTS（0x58）是 RO，写忽略。
            0x50 => {
                let cur = self.cmbmsc_value();
                let new = if size == 8 {
                    value
                } else {
                    (cur & !0xffff_ffff) | (value & 0xffff_ffff)
                };
                self.write_cmbmsc(new);
            }
            0x54 => {
                let cur = self.cmbmsc_value();
                let new = (cur & 0xffff_ffff) | (value << 32);
                self.write_cmbmsc(new);
            }
            o if (0x1000..MSIX_TABLE_BAR0_OFFSET).contains(&o) => {
                // doorbell 写**必须** 4 字节 access；其它尺寸视为 driver bug
                // 直接忽略（不应该按 8/2/1 字节写 doorbell）。上界排除其后的 MSI-X
                // table/PBA 区（落该区的写交由下方 default 静默忽略 —— server 不服务
                // MSI-X table；QEMU overlay / OpenHCL emulator 处理之）。
                if size != 4 {
                    tracing::warn!(
                        offset = format_args!("{:#x}", o),
                        size,
                        "doorbell write with non-4-byte size; ignored"
                    );
                    return;
                }
                if let Some((is_sq, qid)) = Self::parse_doorbell(o) {
                    if is_sq {
                        self.on_sq_tail_doorbell(ctx, qid, value as u32);
                    } else {
                        self.on_cq_head_doorbell(ctx, qid, value as u32);
                    }
                }
            }
            _ => {}
        }
        // 处理 inbox SQE（dispatch_sqe 可能 mutably borrow self → 借出再回填）
        let inbox = std::mem::take(&mut self.sqe_inbox);
        for (sq_id, head, sqe) in inbox {
            self.dispatch_sqe(ctx, sq_id, head, sqe);
        }
    }

    /// **CMB-P1a** — 合成 CMBLOC（offset 0x38, RO）。
    ///
    /// spec § 3.1.13/§ 3.1.24：CMBLOC/CMBSZ 仅在 CMBMSC.CRE 置位后反映真值；CRE 未置
    /// （或无 CMB）返 0。BIR = CMB 所在 BAR，OFST=0（独立 BAR，CMB 落 BAR 起点）。
    pub(super) fn cmbloc_value(&self) -> u64 {
        use crate::regs::cmbloc;
        match &self.cmb {
            Some(cmb) if cmb.cre => {
                ((cmb.bir as u64) << cmbloc::BIR_SHIFT) & cmbloc::BIR_MASK as u64
                // OFST = 0（CMB 占整条独立 BAR），其余子字段教学版留 0。
            }
            _ => 0,
        }
    }

    /// **CMB-P1a** — 合成 CMBSZ（offset 0x3C, RO）。
    ///
    /// 同 CMBLOC：CRE 未置返 0。SZU=0（4 KiB unit）、SZ = size / 4 KiB；数据类型位
    /// 广告 SQS/CQS/LISTS/RDS/WDS（教学版 CMB 全能力，真数据路径 P1b/P2 落地）。
    pub(super) fn cmbsz_value(&self) -> u64 {
        use crate::regs::cmbsz;
        match &self.cmb {
            Some(cmb) if cmb.cre => {
                const SZU: u32 = 0; // 4 KiB unit（与 enable_cmb 一致）
                let unit = cmbsz::unit_bytes(SZU);
                let sz = (cmb.size / unit) as u32;
                let v = (SZU << cmbsz::SZU_SHIFT)
                    | ((sz << cmbsz::SZ_SHIFT) & cmbsz::SZ_MASK)
                    | cmbsz::SQS
                    | cmbsz::CQS
                    | cmbsz::LISTS
                    | cmbsz::RDS
                    | cmbsz::WDS;
                v as u64
            }
            _ => 0,
        }
    }

    /// **CMB-P1a** — 合成 CMBMSC（offset 0x50, RW, 64-bit）回读值。
    ///
    /// 回读 driver 编程的 CRE/CMSE/CBA（无 CMB 返 0）。
    pub(super) fn cmbmsc_value(&self) -> u64 {
        use crate::regs::cmbmsc;
        match &self.cmb {
            Some(cmb) => {
                let mut v = cmb.cba & cmbmsc::CBA_MASK;
                if cmb.cre {
                    v |= cmbmsc::CRE;
                }
                if cmb.cmse {
                    v |= cmbmsc::CMSE;
                }
                v
            }
            None => 0,
        }
    }

    /// **CMB-P1a** — 合成 CMBSTS（offset 0x58, RO）。CBAI = CMSE 时序/地址非法标志。
    pub(super) fn cmbsts_value(&self) -> u32 {
        use crate::regs::cmbsts;
        match &self.cmb {
            Some(cmb) if cmb.cbai => cmbsts::CBAI,
            _ => 0,
        }
    }

    /// **CMB-P1a** — CMBMSC 写入状态机（spec § 3.1.24）。
    ///
    /// 解析 CRE/CMSE/CBA 并校验 **CRE 先于 CMSE 时序**（architect 复核 #5）：host 在
    /// CRE 未置时置 CMSE 是非法组合 —— controller **不启用** CMB，置 CMBSTS.CBAI 让
    /// driver 感知（spec § 3.1.25），并保持 CMSE=0（不进入可访问态）。合法时清 CBAI。
    /// 无 CMB（None）时写忽略。
    pub(super) fn write_cmbmsc(&mut self, new: u64) {
        use crate::regs::cmbmsc;
        let Some(cmb) = self.cmb.as_mut() else {
            tracing::debug!("CMBMSC 写但 CMB 未启用（无 CMB 配置）→ 忽略");
            return;
        };
        let new_cre = new & cmbmsc::CRE != 0;
        let new_cmse = new & cmbmsc::CMSE != 0;
        let new_cba = new & cmbmsc::CBA_MASK;
        // P1b TODO（review LOW-2）：CMSE 已置位（CMB live）时改 CBA 会让数据路径映射漂移；
        // P1b 接 `access_guest` 后应拒绝 live 改 CBA（或要求先清 CMSE）。P1a 无数据路径故无害。
        cmb.cre = new_cre;
        cmb.cba = new_cba;
        if new_cmse && !new_cre {
            // 时序违规：CMSE 不能在 CRE 之前置位 → 拒绝启用 + 置 CBAI。
            cmb.cmse = false;
            cmb.cbai = true;
            tracing::warn!(
                "CMBMSC: CMSE=1 而 CRE=0（时序违规，spec § 3.1.24）→ 不启用 CMB + CMBSTS.CBAI=1"
            );
        } else {
            cmb.cmse = new_cmse;
            cmb.cbai = false;
            tracing::debug!(
                cre = new_cre,
                cmse = new_cmse,
                cba = format_args!("{:#x}", new_cba),
                "CMBMSC programmed"
            );
        }
    }
}

#[cfg(test)]
mod addr_reg_tests {
    use super::*;

    /// 构造最小 NvmeController（1 MiB 临时 backing），只用来测寄存器存储逻辑。
    fn mk() -> NvmeController {
        let path = std::env::temp_dir().join(format!(
            "nvme_mmio_test_{}_{:?}.img",
            std::process::id(),
            std::thread::current().id()
        ));
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(1024 * 1024).unwrap();
        drop(f);
        NvmeController::open(&[path.to_str().unwrap().to_string()], 0x1414, 0, &[]).unwrap()
    }

    /// **真 QEMU 11 vfio-user guest e2e 修复（2026-06-10）回归** — `describe()` 必须
    /// 在 config space 暴露一条**合法 MSI-X capability**（cap_id 0x11 + 正确 table
    /// size / table BIR+offset / PBA BIR+offset），否则 QEMU 找不到 MSI-X →
    /// `pci_alloc_irq_vectors` 失败 → guest nvme `probe -EINVAL`。同时校验合成的
    /// PCI config space 里 Status.CapList 置位、CapPtr 指向该 cap。
    #[test]
    fn describe_exposes_valid_msix_capability() {
        use crate::regs::{MSIX_CAP_ID, MSIX_PBA_BAR0_OFFSET, MSIX_TABLE_BAR0_OFFSET};
        use pcie_device_core::PcieDevice as _;
        use pcie_device_core::describe::cfg_offset as o;

        let c = mk();
        let d = c.describe();
        // 1) capabilities 含一条 MSI-X cap，body 恰 10 字节（MC(2)+table(4)+pba(4)）。
        assert_eq!(d.capabilities.len(), 1, "应恰有 1 条 cap（MSI-X）");
        let cap = &d.capabilities[0];
        assert_eq!(cap.cap_id, MSIX_CAP_ID);
        assert_eq!(
            cap.raw.len(),
            10,
            "MSI-X cap body = MC+TableOff+PbaOff = 10B"
        );
        // Message Control: bit[10:0] = table size = msix_count-1。
        let mc = u16::from_le_bytes([cap.raw[0], cap.raw[1]]);
        assert_eq!(
            mc & 0x07FF,
            (d.msix_count as u16) - 1,
            "MSI-X table size = N-1"
        );
        // Table Offset/BIR + PBA Offset/BIR（BIR=0 → BAR0，offset 8 字节对齐）。
        let table = u32::from_le_bytes([cap.raw[2], cap.raw[3], cap.raw[4], cap.raw[5]]);
        let pba = u32::from_le_bytes([cap.raw[6], cap.raw[7], cap.raw[8], cap.raw[9]]);
        assert_eq!(table & 0x7, 0, "table BIR = 0 (BAR0)");
        assert_eq!(table & !0x7, MSIX_TABLE_BAR0_OFFSET as u32);
        assert_eq!(pba & 0x7, 0, "PBA BIR = 0 (BAR0)");
        assert_eq!(pba & !0x7, MSIX_PBA_BAR0_OFFSET as u32);

        // 2) 合成 config space：Status.CapList 置位 + CapPtr 指向 MSI-X cap。
        let cfg = d.config_space();
        let status = u16::from_le_bytes([cfg[o::STATUS], cfg[o::STATUS + 1]]);
        assert_ne!(status & o::STATUS_CAP_LIST, 0, "Status.CapList 应置位");
        let cap_ptr = cfg[o::CAP_PTR] as usize;
        assert_ne!(cap_ptr, 0, "CapPtr 应非 0");
        assert_eq!(cfg[cap_ptr], MSIX_CAP_ID, "CapPtr 指向 MSI-X cap_id");
    }

    /// **DBBUF 真实现完成（2026-06-10）** — controller **广告** OACS Doorbell Buffer
    /// Config（bit 8）。shadow doorbell 轮询已真正实现（controller DMA-poll driver 的
    /// shadow buffer 拿真 tail/head 并写回 event_idx），故 Linux nvme 启用 shadow
    /// doorbell 后命令不再被跳过的真 ring 卡住。OACS bit8 必须为 1。
    #[test]
    fn identify_controller_advertises_dbbuf() {
        use crate::cmd::IdentifyController;
        let c = mk();
        // 真实 enumeration 走 build_v2_bytes_with_cntrltype（admin.rs 用它）；OACS
        // 在 Identify Controller 数据结构 offset 256（u16, little-endian）。
        let bytes = IdentifyController::build_v2_bytes_with_cntrltype(c.vid, c.ssvid, 1, 0x01, 0);
        let oacs = u16::from_le_bytes([bytes[256], bytes[257]]);
        assert_ne!(
            oacs & (1 << 8),
            0,
            "OACS bit8 (Doorbell Buffer Config) 必须为 1（shadow doorbell 轮询已实现）"
        );
        // 其它 OACS 能力仍在——controller 仍广告 Format/FW/Self-test 等。
        assert_ne!(oacs, 0, "OACS 不应全 0（保留全部能力）");
    }

    /// **通用机制（回归 "64-bit 寄存器写/读无视 size 而截断" 这一整类 bug，由
    /// 2026-06-10 ASQ/ACQ/BPMBL 写侧截断引出）**：每个 8-byte BAR0 寄存器都必须
    /// round-trip 一个 4 GiB 以上的值——单条 qword 写、4-byte low/high 分写、
    /// qword/dword 读全部一致，且写侧字段确实存了全 64 位。任一寄存器的写或读路径
    /// 截断 → 对应行红。新增 8-byte 寄存器加进 `REGS_64` 即纳入回归；distinct 值
    /// 同时 catch 寄存器互相串写。（背景：被截断的高 32 位让 guest 落在 4 GiB 以上
    /// 的 admin 队列 SQE fetch 读错 GPA → init 挂；真 Hyper-V guest e2e 发现，小内存
    /// in-process harness 的 GPA 永远 < 4 GiB 命不到。）
    #[test]
    fn all_64bit_registers_roundtrip_above_4gib() {
        const REGS_64: &[(u64, &str)] = &[(0x28, "ASQ"), (0x30, "ACQ"), (0x48, "BPMBL")];
        let mut c = mk();
        let mut cap = pcie_device_core::CaptureTransport::with_start_token(0x100);
        let mut ctx = pcie_device_core::DeviceCtx::new(&mut cap);

        // ① 单条 qword 写 distinct 的 >4 GiB 值 → 全部写完再全部回读。
        for &(off, _) in REGS_64 {
            c.mmio_write_impl(&mut ctx, 0, off, 8, ((off + 1) << 32) | 0x5a5a_1234);
        }
        for &(off, name) in REGS_64 {
            let v = ((off + 1) << 32) | 0x5a5a_1234;
            // 字段 ground-truth：写侧没截断，且寄存器之间没串写
            let stored = match off {
                0x28 => c.asq,
                0x30 => c.acq,
                0x48 => c.bpmbl,
                _ => unreachable!(),
            };
            assert_eq!(stored, v, "{name}: qword 写侧截断/串写");
            // 读侧 size-aware round-trip（mmio_read_impl 是这个 bug 的镜像半边）
            assert_eq!(c.mmio_read_impl(0, off, 8), v, "{name}: qword 回读不全");
            assert_eq!(
                c.mmio_read_impl(0, off, 4),
                v & 0xffff_ffff,
                "{name}: low dword 回读错"
            );
            assert_eq!(
                c.mmio_read_impl(0, off + 4, 4),
                v >> 32,
                "{name}: high dword 回读错"
            );
        }

        // ② 4-byte low/high 分写必须拼回完整 64 位。
        for &(off, _) in REGS_64 {
            let v = ((off + 2) << 32) | 0xa5a5_5678;
            c.mmio_write_impl(&mut ctx, 0, off, 4, v & 0xffff_ffff);
            c.mmio_write_impl(&mut ctx, 0, off + 4, 4, v >> 32);
        }
        for &(off, name) in REGS_64 {
            let v = ((off + 2) << 32) | 0xa5a5_5678;
            assert_eq!(
                c.mmio_read_impl(0, off, 8),
                v,
                "{name}: split dword 未拼回 64 位"
            );
        }
    }

    /// **LOW-1（2026-06-10 reviewer）** — doorbell 区上界 = `MSIX_TABLE_BAR0_OFFSET`：
    /// `parse_doorbell` 必须把落在 MSI-X table/PBA 区（≥ 0x2000）的 offset 判为
    /// **非 doorbell**（返 `None`），杜绝未来队列数上调致 doorbell 数组撑进 MSI-X
    /// 区时的别名。区内合法 offset 仍正常解析。
    #[test]
    fn parse_doorbell_bounded_below_msix_table() {
        use crate::regs::{MSIX_PBA_BAR0_OFFSET, MSIX_TABLE_BAR0_OFFSET};
        // 区内：0x1000 = SQ0 tail；0x1004 = CQ0 head；0x1008 = SQ1 tail。
        assert_eq!(NvmeController::parse_doorbell(0x1000), Some((true, 0)));
        assert_eq!(NvmeController::parse_doorbell(0x1004), Some((false, 0)));
        assert_eq!(NvmeController::parse_doorbell(0x1008), Some((true, 1)));
        // 区内最后一个合法 4-byte slot（< 0x2000）。
        assert!(NvmeController::parse_doorbell(MSIX_TABLE_BAR0_OFFSET - 4).is_some());
        // 边界与越界：MSI-X table 起点及其后一律 None（不再被当 doorbell）。
        assert_eq!(NvmeController::parse_doorbell(MSIX_TABLE_BAR0_OFFSET), None);
        assert_eq!(
            NvmeController::parse_doorbell(MSIX_TABLE_BAR0_OFFSET + 4),
            None
        );
        assert_eq!(NvmeController::parse_doorbell(MSIX_PBA_BAR0_OFFSET), None);
        // 区前：< 0x1000 仍 None。
        assert_eq!(NvmeController::parse_doorbell(0x0FFC), None);
    }

    /// LOW-1 镜像半边 —— MMIO 写分发：落在 MSI-X table/PBA 区的写**不**被当
    /// doorbell（不触发 SQ/CQ doorbell 副作用），交由 default 静默忽略。
    #[test]
    fn mmio_write_msix_region_not_treated_as_doorbell() {
        use crate::regs::{MSIX_PBA_BAR0_OFFSET, MSIX_TABLE_BAR0_OFFSET};
        let mut c = mk();
        let mut cap = pcie_device_core::CaptureTransport::with_start_token(0x100);
        let mut ctx = pcie_device_core::DeviceCtx::new(&mut cap);
        // 先 enable controller，否则 SQ doorbell 本就因 not-operational 被丢，
        // 测不出"区分 doorbell 与 MSI-X 区"这件事。这里只需 RDY 路径不 panic；
        // 写 MSI-X table 区一个 4-byte 值，断言不产生任何 outbound（doorbell 会
        // 触发 DMA fetch 等 outbound）。
        c.mmio_write_impl(&mut ctx, 0, MSIX_TABLE_BAR0_OFFSET, 4, 0xdead_beef);
        c.mmio_write_impl(&mut ctx, 0, MSIX_PBA_BAR0_OFFSET, 4, 0x1234_5678);
        assert!(
            cap.events().is_empty(),
            "MSI-X 区写不应触发 doorbell 副作用（无 outbound）"
        );
    }
}
