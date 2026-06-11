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
use crate::controller::SelfTestCompleted;
use crate::controller::ZoneState;
use crate::controller::parse_prp_list;
use crate::controller::try_mmap_file;
use crate::regs::*;
use pcie_device_core::*;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use zerocopy::IntoBytes;

/// **Phase S2** — Simple Copy 范围冲突检测 (NVM CS § 3.3.7)。
///
/// 给定 destination start LBA `sdlba` + 累计 dst_total LBA + source `ranges`
/// (slba, nlb) 列表，若任两 source range 互重叠 / source 与 destination
/// 区间互重叠 → 返 true（应返 SC 0x80 CONFLICTING_ATTRIBUTES）。
/// 溢出当冲突处理（更保守）。O(n²)，n ≤ 128（range list ≤ 1 page）。
pub(crate) fn check_copy_range_conflict(sdlba: u64, dst_total: u64, ranges: &[(u64, u32)]) -> bool {
    let dst_end = sdlba.checked_add(dst_total);
    ranges.iter().enumerate().any(|(i, &(s1, n1))| {
        let e1 = s1.checked_add(n1 as u64);
        let src_overlap = ranges.iter().enumerate().any(|(j, &(s2, n2))| {
            if i == j {
                return false;
            }
            let e2 = s2.checked_add(n2 as u64);
            match (e1, e2) {
                (Some(e1), Some(e2)) => s1 < e2 && s2 < e1,
                _ => true,
            }
        });
        let dst_overlap = match (e1, dst_end) {
            (Some(e1), Some(de)) => s1 < de && sdlba < e1,
            _ => true,
        };
        src_overlap || dst_overlap
    })
}

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
        //
        // **DBBUF（spec § 7.13）token 派发** —— 必须在 `!ok` 通用清理**之前**：
        // shadow-poll 读 / event_idx 写各有独立的 in-flight 表与 ok 处理（读失败要
        // 收链而非走 IO 清理路径；event_idx 写完成静默消费）。它们不属于 pending_ios /
        // pending_fetches。
        if self.pending_eventidx_writes.remove(&token) {
            // event_idx DMA-write 完成：无后续动作（成败都不影响正确性 —— 下次唤醒
            // 会重读 shadow / 重写 eventidx）。仅静默消费 token，避免落 unknown-token 警告。
            tracing::trace!(token, ok, "DBBUF: event_idx write completed");
            return;
        }
        if let Some(p) = self.pending_shadow_polls.remove(&token) {
            self.handle_shadow_poll_complete(ctx, p, ok, data);
            return;
        }
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
                    PendingOp::NvmSglFetch { op_id, .. } | PendingOp::NvmSglData { op_id, .. } => {
                        // **Phase R2** — SGL 多 fragment op：清 sibling pending +
                        // 累积器，保证只 post 一次 error CQE（同 C1 修复语义）。
                        self.pending_ios.retain(|_, q| match q.op {
                            PendingOp::NvmSglFetch { op_id: o, .. }
                            | PendingOp::NvmSglData { op_id: o, .. } => o != op_id,
                            _ => true,
                        });
                        self.sgl_ops.remove(&op_id);
                    }
                    _ => {}
                }
                self.stat_num_err_log_entries += 1;
                // Phase G：push 进 error_log 环形 buffer，Get Log Page 0x01 用。
                // sc=DATA_TRANSFER_ERROR (0x04) 在 SF bits 8..1 = 0x04 << 1。
                // 区分 NVM IO vs admin：admin SQ id=0 时 nsid=0xFFFF_FFFF
                // 表示 "not applicable"（reviewer M3 修复）。LBA 暂用 0；
                // 真要追踪需在 PendingOp 变体存 SLBA。
                let sf = sc::sf_of(sc::DATA_TRANSFER_ERROR);
                let nsid = if p.sq_id == 0 { 0xFFFF_FFFF } else { 1 };
                self.push_error_log(p.sq_id, p.cid, sf, 0, nsid);
                let cq = self.cqs.get(&p.cq_id);
                let phase = cq.map(|c| c.phase).unwrap_or(1);
                let cqe = Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR);
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
                    // **2026-06-09 纯 4K** — 按 per-NS 扇区字节算（512 或 4096）；
                    // plain NS 无 meta，扇区 = 1<<lbads。NS 缺失退回 512。
                    let sector = 1u64 << self.namespaces.get(&p.nsid).map_or(9u8, |n| n.lbads);
                    let bytes = num_blocks as u64 * sector;
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
                        ns.write_at(&data, lba * sector)
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
                                sc::sf_of(sc::DATA_TRANSFER_ERROR),
                                lba,
                                1,
                            );
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR)
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
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR)
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
                                        sc::sf_of(sc::DATA_TRANSFER_ERROR),
                                        lba,
                                        nsid,
                                    );
                                    Cqe::error(
                                        p.cid,
                                        p.sq_id,
                                        p.sq_head,
                                        phase,
                                        sc::DATA_TRANSFER_ERROR,
                                    )
                                }
                            }
                        }
                    } else {
                        Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_NAMESPACE)
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
                    // **Phase L1 + reviewer H1 + Phase Q2** — ZNS Zone Append
                    // 完成回调。
                    // 1. DMA-read 数据落盘到 assigned_lba 处（PI NS 时 per-LBA
                    //    compute tuple + interleave 4104-byte blocks 写）
                    // 2. 成功 → success CQE 携 assigned_lba（dw0=low32, dw1=hi32）
                    //    （ZNS CS § 3.2.4 要求）
                    // 3. 失败 → 回滚 WP/state 到 prev_wp/prev_state，post error CQE
                    let nsid = p.nsid;
                    let res = if let Some(ns) = self.namespaces.get_mut(&nsid) {
                        let block_bytes = ns.block_bytes() as usize;
                        let data_bytes = ns.data_bytes() as usize;
                        let pi_type = ns.pi_type;
                        let pi_first = ns.pi_first;
                        let host_bytes = num_blocks as usize * data_bytes;
                        if ns.pi_enabled() {
                            // Per-LBA compute PI tuple + interleave 4104-byte 写
                            let mut all_ok = Ok(());
                            for i in 0..num_blocks as usize {
                                let lba_i = assigned_lba + i as u64;
                                let data_chunk = &data[i * data_bytes..(i + 1) * data_bytes];
                                let tuple = crate::pi::PiTuple::compute(data_chunk, lba_i, pi_type);
                                let tuple_bytes = tuple.to_bytes();
                                let mut block = vec![0u8; block_bytes];
                                if pi_first {
                                    block[0..8].copy_from_slice(&tuple_bytes);
                                    block[8..8 + data_bytes].copy_from_slice(data_chunk);
                                } else {
                                    block[0..data_bytes].copy_from_slice(data_chunk);
                                    block[data_bytes..data_bytes + 8].copy_from_slice(&tuple_bytes);
                                }
                                if let Err(e) = ns.write_at(&block, lba_i * block_bytes as u64) {
                                    all_ok = Err(e);
                                    break;
                                }
                            }
                            all_ok
                        } else {
                            // 非 PI NS：直接写 host_bytes 字节
                            ns.write_at(&data[..host_bytes], assigned_lba * block_bytes as u64)
                        }
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
                                sc::sf_of(sc::DATA_TRANSFER_ERROR),
                                assigned_lba,
                                nsid,
                            );
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR)
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
                                sc::sf_of(sc::DATA_TRANSFER_ERROR),
                                accum.slba + written_count as u64,
                                nsid,
                            );
                            Cqe::error(
                                accum.cid,
                                accum.sq_id,
                                accum.sq_head,
                                phase,
                                sc::DATA_TRANSFER_ERROR,
                            )
                        }
                    } else {
                        Cqe::error(
                            accum.cid,
                            accum.sq_id,
                            accum.sq_head,
                            phase,
                            sc::INVALID_NAMESPACE,
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
                        let cqe =
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR);
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
                        // **2026-06-09 纯 4K** — 按 per-NS 扇区字节计算偏移。
                        let sector = 1u64 << ns.lbads;
                        // **Phase S2** — CONFLICTING_ATTRIBUTES (SC 0x80, SCT
                        // Cmd-Spec)：source ranges 互相重叠或与 [sdlba, sdlba+
                        // dst_total) destination 区间重叠时，spec § 3.3.7 (NVM
                        // CS) 要求 controller 拒绝。详见 check_copy_range_conflict。
                        if check_copy_range_conflict(sdlba, dst_total, &ranges) {
                            tracing::debug!(
                                nsid,
                                sdlba,
                                num_ranges,
                                "COPY rejected: ranges conflict (S2 SC 0x80)"
                            );
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::CONFLICTING_ATTRIBUTES)
                        } else {
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
                                Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::LBA_OUT_OF_RANGE)
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
                                        sc::sf_of(sc::DATA_TRANSFER_ERROR),
                                        sdlba,
                                        nsid,
                                    );
                                    Cqe::error(
                                        p.cid,
                                        p.sq_id,
                                        p.sq_head,
                                        phase,
                                        sc::DATA_TRANSFER_ERROR,
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
                        }
                    } else {
                        Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_NAMESPACE)
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
                PendingOp::AdminNsAttachmentList { sel } => {
                    // **Phase S4** — Controller List 4 KiB 已读完。
                    // 解析 NumIDs + cntlid[]，若包含本 controller cntlid=1
                    // → 切换 namespaces[p.nsid].attached。
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = if data.len() < 2 {
                        Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::DATA_TRANSFER_ERROR)
                    } else {
                        let num_ids = u16::from_le_bytes([data[0], data[1]]) as usize;
                        let max_ids = ((data.len() - 2) / 2).min(2047);
                        let n = num_ids.min(max_ids);
                        let mut targets_self = false;
                        for i in 0..n {
                            let off = 2 + i * 2;
                            let cntlid = u16::from_le_bytes([data[off], data[off + 1]]);
                            if cntlid == 1 {
                                // 本 controller cntlid = 1 (Identify Controller.cntlid)
                                targets_self = true;
                            }
                        }
                        if !targets_self {
                            // List 不含本 ctrl → spec § 5.20 "Controller Not Attached" /
                            // attach 时 "Namespace Not Attached" 等；简化返 success
                            // (no-op — 不针对本 ctrl)。
                            tracing::debug!(
                                nsid = p.nsid,
                                sel,
                                "NS Attachment list 不含 cntlid=1，no-op"
                            );
                            Cqe::success(p.cid, p.sq_id, p.sq_head, phase)
                        } else if let Some(ns) = self.namespaces.get_mut(&p.nsid) {
                            match sel {
                                0 => {
                                    // Attach
                                    if ns.attached {
                                        tracing::debug!(
                                            nsid = p.nsid,
                                            "NS already attached (SC 0x18)"
                                        );
                                        Cqe::error(
                                            p.cid,
                                            p.sq_id,
                                            p.sq_head,
                                            phase,
                                            sc::NAMESPACE_ALREADY_ATTACHED,
                                        )
                                    } else {
                                        ns.attached = true;
                                        tracing::info!(nsid = p.nsid, "NS attached");
                                        Cqe::success(p.cid, p.sq_id, p.sq_head, phase)
                                    }
                                }
                                1 => {
                                    // Detach
                                    if !ns.attached {
                                        tracing::debug!(
                                            nsid = p.nsid,
                                            "NS already detached (SC 0x19)"
                                        );
                                        Cqe::error(
                                            p.cid,
                                            p.sq_id,
                                            p.sq_head,
                                            phase,
                                            sc::NAMESPACE_NOT_ATTACHED,
                                        )
                                    } else {
                                        ns.attached = false;
                                        tracing::info!(nsid = p.nsid, "NS detached");
                                        Cqe::success(p.cid, p.sq_id, p.sq_head, phase)
                                    }
                                }
                                _ => {
                                    Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_FIELD)
                                }
                            }
                        } else {
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_NAMESPACE)
                        }
                    };
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
                        let cqe = Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_FIELD);
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
                        Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_FIELD)
                    } else {
                        let nsze = u64::from_le_bytes(data[0..8].try_into().unwrap());
                        let flbas = data[26] & 0x0f;
                        let dps = data[29];
                        let pi_type = dps & 0x07;
                        let pi_first = (dps & 0x08) == 0;
                        if nsze == 0 || flbas > 1 || pi_type > 1 {
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_FIELD)
                        } else {
                            let new_lbads = if flbas == 0 { 9u8 } else { 12u8 };
                            let new_meta_size = if flbas == 0 { 0u8 } else { 8u8 };
                            // 分配新 NSID（next available）+ 创 temp file backing。
                            // **review H-1（CRITICAL 越界）+ M-1（运行时上限生效）**：
                            // find 限在 1..=**运行时 max_namespaces**（≤ 编译期
                            // NAMESPACE_SLOT_CAPACITY 槽位容量）。① 防 DenseMap 越界
                            // 静默丢弃 + temp 文件泄漏（H-1）；② 真正 enforce
                            // `--max-namespaces N` 的模拟上限——满时返
                            // NAMESPACE_ID_UNAVAILABLE 而非继续创到槽位容量（M-1）。
                            let ns_limit = self.max_namespaces;
                            let new_nsid = (1..=ns_limit)
                                .find(|n| !self.namespaces.contains_key(n))
                                .unwrap_or(0);
                            if new_nsid == 0 {
                                // 无空闲 NSID（已达硬上限）→ Namespace Identifier
                                // Unavailable。在创 temp 文件**之前**返回，无泄漏。
                                Cqe::error(
                                    p.cid,
                                    p.sq_id,
                                    p.sq_head,
                                    phase,
                                    sc::NAMESPACE_ID_UNAVAILABLE,
                                )
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
                                                nswp: 0,
                                                attached: true,
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
                    let total_bytes = {
                        // **2026-06-09 纯 4K** — 按 per-NS 扇区算总字节。
                        let sector = 1u64 << self.namespaces.get(&nsid).map_or(9u8, |n| n.lbads);
                        num_blocks as u64 * sector
                    };
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
                        // **2026-06-09 纯 4K** — 按 per-NS 扇区算总字节。
                        let sector = 1u64 << self.namespaces.get(&op.nsid).map_or(9u8, |n| n.lbads);
                        let total_bytes = op.num_blocks as u64 * sector;
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
                    // **2026-06-09 纯 4K** — 按 per-NS 扇区算字节/偏移。
                    let sector = 1u64 << self.namespaces.get(&p.nsid).map_or(9u8, |n| n.lbads);
                    let bytes = num_blocks as u64 * sector;
                    let mut backing_buf = vec![0u8; bytes as usize];
                    let cq = self.cqs.get(&p.cq_id);
                    let phase = cq.map(|c| c.phase).unwrap_or(1);
                    let cqe = if let Some(ns) = self.namespaces.get_mut(&p.nsid) {
                        // **Phase M2** — read_at 走 mmap 零拷贝
                        match ns.read_at(&mut backing_buf, lba * sector) {
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
                                        sc::sf_of(sc::COMPARE_FAILURE),
                                        lba,
                                        1,
                                    );
                                    Cqe::error(
                                        p.cid,
                                        p.sq_id,
                                        p.sq_head,
                                        phase,
                                        sc::COMPARE_FAILURE,
                                    )
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, lba, "Compare: backing read failed");
                                self.stat_num_err_log_entries += 1;
                                self.push_error_log(
                                    p.sq_id,
                                    p.cid,
                                    sc::sf_of(sc::DATA_TRANSFER_ERROR),
                                    lba,
                                    1,
                                );
                                Cqe::error(
                                    p.cid,
                                    p.sq_id,
                                    p.sq_head,
                                    phase,
                                    sc::DATA_TRANSFER_ERROR,
                                )
                            }
                        }
                    } else {
                        tracing::warn!(nsid = p.nsid, "Compare: unknown NSID at completion");
                        Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_NAMESPACE)
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
                    let sector = 1u64 << self.namespaces.get(&p.nsid).map_or(9u8, |n| n.lbads);
                    let bytes = num_blocks as u64 * sector;
                    let mut backing_buf = vec![0u8; bytes as usize];
                    let phase = self.cqs.get(&p.cq_id).map(|c| c.phase).unwrap_or(1);
                    let write_cid = write_sqe.cid();
                    let (compare_cqe, write_action) = if let Some(ns) =
                        self.namespaces.get_mut(&p.nsid)
                    {
                        match ns.read_at(&mut backing_buf, lba * sector) {
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
                                        sc::sf_of(sc::COMPARE_FAILURE),
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
                                    sc::sf_of(sc::DATA_TRANSFER_ERROR),
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
                                    ),
                                    None,
                                )
                            }
                        }
                    } else {
                        (
                            Cqe::error(p.cid, p.sq_id, p.sq_head, phase, sc::INVALID_NAMESPACE),
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
                        // **2026-06-09 纯 4K** — 按 per-NS 扇区字节计算偏移。
                        let sector =
                            1u64 << self.namespaces.get(&accum.nsid).map_or(9u8, |n| n.lbads);
                        let bytes = accum.num_blocks as u64 * sector;
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
                            ns.write_at(&full, accum.lba * sector)
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
                                    sc::sf_of(sc::DATA_TRANSFER_ERROR),
                                    accum.lba,
                                    1,
                                );
                                Cqe::error(
                                    accum.cid,
                                    accum.sq_id,
                                    accum.sq_head,
                                    phase,
                                    sc::DATA_TRANSFER_ERROR,
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
                            // **2026-06-09 纯 4K** — 按 per-NS 扇区算总字节。
                            let op = &self.prp_list_ops[&op_id];
                            let sector =
                                1u64 << self.namespaces.get(&op.nsid).map_or(9u8, |n| n.lbads);
                            let total_bytes = op.num_blocks as u64 * sector;
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
                        // **2026-06-09 纯 4K** — 按 per-NS 扇区算总字节/偏移。
                        let sector = 1u64 << self.namespaces.get(&op.nsid).map_or(9u8, |n| n.lbads);
                        let total_bytes = op.num_blocks as u64 * sector;
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
                            ns.write_at(&full, op.lba * sector)
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
                                    sc::sf_of(sc::DATA_TRANSFER_ERROR),
                                    op.lba,
                                    1,
                                );
                                Cqe::error(
                                    op.cid,
                                    op.sq_id,
                                    op.sq_head,
                                    phase,
                                    sc::DATA_TRANSFER_ERROR,
                                )
                            }
                        };
                        self.post_cqe(ctx, op.cq_id, cqe);
                    }
                }
                PendingOp::NvmReadPrpListFetch { op_id } => {
                    // PRP list 页到达 (device→host：Read / Get Log Page / Zone+Reservation
                    // Report)。**C1② PRP-list chaining**：列表可能跨多页——非末页的末位
                    // entry 是指向下一 list 页的 **chain pointer**（spec § 4.1.2）。本臂做
                    // walk：累积 data-page GPA 到 `list_entries`，遇 chain 就再 DMA-read 下一
                    // list 页（重入本臂）；walk 集齐 (total_pages-1) 个 data GPA 后再 scatter。
                    let entries = parse_prp_list(&data);
                    let entries_per_page = (NVME_PAGE_SIZE / 8) as usize; // 512
                    let chain_ptr: Option<u64> = {
                        let Some(op) = self.prp_list_ops.get_mut(&op_id) else {
                            tracing::warn!(op_id, "ReadPrpListFetch unknown op_id");
                            return;
                        };
                        let needed = (op.total_pages - 1) as usize; // data-page GPA 总数
                        let acc = op.list_entries.get_or_insert_with(Vec::new);
                        let remaining = needed - acc.len();
                        // **panic-free（reviewer MEDIUM-1）**：截断的 DMA read 可能让
                        // `entries.len()` < 预期，用 iter().take() 而非切片下标，避免越界 panic。
                        if remaining > entries_per_page {
                            // 满页 chained：前 (entries_per_page-1) 个是 data，末位是 chain。
                            let n_data = entries_per_page - 1;
                            acc.extend(entries.iter().take(n_data).copied());
                            // 末位 chain pointer；若页被截断（短读）→ 无 chain，按最终页收尾。
                            entries.get(entries_per_page - 1).copied()
                        } else {
                            // 最终 list 页：前 `remaining` 个全是 data，无 chain。
                            acc.extend(entries.iter().take(remaining).copied());
                            None
                        }
                    };
                    let (sq_id, cid, sq_head, cq_id, nsid) = {
                        let op = &self.prp_list_ops[&op_id];
                        (op.sq_id, op.cid, op.sq_head, op.cq_id, op.nsid)
                    };
                    if let Some(next_list_gpa) = chain_ptr {
                        // 继续 walk：DMA-read 下一 list 页（重入 NvmReadPrpListFetch）。
                        let tok = ctx.dma_read(next_list_gpa, NVME_PAGE_SIZE as u32);
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
                        return;
                    }
                    // walk 完成 → scatter。take 全部页 buffer + list_entries。
                    let (total_pages, prp1_gpa, list, pages) = {
                        // op present：本调用上方 1768 行已确认、且本（非 chain）路径未移除。
                        let op = self.prp_list_ops.get_mut(&op_id).unwrap();
                        let list = op.list_entries.clone().unwrap_or_default();
                        let pages: Vec<Vec<u8>> = op
                            .data_pages
                            .iter_mut()
                            .map(|p| p.take().unwrap_or_default())
                            .collect();
                        (op.total_pages, op.prp1_gpa, list, pages)
                    };
                    // 不变量：list.len() + 1 (PRP1) == total_pages（chaining 后仍成立）。
                    debug_assert_eq!(
                        list.len() as u32 + 1,
                        total_pages,
                        "PRP list (chained) size != total_pages-1"
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
                        // Phase F：PRP-list Read 完成 → 计数 host read。
                        // **P2 (2026-06-10)**：admin/report 数据写（Get Log Page /
                        // Zone Report / Reservation Report）也复用本 PRP-list 机件，
                        // 它们 num_blocks=0、不是 NVM host read → 守 `> 0` 不污染 SMART
                        // （与 NvmReadDmaWrite success arm 一致）。
                        if op.num_blocks > 0 {
                            self.stat_host_reads += 1;
                            self.stat_lba_read += op.num_blocks as u64;
                        }
                        let cqe = Cqe::success(op.cid, op.sq_id, op.sq_head, phase);
                        self.post_cqe(ctx, op.cq_id, cqe);
                    }
                }
                PendingOp::NvmSglFetch { op_id, is_last } => {
                    // **Phase R2a/R2b** — SGL segment 页到达：parse descriptor →
                    // 累积 fragment plan（每片 host address + 数据流偏移 + 长度）。
                    //
                    // `is_last`：本段经哪种 descriptor 到达——
                    //   - true（经 Last Segment 到达，spec § 4.4）：本段全是数据
                    //     descriptor（Data Block / Bit Bucket），无 continuation。
                    //   - false（经 Segment 到达）：本段**末位** descriptor 是
                    //     continuation（Segment / Last Segment）指向下一段，前缀
                    //     才是数据 descriptor → 递归 fetch 下一段（R2b chain）。
                    // 全段 walk 完后（is_last 段处理完）启动逐 fragment 传输。
                    //
                    // R2a：仅 Data Block。Bit Bucket → R2c；Keyed → reject。
                    let descs = match crate::sgl::parse_sgl_list(&data) {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::warn!(op_id, err = e, "SGL segment 解析失败");
                            // segment 字节非 16 倍数等 = segment descriptor 本身非法。
                            self.finish_sgl_error(ctx, op_id, sc::INVALID_SGL_SEGMENT_DESCRIPTOR);
                            return;
                        }
                    };
                    // segment-hop 上限：防恶意 driver 构造 Segment 自环无限 fetch。
                    let hops = match self.sgl_ops.get_mut(&op_id) {
                        Some(op) => {
                            op.walk_segments += 1;
                            op.walk_segments
                        }
                        None => {
                            tracing::warn!(op_id, "NvmSglFetch unknown op_id");
                            return;
                        }
                    };
                    if hops > crate::controller::MAX_SGL_SEGMENTS {
                        tracing::warn!(op_id, hops, "SGL segment chain 超上限（疑似自环）");
                        self.finish_sgl_error(ctx, op_id, sc::SGL_INVALID_NUMBER_OF_DESCRIPTORS);
                        return;
                    }
                    // 分出数据 descriptor 与 continuation：
                    //   is_last：全部是数据 descriptor，无 continuation。
                    //   !is_last：末位须 (Last)Segment continuation，前缀是数据。
                    let (data_descs, cont): (
                        &[crate::sgl::SglDescriptor],
                        Option<&crate::sgl::SglDescriptor>,
                    ) = if is_last {
                        (&descs[..], None)
                    } else if let Some((last, head)) = descs.split_last() {
                        (head, Some(last))
                    } else {
                        // 非-last 段必须含 ≥1 descriptor（至少 continuation）。
                        tracing::warn!(op_id, "SGL 非-last segment 为空");
                        self.finish_sgl_error(ctx, op_id, sc::SGL_DESCRIPTOR_TYPE_INVALID);
                        return;
                    };
                    // 处理数据 descriptor → append op.frags（stream offset 由
                    // firmware 自算的 walk_offset 决定，非 driver 输入）。
                    let mut walk_err: Option<u16> = None;
                    {
                        let op = self.sgl_ops.get_mut(&op_id).unwrap();
                        for d in data_descs {
                            if d.sub_type != 0 {
                                tracing::warn!(sub_type = d.sub_type, "SGL fragment sub_type 非 0");
                                walk_err = Some(sc::SGL_DESCRIPTOR_TYPE_INVALID);
                                break;
                            }
                            match d.sgl_type {
                                crate::sgl::SglType::DataBlock => {
                                    // 0 长度 Data Block 无数据传输：跳过（不 push
                                    // frag / 不发 DMA），防恶意 driver 用海量
                                    // 0-length block 制造 no-op DMA 放大（覆盖检查
                                    // 不约束其数量）。
                                    if d.length == 0 {
                                        continue;
                                    }
                                    op.frags.push(crate::controller::SglPlanFrag {
                                        address: d.address,
                                        stream_offset: op.walk_offset,
                                        length: d.length,
                                    });
                                    op.walk_offset = op.walk_offset.saturating_add(d.length as u64);
                                }
                                crate::sgl::SglType::BitBucket => {
                                    // **Phase R2c** — Bit Bucket（spec § 4.4.1）。
                                    // READ (controller→host)：跳过 length 字节输出
                                    //   ——advance stream offset（discard 这段 backing
                                    //   数据，**不**发 dma_write），其后 fragment 的
                                    //   stream 偏移据此前移。
                                    // WRITE (host→controller)：spec 规定 Bit Bucket
                                    //   length **视为 0**（如同不存在）——不 advance、
                                    //   不 DMA。
                                    // 两方向都不产生 transfer fragment。
                                    if !op.is_write {
                                        op.walk_offset =
                                            op.walk_offset.saturating_add(d.length as u64);
                                    }
                                }
                                other => {
                                    // 数据位置出现 (Last)Segment = 非法（chain 只能
                                    // 在段末位）；Keyed/Transport → reject (NVMe-oF)。
                                    tracing::warn!(
                                        kind = ?other,
                                        "SGL 数据位置非 Data Block/Bit Bucket (chain 须末位)"
                                    );
                                    walk_err = Some(sc::SGL_DESCRIPTOR_TYPE_INVALID);
                                    break;
                                }
                            }
                        }
                    }
                    if let Some(sc_byte) = walk_err {
                        self.finish_sgl_error(ctx, op_id, sc_byte);
                        return;
                    }
                    match cont {
                        None => {
                            // Last Segment 段处理完 → walk 结束，启动数据传输。
                            self.start_sgl_transfer(ctx, op_id);
                        }
                        Some(c) => {
                            // **R2b** — 末位 continuation：用与 SGL1 同一份 wire
                            // 校验谓词（type ∈ {Segment, Last Segment}、sub_type=0、
                            // length 16 倍数 / 非零 / ≤1 page）后 DMA-read 下一段。
                            let (next_len, next_is_last) =
                                match crate::controller::io::validate_segment_pointer(c) {
                                    Ok(t) => t,
                                    Err(sc_byte) => {
                                        self.finish_sgl_error(ctx, op_id, sc_byte);
                                        return;
                                    }
                                };
                            let (sq_id, cid, sq_head, cq_id, nsid) = {
                                let op = &self.sgl_ops[&op_id];
                                (op.sq_id, op.cid, op.sq_head, op.cq_id, op.nsid)
                            };
                            let tok = ctx.dma_read(c.address, next_len);
                            self.pending_ios.insert(
                                tok,
                                PendingIo {
                                    sq_id,
                                    cid,
                                    sq_head,
                                    cq_id,
                                    nsid,
                                    op: PendingOp::NvmSglFetch {
                                        op_id,
                                        is_last: next_is_last,
                                    },
                                },
                            );
                        }
                    }
                }
                PendingOp::NvmSglData { op_id, frag_idx } => {
                    // **Phase R2a** — 一个数据 fragment DMA 完成。
                    // WRITE：把 dma_read 回来的 host 数据拷进 gather buffer（按
                    // 该 fragment 的 stream 偏移）。READ：scatter 已完成无需回写。
                    let (done_all, bad) = if let Some(op) = self.sgl_ops.get_mut(&op_id) {
                        let mut bad = false;
                        if op.is_write {
                            if let Some(frag) = op.frags.get(frag_idx as usize).copied() {
                                let off = frag.stream_offset as usize;
                                let end = off.saturating_add(frag.length as usize);
                                if data.len() == frag.length as usize && end <= op.data.len() {
                                    op.data[off..end].copy_from_slice(&data);
                                } else {
                                    tracing::warn!(
                                        op_id,
                                        frag_idx,
                                        got = data.len(),
                                        want = frag.length,
                                        "SGL gather fragment 字节数异常"
                                    );
                                    bad = true;
                                }
                            } else {
                                tracing::warn!(op_id, frag_idx, "SGL gather frag_idx 越界");
                                bad = true;
                            }
                        }
                        op.transfers_done += 1;
                        (op.transfers_done >= op.transfers_total, bad)
                    } else {
                        tracing::warn!(op_id, frag_idx, "NvmSglData unknown op_id");
                        (false, false)
                    };
                    if bad {
                        self.finish_sgl_error(ctx, op_id, sc::DATA_TRANSFER_ERROR);
                    } else if done_all {
                        self.finish_sgl_done(ctx, op_id);
                    }
                }
            }
            return;
        }
        tracing::debug!(token, "DMA completion for unknown token (likely 2nd PRP)");
    }

    /// **Phase R2** — segment walk 全部完成后启动数据传输阶段。
    /// 先校验 fragment 覆盖正好 == expected_bytes（不足/超出都是 driver 编码
    /// 错），再逐 fragment dispatch：READ = 从 data 切片 dma_write(scatter)；
    /// WRITE = dma_read(gather) host 后填 data。0 fragment（仅当 expected==0）
    /// 直接完成。
    fn start_sgl_transfer(&mut self, ctx: &mut DeviceCtx<'_>, op_id: u64) {
        let (walk_offset, expected) = match self.sgl_ops.get(&op_id) {
            Some(op) => (op.walk_offset, op.expected_bytes),
            None => {
                tracing::warn!(op_id, "start_sgl_transfer unknown op_id");
                return;
            }
        };
        if walk_offset != expected {
            tracing::warn!(
                op_id,
                got = walk_offset,
                expected,
                "SGL fragment 覆盖与传输大小不符"
            );
            // **R2d** — 数据 SGL 总长度与命令传输大小不符 → Data SGL Length Invalid。
            self.finish_sgl_error(ctx, op_id, sc::DATA_SGL_LENGTH_INVALID);
            return;
        }
        // 取出 plan + 上下文（clone plan 以脱离对 sgl_ops 的借用，便于循环内
        // 同时读 op.data 切片）。
        let (sq_id, cid, sq_head, cq_id, nsid, is_write, plan) = {
            let op = self.sgl_ops.get_mut(&op_id).unwrap();
            op.transfers_total = op.frags.len() as u32;
            (
                op.sq_id,
                op.cid,
                op.sq_head,
                op.cq_id,
                op.nsid,
                op.is_write,
                op.frags.clone(),
            )
        };
        if plan.is_empty() {
            // 0 fragment（expected 也应为 0，已被上面覆盖检查挡掉非零情形）→
            // 无数据传输，直接完成。
            self.finish_sgl_done(ctx, op_id);
            return;
        }
        for (idx, frag) in plan.iter().enumerate() {
            let frag_idx = idx as u32;
            let tok = if is_write {
                // WRITE gather：dma_read host → 后续填 data。
                ctx.dma_read(frag.address, frag.length)
            } else {
                // READ scatter：从 data 切片 dma_write 到 host。
                let off = frag.stream_offset as usize;
                let end = off + frag.length as usize;
                let slice = self.sgl_ops[&op_id].data[off..end].to_vec();
                ctx.dma_write(frag.address, slice)
            };
            self.pending_ios.insert(
                tok,
                PendingIo {
                    sq_id,
                    cid,
                    sq_head,
                    cq_id,
                    nsid,
                    op: PendingOp::NvmSglData { op_id, frag_idx },
                },
            );
        }
    }

    /// **Phase R2** — SGL op 全 fragment 传输完成 → 终结。
    /// WRITE：把 gather buffer 一次性写 backing 再 post CQE；READ：data 已
    /// scatter 到 host，直接 post success CQE。
    fn finish_sgl_done(&mut self, ctx: &mut DeviceCtx<'_>, op_id: u64) {
        let Some(op) = self.sgl_ops.remove(&op_id) else {
            return;
        };
        let cq = self.cqs.get(&op.cq_id);
        let phase = cq.map(|c| c.phase).unwrap_or(1);
        if op.is_write {
            let sector = op.sector_bytes;
            let res = if let Some(ns) = self.namespaces.get_mut(&op.nsid) {
                ns.write_at(&op.data, op.lba * sector)
            } else {
                Err(std::io::Error::other(format!("unknown NSID {}", op.nsid)))
            };
            let cqe = match res {
                Ok(()) => {
                    self.stat_host_writes += 1;
                    self.stat_lba_written += op.num_blocks as u64;
                    if let Some(ns) = self.namespaces.get_mut(&op.nsid) {
                        crate::controller::io::advance_zns_wp(ns, op.lba, op.num_blocks);
                    }
                    Cqe::success(op.cid, op.sq_id, op.sq_head, phase)
                }
                Err(e) => {
                    tracing::warn!(error = %e, lba = op.lba, "SGL Write backing 写失败");
                    self.stat_num_err_log_entries += 1;
                    self.push_error_log(
                        op.sq_id,
                        op.cid,
                        sc::sf_of(sc::DATA_TRANSFER_ERROR),
                        op.lba,
                        op.nsid,
                    );
                    Cqe::error(op.cid, op.sq_id, op.sq_head, phase, sc::DATA_TRANSFER_ERROR)
                }
            };
            self.post_cqe(ctx, op.cq_id, cqe);
        } else {
            self.stat_host_reads += 1;
            self.stat_lba_read += op.num_blocks as u64;
            let cqe = Cqe::success(op.cid, op.sq_id, op.sq_head, phase);
            self.post_cqe(ctx, op.cq_id, cqe);
        }
    }

    /// **Phase R2** — SGL op 出错：清同 op 的 sibling pending + 累积器 +
    /// 记 error log + post 一次 error CQE（保证 spec § 4.6.1 "one CQE per
    /// command"）。
    fn finish_sgl_error(&mut self, ctx: &mut DeviceCtx<'_>, op_id: u64, status: u16) {
        self.pending_ios.retain(|_, q| match q.op {
            PendingOp::NvmSglFetch { op_id: o, .. } | PendingOp::NvmSglData { op_id: o, .. } => {
                o != op_id
            }
            _ => true,
        });
        if let Some(op) = self.sgl_ops.remove(&op_id) {
            let cq = self.cqs.get(&op.cq_id);
            let phase = cq.map(|c| c.phase).unwrap_or(1);
            self.stat_num_err_log_entries += 1;
            self.push_error_log(op.sq_id, op.cid, sc::sf_of(status), op.lba, op.nsid);
            let cqe = Cqe::error(op.cid, op.sq_id, op.sq_head, phase, status);
            self.post_cqe(ctx, op.cq_id, cqe);
        }
    }
}
