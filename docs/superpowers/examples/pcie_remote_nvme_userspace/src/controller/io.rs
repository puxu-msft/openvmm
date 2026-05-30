// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVM IO command dispatch — Read / Write (含双 PRP) / Flush。
//!
//! 拆出来减小 `controller/mod.rs` 体积（H6 reviewer 建议）。
//! `on_dma_complete` 内的 IO 完成路径仍在 mod.rs，因为它需要访问
//! `dual_prp_writes` / `pending_ios` 全局状态 + 调 `post_cqe`。

use crate::cmd::*;
use crate::controller::NvmeController;
use crate::controller::PendingIo;
use crate::controller::PendingOp;
use crate::controller::SECTOR_SIZE;
use crate::controller::WriteAccum;
use crate::regs::*;
use pcie_remote_userspace_sdk::*;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

impl NvmeController {

    /// IO command dispatch。Read/Write 走 DMA。
    pub(super) fn dispatch_io(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        sq_id: u16,
        sqe: Sqe,
        cid: u16,
        sq_head: u16,
        cq_id: u16,
    ) -> Option<Cqe> {
        let phase = self.cqs.get(&cq_id).map(|c| c.phase).unwrap_or(1);
        match sqe.opcode() {
            nvm_opc::READ => {
                let cdw10 = sqe.cdw10;
                let cdw11 = sqe.cdw11;
                let cdw12 = sqe.cdw12;
                let prp1 = sqe.prp1;
                let prp2 = sqe.prp2;
                let slba = cdw10 as u64 | ((cdw11 as u64) << 32);
                let nlb = (cdw12 & 0xffff) as u32 + 1;
                let bytes = nlb as u64 * SECTOR_SIZE;
                tracing::debug!(slba, nlb, bytes, prp1 = format_args!("{:#x}", prp1), "NVM READ");
                if bytes > MDTS_MAX_BYTES {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                if slba + nlb as u64 > self.total_lba {
                    return Some(Cqe::error(
                        cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE, 0,
                    ));
                }
                // 从文件读到 buf
                let mut buf = vec![0u8; bytes as usize];
                if let Err(e) = self
                    .file
                    .seek(SeekFrom::Start(slba * SECTOR_SIZE))
                    .and_then(|_| self.file.read_exact(&mut buf))
                {
                    tracing::warn!(error = %e, slba, nlb, "READ: backing file read failed");
                    return Some(Cqe::error(
                        cid, sq_id, sq_head, phase, sc::DATA_TRANSFER_ERROR, 0,
                    ));
                }
                // DMA write to PRP1（v1：bytes ≤ 8 KiB = 2 page = PRP1 + PRP2）
                if bytes <= NVME_PAGE_SIZE {
                    let tok = ctx.dma_write(prp1, buf);
                    self.pending_ios.insert(
                        tok,
                        PendingIo {
                            sq_id, cid, sq_head, cq_id,
                            op: PendingOp::NvmReadDmaWrite,
                        },
                    );
                } else {
                    let half = NVME_PAGE_SIZE as usize;
                    let (b1, b2) = buf.split_at(half);
                    let _tok1 = ctx.dma_write(prp1, b1.to_vec());
                    let tok2 = ctx.dma_write(prp2, b2.to_vec());
                    self.pending_ios.insert(
                        tok2,
                        PendingIo {
                            sq_id, cid, sq_head, cq_id,
                            op: PendingOp::NvmReadDmaWrite,
                        },
                    );
                }
                None
            }
            nvm_opc::WRITE => {
                // 复制 packed 字段到本地变量（packed struct field 取引用 UB，
                // 直接 as u64 在新 rustc 也会触发警告）。
                let cdw10 = sqe.cdw10;
                let cdw11 = sqe.cdw11;
                let cdw12 = sqe.cdw12;
                let prp1 = sqe.prp1;
                let prp2 = sqe.prp2;
                let slba = cdw10 as u64 | ((cdw11 as u64) << 32);
                let nlb = (cdw12 & 0xffff) as u32 + 1;
                let bytes = nlb as u64 * SECTOR_SIZE;
                tracing::debug!(slba, nlb, bytes, prp1 = format_args!("{:#x}", prp1), "NVM WRITE");
                if bytes > MDTS_MAX_BYTES {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                if slba + nlb as u64 > self.total_lba {
                    return Some(Cqe::error(
                        cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE, 0,
                    ));
                }
                // DMA read from PRP1 (+ 可选 PRP2)
                if bytes <= NVME_PAGE_SIZE {
                    let tok = ctx.dma_read(prp1, bytes as u32);
                    self.pending_ios.insert(
                        tok,
                        PendingIo {
                            sq_id, cid, sq_head, cq_id,
                            op: PendingOp::NvmWriteDmaRead { lba: slba, num_blocks: nlb },
                        },
                    );
                } else {
                    // 双 PRP：PRP1 = 第一页 (4 KiB)，PRP2 = 第二页（最多 4 KiB）。
                    // 分配独立 op_id，PRP1/PRP2 完成回调通过 op_id 关联 ——
                    // 解决 PRP2 先到 PRP1 的乱序数据损坏 + leak 问题（C1）。
                    let op_id = self.next_op_id;
                    self.next_op_id = self.next_op_id.wrapping_add(1);
                    self.dual_prp_writes.insert(
                        op_id,
                        WriteAccum {
                            sq_id, cid, sq_head, cq_id,
                            lba: slba, num_blocks: nlb,
                            prp1_data: None, prp2_data: None,
                        },
                    );
                    let prp2_bytes = (bytes - NVME_PAGE_SIZE) as u32;
                    let tok1 = ctx.dma_read(prp1, NVME_PAGE_SIZE as u32);
                    let tok2 = ctx.dma_read(prp2, prp2_bytes);
                    self.pending_ios.insert(
                        tok1,
                        PendingIo {
                            sq_id, cid, sq_head, cq_id,
                            op: PendingOp::NvmWriteDualPrp { op_id, is_prp1: true },
                        },
                    );
                    self.pending_ios.insert(
                        tok2,
                        PendingIo {
                            sq_id, cid, sq_head, cq_id,
                            op: PendingOp::NvmWriteDualPrp { op_id, is_prp1: false },
                        },
                    );
                }
                None
            }
            nvm_opc::FLUSH => {
                let _ = self.file.sync_all();
                Some(Cqe::success(cid, sq_id, sq_head, phase))
            }
            opc => {
                tracing::warn!(opc, "unsupported NVM opcode");
                Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_OPCODE, 0))
            }
        }
    }

}
