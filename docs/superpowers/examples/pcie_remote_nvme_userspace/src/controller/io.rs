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
                let nsid = sqe.nsid;
                let slba = cdw10 as u64 | ((cdw11 as u64) << 32);
                let nlb = (cdw12 & 0xffff) as u32 + 1;
                let bytes = nlb as u64 * SECTOR_SIZE;
                tracing::debug!(
                    nsid,
                    slba,
                    nlb,
                    bytes,
                    prp1 = format_args!("{:#x}", prp1),
                    "NVM READ"
                );
                if bytes > MDTS_MAX_BYTES {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                // **Phase H4** — NSID 校验 + 取 NS（含 total_lba）
                let Some(ns) = self.ns(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                        0,
                    ));
                };
                let total_lba = ns.total_lba;
                // H4：checked_add 防 slba + nlb 溢出（driver bug / 恶意输入）。
                match slba.checked_add(nlb as u64) {
                    Some(end) if end <= total_lba => {}
                    _ => {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::LBA_OUT_OF_RANGE,
                            0,
                        ));
                    }
                }
                // 从文件读到 buf（per-NSID）
                let mut buf = vec![0u8; bytes as usize];
                let ns_mut = self.ns_mut(nsid).unwrap();
                if let Err(e) = ns_mut
                    .file
                    .seek(SeekFrom::Start(slba * SECTOR_SIZE))
                    .and_then(|_| ns_mut.file.read_exact(&mut buf))
                {
                    tracing::warn!(error = %e, nsid, slba, nlb, "READ: backing file read failed");
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::DATA_TRANSFER_ERROR,
                        0,
                    ));
                }
                // 三档 PRP 分流（NVMe spec § 4.4）：
                //   ≤ 1 page (4 KiB)：单 PRP1
                //   ≤ 2 page (8 KiB)：PRP1 + PRP2 直接指针
                //   > 2 page (≤ MDTS)：PRP1 + PRP2 指向 PRP list 页
                if bytes <= NVME_PAGE_SIZE {
                    let tok = ctx.dma_write(prp1, buf);
                    self.pending_ios.insert(
                        tok,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmReadDmaWrite { num_blocks: nlb },
                        },
                    );
                } else if bytes <= 2 * NVME_PAGE_SIZE {
                    let half = NVME_PAGE_SIZE as usize;
                    let (b1, b2) = buf.split_at(half);
                    let tok1 = ctx.dma_write(prp1, b1.to_vec());
                    let tok2 = ctx.dma_write(prp2, b2.to_vec());
                    self.pending_ios.insert(
                        tok1,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmReadDualPrpSiblingHalf,
                        },
                    );
                    self.pending_ios.insert(
                        tok2,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmReadDmaWrite { num_blocks: nlb },
                        },
                    );
                } else {
                    // **Phase E** — PRP list path (Read > 2 page)。
                    let total_pages = bytes.div_ceil(NVME_PAGE_SIZE) as u32;
                    let mut data_pages: Vec<Option<Vec<u8>>> =
                        Vec::with_capacity(total_pages as usize);
                    for i in 0..total_pages {
                        let off = i as usize * NVME_PAGE_SIZE as usize;
                        let end = ((i + 1) as usize * NVME_PAGE_SIZE as usize).min(buf.len());
                        data_pages.push(Some(buf[off..end].to_vec()));
                    }
                    let op_id = self.alloc_op_id();
                    self.prp_list_ops.insert(
                        op_id,
                        crate::controller::PrpListOp {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            lba: slba,
                            num_blocks: nlb,
                            is_write: false,
                            prp1_gpa: prp1,
                            list_entries: None,
                            total_pages,
                            pages_done: 0,
                            data_pages,
                        },
                    );
                    let tok = ctx.dma_read(prp2, NVME_PAGE_SIZE as u32);
                    self.pending_ios.insert(
                        tok,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmReadPrpListFetch { op_id },
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
                let nsid = sqe.nsid;
                let slba = cdw10 as u64 | ((cdw11 as u64) << 32);
                let nlb = (cdw12 & 0xffff) as u32 + 1;
                let bytes = nlb as u64 * SECTOR_SIZE;
                tracing::debug!(
                    nsid,
                    slba,
                    nlb,
                    bytes,
                    prp1 = format_args!("{:#x}", prp1),
                    "NVM WRITE"
                );
                if bytes > MDTS_MAX_BYTES {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                // **Phase H4** — NSID 校验
                let Some(ns) = self.ns(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                        0,
                    ));
                };
                let total_lba = ns.total_lba;
                match slba.checked_add(nlb as u64) {
                    Some(end) if end <= total_lba => {}
                    _ => {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::LBA_OUT_OF_RANGE,
                            0,
                        ));
                    }
                }
                // 三档 PRP 分流（同 READ 路径）：≤1page / ≤2page / PRP list。
                if bytes <= NVME_PAGE_SIZE {
                    let tok = ctx.dma_read(prp1, bytes as u32);
                    self.pending_ios.insert(
                        tok,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmWriteDmaRead {
                                lba: slba,
                                num_blocks: nlb,
                            },
                        },
                    );
                } else if bytes <= 2 * NVME_PAGE_SIZE {
                    let op_id = self.alloc_op_id();
                    self.dual_prp_writes.insert(
                        op_id,
                        WriteAccum {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            lba: slba,
                            num_blocks: nlb,
                            prp1_data: None,
                            prp2_data: None,
                        },
                    );
                    let prp2_bytes = (bytes - NVME_PAGE_SIZE) as u32;
                    let tok1 = ctx.dma_read(prp1, NVME_PAGE_SIZE as u32);
                    let tok2 = ctx.dma_read(prp2, prp2_bytes);
                    self.pending_ios.insert(
                        tok1,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmWriteDualPrp {
                                op_id,
                                is_prp1: true,
                            },
                        },
                    );
                    self.pending_ios.insert(
                        tok2,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmWriteDualPrp {
                                op_id,
                                is_prp1: false,
                            },
                        },
                    );
                } else {
                    // **Phase E** — PRP list path (Write > 2 page)。
                    let total_pages = bytes.div_ceil(NVME_PAGE_SIZE) as u32;
                    let op_id = self.alloc_op_id();
                    let mut data_pages: Vec<Option<Vec<u8>>> =
                        Vec::with_capacity(total_pages as usize);
                    for _ in 0..total_pages {
                        data_pages.push(None);
                    }
                    self.prp_list_ops.insert(
                        op_id,
                        crate::controller::PrpListOp {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            lba: slba,
                            num_blocks: nlb,
                            is_write: true,
                            prp1_gpa: prp1,
                            list_entries: None,
                            total_pages,
                            pages_done: 0,
                            data_pages,
                        },
                    );
                    // 先 fetch PRP list 页本身
                    let tok_list = ctx.dma_read(prp2, NVME_PAGE_SIZE as u32);
                    self.pending_ios.insert(
                        tok_list,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmWritePrpListFetch { op_id },
                        },
                    );
                    // 同时 fetch PRP1 数据页（页 idx 0）
                    let tok_prp1 = ctx.dma_read(prp1, NVME_PAGE_SIZE as u32);
                    self.pending_ios.insert(
                        tok_prp1,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmWritePrpListData { op_id, page_idx: 0 },
                        },
                    );
                }
                None
            }
            nvm_opc::FLUSH => {
                // **H3 修复**：FLUSH 失败必须返 DATA_TRANSFER_ERROR。
                // VWC=present 让 driver 依赖 FLUSH 做 durability 承诺；
                // 吞错会让 driver 误信数据已落盘。
                // **Phase H4**：nsid=0xFFFF_FFFF = flush all NS（spec
                // § 6.7）；具体 NSID 仅 flush 该 NS。
                let nsid = sqe.nsid;
                let targets: Vec<u32> = if nsid == 0xFFFF_FFFF {
                    self.namespaces.keys().copied().collect()
                } else if self.namespaces.contains_key(&nsid) {
                    vec![nsid]
                } else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                        0,
                    ));
                };
                for target in targets {
                    let ns = self.namespaces.get_mut(&target).unwrap();
                    if let Err(e) = ns.file.sync_all() {
                        tracing::warn!(error = %e, nsid = target, "FLUSH sync_all failed");
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
                            0,
                        ));
                    }
                }
                Some(Cqe::success(cid, sq_id, sq_head, phase))
            }
            nvm_opc::WRITE_ZEROES => {
                // NVMe NVM CS Spec § 3.3.4 Write Zeroes — 把 [SLBA, SLBA+NLB)
                // 范围内的 LBA 全清零。CDW10/11 = SLBA，CDW12 bits 15:0 = NLB
                // (zero-based)。无 DMA，无 MDTS 限制（spec 允许整盘 nlb）。
                let nsid = sqe.nsid;
                let slba = sqe.cdw10 as u64 | ((sqe.cdw11 as u64) << 32);
                let nlb = (sqe.cdw12 & 0xffff) as u32 + 1;
                let bytes = nlb as u64 * SECTOR_SIZE;
                tracing::debug!(nsid, slba, nlb, bytes, "NVM WRITE ZEROES");
                let Some(ns) = self.ns(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                        0,
                    ));
                };
                let total_lba = ns.total_lba;
                // **H4 修复**：用 checked_add 防 slba + nlb 溢出。
                match slba.checked_add(nlb as u64) {
                    Some(end) if end <= total_lba => {} // 范围合法
                    _ => {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::LBA_OUT_OF_RANGE,
                            0,
                        ));
                    }
                }
                // **C2 修复**：分块写复用 4 KiB 零 buffer，避免大 nlb 时
                // 一次性分配 32 MiB+ Vec OOM。每块独立 write，spec 允许
                // controller 在中途因 error abort（我们这里若中途失败直接返）。
                const CHUNK: usize = 4096;
                let zero_buf = [0u8; CHUNK];
                let mut remaining = bytes as usize;
                let mut off = slba * SECTOR_SIZE;
                let ns = self.ns_mut(nsid).unwrap();
                if let Err(e) = ns.file.seek(SeekFrom::Start(off)) {
                    tracing::warn!(error = %e, slba, "WRITE ZEROES seek failed");
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::DATA_TRANSFER_ERROR,
                        0,
                    ));
                }
                while remaining > 0 {
                    let n = remaining.min(CHUNK);
                    if let Err(e) = std::io::Write::write_all(&mut ns.file, &zero_buf[..n]) {
                        tracing::warn!(error = %e, slba, nlb, off, "WRITE ZEROES chunk failed");
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
                            0,
                        ));
                    }
                    remaining -= n;
                    off += n as u64;
                }
                Some(Cqe::success(cid, sq_id, sq_head, phase))
            }
            nvm_opc::DSM => {
                // NVMe NVM CS Spec § 3.3.5 Dataset Management — TRIM/UNMAP
                // 类似语义。CDW10 bits 7:0 = NR (number of ranges - 1)。
                // CDW11 bits 0:2 = attribute (IDR/IDW/AD = Attribute Deallocate)。
                // PRP1 指向 16-byte * (NR+1) 个 range descriptor。
                //
                // 我们当前不真 TRIM 底层文件（host file system 通常自己处理
                // sparse hole），返 success 让 driver 信任 deallocate 完成。
                // 真实现可 punch_hole + fallocate(FALLOC_FL_PUNCH_HOLE)。
                let nr = (sqe.cdw10 & 0xff) as u32 + 1;
                let ad = sqe.cdw11 & 0x4 != 0;
                tracing::debug!(nr, ad, "DSM Dataset Management (no-op success)");
                Some(Cqe::success(cid, sq_id, sq_head, phase))
            }
            nvm_opc::COMPARE => {
                // **Phase H3** — NVMe NVM CS Spec § 3.3.2 Compare：读 LBA 与
                // host 提供数据比较。
                let cdw10 = sqe.cdw10;
                let cdw11 = sqe.cdw11;
                let cdw12 = sqe.cdw12;
                let prp1 = sqe.prp1;
                let nsid = sqe.nsid;
                let slba = cdw10 as u64 | ((cdw11 as u64) << 32);
                let nlb = (cdw12 & 0xffff) as u32 + 1;
                let bytes = nlb as u64 * SECTOR_SIZE;
                tracing::debug!(nsid, slba, nlb, bytes, "NVM COMPARE");
                if bytes > MDTS_MAX_BYTES {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                let Some(ns) = self.ns(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                        0,
                    ));
                };
                let total_lba = ns.total_lba;
                match slba.checked_add(nlb as u64) {
                    Some(end) if end <= total_lba => {}
                    _ => {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::LBA_OUT_OF_RANGE,
                            0,
                        ));
                    }
                }
                if bytes > NVME_PAGE_SIZE {
                    tracing::warn!(
                        bytes,
                        "Compare > 4 KiB not yet implemented; returning success (placeholder)"
                    );
                    return Some(Cqe::success(cid, sq_id, sq_head, phase));
                }
                let tok = ctx.dma_read(prp1, bytes as u32);
                self.pending_ios.insert(
                    tok,
                    PendingIo {
                        sq_id,
                        cid,
                        sq_head,
                        cq_id,
                        nsid,
                        op: PendingOp::NvmCompareSinglePrp {
                            lba: slba,
                            num_blocks: nlb,
                        },
                    },
                );
                None
            }
            nvm_opc::VERIFY => {
                let nsid = sqe.nsid;
                let slba = sqe.cdw10 as u64 | ((sqe.cdw11 as u64) << 32);
                let nlb = (sqe.cdw12 & 0xffff) as u32 + 1;
                tracing::debug!(nsid, slba, nlb, "Verify (no-op success)");
                let Some(ns) = self.ns(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                        0,
                    ));
                };
                let total_lba = ns.total_lba;
                match slba.checked_add(nlb as u64) {
                    Some(end) if end <= total_lba => {}
                    _ => {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::LBA_OUT_OF_RANGE,
                            0,
                        ));
                    }
                }
                Some(Cqe::success(cid, sq_id, sq_head, phase))
            }
            nvm_opc::WRITE_UNCORRECTABLE => {
                // NVMe NVM CS Spec § 3.3.6 Write Uncorrectable — 在指定
                // LBA 范围"种下" uncorrectable error，下次 Read 应返
                // UNRECOVERED_READ_ERROR (SC 0x81 SCT=0x02)。
                // 我们 backing 无 ECC 概念；返 INVALID_OPCODE 让 driver
                // 走 fallback。
                tracing::debug!(cid, "Write Uncorrectable (INVALID_OPCODE)");
                Some(Cqe::error(
                    cid,
                    sq_id,
                    sq_head,
                    phase,
                    sc::INVALID_OPCODE,
                    0,
                ))
            }
            opc => {
                tracing::warn!(opc, "unsupported NVM opcode");
                Some(Cqe::error(
                    cid,
                    sq_id,
                    sq_head,
                    phase,
                    sc::INVALID_OPCODE,
                    0,
                ))
            }
        }
    }
}
