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
use crate::controller::ZnsState;
use crate::controller::ZoneState;
use crate::regs::*;
use pcie_remote_userspace_sdk::*;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

/// **Reviewer H2** — Zone State Machine spec 一致性（ZNS CS § 4.4 Figure
/// "Zone State Machine"）。返回 `None` = 合法 transition，`Some(sc)` = SC byte。
///
/// 真硬件的 transition matrix（来源 spec）：
///
/// | from \ ZSA | Close(1) | Finish(2) | Open(3)  | Reset(4) | Offline(5) |
/// |------------|----------|-----------|----------|----------|------------|
/// | Empty      | -        | OK        | OK       | OK(no-op)| ZSTI       |
/// | ImplOpen   | OK       | OK        | OK       | OK       | ZSTI       |
/// | ExplOpen   | OK       | OK        | OK(no-op)| OK       | ZSTI       |
/// | Closed     | OK(no-op)| OK        | OK       | OK       | ZSTI       |
/// | Full       | -        | -(no-op)  | ZSTI     | OK       | OK         |
/// | ReadOnly   | ZRO      | ZRO       | ZRO      | ZRO      | OK         |
/// | Offline    | ZOFF     | ZOFF      | ZOFF     | ZOFF     | OK(no-op)  |
///
/// 缩写：ZSTI = INVALID_ZONE_STATE_TRANSITION (0xBF), ZRO = ZONE_IS_READ_ONLY,
/// ZOFF = ZONE_IS_OFFLINE。`-` = no-op（成功无副作用）。
///
/// 不在表中的 ZSA → INVALID_FIELD。
pub(crate) fn check_zsa_transition(state: ZoneState, zsa: u8) -> Option<u8> {
    use ZoneState::*;
    match (state, zsa) {
        // Close
        (Empty, 0x01) | (Full, 0x01) => None, // no-op
        (ImplicitOpen | ExplicitOpen | Closed, 0x01) => None,
        (ReadOnly, 0x01) => Some(sc::ZONE_IS_READ_ONLY),
        (Offline, 0x01) => Some(sc::ZONE_IS_OFFLINE),
        // Finish
        (Empty | ImplicitOpen | ExplicitOpen | Closed | Full, 0x02) => None,
        (ReadOnly, 0x02) => Some(sc::ZONE_IS_READ_ONLY),
        (Offline, 0x02) => Some(sc::ZONE_IS_OFFLINE),
        // Open
        (Empty | ImplicitOpen | ExplicitOpen | Closed, 0x03) => None,
        (Full, 0x03) => Some(sc::INVALID_ZONE_STATE_TRANSITION),
        (ReadOnly, 0x03) => Some(sc::ZONE_IS_READ_ONLY),
        (Offline, 0x03) => Some(sc::ZONE_IS_OFFLINE),
        // Reset
        (Empty | ImplicitOpen | ExplicitOpen | Closed | Full, 0x04) => None,
        (ReadOnly, 0x04) => Some(sc::ZONE_IS_READ_ONLY),
        (Offline, 0x04) => Some(sc::ZONE_IS_OFFLINE),
        // Offline（只能从 Full/ReadOnly/Offline 进入；ImplicitOpen 等违反 SWR）
        (Full | ReadOnly | Offline, 0x05) => None,
        (Empty | ImplicitOpen | ExplicitOpen | Closed, 0x05) => {
            Some(sc::INVALID_ZONE_STATE_TRANSITION)
        }
        // Unknown ZSA
        _ => Some(sc::INVALID_FIELD),
    }
}

/// **Phase L1 + reviewer M1** — 构造 ZNS Report Zones 响应 buffer
/// (spec ZNS § 4.5.2)。
///
/// 64-byte header + 64-byte * 每 zone descriptor。
///
/// 边界：driver 可能传 bytes < 64 来 size-probe。我们：
/// - 始终返回 `bytes` 字节（spec 要求 buffer 大小完全等于 driver 请求）
/// - 即使 bytes < 8 也填进截断后的 NZ 字段（driver 至少能看到部分计数）
/// - report_n 字段反映实际能装下的 zone 数（容纳 driver 用小 buf 试探）
fn build_zone_report(zns: &ZnsState, start_zone_idx: usize, bytes: usize) -> Vec<u8> {
    let n_zones = zns.zones.len();
    let max_in_buf = if bytes > 64 { (bytes - 64) / 64 } else { 0 };
    let report_n = max_in_buf.min(n_zones.saturating_sub(start_zone_idx));
    let mut buf = vec![0u8; bytes];
    // Header: bytes 0..8 = NZ (number of zones in this response)
    // 即使 bytes < 8，也要把能写的字节写进去（driver 看到 partial NZ 仍能
    // 推断 buffer 太小 → 用更大 buf 重试）
    let nz_bytes = (report_n as u64).to_le_bytes();
    let nz_len = nz_bytes.len().min(bytes);
    buf[..nz_len].copy_from_slice(&nz_bytes[..nz_len]);
    // bytes 8..64 reserved
    // Per-zone descriptor @ 64 + i * 64
    for (i, idx) in (start_zone_idx..start_zone_idx + report_n).enumerate() {
        let zone = &zns.zones[idx];
        let off = 64 + i * 64;
        if off + 64 > buf.len() {
            break;
        }
        // byte 0: ZT (Zone Type) — 2 = Sequential Write Required
        buf[off] = 0x02;
        // byte 1: ZS (Zone State) bits 7:4
        buf[off + 1] = match zone.state {
            ZoneState::Empty => 0x10,
            ZoneState::ImplicitOpen => 0x20,
            ZoneState::ExplicitOpen => 0x30,
            ZoneState::Closed => 0x40,
            ZoneState::ReadOnly => 0xD0,
            ZoneState::Full => 0xE0,
            ZoneState::Offline => 0xF0,
        };
        // byte 2: ZA (Zone Attributes) — 0
        // byte 3 reserved
        // bytes 8..16: ZCAP (Zone Capacity)
        buf[off + 8..off + 16].copy_from_slice(&zns.zone_capacity.to_le_bytes());
        // bytes 16..24: ZSLBA (Zone Start LBA)
        let zslba = (idx as u64) * zns.zone_size;
        buf[off + 16..off + 24].copy_from_slice(&zslba.to_le_bytes());
        // bytes 24..32: WP (Write Pointer，绝对 LBA)
        let wp_abs = zslba + zone.write_pointer;
        buf[off + 24..off + 32].copy_from_slice(&wp_abs.to_le_bytes());
        // bytes 32..64 reserved / vendor
    }
    buf
}

/// Test-only wrapper：保留 `build_zone_report` 私有，但单元测试需要调它。
#[cfg(test)]
pub(crate) fn __test_build_zone_report(
    zns: &ZnsState,
    start_zone_idx: usize,
    bytes: usize,
) -> Vec<u8> {
    build_zone_report(zns, start_zone_idx, bytes)
}

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
        // **reviewer H2 修复** — NVMe spec § 5.26：Sanitize in-progress
        // 时所有 IO（除 Sanitize Status / AER）必须 abort with SC 0x12
        // 'Sanitize In Progress'。Driver 会等 Sanitize 完成再重试。
        if self.sanitize.is_some() {
            tracing::warn!(opc = sqe.opcode(), "IO rejected: Sanitize in progress");
            return Some(Cqe::error(
                cid,
                sq_id,
                sq_head,
                phase,
                sc::SANITIZE_IN_PROGRESS,
                0,
            ));
        }
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
                // **Phase H7** — PRACT (Protection Information Action) bit 29
                // 表示 driver 期望 controller 自动校验/生成 PI tuple。我们
                // 没真实现 CRC/RefTag 引擎；driver 若发 PRACT=1 我们 reject
                // 让其 fallback 到 PI=0 path（spec § 8.3：controller 可返
                // INVALID_PROTECTION_INFO = SC 0x81，但 INVALID_FIELD 也
                // 让 driver 知道）。
                if (cdw12 >> 29) & 0x1 != 0 {
                    tracing::warn!(nsid, "NVM READ with PRACT=1 not supported");
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
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
                // **Phase K1/K4** — IO 路径分流：
                //   - LBAF[0] (512B no-meta no-PI) → 默认走原路径
                //   - LBAF[1] (4 KiB + 8B meta) + PI Type 1 + 单 LBA →
                //     走 K4a/K4b 真 PI 路径
                //   - 其它（如 LBAF[1] 多 LBA、Type 2/3）→ INVALID_FIELD
                //     直到 K4c 多 LBA 路径完整实现
                let is_pi_path = ns.lbads == 12 && ns.meta_size == 8 && ns.pi_type == 1;
                if !is_pi_path && (ns.lbads != 9 || ns.meta_size != 0 || ns.pi_enabled()) {
                    // **Reviewer H5** — 用 INVALID_PROTECTION_INFO 而非 INVALID_FIELD
                    // 让 driver 区分 "PI 格式不受支持" vs "命令字段错误"。
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_PROTECTION_INFO,
                        0,
                    ));
                }
                if is_pi_path && nlb != 1 {
                    // K4a/b 单 LBA only；多 LBA 路径 (K4c) 限定 ≤ 1 page 因
                    // 单 4 KiB 只能装 1 个 LBA，driver 用多 LBA 必发 ≥ 2 page
                    // = dual-PRP / PRP-list 路径才能传完整 data；这里
                    // single-PRP entry 已最大 1 LBA。
                    tracing::error!(
                        nsid,
                        nlb,
                        "multi-LBA PI Read rejected (K4c TODO; use INVALID_PROTECTION_INFO)"
                    );
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_PROTECTION_INFO,
                        0,
                    ));
                }
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
                // **Phase K4b** — PI 路径单 LBA：file size = 4096 + 8 = 4104；
                // 拆 data / tuple → verify → 只 dma_write data 部分给 PRP1
                if is_pi_path {
                    let pi_type = ns.pi_type;
                    let pi_first = ns.pi_first;
                    let block_bytes = ns.block_bytes() as usize; // 4104
                    let data_bytes = ns.data_bytes() as usize; // 4096
                    let mut block_buf = vec![0u8; block_bytes];
                    let ns_mut = self.ns_mut(nsid).unwrap();
                    if let Err(e) = ns_mut
                        .file
                        .seek(SeekFrom::Start(slba * block_bytes as u64))
                        .and_then(|_| ns_mut.file.read_exact(&mut block_buf))
                    {
                        tracing::warn!(error = %e, nsid, slba, "PI READ: backing read failed");
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
                            0,
                        ));
                    }
                    // 拆 data / tuple — pi_first 决定 tuple 在头还是尾
                    let (data_slice, tuple_slice) = if pi_first {
                        (&block_buf[8..8 + data_bytes], &block_buf[0..8])
                    } else {
                        (
                            &block_buf[0..data_bytes],
                            &block_buf[data_bytes..data_bytes + 8],
                        )
                    };
                    let tuple_arr: [u8; 8] = tuple_slice.try_into().unwrap();
                    let pi = crate::pi::PiTuple::from_bytes(&tuple_arr);
                    let check = pi.verify(data_slice, slba, pi_type);
                    if let Some(sc) = check.to_sc() {
                        // PI 校验失败 → 返 Media/Data Integrity SC
                        tracing::warn!(nsid, slba, ?check, "PI READ verify FAIL");
                        self.stat_num_err_log_entries += 1;
                        self.push_error_log(sq_id, cid, (sc as u16) << 1, slba, nsid);
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc, 0));
                    }
                    // verify OK → dma_write 仅 data 部分 (4 KiB)
                    let tok = ctx.dma_write(prp1, data_slice.to_vec());
                    self.pending_ios.insert(
                        tok,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmReadPiDmaWrite { num_blocks: nlb },
                        },
                    );
                    return None;
                }
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
                // Phase H7：PRACT bit 29，参 READ 注释
                if (cdw12 >> 29) & 0x1 != 0 {
                    tracing::warn!(nsid, "NVM WRITE with PRACT=1 not supported");
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
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
                if ns.lbads != 9 || ns.meta_size != 0 || ns.pi_enabled() {
                    // **Phase K4a** — PI 单 LBA Write 路径
                    let is_pi_path = ns.lbads == 12 && ns.meta_size == 8 && ns.pi_type == 1;
                    if !is_pi_path {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::INVALID_PROTECTION_INFO,
                            0,
                        ));
                    }
                    if nlb != 1 {
                        // **Reviewer H5** — 多 LBA PI Write 未实现（K4c TODO）。
                        // INVALID_PROTECTION_INFO 让 driver 知道是 PI 限制而非
                        // command bug；driver 可降级回 LBAF[0]+DPS=0 重试。
                        tracing::error!(nsid, slba, nlb,
                            "multi-LBA PI Write rejected (K4c TODO)");
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::INVALID_PROTECTION_INFO,
                            0,
                        ));
                    }
                    let total_lba = ns.total_lba;
                    if slba >= total_lba {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::LBA_OUT_OF_RANGE,
                            0,
                        ));
                    }
                    let pi_bytes = ns.data_bytes() as u32;
                    let tok = ctx.dma_read(prp1, pi_bytes);
                    self.pending_ios.insert(
                        tok,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmWritePi {
                                lba: slba,
                                num_blocks: nlb,
                            },
                        },
                    );
                    return None;
                }
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
                //
                // **reviewer H4 修复**：与 Format 同样：若 in-flight Write
                // 与 Zeroes 范围 overlap，sync-Zeroes 完成后异步 Write 完
                // 成会覆盖，driver 视角 LBA 内容不可预测。简化：拒绝任何
                // 有 IO in-flight 时的 Zeroes（spec 0x84 'Format/Sanitize
                // In Progress' 也可类比此场景，但 NVMe 没专门 SC）。
                if !self.pending_ios.is_empty()
                    || !self.dual_prp_writes.is_empty()
                    || !self.prp_list_ops.is_empty()
                {
                    tracing::warn!(
                        pending_ios = self.pending_ios.len(),
                        "WRITE ZEROES rejected: IO in flight (ordering hazard)"
                    );
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
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
                let is_pi_path = ns.lbads == 12 && ns.meta_size == 8 && ns.pi_type == 1;
                if !is_pi_path && (ns.lbads != 9 || ns.meta_size != 0 || ns.pi_enabled()) {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
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
                if is_pi_path {
                    // **Phase K4c + reviewer H3** — PI Write Zeroes：每 LBA 写
                    // zero data + 自 compute PI tuple (Guard=CRC16(zeros)=0,
                    // RefTag=LBA for Type 1)。用 positional IO 避免与并发 PI
                    // Read 共用 file cursor 导致 race。
                    let pi_type = ns.pi_type;
                    let pi_first = ns.pi_first;
                    let block_bytes = ns.block_bytes() as usize;
                    let data_bytes = ns.data_bytes() as usize;
                    let zero_data = vec![0u8; data_bytes];
                    let ns_mut = self.ns_mut(nsid).unwrap();
                    for off in 0..nlb {
                        let lba = slba + off as u64;
                        let tuple = crate::pi::PiTuple::compute(&zero_data, lba, pi_type);
                        let tuple_bytes = tuple.to_bytes();
                        let mut block = vec![0u8; block_bytes];
                        if pi_first {
                            block[0..8].copy_from_slice(&tuple_bytes);
                            // data 部分已是全 0
                        } else {
                            block[data_bytes..data_bytes + 8].copy_from_slice(&tuple_bytes);
                        }
                        if let Err(e) = ns_mut.write_at(&block, lba * block_bytes as u64) {
                            tracing::warn!(error = %e, nsid, lba, "WZ PI write fail");
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
                    return Some(Cqe::success(cid, sq_id, sq_head, phase));
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
                // **Phase H3 + K2** — NVMe NVM CS Spec § 3.3.2 Compare：
                // 全 3 档 PRP 路径 (≤4K / ≤8K / PRP list 至 MDTS=128K)。
                let cdw10 = sqe.cdw10;
                let cdw11 = sqe.cdw11;
                let cdw12 = sqe.cdw12;
                let prp1 = sqe.prp1;
                let prp2 = sqe.prp2;
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
                if ns.lbads != 9 || ns.meta_size != 0 || ns.pi_enabled() {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
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
                            op: PendingOp::NvmCompareSinglePrp {
                                lba: slba,
                                num_blocks: nlb,
                            },
                        },
                    );
                } else if bytes <= 2 * NVME_PAGE_SIZE {
                    // **Phase K2** — 双 PRP Compare 累积
                    let op_id = self.alloc_op_id();
                    self.compare_ops.insert(
                        op_id,
                        crate::controller::CompareAccum {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            lba: slba,
                            num_blocks: nlb,
                            prp1_data: None,
                            prp2_data: None,
                            data_pages: Vec::new(),
                            total_pages: 0,
                            pages_done: 0,
                            list_entries: None,
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
                            op: PendingOp::NvmCompareDualPrp {
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
                            op: PendingOp::NvmCompareDualPrp {
                                op_id,
                                is_prp1: false,
                            },
                        },
                    );
                } else {
                    // **Phase K2** — PRP list Compare
                    let total_pages = bytes.div_ceil(NVME_PAGE_SIZE) as u32;
                    let mut data_pages: Vec<Option<Vec<u8>>> =
                        Vec::with_capacity(total_pages as usize);
                    for _ in 0..total_pages {
                        data_pages.push(None);
                    }
                    let op_id = self.alloc_op_id();
                    self.compare_ops.insert(
                        op_id,
                        crate::controller::CompareAccum {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            lba: slba,
                            num_blocks: nlb,
                            prp1_data: None,
                            prp2_data: None,
                            data_pages,
                            total_pages,
                            pages_done: 0,
                            list_entries: None,
                        },
                    );
                    let tok_list = ctx.dma_read(prp2, NVME_PAGE_SIZE as u32);
                    self.pending_ios.insert(
                        tok_list,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmComparePrpListFetch { op_id },
                        },
                    );
                    let tok_prp1 = ctx.dma_read(prp1, NVME_PAGE_SIZE as u32);
                    self.pending_ios.insert(
                        tok_prp1,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::NvmComparePrpListData { op_id, page_idx: 0 },
                        },
                    );
                }
                None
            }
            nvm_opc::VERIFY => {
                // **Phase K4c (partial)** — Verify spec § 3.3.10：仅 read
                // backing + verify PI（无 data transfer）。
                let nsid = sqe.nsid;
                let slba = sqe.cdw10 as u64 | ((sqe.cdw11 as u64) << 32);
                let nlb = (sqe.cdw12 & 0xffff) as u32 + 1;
                tracing::debug!(nsid, slba, nlb, "Verify");
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
                let is_pi_path = ns.lbads == 12 && ns.meta_size == 8 && ns.pi_type == 1;
                if !is_pi_path && (ns.lbads != 9 || ns.meta_size != 0 || ns.pi_enabled()) {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
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
                if is_pi_path {
                    // Phase K4c + reviewer H3 — 逐 LBA 读 4104 byte + verify
                    // tuple；任一 fail 返对应 Media/Data Integrity SC + 立即停
                    // （spec 允许）。用 positional read_at 避免与并发 PI Write
                    // 共用 file cursor。
                    let pi_type = ns.pi_type;
                    let pi_first = ns.pi_first;
                    let block_bytes = ns.block_bytes() as usize;
                    let data_bytes = ns.data_bytes() as usize;
                    let ns_mut = self.ns_mut(nsid).unwrap();
                    for off in 0..nlb {
                        let lba = slba + off as u64;
                        let mut block = vec![0u8; block_bytes];
                        if let Err(e) = ns_mut.read_at(&mut block, lba * block_bytes as u64) {
                            tracing::warn!(error = %e, nsid, lba, "Verify PI read fail");
                            return Some(Cqe::error(
                                cid,
                                sq_id,
                                sq_head,
                                phase,
                                sc::DATA_TRANSFER_ERROR,
                                0,
                            ));
                        }
                        let (data_slice, tuple_slice) = if pi_first {
                            (&block[8..8 + data_bytes], &block[0..8])
                        } else {
                            (&block[0..data_bytes], &block[data_bytes..data_bytes + 8])
                        };
                        let tuple_arr: [u8; 8] = tuple_slice.try_into().unwrap();
                        let pi = crate::pi::PiTuple::from_bytes(&tuple_arr);
                        let check = pi.verify(data_slice, lba, pi_type);
                        if let Some(sc_code) = check.to_sc() {
                            tracing::warn!(nsid, lba, ?check, "Verify PI FAIL");
                            self.stat_num_err_log_entries += 1;
                            self.push_error_log(sq_id, cid, (sc_code as u16) << 1, lba, nsid);
                            return Some(Cqe::error(cid, sq_id, sq_head, phase, sc_code, 0));
                        }
                    }
                    tracing::debug!(nsid, slba, nlb, "Verify PI OK");
                }
                Some(Cqe::success(cid, sq_id, sq_head, phase))
            }
            nvm_opc::ZONE_MGMT_SEND => {
                // **Phase L1 + reviewer H2 修复** — Zone Management Send
                // (ZNS CS § 4.4)。
                // CDW10/11 = SLBA (zone 起点)；CDW13 bits 7:0 = ZSA
                // (Zone Send Action)：
                //   0x01 Close, 0x02 Finish, 0x03 Open, 0x04 Reset, 0x05 Offline
                // CDW13 bit 8 = Select All（无视 SLBA，对全 zone 生效，spec
                // 主要给 Reset All / Offline All / Close All / Finish All 用）
                let nsid = sqe.nsid;
                let slba = sqe.cdw10 as u64 | ((sqe.cdw11 as u64) << 32);
                let zsa = (sqe.cdw13 & 0xff) as u8;
                let select_all = (sqe.cdw13 & 0x100) != 0;
                let Some(ns) = self.ns_mut(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                        0,
                    ));
                };
                let Some(zns) = ns.zns.as_mut() else {
                    tracing::warn!(nsid, "Zone Mgmt Send: not a ZNS NS");
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_OPCODE,
                        0,
                    ));
                };
                // 构造要处理的 zone 索引列表（select-all → 全部；否则只一个）
                let zone_indices: Vec<usize> = if select_all {
                    (0..zns.zones.len()).collect()
                } else {
                    let zone_idx = (slba / zns.zone_size) as usize;
                    if zone_idx >= zns.zones.len() {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::LBA_OUT_OF_RANGE,
                            0,
                        ));
                    }
                    vec![zone_idx]
                };
                // 第一遍：spec-compliant transition 校验（任一非法 → 整命令 fail）
                // + 提前算 open/active 资源差额
                let max_open = zns.max_open as usize;
                let max_active = zns.max_active as usize;
                let cur_open = zns
                    .zones
                    .iter()
                    .filter(|z| {
                        matches!(z.state, ZoneState::ImplicitOpen | ZoneState::ExplicitOpen)
                    })
                    .count();
                let cur_active = zns
                    .zones
                    .iter()
                    .filter(|z| {
                        matches!(
                            z.state,
                            ZoneState::ImplicitOpen | ZoneState::ExplicitOpen | ZoneState::Closed
                        )
                    })
                    .count();
                let mut new_opens = 0usize;
                let mut new_active = 0usize;
                for &i in &zone_indices {
                    let z = zns.zones[i];
                    let sc_check = check_zsa_transition(z.state, zsa);
                    if let Some(sc_byte) = sc_check {
                        if select_all {
                            // Select-All 时仅跳过不合法 zone（Linux blkzone reset-all
                            // 在 zone Offline 上不该 fail 整命令）
                            tracing::trace!(zone_idx = i, ?z.state, zsa, "skip illegal in select-all");
                            continue;
                        }
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc_byte, 0));
                    }
                    // 资源差额：Open ZSA 把 Closed/Empty 变 ExplicitOpen
                    if zsa == 0x03 && matches!(z.state, ZoneState::Empty | ZoneState::Closed) {
                        new_opens += 1;
                        if matches!(z.state, ZoneState::Empty) {
                            new_active += 1;
                        }
                    }
                }
                // 资源 cap 检查（Open ZSA only；max_open/max_active = 0 表示 unlimited）
                if zsa == 0x03 {
                    if max_open > 0 && cur_open + new_opens > max_open {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::TOO_MANY_OPEN_ZONES,
                            0,
                        ));
                    }
                    if max_active > 0 && cur_active + new_active > max_active {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::TOO_MANY_ACTIVE_ZONES,
                            0,
                        ));
                    }
                }
                // 第二遍：apply transitions
                let zone_capacity = zns.zone_capacity;
                for i in zone_indices {
                    // 二次校验（应已通过；select-all 时跳过非法）
                    let zone = &mut zns.zones[i];
                    if check_zsa_transition(zone.state, zsa).is_some() {
                        continue;
                    }
                    match zsa {
                        0x01 => zone.state = ZoneState::Closed,
                        0x02 => {
                            zone.state = ZoneState::Full;
                            zone.write_pointer = zone_capacity;
                        }
                        0x03 => zone.state = ZoneState::ExplicitOpen,
                        0x04 => {
                            zone.state = ZoneState::Empty;
                            zone.write_pointer = 0;
                        }
                        0x05 => zone.state = ZoneState::Offline,
                        _ => unreachable!("check_zsa_transition gates unknown ZSA"),
                    }
                }
                tracing::info!(nsid, zsa, select_all, "Zone Mgmt Send OK");
                Some(Cqe::success(cid, sq_id, sq_head, phase))
            }
            nvm_opc::ZONE_MGMT_RECEIVE => {
                // **Phase L1** — Zone Management Receive (ZNS CS § 4.5)。
                // CDW10/11 = SLBA, CDW12 = NUMD (dwords - 1)，CDW13 bits 7:0
                // = ZRA：0x00 = Report Zones. PRP1 → response buffer。
                let nsid = sqe.nsid;
                let slba = sqe.cdw10 as u64 | ((sqe.cdw11 as u64) << 32);
                // **Reviewer M2** — checked_add 防 cdw12=0xFFFF_FFFF 时
                // numd 回绕到 0 → bytes=0 silent。
                let Some(numd) = sqe.cdw12.checked_add(1) else {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                };
                let bytes = numd as usize * 4;
                let zra = (sqe.cdw13 & 0xff) as u8;
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
                let Some(zns) = ns.zns.as_ref() else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_OPCODE,
                        0,
                    ));
                };
                if zra != 0 {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                let start_zone_idx = (slba / zns.zone_size) as usize;
                let buf = build_zone_report(zns, start_zone_idx, bytes);
                self.dma_write_then_complete(ctx, sqe.prp1, buf, cid, sq_id, sq_head, cq_id);
                None
            }
            nvm_opc::ZONE_APPEND => {
                // **Phase L1 + reviewer H1/H8 修复** — Zone Append (ZNS CS § 4.3)。
                // CDW10/11 = ZSLBA (zone 起点)；driver 不知 WP，controller
                // 把数据写在当前 WP 处，把实际 LBA 写回 CQE.dw0/dw1。
                // CDW12 bits 15:0 = NLB - 1。
                let nsid = sqe.nsid;
                let prp1 = sqe.prp1;
                let zslba = sqe.cdw10 as u64 | ((sqe.cdw11 as u64) << 32);
                let nlb = (sqe.cdw12 & 0xffff) as u32 + 1;
                let bytes = nlb as u64 * SECTOR_SIZE;
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
                // **H8 修复** — PI-enabled NS 上的 ZONE_APPEND 暂未实现
                // (data 路径 4096 → backing 4104 转换尚未与 ZNS 路径合并)；
                // 拒绝避免静默落 4 KiB 到本应 4104 块对齐的位置导致后续
                // PI Read 全 GuardFail。
                if ns.lbads != 9 || ns.meta_size != 0 || ns.pi_enabled() {
                    tracing::warn!(nsid, "ZONE_APPEND on PI NS rejected (K4c-ZNS unimplemented)");
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_PROTECTION_INFO,
                        0,
                    ));
                }
                let Some(zns) = ns.zns.as_ref() else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_OPCODE,
                        0,
                    ));
                };
                if bytes > NVME_PAGE_SIZE {
                    // 单 PRP only for ZNS Append (教学简化)
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                let zone_idx = (zslba / zns.zone_size) as usize;
                if zone_idx >= zns.zones.len() {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::LBA_OUT_OF_RANGE,
                        0,
                    ));
                }
                let zone = zns.zones[zone_idx];
                if matches!(zone.state, ZoneState::ReadOnly) {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::ZONE_IS_READ_ONLY,
                        0,
                    ));
                }
                if matches!(zone.state, ZoneState::Offline) {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::ZONE_IS_OFFLINE, 0));
                }
                if matches!(zone.state, ZoneState::Full) {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::ZONE_IS_FULL, 0));
                }
                if zone.write_pointer + nlb as u64 > zns.zone_capacity {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::ZONE_BOUNDARY_ERR,
                        0,
                    ));
                }
                // **Reviewer H2** — 隐式 Open 也要受 MAR/MOR 约束（spec ZNS § 2.2）
                if matches!(zone.state, ZoneState::Empty | ZoneState::Closed) {
                    let max_open = zns.max_open as usize;
                    let max_active = zns.max_active as usize;
                    let cur_open = zns
                        .zones
                        .iter()
                        .filter(|z| {
                            matches!(
                                z.state,
                                ZoneState::ImplicitOpen | ZoneState::ExplicitOpen
                            )
                        })
                        .count();
                    let cur_active = zns
                        .zones
                        .iter()
                        .filter(|z| {
                            matches!(
                                z.state,
                                ZoneState::ImplicitOpen
                                    | ZoneState::ExplicitOpen
                                    | ZoneState::Closed
                            )
                        })
                        .count();
                    if max_open > 0 && cur_open + 1 > max_open {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::TOO_MANY_OPEN_ZONES,
                            0,
                        ));
                    }
                    if matches!(zone.state, ZoneState::Empty)
                        && max_active > 0
                        && cur_active + 1 > max_active
                    {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::TOO_MANY_ACTIVE_ZONES,
                            0,
                        ));
                    }
                }
                let assigned_lba = zslba + zone.write_pointer;
                let prev_wp = zone.write_pointer;
                let prev_state = zone.state;
                // 预占 WP + state（实际 LBA 已通过 assigned_lba 锁定）。
                // 失败回滚走 NvmZoneAppend completion 路径。
                {
                    let ns_mut = self.ns_mut(nsid).unwrap();
                    let zns_mut = ns_mut.zns.as_mut().unwrap();
                    let zone_mut = &mut zns_mut.zones[zone_idx];
                    zone_mut.write_pointer += nlb as u64;
                    if zone_mut.write_pointer >= zns_mut.zone_capacity {
                        zone_mut.state = ZoneState::Full;
                    } else if matches!(zone_mut.state, ZoneState::Empty | ZoneState::Closed) {
                        zone_mut.state = ZoneState::ImplicitOpen;
                    }
                }
                // DMA-read data → 完成回调 NvmZoneAppend 负责：写文件 +
                // success(CQE dw0/dw1=assigned_lba) 或 failure(rollback +
                // error CQE)。
                let tok = ctx.dma_read(prp1, bytes as u32);
                self.pending_ios.insert(
                    tok,
                    PendingIo {
                        sq_id,
                        cid,
                        sq_head,
                        cq_id,
                        nsid,
                        op: PendingOp::NvmZoneAppend {
                            zone_idx,
                            assigned_lba,
                            prev_wp,
                            prev_state,
                            num_blocks: nlb,
                        },
                    },
                );
                tracing::info!(
                    nsid,
                    zone_idx,
                    assigned_lba,
                    nlb,
                    "Zone Append: WP reserved, awaiting DMA"
                );
                None
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
            nvm_opc::RESERVATION_REGISTER => {
                // **Phase H6** — NVMe spec § 6.13 Reservation Register。
                // CDW10 bits 2:0 = RREGA (Register Action: 0=register key,
                //                       1=unregister, 2=replace)
                //          bit 3 = IEKEY (ignore existing key — 不校验 CRKEY)
                //          bits 31:30 = CPTPL (persist through power loss)
                // PRP1 → 16-byte buffer: CRKEY (8 byte) + NRKEY (8 byte)。
                self.dispatch_reservation_cmd(
                    ctx,
                    sqe,
                    cid,
                    sq_id,
                    sq_head,
                    cq_id,
                    phase,
                    crate::controller::ReservationKind::Register,
                )
            }
            nvm_opc::RESERVATION_ACQUIRE => {
                // **Phase H6** — spec § 6.11。CDW10 bits 2:0 = RACQA
                //   0 = Acquire, 1 = Preempt, 2 = Preempt and Abort
                //   bits 15:8 = RTYPE (reservation type 1..6)
                // PRP1 → 16 byte: CRKEY + PRKEY (preempted key)
                self.dispatch_reservation_cmd(
                    ctx,
                    sqe,
                    cid,
                    sq_id,
                    sq_head,
                    cq_id,
                    phase,
                    crate::controller::ReservationKind::Acquire,
                )
            }
            nvm_opc::RESERVATION_RELEASE => {
                // **Phase H6** — spec § 6.15。CDW10 bits 2:0 = RRELA
                //   0 = Release, 1 = Clear（释放所有 reservations）
                //   bits 15:8 = RTYPE
                // PRP1 → 8 byte: CRKEY
                self.dispatch_reservation_cmd(
                    ctx,
                    sqe,
                    cid,
                    sq_id,
                    sq_head,
                    cq_id,
                    phase,
                    crate::controller::ReservationKind::Release,
                )
            }
            nvm_opc::RESERVATION_REPORT => {
                // **Phase H6 + O 修复 H3** — spec § 6.14 Reservation Report。
                // CDW10 = NUMD (dwords - 1)，CDW11 bit 0 = EDS (Extended
                // Data Structure)。EDS=1 → per-registrant 64 byte (含 16
                // byte HOSTID) 而非 24 byte；目前未实现 EDS=1 layout，
                // reject INVALID_FIELD 比静默返错 layout 安全。
                let nsid = sqe.nsid;
                // **Reviewer M2** — checked_add 防 NUMD=0xFFFF_FFFF 时回绕。
                let Some(numd) = sqe.cdw10.checked_add(1) else {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                };
                let eds = sqe.cdw11 & 0x1 != 0;
                if eds {
                    tracing::warn!(nsid, "Reservation Report EDS=1 not yet supported");
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0));
                }
                let bytes = numd as usize * 4;
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
                let buf = super::reservation::build_reservation_report(ns, bytes);
                self.dma_write_then_complete(ctx, sqe.prp1, buf, cid, sq_id, sq_head, cq_id);
                None
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

    /// **Phase H6** — Reservation Register/Acquire/Release 共用入口：
    /// 先 NSID 校验 → DMA-read PRP1 → 完成回调按 op_kind 修 ns.reservation。
    #[allow(clippy::too_many_arguments)]
    fn dispatch_reservation_cmd(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        sqe: Sqe,
        cid: u16,
        sq_id: u16,
        sq_head: u16,
        cq_id: u16,
        phase: u8,
        kind: crate::controller::ReservationKind,
    ) -> Option<Cqe> {
        let nsid = sqe.nsid;
        if self.ns(nsid).is_none() {
            return Some(Cqe::error(
                cid,
                sq_id,
                sq_head,
                phase,
                sc::INVALID_NAMESPACE,
                0,
            ));
        }
        let action = (sqe.cdw10 & 0x7) as u8;
        let rtype = ((sqe.cdw10 >> 8) & 0xff) as u8;
        // 所有三个 cmd 数据 buffer 都 ≤ 16 byte，单 PRP1 足够。
        let bytes = match kind {
            crate::controller::ReservationKind::Release => 8u32,
            _ => 16u32,
        };
        let tok = ctx.dma_read(sqe.prp1, bytes);
        self.pending_ios.insert(
            tok,
            crate::controller::PendingIo {
                sq_id,
                cid,
                sq_head,
                cq_id,
                nsid,
                op: crate::controller::PendingOp::NvmReservationCmd {
                    op_kind: kind,
                    action,
                    rtype,
                },
            },
        );
        None
    }
}
