// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVM IO 完成回调路径：`on_dma_complete` 单方法按 PendingOp variant 派发。
//!
//! 从 `controller/mod.rs` 拆出来（reviewer H-3 修复）：原 1576-line 单方法
//! 包含 ~20 个 PendingOp variant 处理 + DMA fail cleanup + fetch_sqe 派发。
//! 拆到本文件让 mod.rs 更接近 'controller state + dispatch' 单一职责。
//!
//! 注：仍保留单方法形态（按 variant 拆 sub-method 收益不大且改动量大），
//! 但文件级别隔离已显著减小 mod.rs 体积。

use crate::cmd::*;
use crate::controller::Namespace;
use crate::controller::NvmeController;
use crate::controller::PendingIo;
use crate::controller::PendingOp;
use crate::controller::SECTOR_SIZE;
use crate::controller::SelfTestCompleted;
use crate::controller::ZoneState;
use crate::controller::parse_prp_list;
use crate::controller::try_mmap_file;
use crate::regs::*;
use pcie_remote_userspace_sdk::*;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use zerocopy::IntoBytes;

impl NvmeController {
    pub(super) fn on_dma_complete_impl(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        token: u64,
        ok: bool,
        data: Vec<u8>,
    ) {
        // **H-3 修复** — 之前是 mod.rs 中 1500+ 行的单方法。已移到本
        // controller/completion.rs 文件级隔离。按 PendingOp variant 再
        // 细拆 sub-method 收益不大（多数 variant 共享 phase/cq/post_cqe
        // 引用），但文件级拆分让 mod.rs 显著瘦身。
        // 已抽出的纯函数 helpers：advance_zns_wp / check_zns_write /
        // check_zns_read / apply_zsa / check_zsa_transition /
        // should_fire_irq / build_zone_report。
        if !ok {
            tracing::warn!(token, "DMA failed");
            // 清相关 pending（IO 或 fetch）
            self.pending_fetches.remove(&token);
            if let Some(p) = self.pending_ios.remove(&token) {
                // **C1 修复** — 多段 op (dual PRP / PRP list) 必须清掉
                // sibling 的 pending_ios + 累积器，并保证只 post 一次
                // error CQE。否则 sibling 完成时会进 unknown-op_id 分支
                // warn + leak，driver 还可能收两次 error CQE 违反 NVMe
                // spec § 4.6.1 "one CQE per command"。
                match p.op {
                    PendingOp::NvmWriteDualPrp { op_id, .. } => {
                        self.pending_ios.retain(|_, q| {
                            !matches!(q.op,
                                PendingOp::NvmWriteDualPrp { op_id: o, .. } if o == op_id)
                        });
                        self.dual_prp_writes.remove(&op_id);
                    }
                    PendingOp::NvmWritePrpListFetch { op_id }
                    | PendingOp::NvmWritePrpListData { op_id, .. }
                    | PendingOp::NvmReadPrpListFetch { op_id }
                    | PendingOp::NvmReadPrpListData { op_id, .. } => {
                        self.pending_ios.retain(|_, q| match q.op {
                            PendingOp::NvmWritePrpListFetch { op_id: o }
                            | PendingOp::NvmWritePrpListData { op_id: o, .. }
                            | PendingOp::NvmReadPrpListFetch { op_id: o }
                            | PendingOp::NvmReadPrpListData { op_id: o, .. } => o != op_id,
                            _ => true,
                        });
                        self.prp_list_ops.remove(&op_id);
                    }
                    PendingOp::NvmCompareDualPrp { op_id, .. }
                    | PendingOp::NvmComparePrpListFetch { op_id }
                    | PendingOp::NvmComparePrpListData { op_id, .. } => {
                        // **Phase K2** — Compare 多段 sibling cleanup (同 C1 修复)
                        self.pending_ios.retain(|_, q| match q.op {
                            PendingOp::NvmCompareDualPrp { op_id: o, .. }
                            | PendingOp::NvmComparePrpListFetch { op_id: o }
                            | PendingOp::NvmComparePrpListData { op_id: o, .. } => o != op_id,
                            _ => true,
                        });
                        self.compare_ops.remove(&op_id);
                    }
                    PendingOp::NvmWritePiMulti { op_id, .. } => {
                        // **Phase K4c** — 多 LBA PI Write sibling cleanup
                        self.pending_ios.retain(|_, q| {
                            !matches!(q.op,
                                PendingOp::NvmWritePiMulti { op_id: o, .. } if o == op_id)
                        });
                        // **Phase K4c-list** — 同 op 的 list fetch 也清
                        self.pending_ios.retain(|_, q| {
                            !matches!(q.op,
                                PendingOp::NvmWritePiListFetch { op_id: o } if o == op_id)
                        });
                        self.pi_writes.remove(&op_id);
                    }
                    PendingOp::NvmWritePiListFetch { op_id } => {
                        self.pending_ios.retain(|_, q| {
                            !matches!(q.op,
                                PendingOp::NvmWritePiMulti { op_id: o, .. } if o == op_id)
                        });
                        self.pi_writes.remove(&op_id);
                    }
                    PendingOp::NvmReadPiListFetch { op_id }
                    | PendingOp::NvmReadPiListData { op_id, .. } => {
                        self.pending_ios.retain(|_, q| match q.op {
                            PendingOp::NvmReadPiListFetch { op_id: o }
                            | PendingOp::NvmReadPiListData { op_id: o, .. } => o != op_id,
                            _ => true,
                        });
                        self.pi_reads.remove(&op_id);
                    }
                    PendingOp::NvmReadDualPrpSiblingHalf => {
                        // sibling tok2 (NvmReadDmaWrite) 还在 pending_ios 中；
                        // 移除避免它后续到达时给 driver post success CQE
                        // 覆盖本 error。
                        self.pending_ios.retain(|_, q| {
                            !matches!(
                                q.op,
                                PendingOp::NvmReadDmaWrite { .. }
                                    if q.cid == p.cid && q.sq_id == p.sq_id
                            )
                        });
                    }
                    PendingOp::NvmZoneAppend {
                        zone_idx,
                        prev_wp,
                        prev_state,
                        num_blocks,
                        ..
                    } => {
                        // **Reviewer H1 + M-1** — Append DMA-fail：尝试回滚预占
                        // 的 WP/state。只在 WP 尚未被更新 Append 覆盖时回滚（即
                        // 当前 WP 恰好 = prev_wp + num_blocks）。否则更晚的
                        // Append 已预占，rollback 会破坏其 assigned_lba —
                        // 这种情况留 hole（spec 允许 zone gap，driver 通过
                        // Read SLBA 看到 zero data 推断）。
                        if let Some(ns) = self.namespaces.get_mut(&p.nsid)
                            && let Some(zns) = ns.zns.as_mut()
                            && let Some(zone) = zns.zones.get_mut(zone_idx)
                        {
                            let expected_wp = prev_wp + num_blocks as u64;
                            if zone.write_pointer == expected_wp {
                                zone.write_pointer = prev_wp;
                                zone.state = prev_state;
                            } else {
                                tracing::warn!(
                                    zone_idx,
                                    cur_wp = zone.write_pointer,
                                    expected = expected_wp,
                                    "Append DMA-fail: newer reservation present, leaving hole"
                                );
                            }
                        }
                    }
                    _ => {}
                }
                self.stat_num_err_log_entries += 1;
                // Phase G：push 进 error_log 环形 buffer，Get Log Page 0x01 用。
                // sc=DATA_TRANSFER_ERROR (0x04) 在 SF bits 8..1 = 0x04 << 1。
                // 区分 NVM IO vs admin：admin SQ id=0 时 nsid=0xFFFF_FFFF
                // 表示 "not applicable"（reviewer M3 修复）。LBA 暂用 0；
                // 真要追踪需在 PendingOp 变体存 SLBA。
                let sf = (sc::DATA_TRANSFER_ERROR as u16) << 1;
                let nsid = if p.sq_id == 0 { 0xFFFF_FFFF } else { 1 };
                self.push_error_log(p.sq_id, p.cid, sf, 0, nsid);
                let cq = self.cqs.get(&p.cq_id);
                let phase = cq.map(|c| c.phase).unwrap_or(1);
                let cqe = Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR, 0);
                self.post_cqe(ctx, p.cq_id, cqe);
            }
            return;
        }
        // 1) fetch_sqe completion
        if self.pending_fetches.contains_key(&token) {
            self.on_fetched_sqes(token, data);
            // 处理新入的 SQE
            let inbox = std::mem::take(&mut self.sqe_inbox);
            for (sq_id, head, sqe) in inbox {
                self.dispatch_sqe(ctx, sq_id, head, sqe);
            }
            return;
        }
        // 2) IO completion
        if let Some(p) = self.pending_ios.remove(&token) {
            match p.op {
                PendingOp::NvmWriteDmaRead { lba, num_blocks } => {
                    let bytes = num_blocks as u64 * SECTOR_SIZE;
                    let first8 = if data.len() >= 8 {
                        u64::from_le_bytes(data[..8].try_into().unwrap_or([0; 8]))
                    } else {
                        0
                    };
                    tracing::debug!(
                        lba,
                        num_blocks,
                        data_len = data.len(),
                        expected = bytes,
                        first8 = format_args!("{:#x}", first8),
                        "NVM Write DMA-read complete"
                    );
                    if data.len() != bytes as usize {
                        tracing::warn!(
                            got = data.len(),
                            want = bytes,
                            "NVM Write DMA-read byte mismatch"
                        );
                    }
                    // **Phase H4 + M2** — 用 p.nsid 选 NS，走 write_at（mmap
                    // 零拷贝 fast path）。Admin DMA-write 是 NvmReadDmaWrite
                    // { num_blocks: 0 } 不走这里。
                    let res = if let Some(ns) = self.namespaces.get_mut(&p.nsid) {
                        ns.write_at(&data, lba * SECTOR_SIZE)
                    } else {
                        Err(std::io::Error::other(format!("unknown NSID {}", p.nsid)))
                    };
                    // 注：不 per-IO fsync（H5）；driver 用 NVM FLUSH (opc 0x00)
                    // 显式拿持久化承诺；Identify Controller VWC=1 已声明
                    // volatile write cache，driver 会主动发 FLUSH。
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = match res {
                        Ok(()) => {
                            // Phase F：真 IO 写完成 → 计数。
                            self.stat_host_writes += 1;
                            self.stat_lba_written += num_blocks as u64;
                            // **Reviewer M-2 + H-1** — 落到 ZNS NS 时推进 WP
                            // + 状态 transition（共享 io::advance_zns_wp）。
                            if let Some(ns) = self.namespaces.get_mut(&p.nsid) {
                                crate::controller::io::advance_zns_wp(ns, lba, num_blocks);
                            }
                            Cqe::success(p.cid, p.sq_id, p.sq_head, phase)
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, lba, num_blocks, "NVM Write file write failed");
                            self.stat_num_err_log_entries += 1;
                            self.push_error_log(
                                p.sq_id,
                                p.cid,
                                (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                lba,
                                1,
                            );
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR, 0)
                        }
                    };
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmReadDmaWrite { num_blocks } => {
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    // Phase F：num_blocks > 0 表示真 NVM Read；= 0 是 admin
                    // Identify/Log Page，不计入 SMART host read。
                    if num_blocks > 0 {
                        self.stat_host_reads += 1;
                        self.stat_lba_read += num_blocks as u64;
                    }
                    let cqe = Cqe::success(p.cid, p.sq_id, p.sq_head, phase);
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmReadPiDmaWrite { num_blocks } => {
                    // **Phase K4b** — PI Read 已 verify + dma_write data 完成，
                    // 与 NvmReadDmaWrite 等价（计数）。
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    self.stat_host_reads += 1;
                    self.stat_lba_read += num_blocks as u64;
                    let cqe = Cqe::success(p.cid, p.sq_id, p.sq_head, phase);
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmReadPiListFetch { op_id } => {
                    // **Phase K4c-list** — PI Read list 页解析；逐 entry
                    // dma_write 后续数据页（page_idx 1..pages_total）。
                    let Some(accum) = self.pi_reads.get(&op_id) else {
                        tracing::warn!(op_id, "NvmReadPiListFetch: unknown op_id");
                        return;
                    };
                    let entries_needed = (accum.pages_total - 1) as usize;
                    if data.len() < entries_needed * 8 {
                        tracing::error!(
                            op_id,
                            got = data.len(),
                            want = entries_needed * 8,
                            "K4c-list PI Read: list page short read"
                        );
                        let accum = self.pi_reads.remove(&op_id).unwrap();
                        let phase = self.cqs.get(&accum.cq_id).map(|c| c.phase).unwrap_or(1);
                        let cqe = Cqe::error(
                            accum.cid,
                            accum.sq_id,
                            accum.sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
                            0,
                        );
                        self.post_cqe(ctx, accum.cq_id, cqe);
                        self.pending_ios.retain(|_, q| {
                            !matches!(q.op,
                                PendingOp::NvmReadPiListData { op_id: o, .. } if o == op_id)
                        });
                        return;
                    }
                    let sq_id = accum.sq_id;
                    let cid = accum.cid;
                    let sq_head = accum.sq_head;
                    let cq_id = accum.cq_id;
                    let nsid = accum.nsid;
                    let total = accum.data_only.len();
                    for i in 0..entries_needed {
                        let gpa = u64::from_le_bytes(data[i * 8..i * 8 + 8].try_into().unwrap());
                        let page_idx = (i + 1) as u32;
                        let off = page_idx as usize * NVME_PAGE_SIZE as usize;
                        let remaining = total.saturating_sub(off);
                        if remaining == 0 {
                            break;
                        }
                        let bytes = remaining.min(NVME_PAGE_SIZE as usize);
                        // 取 slice copy 出来发 dma_write
                        let accum_ref = self.pi_reads.get(&op_id).unwrap();
                        let chunk = accum_ref.data_only[off..off + bytes].to_vec();
                        let tok = ctx.dma_write(gpa, chunk);
                        self.pending_ios.insert(
                            tok,
                            PendingIo {
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
                                nsid,
                                op: PendingOp::NvmReadPiListData { op_id, page_idx },
                            },
                        );
                    }
                }
                PendingOp::NvmReadPiListData { op_id, page_idx: _ } => {
                    // **Phase K4c-list** — per-page dma_write 完成；累 pages_done
                    // 直到全 page 完成 → post success CQE + 计 counter。
                    let Some(accum) = self.pi_reads.get_mut(&op_id) else {
                        tracing::warn!(op_id, "NvmReadPiListData: unknown op_id");
                        return;
                    };
                    accum.pages_done += 1;
                    if accum.pages_done < accum.pages_total {
                        return;
                    }
                    let accum = self.pi_reads.remove(&op_id).unwrap();
                    let phase = self.cqs.get(&accum.cq_id).map(|c| c.phase).unwrap_or(1);
                    self.stat_host_reads += 1;
                    self.stat_lba_read += accum.num_blocks as u64;
                    let cqe = Cqe::success(accum.cid, accum.sq_id, accum.sq_head, phase);
                    self.post_cqe(ctx, accum.cq_id, cqe);
                }
                PendingOp::NvmWritePi { lba, num_blocks } => {
                    // **Phase K4a** — DMA-read 4 KiB data 完成；compute PI
                    // tuple + interleave 写到 backing file (block_bytes=4104)。
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let nsid = p.nsid;
                    let cqe = if let Some(ns) = self.namespaces.get_mut(&nsid) {
                        let pi_type = ns.pi_type;
                        let pi_first = ns.pi_first;
                        let block_bytes = ns.block_bytes() as usize;
                        let data_bytes = ns.data_bytes() as usize;
                        if data.len() != data_bytes {
                            tracing::warn!(
                                got = data.len(),
                                want = data_bytes,
                                "PI Write DMA-read length mismatch"
                            );
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR, 0)
                        } else {
                            let tuple = crate::pi::PiTuple::compute(&data, lba, pi_type);
                            let tuple_bytes = tuple.to_bytes();
                            // Interleave: pi_first → [tuple(8)][data(4096)]
                            //             else      → [data(4096)][tuple(8)]
                            let mut block = vec![0u8; block_bytes];
                            if pi_first {
                                block[0..8].copy_from_slice(&tuple_bytes);
                                block[8..8 + data_bytes].copy_from_slice(&data);
                            } else {
                                block[0..data_bytes].copy_from_slice(&data);
                                block[data_bytes..data_bytes + 8].copy_from_slice(&tuple_bytes);
                            }
                            // **Phase M2** — write_at 走 mmap 零拷贝
                            let res = ns.write_at(&block, lba * block_bytes as u64);
                            match res {
                                Ok(()) => {
                                    self.stat_host_writes += 1;
                                    self.stat_lba_written += num_blocks as u64;
                                    // **Phase L1f** — ZNS + PI 组合：WP 推进
                                    // 与 plain Write 完成路径对称（io::advance_zns_wp）。
                                    crate::controller::io::advance_zns_wp(ns, lba, num_blocks);
                                    tracing::debug!(
                                        nsid,
                                        lba,
                                        pi_type,
                                        pi_first,
                                        guard = tuple.guard,
                                        ref_tag = tuple.ref_tag,
                                        "PI Write OK"
                                    );
                                    Cqe::success(p.cid, p.sq_id, p.sq_head, phase)
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, nsid, lba, "PI Write backing fail");
                                    self.stat_num_err_log_entries += 1;
                                    self.push_error_log(
                                        p.sq_id,
                                        p.cid,
                                        (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                        lba,
                                        nsid,
                                    );
                                    Cqe::error(
                                        p.cid,
                                        p.sq_id,
                                        p.sq_head,
                                        phase,
                                        sc::DATA_TRANSFER_ERROR,
                                        0,
                                    )
                                }
                            }
                        }
                    } else {
                        Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_NAMESPACE, 0)
                    };
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmReadDualPrpSiblingHalf => {
                    // **H4**：成功路径无 op — counter + success CQE 全由
                    // tok2 (NvmReadDmaWrite) 处理；这里只是消化 token
                    // 让生命周期闭合。失败路径在 on_dma_complete 顶部 ok==
                    // false 分支统一 post error CQE（参 mod.rs DMA fail）。
                    tracing::trace!(token, "dual-PRP Read sibling half ok (no-op)");
                }
                PendingOp::NvmZoneAppend {
                    zone_idx,
                    assigned_lba,
                    prev_wp,
                    prev_state,
                    num_blocks,
                } => {
                    // **Phase L1 + reviewer H1 修复** — ZNS Zone Append 完成回调。
                    // 1. DMA-read 数据落盘到 assigned_lba 处
                    // 2. 成功 → success CQE 携 assigned_lba（dw0=low32, dw1=hi32）
                    //    （ZNS CS § 3.2.4 要求）
                    // 3. 失败 → 回滚 WP/state 到 prev_wp/prev_state，post error CQE
                    let bytes = num_blocks as u64 * SECTOR_SIZE;
                    let nsid = p.nsid;
                    let res = if let Some(ns) = self.namespaces.get_mut(&nsid) {
                        // **Phase M2** — write_at 走 mmap 零拷贝
                        ns.write_at(&data[..bytes as usize], assigned_lba * SECTOR_SIZE)
                    } else {
                        Err(std::io::Error::other(format!("unknown NSID {}", nsid)))
                    };
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = match res {
                        Ok(()) => {
                            self.stat_host_writes += 1;
                            self.stat_lba_written += num_blocks as u64;
                            tracing::debug!(
                                nsid,
                                zone_idx,
                                assigned_lba,
                                num_blocks,
                                "Zone Append OK"
                            );
                            let mut c = Cqe::success(p.cid, p.sq_id, p.sq_head, phase);
                            // ZNS spec：CQE dw0/dw1 = assigned LBA（little-endian
                            // 双字拼成 64-bit）。driver 用这个值后续 Read。
                            c.cdw0 = (assigned_lba & 0xFFFF_FFFF) as u32;
                            c.cdw1 = (assigned_lba >> 32) as u32;
                            c
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, nsid, zone_idx, assigned_lba,
                                "Zone Append backing failed; rolling back WP/state");
                            // **Reviewer M-1** — 同上 DMA-fail 路径：仅在
                            // 当前 WP 仍 = prev_wp + num_blocks 时回滚，否则
                            // 留 hole 避免破坏更晚 reservation 的 assigned_lba。
                            if let Some(ns) = self.namespaces.get_mut(&nsid)
                                && let Some(zns) = ns.zns.as_mut()
                                && let Some(zone) = zns.zones.get_mut(zone_idx)
                            {
                                let expected_wp = prev_wp + num_blocks as u64;
                                if zone.write_pointer == expected_wp {
                                    zone.write_pointer = prev_wp;
                                    zone.state = prev_state;
                                } else {
                                    tracing::warn!(
                                        zone_idx,
                                        cur_wp = zone.write_pointer,
                                        expected = expected_wp,
                                        "leaving hole (newer reservation present)"
                                    );
                                }
                            }
                            self.stat_num_err_log_entries += 1;
                            self.push_error_log(
                                p.sq_id,
                                p.cid,
                                (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                assigned_lba,
                                nsid,
                            );
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR, 0)
                        }
                    };
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmWritePiListFetch { op_id } => {
                    // **Phase K4c-list** — PRP-list 页已到。解析 u64 array，
                    // 逐 entry DMA-read 数据页（page_idx 从 1 开始；page 0
                    // 是 PRP1 直接发的）。每页大小 = NVME_PAGE_SIZE，最后页
                    // 可能 < page（dispatch 时按 received.len() truncate）。
                    let Some(accum) = self.pi_writes.get_mut(&op_id) else {
                        tracing::warn!(op_id, "NvmWritePiListFetch: unknown op_id");
                        return;
                    };
                    let entries_needed = (accum.pages_total - 1) as usize;
                    if data.len() < entries_needed * 8 {
                        tracing::error!(
                            op_id,
                            got = data.len(),
                            want = entries_needed * 8,
                            "K4c-list: PRP list page short read"
                        );
                        let accum = self.pi_writes.remove(&op_id).unwrap();
                        let phase = self.cqs.get(&accum.cq_id).map(|c| c.phase).unwrap_or(1);
                        let cqe = Cqe::error(
                            accum.cid,
                            accum.sq_id,
                            accum.sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
                            0,
                        );
                        self.post_cqe(ctx, accum.cq_id, cqe);
                        self.pending_ios.retain(|_, q| {
                            !matches!(q.op,
                                PendingOp::NvmWritePiMulti { op_id: o, .. } if o == op_id)
                        });
                        return;
                    }
                    accum.prp_list_pending = false;
                    let sq_id = accum.sq_id;
                    let cid = accum.cid;
                    let sq_head = accum.sq_head;
                    let cq_id = accum.cq_id;
                    let nsid = accum.nsid;
                    let total = accum.data_bytes_total;
                    // Drop &mut accum，开始 dispatch DMA-read
                    let _ = accum;
                    for i in 0..entries_needed {
                        let gpa = u64::from_le_bytes(data[i * 8..i * 8 + 8].try_into().unwrap());
                        let page_idx = (i + 1) as u32;
                        let off = page_idx as usize * NVME_PAGE_SIZE as usize;
                        let remaining = total.saturating_sub(off);
                        let bytes = remaining.min(NVME_PAGE_SIZE as usize) as u32;
                        if bytes == 0 {
                            break;
                        }
                        let tok = ctx.dma_read(gpa, bytes);
                        self.pending_ios.insert(
                            tok,
                            PendingIo {
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
                                nsid,
                                op: PendingOp::NvmWritePiMulti { op_id, page_idx },
                            },
                        );
                    }
                }
                PendingOp::NvmWritePiMulti { op_id, page_idx } => {
                    // **Phase K4c + Reviewer H-3 (7轮)** — 多 LBA PI Write
                    // 数据段到达。把数据 copy 到 accumulator.received 中
                    // 对应 page；所有 page 到齐时按 LBA 切 4096 + compute
                    // PI + interleave 写文件。
                    let Some(accum) = self.pi_writes.get_mut(&op_id) else {
                        tracing::warn!(op_id, "NvmWritePiMulti: unknown op_id");
                        return;
                    };
                    let off = page_idx as usize * NVME_PAGE_SIZE as usize;
                    // Bounds-check defensively（Reviewer LOW 1 后跟进）：
                    // 用 checked_add 防 off + data.len() 理论 usize 溢出
                    // （32-bit 平台上 page_idx u32 × 4096 + data.len() 4096
                    // 仍 < usize::MAX，但 checked_add 是 defense-in-depth）。
                    let Some(end_off) = off.checked_add(data.len()) else {
                        tracing::error!(
                            op_id,
                            page_idx,
                            data_len = data.len(),
                            "K4c PI Write: page off+len overflow; aborting"
                        );
                        let accum = self.pi_writes.remove(&op_id).unwrap();
                        let phase = self.cqs.get(&accum.cq_id).map(|c| c.phase).unwrap_or(1);
                        let cqe = Cqe::error(
                            accum.cid,
                            accum.sq_id,
                            accum.sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
                            0,
                        );
                        self.post_cqe(ctx, accum.cq_id, cqe);
                        self.pending_ios.retain(|_, q| {
                            !matches!(q.op,
                                PendingOp::NvmWritePiMulti { op_id: o, .. } if o == op_id)
                        });
                        return;
                    };
                    if off >= accum.received.len() || end_off > accum.received.len() {
                        tracing::error!(
                            op_id,
                            page_idx,
                            data_len = data.len(),
                            cap = accum.received.len(),
                            "K4c PI Write: page out of bounds; aborting op"
                        );
                        let accum = self.pi_writes.remove(&op_id).unwrap();
                        let phase = self.cqs.get(&accum.cq_id).map(|c| c.phase).unwrap_or(1);
                        let cqe = Cqe::error(
                            accum.cid,
                            accum.sq_id,
                            accum.sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
                            0,
                        );
                        self.post_cqe(ctx, accum.cq_id, cqe);
                        // 顺便清 sibling tokens（防止 silently 完成）
                        self.pending_ios.retain(|_, q| {
                            !matches!(q.op,
                                PendingOp::NvmWritePiMulti { op_id: o, .. } if o == op_id)
                        });
                        return;
                    }
                    accum.received[off..off + data.len()].copy_from_slice(&data);
                    accum.pages_done += 1;
                    if accum.pages_done < accum.pages_total {
                        return; // 还有 page 未到，等下一次完成
                    }
                    // 所有 page 到齐 — 拿 accumulator 出来处理
                    let accum = self.pi_writes.remove(&op_id).unwrap();
                    let nsid = accum.nsid;
                    let cq_id = accum.cq_id;
                    let cq = self.cqs.get(&cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = if let Some(ns) = self.namespaces.get_mut(&nsid) {
                        let pi_type = ns.pi_type;
                        let pi_first = ns.pi_first;
                        let block_bytes = ns.block_bytes() as usize;
                        let data_bytes = ns.data_bytes() as usize;
                        let mut written_count: u32 = 0;
                        let mut last_err: Option<std::io::Error> = None;
                        for i in 0..accum.num_blocks as usize {
                            let lba = accum.slba + i as u64;
                            let data_chunk = &accum.received[i * data_bytes..(i + 1) * data_bytes];
                            let tuple = crate::pi::PiTuple::compute(data_chunk, lba, pi_type);
                            let tuple_bytes = tuple.to_bytes();
                            let mut block = vec![0u8; block_bytes];
                            if pi_first {
                                block[0..8].copy_from_slice(&tuple_bytes);
                                block[8..8 + data_bytes].copy_from_slice(data_chunk);
                            } else {
                                block[0..data_bytes].copy_from_slice(data_chunk);
                                block[data_bytes..data_bytes + 8].copy_from_slice(&tuple_bytes);
                            }
                            if let Err(e) = ns.write_at(&block, lba * block_bytes as u64) {
                                last_err = Some(e);
                                break;
                            }
                            written_count += 1;
                        }
                        let ok = written_count == accum.num_blocks;
                        if ok {
                            self.stat_host_writes += 1;
                            self.stat_lba_written += accum.num_blocks as u64;
                            // **Phase L1f** — ZNS + PI 组合 WP 推进
                            crate::controller::io::advance_zns_wp(ns, accum.slba, accum.num_blocks);
                            tracing::debug!(
                                nsid,
                                slba = accum.slba,
                                num_blocks = accum.num_blocks,
                                "K4c multi-LBA PI Write OK"
                            );
                            Cqe::success(accum.cid, accum.sq_id, accum.sq_head, phase)
                        } else {
                            let e = last_err.unwrap();
                            tracing::warn!(
                                error = %e,
                                nsid,
                                slba = accum.slba,
                                written_count,
                                total = accum.num_blocks,
                                "K4c PI Write partial fail"
                            );
                            // **Reviewer H-B (9轮) + 10轮 regression fix** —
                            // written_count 个 LBA 已落盘。**关键区分**：
                            // - 非 ZNS NS：retry 同 cmd 是幂等（重写相同 data
                            //   到 same LBA），不动 WP 即可。
                            // - ZNS NS：advance_zns_wp by partial 会让 driver
                            //   重试时撞 ZONE_INVALID_WRITE（slba 与 WP 不匹配），
                            //   permanent error。改为**不动 WP**：partial 落盘
                            //   data 在重试后被 overwrite（NVMe ZNS spec § 4.4
                            //   允许重试 same LBA in ImplicitOpen zone）。
                            //   注：partial backing 与 WP 不一致，但 reads
                            //   from above-WP region 在 ZNS 允许返 zero，
                            //   驱动语义上 invisible 直到 retry 覆盖。
                            if written_count > 0 && ns.zns.is_none() {
                                self.stat_host_writes += 1;
                                self.stat_lba_written += written_count as u64;
                            }
                            self.stat_num_err_log_entries += 1;
                            self.push_error_log(
                                accum.sq_id,
                                accum.cid,
                                (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                accum.slba + written_count as u64,
                                nsid,
                            );
                            Cqe::error(
                                accum.cid,
                                accum.sq_id,
                                accum.sq_head,
                                phase,
                                sc::DATA_TRANSFER_ERROR,
                                0,
                            )
                        }
                    } else {
                        Cqe::error(
                            accum.cid,
                            accum.sq_id,
                            accum.sq_head,
                            phase,
                            sc::INVALID_NAMESPACE,
                            0,
                        )
                    };
                    self.post_cqe(ctx, cq_id, cqe);
                }
                PendingOp::NvmCopyFetchRanges { sdlba, num_ranges } => {
                    // **Phase O1** — Simple Copy ranges 已到达；解析 32-byte
                    // descriptors → per-range backing read + write 到 sdlba。
                    // 全 controller 侧 backing，无 host DMA。
                    let nsid = p.nsid;
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let expected = num_ranges as usize * 32;
                    if data.len() < expected {
                        tracing::warn!(
                            got = data.len(),
                            want = expected,
                            "COPY range list DMA short read"
                        );
                        let cqe = Cqe::error(
                            p.cid,
                            p.sq_id,
                            p.sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
                            0,
                        );
                        self.post_cqe(ctx, p.cq_id, cqe);
                        return;
                    }
                    // Parse ranges & total destination NLB
                    let mut ranges: Vec<(u64, u32)> = Vec::with_capacity(num_ranges as usize);
                    let mut dst_total: u64 = 0;
                    for i in 0..num_ranges as usize {
                        let base = i * 32;
                        let slba = u64::from_le_bytes(data[base..base + 8].try_into().unwrap());
                        let nlb = u16::from_le_bytes(data[base + 16..base + 18].try_into().unwrap())
                            as u32
                            + 1; // 0-based
                        ranges.push((slba, nlb));
                        dst_total += nlb as u64;
                    }
                    let cqe = if let Some(ns) = self.namespaces.get_mut(&nsid) {
                        let sector = SECTOR_SIZE;
                        // **Reviewer H-2** — checked_add 防 sdlba/slba 来自
                        // driver / 恶意输入溢出 u64 后绕过 bounds 检查。
                        let ok_bounds = sdlba
                            .checked_add(dst_total)
                            .is_some_and(|e| e <= ns.total_lba)
                            && ranges.iter().all(|&(slba, nlb)| {
                                slba.checked_add(nlb as u64)
                                    .is_some_and(|e| e <= ns.total_lba)
                            });
                        if !ok_bounds {
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::LBA_OUT_OF_RANGE, 0)
                        } else {
                            // Per-range: read backing → write to sdlba offset
                            let mut dst_off_lba = sdlba;
                            let mut copy_err: Option<std::io::Error> = None;
                            for &(slba, nlb) in &ranges {
                                let bytes = nlb as usize * sector as usize;
                                let mut buf = vec![0u8; bytes];
                                if let Err(e) = ns.read_at(&mut buf, slba * sector) {
                                    copy_err = Some(e);
                                    break;
                                }
                                if let Err(e) = ns.write_at(&buf, dst_off_lba * sector) {
                                    copy_err = Some(e);
                                    break;
                                }
                                dst_off_lba += nlb as u64;
                            }
                            if let Some(e) = copy_err {
                                tracing::warn!(error = %e, nsid, sdlba, "COPY backing IO fail");
                                self.stat_num_err_log_entries += 1;
                                self.push_error_log(
                                    p.sq_id,
                                    p.cid,
                                    (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                    sdlba,
                                    nsid,
                                );
                                Cqe::error(
                                    p.cid,
                                    p.sq_id,
                                    p.sq_head,
                                    phase,
                                    sc::DATA_TRANSFER_ERROR,
                                    0,
                                )
                            } else {
                                // 计入 host_reads + host_writes (NVMe spec § 5.16.1.2
                                // SMART 把 Copy 既算 read 也算 write，因为 backing
                                // 真的双程 IO 了)
                                self.stat_host_reads += 1;
                                self.stat_host_writes += 1;
                                self.stat_lba_read += dst_total;
                                self.stat_lba_written += dst_total;
                                tracing::debug!(
                                    nsid,
                                    sdlba,
                                    dst_total,
                                    num_ranges,
                                    "COPY OK (controller-side backing)"
                                );
                                Cqe::success(p.cid, p.sq_id, p.sq_head, phase)
                            }
                        }
                    } else {
                        Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_NAMESPACE, 0)
                    };
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::AdminSetHostIdentifier { exhid } => {
                    // **Phase K9** — host_id DMA-read 完成，存到 controller。
                    if data.len() >= 8 {
                        self.host_id_lo = u64::from_le_bytes(data[0..8].try_into().unwrap());
                    }
                    if exhid && data.len() >= 16 {
                        self.host_id_hi = u64::from_le_bytes(data[8..16].try_into().unwrap());
                    } else if !exhid {
                        self.host_id_hi = 0;
                    }
                    tracing::info!(
                        exhid,
                        lo = format_args!("{:#x}", self.host_id_lo),
                        hi = format_args!("{:#x}", self.host_id_hi),
                        "Set Features Host Identifier"
                    );
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = Cqe::success(p.cid, p.sq_id, p.sq_head, phase);
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::AdminFwDownloadChunk { offset_bytes } => {
                    // **Phase H5 + Reviewer M-H1 (10轮)** — FW chunk DMA-read
                    // 完成，写入累积 buffer。Bound 总尺寸防 driver 恶意发
                    // offset_bytes = 0xFFFF_FFFF 触发 multi-GB Vec OOM。
                    // 64 MiB 教学 cap — 真硬件 FW image 通常 1-10 MiB。
                    const FW_MAX_BYTES: usize = 64 * 1024 * 1024;
                    let off = offset_bytes as usize;
                    let end = off.saturating_add(data.len());
                    if end > FW_MAX_BYTES {
                        tracing::warn!(
                            offset_bytes,
                            chunk_len = data.len(),
                            end,
                            cap = FW_MAX_BYTES,
                            "FW Download exceeds 64 MiB cap → INVALID_FIELD"
                        );
                        let cq = self.cqs.get(&p.cq_id);
                        let phase = cq.map(|c| c.phase).unwrap_or(1);
                        let cqe =
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_FIELD, 0);
                        self.post_cqe(ctx, p.cq_id, cqe);
                        return;
                    }
                    if end > self.fw_download_buf.len() {
                        self.fw_download_buf.resize(end, 0);
                    }
                    self.fw_download_buf[off..end].copy_from_slice(&data);
                    tracing::debug!(
                        offset_bytes,
                        len = data.len(),
                        total_buf = self.fw_download_buf.len(),
                        "FW Download chunk applied"
                    );
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = Cqe::success(p.cid, p.sq_id, p.sq_head, phase);
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::AdminNsCreate => {
                    // **Phase K3** — NS Mgmt Create：parse 4 KiB Identify NS：
                    //   NSZE @ 0..8 (LBA count)
                    //   NCAP @ 8..16
                    //   FLBAS @ 26 (LBAF index bits 3:0)
                    //   DPS @ 29 (PI type bits 2:0 + bit 3 first/last)
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let mut cqe = if data.len() < 30 {
                        Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_FIELD, 0)
                    } else {
                        let nsze = u64::from_le_bytes(data[0..8].try_into().unwrap());
                        let flbas = data[26] & 0x0f;
                        let dps = data[29];
                        let pi_type = dps & 0x07;
                        let pi_first = (dps & 0x08) == 0;
                        if nsze == 0 || flbas > 1 || pi_type > 1 {
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_FIELD, 0)
                        } else {
                            let new_lbads = if flbas == 0 { 9u8 } else { 12u8 };
                            let new_meta_size = if flbas == 0 { 0u8 } else { 8u8 };
                            // 分配新 NSID（next available）+ 创 temp file backing
                            let new_nsid = (1..=u32::MAX)
                                .find(|n| !self.namespaces.contains_key(n))
                                .unwrap_or(0);
                            if new_nsid == 0 {
                                Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INTERNAL_ERROR, 0)
                            } else {
                                let dir = std::env::temp_dir();
                                let path = dir.join(format!(
                                    "nvme_ns_{}_{}.img",
                                    std::process::id(),
                                    new_nsid
                                ));
                                let block = (1u64 << new_lbads) + new_meta_size as u64;
                                let total_bytes = nsze * block;
                                let create_result = std::fs::OpenOptions::new()
                                    .read(true)
                                    .write(true)
                                    .create(true)
                                    .truncate(true)
                                    .open(&path)
                                    .and_then(|f| {
                                        f.set_len(total_bytes)?;
                                        Ok(f)
                                    });
                                match create_result {
                                    Ok(file) => {
                                        let path_str = path.to_string_lossy().into_owned();
                                        tracing::info!(
                                            new_nsid,
                                            nsze,
                                            new_lbads,
                                            new_meta_size,
                                            pi_type,
                                            path = %path_str,
                                            "NS Mgmt Create OK"
                                        );
                                        self.namespaces.insert(
                                            new_nsid,
                                            Namespace {
                                                mmap: try_mmap_file(&file),
                                                file,
                                                total_lba: nsze,
                                                path: path_str,
                                                lbads: new_lbads,
                                                meta_size: new_meta_size,
                                                pi_type,
                                                pi_first,
                                                registrants: Vec::new(),
                                                reservation: None,
                                                reservation_gen: 0,
                                                ptpl: false,
                                                zns: None,
                                            },
                                        );
                                        let mut c = Cqe::success(p.cid, p.sq_id, p.sq_head, phase);
                                        c.cdw0 = new_nsid;
                                        c
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "NS Mgmt Create: file alloc failed");
                                        Cqe::error(
                                            p.cid,
                                            p.sq_id,
                                            p.sq_head,
                                            phase,
                                            sc::INTERNAL_ERROR,
                                            0,
                                        )
                                    }
                                }
                            }
                        }
                    };
                    let _ = &mut cqe; // suppress unused_mut if no future mutation
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmCompareDualPrp { op_id, is_prp1 } => {
                    // **Phase K2** — 双 PRP Compare：两段 DMA-read 累积 →
                    // 合并 → byte-compare backing。
                    let ready = if let Some(accum) = self.compare_ops.get_mut(&op_id) {
                        if is_prp1 {
                            accum.prp1_data = Some(data);
                        } else {
                            accum.prp2_data = Some(data);
                        }
                        accum.prp1_data.is_some() && accum.prp2_data.is_some()
                    } else {
                        tracing::warn!(op_id, "Compare dual-PRP unknown op_id");
                        false
                    };
                    if ready {
                        let accum = self.compare_ops.remove(&op_id).unwrap();
                        let cqe = self.compare_finalize(
                            accum.nsid,
                            accum.lba,
                            accum.num_blocks,
                            accum.prp1_data.unwrap(),
                            Some(accum.prp2_data.unwrap()),
                            accum.cid,
                            accum.sq_id,
                            accum.sq_head,
                            accum.cq_id,
                        );
                        self.post_cqe(ctx, accum.cq_id, cqe);
                    }
                }
                PendingOp::NvmComparePrpListFetch { op_id } => {
                    // **Phase K2** — PRP list 页到达，parse + issue per-page
                    // DMA-read。
                    let entries = parse_prp_list(&data);
                    let info: Option<(u32, u32, Vec<u64>)> =
                        self.compare_ops.get_mut(&op_id).map(|op| {
                            let take = (op.total_pages - 1) as usize;
                            let list: Vec<u64> = entries.into_iter().take(take).collect();
                            op.list_entries = Some(list.clone());
                            (op.total_pages, op.num_blocks, list)
                        });
                    let Some((total_pages, num_blocks, list)) = info else {
                        tracing::warn!(op_id, "Compare PRP list fetch unknown op_id");
                        return;
                    };
                    let (sq_id, cid, sq_head, cq_id, nsid) = {
                        let op = &self.compare_ops[&op_id];
                        (op.sq_id, op.cid, op.sq_head, op.cq_id, op.nsid)
                    };
                    let total_bytes = num_blocks as u64 * SECTOR_SIZE;
                    for (i, gpa) in list.iter().enumerate() {
                        let page_idx = (i + 1) as u32;
                        let want_bytes = if page_idx == total_pages - 1 {
                            let last = total_bytes - (page_idx as u64) * NVME_PAGE_SIZE;
                            last as u32
                        } else {
                            NVME_PAGE_SIZE as u32
                        };
                        let tok = ctx.dma_read(*gpa, want_bytes);
                        self.pending_ios.insert(
                            tok,
                            PendingIo {
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
                                nsid,
                                op: PendingOp::NvmComparePrpListData { op_id, page_idx },
                            },
                        );
                    }
                }
                PendingOp::NvmComparePrpListData { op_id, page_idx } => {
                    // **Phase K2** — 单页数据到达 → 填 data_pages[page_idx]，
                    // 全到齐合并 + compare。
                    let done_all = if let Some(op) = self.compare_ops.get_mut(&op_id) {
                        op.data_pages[page_idx as usize] = Some(data);
                        op.pages_done += 1;
                        op.pages_done == op.total_pages
                    } else {
                        tracing::warn!(op_id, page_idx, "Compare PRP list data unknown");
                        false
                    };
                    if done_all {
                        let op = self.compare_ops.remove(&op_id).unwrap();
                        let total_bytes = op.num_blocks as u64 * SECTOR_SIZE;
                        let mut full = Vec::with_capacity(total_bytes as usize);
                        for b in op.data_pages.iter().flatten() {
                            full.extend_from_slice(b);
                        }
                        full.truncate(total_bytes as usize);
                        let cqe = self.compare_finalize(
                            op.nsid,
                            op.lba,
                            op.num_blocks,
                            full,
                            None,
                            op.cid,
                            op.sq_id,
                            op.sq_head,
                            op.cq_id,
                        );
                        self.post_cqe(ctx, op.cq_id, cqe);
                    }
                }
                PendingOp::NvmReservationCmd {
                    op_kind,
                    action,
                    rtype,
                    cptpl,
                } => {
                    // **Phase H6 + P1** — reservation cmd 数据已 DMA-read 到
                    // `data` (16 byte 或 8 byte)；按 op_kind 修 ns.reservation
                    // 状态。Register 路径会按 cptpl 触发 PTPL 持久化。
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = self.apply_reservation_cmd(
                        p.nsid, op_kind, action, rtype, cptpl, &data, p.cid, p.sq_id, p.sq_head,
                        phase,
                    );
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmCompareSinglePrp { lba, num_blocks } => {
                    // **Phase H3** — host buffer 已 DMA-read 到 `data`；
                    // 读 backing file 对应 LBA 范围 → byte-compare。
                    let bytes = num_blocks as u64 * SECTOR_SIZE;
                    let mut backing_buf = vec![0u8; bytes as usize];
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = if let Some(ns) = self.namespaces.get_mut(&p.nsid) {
                        // **Phase M2** — read_at 走 mmap 零拷贝
                        match ns.read_at(&mut backing_buf, lba * SECTOR_SIZE) {
                            Ok(()) => {
                                if data == backing_buf {
                                    tracing::debug!(
                                        lba,
                                        num_blocks,
                                        "Compare success (data == backing)"
                                    );
                                    // 真 IO 读完成 → counter（Compare 也算读 host）
                                    self.stat_host_reads += 1;
                                    self.stat_lba_read += num_blocks as u64;
                                    Cqe::success(p.cid, p.sq_id, p.sq_head, phase)
                                } else {
                                    // 找第一个差异位置便于教学日志
                                    let mismatch_at = data
                                        .iter()
                                        .zip(backing_buf.iter())
                                        .position(|(a, b)| a != b)
                                        .unwrap_or(0);
                                    tracing::warn!(
                                        lba,
                                        num_blocks,
                                        mismatch_at,
                                        "Compare FAILURE: host vs backing mismatch"
                                    );
                                    self.stat_num_err_log_entries += 1;
                                    self.push_error_log(
                                        p.sq_id,
                                        p.cid,
                                        (sc::COMPARE_FAILURE as u16) << 1,
                                        lba,
                                        1,
                                    );
                                    Cqe::error(
                                        p.cid,
                                        p.sq_id,
                                        p.sq_head,
                                        phase,
                                        sc::COMPARE_FAILURE,
                                        sc::SCT_MEDIA_DATA_INTEGRITY,
                                    )
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, lba, "Compare: backing read failed");
                                self.stat_num_err_log_entries += 1;
                                self.push_error_log(
                                    p.sq_id,
                                    p.cid,
                                    (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                    lba,
                                    1,
                                );
                                Cqe::error(
                                    p.cid,
                                    p.sq_id,
                                    p.sq_head,
                                    phase,
                                    sc::DATA_TRANSFER_ERROR,
                                    0,
                                )
                            }
                        }
                    } else {
                        tracing::warn!(nsid = p.nsid, "Compare: unknown NSID at completion");
                        Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_NAMESPACE, 0)
                    };
                    self.post_cqe(ctx, p.cq_id, cqe);
                }
                PendingOp::NvmCompareSinglePrpFused {
                    lba,
                    num_blocks,
                    write_sqe,
                    write_sq_head,
                    write_sq_id,
                } => {
                    // **Phase O3** — Fused Compare-and-Write completion (atomic chain)。
                    // Compare 完成判断：
                    //   pass → 真 dispatch Write (走标准 dispatch_io 路径)
                    //   fail → post COMPARE_FAILURE 给 Compare CID + post 同 SC 给
                    //          Write CID（spec § 6.2：fused 两条都需 individual CQE）
                    let bytes = num_blocks as u64 * SECTOR_SIZE;
                    let mut backing_buf = vec![0u8; bytes as usize];
                    let phase = self.cqs.get(&p.cq_id).map(|c| c.phase).unwrap_or(1);
                    let write_cid = write_sqe.cid();
                    let (compare_cqe, write_action) = if let Some(ns) =
                        self.namespaces.get_mut(&p.nsid)
                    {
                        match ns.read_at(&mut backing_buf, lba * SECTOR_SIZE) {
                            Ok(()) => {
                                if data == backing_buf {
                                    // Compare success → 计 counter + 真 dispatch Write
                                    self.stat_host_reads += 1;
                                    self.stat_lba_read += num_blocks as u64;
                                    tracing::info!(
                                        lba,
                                        num_blocks,
                                        "Fused C+W: Compare PASS → dispatching Write"
                                    );
                                    (Cqe::success(p.cid, p.sq_id, p.sq_head, phase), Some(()))
                                } else {
                                    // Compare fail → 两条都 fail，Write 不写盘
                                    let mismatch_at = data
                                        .iter()
                                        .zip(backing_buf.iter())
                                        .position(|(a, b)| a != b)
                                        .unwrap_or(0);
                                    tracing::warn!(
                                        lba,
                                        num_blocks,
                                        mismatch_at,
                                        "Fused C+W: Compare FAIL → aborting Write (atomic)"
                                    );
                                    self.stat_num_err_log_entries += 1;
                                    self.push_error_log(
                                        p.sq_id,
                                        p.cid,
                                        (sc::COMPARE_FAILURE as u16) << 1,
                                        lba,
                                        p.nsid,
                                    );
                                    (
                                        Cqe::error(
                                            p.cid,
                                            p.sq_id,
                                            p.sq_head,
                                            phase,
                                            sc::COMPARE_FAILURE,
                                            sc::SCT_MEDIA_DATA_INTEGRITY,
                                        ),
                                        None,
                                    )
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, lba, "Fused C+W: backing read failed");
                                self.stat_num_err_log_entries += 1;
                                self.push_error_log(
                                    p.sq_id,
                                    p.cid,
                                    (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                    lba,
                                    p.nsid,
                                );
                                (
                                    Cqe::error(
                                        p.cid,
                                        p.sq_id,
                                        p.sq_head,
                                        phase,
                                        sc::DATA_TRANSFER_ERROR,
                                        0,
                                    ),
                                    None,
                                )
                            }
                        }
                    } else {
                        (
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_NAMESPACE, 0),
                            None,
                        )
                    };
                    self.post_cqe(ctx, p.cq_id, compare_cqe);
                    match write_action {
                        Some(()) => {
                            // Compare pass → 真 dispatch Write 走 NvmWriteDmaRead /
                            // dual-PRP / PRP-list 普通路径。
                            //
                            // **Reviewer M-4 (9轮) atomicity 边界条件**：若
                            // dispatch_io 同步返 Some(error CQE)（如 bounds
                            // / SC 校验失败），driver 会看到 Compare=success +
                            // Write=error。spec § 6.2 要求 fused 两条 individual
                            // CQE 但 atomic 语义需 driver 解释：Write error 后
                            // 数据**未**写入，下次 driver 应重试整对 fused。
                            // 这教学版不做 NS-level "inconsistent" mark；真
                            // production 可加 NS health 标记。
                            if let Some(cqe) = self.dispatch_io(
                                ctx,
                                write_sq_id,
                                write_sqe,
                                write_cid,
                                write_sq_head,
                                p.cq_id,
                            ) {
                                self.post_cqe(ctx, p.cq_id, cqe);
                            }
                        }
                        None => {
                            // Compare fail → Write 也 fail（spec § 6.2：fused 两条都
                            // 个别 CQE 但同 SC；driver 看 atomic 语义保证）
                            let cqe = Cqe::error(
                                write_cid,
                                write_sq_id,
                                write_sq_head,
                                phase,
                                sc::COMPARE_FAILURE,
                                sc::SCT_MEDIA_DATA_INTEGRITY,
                            );
                            self.post_cqe(ctx, p.cq_id, cqe);
                        }
                    }
                }
                PendingOp::NvmWriteDualPrp { op_id, is_prp1 } => {
                    // 任一段到达：填入 accum 对应槽；两段都到时 dispatch 写盘。
                    // 乱序到达自动处理（C1 reviewer 指出 PRP2 先到的 data
                    // corruption + leak 在此被一并解决）。
                    let ready = if let Some(accum) = self.dual_prp_writes.get_mut(&op_id) {
                        if is_prp1 {
                            accum.prp1_data = Some(data);
                        } else {
                            accum.prp2_data = Some(data);
                        }
                        accum.prp1_data.is_some() && accum.prp2_data.is_some()
                    } else {
                        tracing::warn!(op_id, is_prp1, "DualPrp completion for unknown op_id");
                        false
                    };
                    if ready {
                        // 两段都已到达；移出 accum + 合并写文件 + 发 CQE。
                        let accum = self.dual_prp_writes.remove(&op_id).unwrap();
                        let mut full = accum.prp1_data.unwrap();
                        full.extend_from_slice(&accum.prp2_data.unwrap());
                        let bytes = accum.num_blocks as u64 * SECTOR_SIZE;
                        tracing::debug!(
                            lba = accum.lba,
                            num_blocks = accum.num_blocks,
                            full_len = full.len(),
                            expected = bytes,
                            op_id,
                            "NVM Write dual-PRP: both segments ready, writing"
                        );
                        let res = if let Some(ns) = self.namespaces.get_mut(&accum.nsid) {
                            // **Phase M2** — write_at 走 mmap 零拷贝
                            ns.write_at(&full, accum.lba * SECTOR_SIZE)
                        } else {
                            Err(std::io::Error::other(format!(
                                "unknown NSID {}",
                                accum.nsid
                            )))
                        };
                        // 注：不再 per-IO sync_data（H5）；driver 用 NVM FLUSH
                        // (opcode 0x00) 拿持久化承诺，spec-compliant 行为。
                        let cq = self.cqs.get(&accum.cq_id);
                        let phase = cq.map(|c| c.phase).unwrap_or(1);
                        let cqe = match res {
                            Ok(()) => {
                                // Phase F：dual-PRP Write 完成 → 计数。
                                self.stat_host_writes += 1;
                                self.stat_lba_written += accum.num_blocks as u64;
                                // **Reviewer H-2** — ZNS WP 推进（同 NvmWriteDmaRead）
                                if let Some(ns) = self.namespaces.get_mut(&p.nsid) {
                                    crate::controller::io::advance_zns_wp(
                                        ns,
                                        accum.lba,
                                        accum.num_blocks,
                                    );
                                }
                                Cqe::success(accum.cid, accum.sq_id, accum.sq_head, phase)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    lba = accum.lba,
                                    num_blocks = accum.num_blocks,
                                    "NVM Write dual-PRP file write failed"
                                );
                                self.stat_num_err_log_entries += 1;
                                self.push_error_log(
                                    accum.sq_id,
                                    accum.cid,
                                    (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                    accum.lba,
                                    1,
                                );
                                Cqe::error(
                                    accum.cid,
                                    accum.sq_id,
                                    accum.sq_head,
                                    phase,
                                    sc::DATA_TRANSFER_ERROR,
                                    0,
                                )
                            }
                        };
                        self.post_cqe(ctx, accum.cq_id, cqe);
                    }
                }
                PendingOp::NvmWritePrpListFetch { op_id } => {
                    // **Phase E**：PRP list 页 (4 KiB = 512 个 u64) 到达。
                    // Parse 后为 list 中每个 entry dispatch 一个 sub-DMA
                    // (dma_read 数据页到 controller)。
                    let entries = parse_prp_list(&data);
                    let needed: Option<(u32, Vec<u64>)> = self
                        .prp_list_ops
                        .get_mut(&op_id)
                        .map(|op| (op.total_pages, entries.clone()))
                        .map(|(tot, ent)| {
                            let take = (tot - 1) as usize; // PRP1 已 dispatch
                            (tot, ent.into_iter().take(take).collect())
                        });
                    let Some((total_pages, list)) = needed else {
                        tracing::warn!(op_id, "PRP list fetch for unknown op_id");
                        return;
                    };
                    if let Some(op) = self.prp_list_ops.get_mut(&op_id) {
                        op.list_entries = Some(list.clone());
                    }
                    // Issue sub-DMA reads for each list entry (page_idx 1..total)
                    for (i, gpa) in list.iter().enumerate() {
                        let page_idx = (i + 1) as u32;
                        let want_bytes = if page_idx == total_pages - 1 {
                            // 末页可能不满 4 KiB
                            let total_bytes =
                                self.prp_list_ops[&op_id].num_blocks as u64 * SECTOR_SIZE;
                            let last = total_bytes - (page_idx as u64) * NVME_PAGE_SIZE;
                            last as u32
                        } else {
                            NVME_PAGE_SIZE as u32
                        };
                        let tok = ctx.dma_read(*gpa, want_bytes);
                        // 借用 PendingIo 共用字段 sq_id/cid/sq_head/cq_id/nsid
                        let (sq_id, cid, sq_head, cq_id, nsid) = {
                            let op = &self.prp_list_ops[&op_id];
                            (op.sq_id, op.cid, op.sq_head, op.cq_id, op.nsid)
                        };
                        self.pending_ios.insert(
                            tok,
                            PendingIo {
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
                                nsid,
                                op: PendingOp::NvmWritePrpListData { op_id, page_idx },
                            },
                        );
                    }
                }
                PendingOp::NvmWritePrpListData { op_id, page_idx } => {
                    // **Phase E**：单个数据页 DMA-read 到达。填入 data_pages，
                    // 全部齐 → 合并写文件 → CQE。
                    let done_all = if let Some(op) = self.prp_list_ops.get_mut(&op_id) {
                        op.data_pages[page_idx as usize] = Some(data);
                        op.pages_done += 1;
                        op.pages_done == op.total_pages
                    } else {
                        tracing::warn!(op_id, page_idx, "PRP list data completion unknown");
                        false
                    };
                    if done_all {
                        let op = self.prp_list_ops.remove(&op_id).unwrap();
                        // 合并 data_pages → 一段连续 buffer
                        let total_bytes = op.num_blocks as u64 * SECTOR_SIZE;
                        let mut full = Vec::with_capacity(total_bytes as usize);
                        for page in op.data_pages.iter() {
                            if let Some(b) = page {
                                full.extend_from_slice(b);
                            } else {
                                tracing::warn!(op_id, "PRP list page missing on complete");
                            }
                        }
                        full.truncate(total_bytes as usize);
                        tracing::debug!(
                            op_id,
                            lba = op.lba,
                            num_blocks = op.num_blocks,
                            full_len = full.len(),
                            "NVM Write PRP-list: all pages ready, writing"
                        );
                        let res = if let Some(ns) = self.namespaces.get_mut(&op.nsid) {
                            // **Phase M2** — write_at 走 mmap 零拷贝
                            ns.write_at(&full, op.lba * SECTOR_SIZE)
                        } else {
                            Err(std::io::Error::other(format!("unknown NSID {}", op.nsid)))
                        };
                        let cq = self.cqs.get(&op.cq_id);
                        let phase = cq.map(|c| c.phase).unwrap_or(1);
                        let cqe = match res {
                            Ok(()) => {
                                // Phase F：PRP-list Write 完成 → 计数。
                                self.stat_host_writes += 1;
                                self.stat_lba_written += op.num_blocks as u64;
                                // **Reviewer H-2** — ZNS WP 推进
                                if let Some(ns) = self.namespaces.get_mut(&op.nsid) {
                                    crate::controller::io::advance_zns_wp(
                                        ns,
                                        op.lba,
                                        op.num_blocks,
                                    );
                                }
                                Cqe::success(op.cid, op.sq_id, op.sq_head, phase)
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, lba = op.lba, "PRP-list write failed");
                                self.stat_num_err_log_entries += 1;
                                self.push_error_log(
                                    op.sq_id,
                                    op.cid,
                                    (sc::DATA_TRANSFER_ERROR as u16) << 1,
                                    op.lba,
                                    1,
                                );
                                Cqe::error(
                                    op.cid,
                                    op.sq_id,
                                    op.sq_head,
                                    phase,
                                    sc::DATA_TRANSFER_ERROR,
                                    0,
                                )
                            }
                        };
                        self.post_cqe(ctx, op.cq_id, cqe);
                    }
                }
                PendingOp::NvmReadPrpListFetch { op_id } => {
                    // PRP list 页到达 (Read 路径)。Parse + dma_write 已 cached
                    // 的 per-page data 到 PRP1 + list 中每个 GPA。
                    //
                    // **C3 修复后**：dispatch_io 已按页切分 data_pages，这里
                    // 不再 re-read file，直接 take 每页 buffer dma_write。
                    let entries = parse_prp_list(&data);
                    // 提取 op 关键字段 + take 全部页 buffer
                    type ReadPrpListFetchInfo = (u32, u64, Vec<u64>, Vec<Vec<u8>>);
                    let info: Option<ReadPrpListFetchInfo> =
                        self.prp_list_ops.get_mut(&op_id).map(|op| {
                            // M2 修复：parse_prp_list 现在不再 0 终止；用
                            // total_pages-1 精确截 list 长度。
                            let take = (op.total_pages - 1) as usize;
                            let list: Vec<u64> = entries.into_iter().take(take).collect();
                            // Take 全部页（含 PRP1 = idx 0）；data_pages 移空
                            let pages: Vec<Vec<u8>> = op
                                .data_pages
                                .iter_mut()
                                .map(|p| p.take().unwrap_or_default())
                                .collect();
                            (op.total_pages, op.prp1_gpa, list, pages)
                        });
                    let Some((total_pages, prp1_gpa, list, pages)) = info else {
                        tracing::warn!(op_id, "ReadPrpListFetch unknown op_id");
                        return;
                    };
                    if let Some(op) = self.prp_list_ops.get_mut(&op_id) {
                        op.list_entries = Some(list.clone());
                    }
                    let (sq_id, cid, sq_head, cq_id, nsid) = {
                        let op = &self.prp_list_ops[&op_id];
                        (op.sq_id, op.cid, op.sq_head, op.cq_id, op.nsid)
                    };
                    // 不变量校验：list.len() + 1 (PRP1) == total_pages
                    debug_assert_eq!(
                        list.len() as u32 + 1,
                        total_pages,
                        "PRP list size != total_pages-1"
                    );
                    // **Step 2a**: dma_write PRP1 数据（page idx 0）
                    let mut pages_iter = pages.into_iter();
                    let prp1_buf = pages_iter.next().unwrap_or_default();
                    let tok_prp1 = ctx.dma_write(prp1_gpa, prp1_buf);
                    self.pending_ios.insert(
                        tok_prp1,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmReadPrpListData { op_id, page_idx: 0 },
                        },
                    );
                    // **Step 2b**: dma_write 剩余页到 list 中的 GPA
                    for (i, gpa) in list.iter().enumerate() {
                        let page_idx = (i + 1) as u32;
                        let page_buf = pages_iter.next().unwrap_or_default();
                        let tok = ctx.dma_write(*gpa, page_buf);
                        self.pending_ios.insert(
                            tok,
                            PendingIo {
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
                                nsid,
                                op: PendingOp::NvmReadPrpListData { op_id, page_idx },
                            },
                        );
                    }
                }
                PendingOp::NvmReadPrpListData { op_id, page_idx } => {
                    // 一个数据页 dma_write 完成（PRP1 或 list 中某页）。
                    let done_all = if let Some(op) = self.prp_list_ops.get_mut(&op_id) {
                        op.pages_done += 1;
                        // Read 路径：PRP1 + list 中 (total_pages-1) 个 = total_pages 个 dma_write。
                        op.pages_done >= op.total_pages
                    } else {
                        tracing::warn!(op_id, page_idx, "ReadPrpListData unknown op_id");
                        false
                    };
                    let _ = data; // Read dma_write 完成 data 为空
                    if done_all {
                        let op = self.prp_list_ops.remove(&op_id).unwrap();
                        tracing::debug!(
                            op_id,
                            lba = op.lba,
                            num_blocks = op.num_blocks,
                            "NVM Read PRP-list: all pages dma'd, posting CQE"
                        );
                        let cq = self.cqs.get(&op.cq_id);
                        let phase = cq.map(|c| c.phase).unwrap_or(1);
                        // Phase F：PRP-list Read 完成 → 计数。
                        self.stat_host_reads += 1;
                        self.stat_lba_read += op.num_blocks as u64;
                        let cqe = Cqe::success(op.cid, op.sq_id, op.sq_head, phase);
                        self.post_cqe(ctx, op.cq_id, cqe);
                    }
                }
            }
            return;
        }
        tracing::debug!(token, "DMA completion for unknown token (likely 2nd PRP)");
    }
}
