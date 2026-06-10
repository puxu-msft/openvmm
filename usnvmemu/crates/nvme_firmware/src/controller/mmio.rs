// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! BAR0 MMIO read/write 分发。
//!
//! 拆出来减小 `controller/mod.rs` 体积（reviewer M-5 建议）。`mmio_read_impl`
//! / `mmio_write_impl` 都是 `pub(super)`，被 mod.rs 的 `impl PcieDevice`
//! 用 1-line shim 调用。BAR0 寄存器布局 + doorbell 协议都在此文件，让
//! NVMe regs spec layout 集中可读。

use super::*;

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
            // **Phase L3 + Q5** — CMB / BPINFO / PMR 寄存器
            (0x38, _) => 0, // CMBLOC — Q6 (CMB) 仍 0
            (0x3c, _) => 0, // CMBSZ — Q6 (CMB) 仍 0
            // **Phase L3 + Q5 + 12轮 H-Q5** — BPINFO=0 不 advertise boot partition。
            // 之前 BPSZ=1 让 driver 看到 1 partition，但我们没 boot image
            // 服务 → driver 写 BPRSEL 后 poll BRS 永远 0 = hang。BPSZ=0
            // driver enumerate 即跳过。BPRSEL/BPMBL 仍 RW 保 spec
            // register layout 完整（教学读 BAR0 时能看见）。
            (0x40, _) => 0,                  // BPINFO — BPSZ=0 (no BP advertise)
            (0x44, _) => self.bprsel as u64, // BPRSEL — RW，回读上次写值
            (0x48, 8) => self.bpmbl,         // BPMBL 64-bit
            (0x48, 4) => self.bpmbl & 0xFFFF_FFFF, // 低 32
            (0x4c, 4) => self.bpmbl >> 32,   // 高 32
            (0xe00, _) => 0,                 // PMRCAP — Q6 (PMR) 仍 0
            (0xe04, _) => 0,                 // PMRCTL
            (0xe08, _) => 0,                 // PMRSTS
            (o, _) if o >= 0x1000 => 0,      // doorbell reads return 0 (write-only)
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
            // **Phase Q5** — Boot Partition control registers
            0x44 => {
                // BPRSEL = boot partition read select。bits 9:0 = BPRSZ
                // (read size 4 KiB units)；bits 31:10 = BPROF (offset 4 KiB units)；
                // bit 31:30 实际为 BPID (active partition ID)。教学 controller
                // 无真 boot image，写入立即返；driver 拉 BPINFO.BRS 看完成。
                self.bprsel = value as u32;
                tracing::debug!(value, "BPRSEL set (boot partition no-op)");
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
            o if o >= 0x1000 => {
                // doorbell 写**必须** 4 字节 access；其它尺寸视为 driver bug
                // 直接忽略（不应该按 8/2/1 字节写 doorbell）。
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
                        self.on_cq_head_doorbell(qid, value as u32);
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
}
