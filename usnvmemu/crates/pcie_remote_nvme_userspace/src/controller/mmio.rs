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
            0x28 => {
                // ASQ low 32
                self.asq = (self.asq & !0xffff_ffff) | (value & 0xffff_ffff);
            }
            0x2c => {
                self.asq = (self.asq & 0xffff_ffff) | (value << 32);
            }
            0x30 => {
                self.acq = (self.acq & !0xffff_ffff) | (value & 0xffff_ffff);
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
                // BPMBL low 32
                self.bpmbl = (self.bpmbl & !0xffff_ffff) | (value & 0xffff_ffff);
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
