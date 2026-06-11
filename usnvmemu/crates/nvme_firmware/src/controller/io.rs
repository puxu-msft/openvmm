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
use crate::controller::PiReadAccum;
use crate::controller::PiWriteAccum;
use crate::controller::SECTOR_SIZE;
use crate::controller::WriteAccum;
use crate::controller::ZnsState;
use crate::controller::ZoneState;
use crate::regs::*;
use pcie_device_core::*;
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
pub(crate) fn check_zsa_transition(state: ZoneState, zsa: u8) -> Option<u16> {
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

/// **Reviewer H2 (apply 2nd) + LOW** — Apply a single ZSA action to a zone
/// in-place，仅在合法 (check_zsa_transition 返 None) 且 non-no-op 时变更。
/// 返回 `true` 表示发生状态变更。抽成纯函数便于单元测试 apply 路径的副作用
/// （spec 表中标 `-` 的 no-op 不应有副作用 — 之前 commit 误改写）。
///
/// 防御：若 ZSA 未识别（不应发生，调用方已 gate），返 false + trace::error
/// 而非 `unreachable!()` panic，遵循 "never panic on driver input"。
pub(crate) fn apply_zsa(zone: &mut crate::controller::Zone, zsa: u8, zone_capacity: u64) -> bool {
    if check_zsa_transition(zone.state, zsa).is_some() {
        return false;
    }
    let is_noop = matches!(
        (zone.state, zsa),
        (ZoneState::Empty, 0x01)
            | (ZoneState::Full, 0x01)
            | (ZoneState::Full, 0x02)
            | (ZoneState::ExplicitOpen, 0x03)
            | (ZoneState::Closed, 0x01)
            | (ZoneState::Offline, 0x05)
    );
    if is_noop {
        return false;
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
        _ => {
            tracing::error!(zsa, "apply_zsa: unknown ZSA (caller should gate)");
            return false;
        }
    }
    true
}

/// **Reviewer H-1 + M-2** — 共享 ZNS Write-path guard。NVM_WRITE /
/// WRITE_ZEROES / WRITE_UNCORRECTABLE 在 ZNS NS 上落到 zone 前都要 enforce
/// SWR + state + boundary。返回 `Some(cqe)` 表示拒绝，`None` 表示通过。
///
/// 通过后调用方应在完成路径（成功 IO 完成后）推进 WP — 见
/// `mod.rs::on_dma_complete::NvmWriteDmaRead`。
pub(crate) fn check_zns_write(
    ns: &crate::controller::Namespace,
    slba: u64,
    nlb: u32,
    cid: u16,
    sq_id: u16,
    sq_head: u16,
    phase: u8,
) -> Option<Cqe> {
    let zns = ns.zns.as_ref()?;
    let zone_idx = (slba / zns.zone_size) as usize;
    let zone = zns.zones.get(zone_idx)?;
    let zone_end = (zone_idx as u64 + 1) * zns.zone_size;
    if slba + nlb as u64 > zone_end {
        return Some(Cqe::error(
            cid,
            sq_id,
            sq_head,
            phase,
            sc::ZONE_BOUNDARY_ERR,
        ));
    }
    match zone.state {
        ZoneState::ReadOnly => {
            return Some(Cqe::error(
                cid,
                sq_id,
                sq_head,
                phase,
                sc::ZONE_IS_READ_ONLY,
            ));
        }
        ZoneState::Offline => {
            return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::ZONE_IS_OFFLINE));
        }
        ZoneState::Full => {
            return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::ZONE_IS_FULL));
        }
        _ => {}
    }
    let zone_start = zone_idx as u64 * zns.zone_size;
    let expected_lba = zone_start + zone.write_pointer;
    if slba != expected_lba {
        return Some(Cqe::error(
            cid,
            sq_id,
            sq_head,
            phase,
            sc::ZONE_INVALID_WRITE,
        ));
    }
    None
}

/// **Reviewer H-1** — ZNS Read-path guard：Offline zone 拒读
/// (spec § 2.3.4)。其他 state 允许读（包括 ReadOnly）。
pub(crate) fn check_zns_read(
    ns: &crate::controller::Namespace,
    slba: u64,
    cid: u16,
    sq_id: u16,
    sq_head: u16,
    phase: u8,
) -> Option<Cqe> {
    let zns = ns.zns.as_ref()?;
    let zone_idx = (slba / zns.zone_size) as usize;
    let zone = zns.zones.get(zone_idx)?;
    if matches!(zone.state, ZoneState::Offline) {
        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::ZONE_IS_OFFLINE));
    }
    None
}

/// **Reviewer H-1** — ZNS Write 成功后推进 WP + Empty/Closed→ImplicitOpen
/// + WP=capacity→Full。
///
/// 在 dispatch 同步成功路径 (WRITE_ZEROES) 与 completion 异步路径
/// (NvmWriteDmaRead) 共享。
pub(crate) fn advance_zns_wp(ns: &mut crate::controller::Namespace, lba: u64, nlb: u32) {
    let Some(zns) = ns.zns.as_mut() else {
        return;
    };
    let zone_size = zns.zone_size;
    let capacity = zns.zone_capacity;
    let zone_idx = (lba / zone_size) as usize;
    if let Some(zone) = zns.zones.get_mut(zone_idx) {
        zone.write_pointer += nlb as u64;
        if zone.write_pointer >= capacity {
            zone.state = ZoneState::Full;
        } else if matches!(zone.state, ZoneState::Empty | ZoneState::Closed) {
            zone.state = ZoneState::ImplicitOpen;
        }
    }
}

/// **Phase R1/R2** — data pointer 解析结果。
///
/// PRP 路径（PSDT=00 / PSDT=01 inline 单 Data Block）返 `(prp1, prp2)`，复用
/// 现有三档 PRP dispatch。SGL Segment 路径（PSDT=10）返 `SglSegment`，由 caller
/// 路由到平行的 SGL scatter-gather 机件（embedded SGL1 在 bytes 24..40），**不**
/// 复用 PRP dispatch。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataPointer {
    Prp { prp1: u64, prp2: u64 },
    SglSegment,
}

/// **Phase R1** — 解析 SQE 的 data pointer：根据 PSDT 选 PRP 或 SGL。
///
/// 返回 `DataPointer::Prp { prp1, prp2 }` 让 caller 复用现有 PRP 三档 dispatch
/// （≤1 page 单 PRP，≤2 page dual PRP，> 2 page PRP list），或返
/// `DataPointer::SglSegment` 让 caller 走 SGL 数据路径（PSDT=10，见 Phase R2）。
///
/// SGL 教学路径：
/// - PSDT=00：直接返 raw prp1/prp2（标准 PRP 路径）
/// - PSDT=01：解析 SQE 内嵌 16-byte SGL descriptor (bytes 24..40)
///   * 单 Data Block + 长度 ≤ 1 page → 用 address 当 prp1，prp2=0（R1）
///   * 单 Data Block + 长度 > 1 page 或多 fragment → 当前不支持，返
///     SGL_DESCRIPTOR_TYPE_INVALID（任意 scatter-gather 走 PSDT=10 R2）
///   * 其他 type (Bit Bucket / Segment / Keyed) → reject
/// - PSDT=10 (SGL Segment pointer)：返 `SglSegment`，caller 走 R2 segment walk
/// - PSDT=11 reserved → INVALID_FIELD
pub(crate) fn resolve_data_pointers(sqe: &Sqe) -> Result<DataPointer, u16> {
    let psdt = sqe.psdt();
    let prp1 = sqe.prp1;
    let prp2 = sqe.prp2;
    match psdt {
        0b00 => Ok(DataPointer::Prp { prp1, prp2 }),
        0b01 => {
            // Inline SGL descriptor in bytes 24..40
            let bytes = sqe.embedded_sgl_bytes();
            let desc = match crate::sgl::SglDescriptor::parse(&bytes) {
                Some(d) => d,
                None => return Err(sc::SGL_DESCRIPTOR_TYPE_INVALID),
            };
            // sub_type 校验经共享 classifier（与 parse_sgl_list / validate_segment_pointer
            // 同判据）：sub_type=1 (CMB-relative) → 0x12，≥2 (reserved) → 0x11。
            if let Some(err_sc) = crate::sgl::subtype_to_sc(desc.sub_type) {
                return Err(err_sc);
            }
            match desc.sgl_type {
                crate::sgl::SglType::DataBlock => {
                    // 单 Data Block：address → prp1，length 隐含由 NLB 验证。
                    // length 必须 ≥ nlb*sector_size，否则不够装数据。
                    // 教学路径不允许 length > 1 page 的单 SGL (driver 应拆
                    // 多 fragment 走 R2 Segment 链)。
                    if desc.length as u64 > crate::regs::NVME_PAGE_SIZE {
                        tracing::warn!(
                            length = desc.length,
                            "SGL inline Data Block > 1 page; need R2 Segment chain"
                        );
                        return Err(sc::SGL_DESCRIPTOR_TYPE_INVALID);
                    }
                    Ok(DataPointer::Prp {
                        prp1: desc.address,
                        prp2: 0,
                    })
                }
                crate::sgl::SglType::BitBucket => {
                    // Bit Bucket 在 Read = controller 不写 host (driver 丢弃)；
                    // Write = controller 看不到 host data。单 Bit Bucket 没
                    // 意义（无数据传输），reject 让 driver 知道。
                    tracing::warn!("SGL Bit Bucket as sole inline descriptor rejected");
                    Err(sc::SGL_DESCRIPTOR_TYPE_INVALID)
                }
                crate::sgl::SglType::Segment | crate::sgl::SglType::LastSegment => {
                    // PSDT=01 的 inline 应是数据 descriptor 而非 Segment；
                    // 若 driver 想 chain 应用 PSDT=10。
                    tracing::warn!("SGL Segment as inline (PSDT=01) is mis-encoded");
                    Err(sc::SGL_DESCRIPTOR_TYPE_INVALID)
                }
                crate::sgl::SglType::KeyedDataBlock | crate::sgl::SglType::TransportSpecific => {
                    tracing::warn!("SGL Keyed / Transport-specific (NVMe-oF only) unsupported");
                    Err(sc::SGL_DESCRIPTOR_TYPE_INVALID)
                }
            }
        }
        0b10 => {
            // PSDT=10：bytes 24..40 是 SGL Segment descriptor 指向首段 SGL list。
            // **Phase R2** — 返 SglSegment，caller（dispatch_sgl_read/write）
            // DMA-read 该 segment 页后递归 walk 各段 + 逐 fragment scatter/gather。
            Ok(DataPointer::SglSegment)
        }
        _ => Err(sc::INVALID_FIELD),
    }
}

/// **Phase R2** — 校验一个 SGL (Last)Segment **指针** descriptor，返
/// `(segment_len, is_last)`。embedded SGL1（dispatch）与 chain 中段末位
/// continuation（completion）共用，避免 DRY drift（同一份 wire 校验谓词）。
///
/// `is_last` = true 表示 Last Segment（被指向段全是数据 descriptor，无 chain）；
/// false 表示 Segment（被指向段末位仍是 continuation，见 R2b）。
///
/// 校验：sub_type=0 (Address)、type ∈ {Segment, Last Segment}、length 16 倍数 /
/// 非零 / ≤ 1 page（教学单段上限）。
pub(crate) fn validate_segment_pointer(
    desc: &crate::sgl::SglDescriptor,
) -> Result<(u32, bool), u16> {
    // sub_type 校验经共享 classifier（与 parse_sgl_list / resolve_data_pointers 同判据）：
    // sub_type=1 (CMB-relative) → SGL_INVALID_USE_OF_CMB (0x12)，≥2 (reserved) → 0x11。
    if let Some(err_sc) = crate::sgl::subtype_to_sc(desc.sub_type) {
        tracing::warn!(
            sub_type = desc.sub_type,
            "SGL segment 指针 sub_type 非 0 (仅 Address)"
        );
        return Err(err_sc);
    }
    let is_last = match desc.sgl_type {
        crate::sgl::SglType::LastSegment => true,
        crate::sgl::SglType::Segment => false,
        _ => {
            tracing::warn!("SGL segment 指针必须是 (Last)Segment descriptor");
            return Err(sc::SGL_DESCRIPTOR_TYPE_INVALID);
        }
    };
    let len = desc.length;
    if len == 0 || !len.is_multiple_of(16) || len as u64 > NVME_PAGE_SIZE {
        tracing::warn!(len, "SGL segment length 非法（须 16 倍数、非零、≤1 page）");
        return Err(sc::SGL_DESCRIPTOR_TYPE_INVALID);
    }
    Ok((len, is_last))
}

/// **Phase R2** — 解析 PSDT=10 的 embedded SGL1 descriptor（必须是 Segment /
/// Last Segment，sub_type=0）。返 `(segment_addr, segment_len, is_last)`。
/// 校验逻辑见 `validate_segment_pointer`。
fn parse_sgl1_segment(bytes: &[u8; 16]) -> Result<(u64, u32, bool), u16> {
    let desc = crate::sgl::SglDescriptor::parse(bytes).ok_or(sc::SGL_DESCRIPTOR_TYPE_INVALID)?;
    let (len, is_last) = validate_segment_pointer(&desc)?;
    Ok((desc.address, len, is_last))
}

/// **Phase S1** — Write-protection guard：所有写类 IO (WRITE/WRITE_ZEROES/
/// WRITE_UNCORRECTABLE/DSM/COPY/ZONE_APPEND/Zone Mgmt Send) dispatch 入口前
/// 调用，命中返 Some(cqe with NAMESPACE_IS_WRITE_PROTECTED, SCT=Generic)。
pub(crate) fn check_ns_write_protection(
    ns: &crate::controller::Namespace,
    cid: u16,
    sq_id: u16,
    sq_head: u16,
    phase: u8,
) -> Option<Cqe> {
    if ns.nswp != 0 {
        tracing::debug!(wps = ns.nswp, "write rejected: NS Write Protection active");
        return Some(Cqe::error(
            cid,
            sq_id,
            sq_head,
            phase,
            sc::NAMESPACE_IS_WRITE_PROTECTED,
        ));
    }
    None
}

impl NvmeController {
    /// **Phase R2a** — PSDT=10 SGL Segment **Read** 数据路径入口。
    ///
    /// 与 PRP 三档 dispatch 平行：SGL 数据是任意 (address, length) fragment 的
    /// scatter-gather，无法映射成单 (prp1, prp2)。流程：
    ///   1. backing 一次性读到 `data`（按 plain 扇区字节）。
    ///   2. DMA-read embedded SGL1 指向的 segment 页 → `NvmSglFetch`。
    ///   3. fetch 完成 parse descriptor → 构建 fragment plan → 逐 fragment
    ///      `dma_write`（scatter）到 host → 全到齐 post CQE（见 completion.rs）。
    ///
    /// 仅 plain NS（无 PI/meta）；调用前 caller 已确认 is_plain。SGL1 可是
    /// Last Segment（单段）或 Segment（R2b chain 首段，末位 continuation 递归）。
    #[allow(clippy::too_many_arguments)]
    fn dispatch_sgl_read(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        sqe: &Sqe,
        sq_id: u16,
        cid: u16,
        sq_head: u16,
        cq_id: u16,
        nsid: u32,
        slba: u64,
        nlb: u32,
        sector_bytes: u64,
        phase: u8,
    ) -> Option<Cqe> {
        let bytes = nlb as u64 * sector_bytes;
        // 用真实扇区字节复查 MDTS（与 plain READ 一致）。
        if bytes > MDTS_MAX_BYTES {
            return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
        }
        // LBA 边界校验（SGL 走独立分流，故此处自检，不复用 plain 路径的检查）。
        let total_lba = self.ns(nsid).map(|n| n.total_lba).unwrap_or(0);
        match slba.checked_add(nlb as u64) {
            Some(end) if end <= total_lba => {}
            _ => {
                return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE));
            }
        }
        // ZNS Read 校验（Offline zone 拒读，与 plain READ 一致）。
        if let Some(ns) = self.ns(nsid)
            && let Some(cqe) = check_zns_read(ns, slba, cid, sq_id, sq_head, phase)
        {
            return Some(cqe);
        }
        // 解析 embedded SGL1 → 必须 Last Segment（R2a 单段）。
        let (seg_addr, seg_len, is_last) = match parse_sgl1_segment(&sqe.embedded_sgl_bytes()) {
            Ok(t) => t,
            Err(sc_byte) => return Some(Cqe::error(cid, sq_id, sq_head, phase, sc_byte)),
        };
        // **R2b** — SGL1 既可是 Last Segment（单段）也可是 Segment（chain 首段）；
        // `is_last` 透传给 NvmSglFetch 决定本段是否末段。
        // 一次性把 backing 读到 data buffer（READ：先读盘再 scatter）。
        let mut data = vec![0u8; bytes as usize];
        let ns_mut = self.ns_mut(nsid).unwrap();
        if let Err(e) = ns_mut.read_at(&mut data, slba * sector_bytes) {
            tracing::warn!(error = %e, nsid, slba, nlb, "SGL READ backing 读失败");
            return Some(Cqe::error(
                cid,
                sq_id,
                sq_head,
                phase,
                sc::DATA_TRANSFER_ERROR,
            ));
        }
        let op_id = self.alloc_op_id();
        self.sgl_ops.insert(
            op_id,
            crate::controller::SglOp {
                sq_id,
                cid,
                sq_head,
                cq_id,
                nsid,
                lba: slba,
                num_blocks: nlb,
                is_write: false,
                sector_bytes,
                expected_bytes: bytes,
                data,
                frags: Vec::new(),
                walk_offset: 0,
                walk_segments: 0,
                transfers_done: 0,
                transfers_total: 0,
            },
        );
        let tok = ctx.dma_read(seg_addr, seg_len);
        self.pending_ios.insert(
            tok,
            PendingIo {
                sq_id,
                cid,
                sq_head,
                cq_id,
                nsid,
                op: PendingOp::NvmSglFetch { op_id, is_last },
            },
        );
        None
    }

    /// **Phase R2a** — PSDT=10 SGL Segment **Write** 数据路径入口。
    ///
    /// 镜像 `dispatch_sgl_read`，方向相反：
    ///   1. 建空 `data` gather buffer（待按 fragment 填）。
    ///   2. DMA-read embedded SGL1 指向的 segment 页 → `NvmSglFetch`。
    ///   3. fetch 完成 parse descriptor → 逐 fragment `dma_read`（gather）自
    ///      host 填 `data` → 全到齐后一次性 `write_at` backing → post CQE。
    ///
    /// 仅 plain NS；NS Write Protection 已在 caller 校验。SGL1 可单段或 R2b chain。
    #[allow(clippy::too_many_arguments)]
    fn dispatch_sgl_write(
        &mut self,
        ctx: &mut DeviceCtx<'_>,
        sqe: &Sqe,
        sq_id: u16,
        cid: u16,
        sq_head: u16,
        cq_id: u16,
        nsid: u32,
        slba: u64,
        nlb: u32,
        sector_bytes: u64,
        phase: u8,
    ) -> Option<Cqe> {
        let bytes = nlb as u64 * sector_bytes;
        if bytes > MDTS_MAX_BYTES {
            return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
        }
        // LBA 边界校验。
        let total_lba = self.ns(nsid).map(|n| n.total_lba).unwrap_or(0);
        match slba.checked_add(nlb as u64) {
            Some(end) if end <= total_lba => {}
            _ => {
                return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE));
            }
        }
        // ZNS Write 校验（SWR + state + 边界，与 plain WRITE 一致）。
        if let Some(ns) = self.ns(nsid)
            && let Some(cqe) = check_zns_write(ns, slba, nlb, cid, sq_id, sq_head, phase)
        {
            return Some(cqe);
        }
        let (seg_addr, seg_len, is_last) = match parse_sgl1_segment(&sqe.embedded_sgl_bytes()) {
            Ok(t) => t,
            Err(sc_byte) => return Some(Cqe::error(cid, sq_id, sq_head, phase, sc_byte)),
        };
        // **R2b** — SGL1 既可 Last Segment（单段）也可 Segment（chain 首段）。
        let op_id = self.alloc_op_id();
        self.sgl_ops.insert(
            op_id,
            crate::controller::SglOp {
                sq_id,
                cid,
                sq_head,
                cq_id,
                nsid,
                lba: slba,
                num_blocks: nlb,
                is_write: true,
                sector_bytes,
                expected_bytes: bytes,
                data: vec![0u8; bytes as usize],
                frags: Vec::new(),
                walk_offset: 0,
                walk_segments: 0,
                transfers_done: 0,
                transfers_total: 0,
            },
        );
        let tok = ctx.dma_read(seg_addr, seg_len);
        self.pending_ios.insert(
            tok,
            PendingIo {
                sq_id,
                cid,
                sq_head,
                cq_id,
                nsid,
                op: PendingOp::NvmSglFetch { op_id, is_last },
            },
        );
        None
    }

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
            ));
        }
        // **Phase S4** — NS Attachment：若 sqe.nsid 已 detached，IO 全拒
        // INVALID_NAMESPACE (spec § 5.20 detached NS 等同不存在)。
        let nsid = sqe.nsid;
        if nsid != 0
            && nsid != 0xFFFF_FFFF
            && let Some(ns) = self.namespaces.get(&nsid)
            && !ns.attached
        {
            tracing::debug!(nsid, "IO rejected: NS detached");
            return Some(Cqe::error(
                cid,
                sq_id,
                sq_head,
                phase,
                sc::INVALID_NAMESPACE,
            ));
        }
        match sqe.opcode() {
            nvm_opc::READ => {
                let cdw10 = sqe.cdw10;
                let cdw11 = sqe.cdw11;
                let cdw12 = sqe.cdw12;
                let nsid = sqe.nsid;
                let slba = cdw10 as u64 | ((cdw11 as u64) << 32);
                let nlb = (cdw12 & 0xffff) as u32 + 1;
                // **Phase R1/R2** — PSDT (PRP or SGL) dispatch。
                // PSDT=00 (PRP) → 直接用 sqe.prp1/prp2
                // PSDT=01 (SGL inline) → 把 SQE bytes 24..40 解 SGL，单
                //   Data Block 时 address→prp1 复用现有 PRP 路径；否则 reject。
                // PSDT=10 (SGL Segment) → is_sgl=true，走平行 SGL scatter 路径。
                // PSDT=11 → reject (reserved)。
                let (prp1, prp2, is_sgl) = match resolve_data_pointers(&sqe) {
                    Ok(DataPointer::Prp { prp1, prp2 }) => (prp1, prp2, false),
                    Ok(DataPointer::SglSegment) => (0, 0, true),
                    Err(sc_byte) => {
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc_byte));
                    }
                };
                // **Phase Q1** — PRACT (Protection Information Action) bit 29。
                // spec § 8.3.1.2：PRACT=1 = driver 让 controller 自动 strip
                // (Read) / insert (Write) PI tuple。我们 K4 路径本来就是
                // "controller compute tuple on write + verify on read"，
                // 所以 PRACT=1 在 PI NS 上行为与 PRACT=0 一致（PRACT=0 时
                // driver 提供 inline tuple；PRACT=1 时 driver 只传 data，
                // controller 生成。本实现两种都允许并按 driver 期望处理：
                // PRACT=1 + PI NS = 同 PI K4 path（generate）。PRACT=1 +
                // 非 PI NS = INVALID_PROTECTION_INFO（spec 要求 PI capable）。
                let pract = (cdw12 >> 29) & 0x1 != 0;
                // **2026-06-09**：ns 未取前先按 512B 下界做 MDTS 早退 + 日志；
                // 真实传输大小（纯 4K = nlb×4096）在取 ns 后用 sector_bytes
                // 重算并复查 MDTS（见下方 ~814）。512B 下界永不误拒合法请求。
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
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                // **Phase H4** — NSID 校验 + 取 NS（含 total_lba）
                let Some(ns) = self.ns(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                    ));
                };
                // **Phase K1/K4** — IO 路径分流：
                //   - LBAF[0] (512B no-meta no-PI) → 默认走原路径
                //   - LBAF[1] (4 KiB + 8B meta) + PI Type 1 + 单 LBA →
                //     走 K4a/K4b 真 PI 路径
                //   - 其它（如 LBAF[1] 多 LBA、Type 2/3）→ INVALID_FIELD
                //     直到 K4c 多 LBA 路径完整实现
                let is_pi_path = ns.lbads == 12 && ns.meta_size == 8 && ns.pi_type == 1;
                // **2026-06-09 纯 4K** — plain 格式 = 无 meta + 无 PI + lbads∈{9,12}
                // （512B LBAF[0] / 纯 4K LBAF[2]）；数据扇区字节 = 1<<lbads。
                let is_plain =
                    ns.meta_size == 0 && !ns.pi_enabled() && (ns.lbads == 9 || ns.lbads == 12);
                let sector_bytes = 1u64 << ns.lbads;
                let meta_inline_r = ns.meta_inline; // B6b-3：separate(false) 走 MPTR 路径
                // **Phase Q1 + reviewer 12轮 H-Q1**:
                // - PRACT=1 + 非 PI NS = INVALID_PROTECTION_INFO（spec § 8.3.1）
                // - PRACT=0 + PI NS = INVALID_PROTECTION_INFO 因 K4 路径**不支持**
                //   driver-supplied inline tuple；spec PRACT=0 要求 driver 在 PRP
                //   传 N×4104 byte，我们 dma_read 只 N×4096 会丢 tuple。
                //   driver 必须 PRACT=1 (controller 自动 generate/strip)
                if pract && !is_pi_path {
                    tracing::warn!(nsid, "READ PRACT=1 on non-PI NS → INVALID_PROTECTION_INFO");
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_PROTECTION_INFO,
                    ));
                }
                if !pract && is_pi_path {
                    // **B6b-3（separate metadata，PRACT=0）** — separate NS：从 backing
                    // 读 interleaved block → verify stored PI → data 回 PRP + PI tuple 回
                    // MPTR（两条 DMA-write）。inline NS（extended LBA）的 PRACT=0 仍未实现。
                    if meta_inline_r {
                        tracing::warn!(
                            nsid,
                            "READ PRACT=0 on inline(extended-LBA) PI NS unsupported (用 PRACT=1)"
                        );
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::INVALID_PROTECTION_INFO,
                        ));
                    }
                    if nlb != 1 {
                        tracing::warn!(nlb, "separate-meta READ 暂仅支持单 LBA（多 LBA 待续）");
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                    }
                    if sqe.mptr == 0 {
                        tracing::warn!(nsid, "separate-meta READ 需 MPTR（host PI buffer）");
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                    }
                    let mptr = sqe.mptr;
                    let pi_type = ns.pi_type;
                    let pi_first = ns.pi_first;
                    let block_bytes = ns.block_bytes() as usize; // 4104
                    let data_bytes = ns.data_bytes() as usize; // 4096
                    let total_lba = ns.total_lba;
                    if slba >= total_lba {
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE));
                    }
                    // 读 interleaved block（drop ns 不可变借用后用 ns_mut）。
                    let mut block = vec![0u8; block_bytes];
                    let ns_mut = self.ns_mut(nsid).unwrap();
                    if let Err(e) = ns_mut.read_at(&mut block, slba * block_bytes as u64) {
                        tracing::warn!(error = %e, nsid, slba, "separate-meta READ backing fail");
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
                        ));
                    }
                    // 按 pi_first 拆 tuple + data。
                    let (tuple_bytes, data_part): ([u8; 8], Vec<u8>) = if pi_first {
                        (
                            block[0..8].try_into().unwrap(),
                            block[8..8 + data_bytes].to_vec(),
                        )
                    } else {
                        (
                            block[data_bytes..data_bytes + 8].try_into().unwrap(),
                            block[0..data_bytes].to_vec(),
                        )
                    };
                    // verify stored PI（检测 backing 损坏；over-strict 同 WRITE，PRCHK 未门控）。
                    let stored_tuple = crate::pi::PiTuple::from_bytes(&tuple_bytes);
                    match stored_tuple.verify(&data_part, slba, pi_type) {
                        crate::pi::PiCheck::Ok => {}
                        other => {
                            // **reviewer M-1**：映射具体失败类型（Guard 0x82 / RefTag 0x84 /
                            // AppTag 0x83）而非恒 0x82，与 WRITE 侧对称（spec § 4.6.1）。
                            let sc_byte = other.to_sc().unwrap_or(0x82);
                            tracing::warn!(
                                nsid,
                                slba,
                                ?other,
                                "separate-meta READ: stored PI verify 失败"
                            );
                            return Some(Cqe::error(
                                cid,
                                sq_id,
                                sq_head,
                                phase,
                                sc::status(sc_byte, sc::SCT_MEDIA_DATA_INTEGRITY),
                            ));
                        }
                    }
                    // data → PRP1，tuple → MPTR（两条 DMA-write，都完成后 success CQE）。
                    let op_id = self.alloc_op_id();
                    let tok_d = ctx.dma_write(prp1, data_part);
                    self.pending_ios.insert(
                        tok_d,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::SepMetaReadDone { op_id },
                        },
                    );
                    let tok_m = ctx.dma_write(mptr, tuple_bytes.to_vec());
                    self.pending_ios.insert(
                        tok_m,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::SepMetaReadDone { op_id },
                        },
                    );
                    self.sep_meta_reads.insert(
                        op_id,
                        crate::controller::SepMetaReadAccum {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            remaining: 2,
                        },
                    );
                    return None;
                }
                if !is_pi_path && !is_plain {
                    // **Reviewer H5** — 用 INVALID_PROTECTION_INFO 而非 INVALID_FIELD
                    // 让 driver 区分 "PI 格式不受支持" vs "命令字段错误"。
                    // **2026-06-09**：plain 4K(LBAF[2]) 现走 is_plain 放行，不再误拒。
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_PROTECTION_INFO,
                    ));
                }
                // **Phase R2** — SGL Segment（PSDT=10）走平行 scatter 路径。
                // 仅 plain NS（无 PI/meta）；SGL × PI 组合 R2 未覆盖（plan 风险项）。
                if is_sgl {
                    if !is_plain {
                        tracing::warn!(nsid, "SGL READ on non-plain NS unsupported (R2)");
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::INVALID_PROTECTION_INFO,
                        ));
                    }
                    return self.dispatch_sgl_read(
                        ctx,
                        &sqe,
                        sq_id,
                        cid,
                        sq_head,
                        cq_id,
                        nsid,
                        slba,
                        nlb,
                        sector_bytes,
                        phase,
                    );
                }
                if is_pi_path && nlb != 1 {
                    // **Phase K4c** — 多 LBA PI Read：从 backing file 读
                    // nlb*4104 字节 → per-LBA verify PI → dma_write 仅 data
                    // 部分到 host（nlb*4096，按 PRP 三档分流）。
                    let pi_type = ns.pi_type;
                    let pi_first = ns.pi_first;
                    let block_bytes = ns.block_bytes() as usize; // 4104
                    let data_bytes = ns.data_bytes() as usize; // 4096
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
                            ));
                        }
                    }
                    // **C1① MDTS metadata 计入**：extended-LBA（内联 metadata）的
                    // host 传输大小 = block_bytes（data+meta），MDTS 按 spec § 8.x
                    // 须计入 metadata（仅 separate-buffer 才排除）。旧版用 data_bytes
                    // 漏算 meta*nlb → 边界处少拒一档。
                    if (block_bytes as u64 * nlb as u64) > MDTS_MAX_BYTES {
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                    }
                    let mut interleaved = vec![0u8; block_bytes * nlb as usize];
                    let ns_mut = self.ns_mut(nsid).unwrap();
                    if let Err(e) = ns_mut.read_at(&mut interleaved, slba * block_bytes as u64) {
                        tracing::warn!(error = %e, nsid, slba, nlb, "K4c PI READ backing fail");
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
                        ));
                    }
                    // Verify per-LBA + 抽出纯 data 部分
                    let mut data_only = Vec::with_capacity(data_bytes * nlb as usize);
                    for i in 0..nlb as usize {
                        let lba_i = slba + i as u64;
                        let blk = &interleaved[i * block_bytes..(i + 1) * block_bytes];
                        let (data_slice, tuple_slice) = if pi_first {
                            (&blk[8..8 + data_bytes], &blk[0..8])
                        } else {
                            (&blk[0..data_bytes], &blk[data_bytes..data_bytes + 8])
                        };
                        let tuple_arr: [u8; 8] = tuple_slice.try_into().unwrap();
                        let pi = crate::pi::PiTuple::from_bytes(&tuple_arr);
                        let check = pi.verify(data_slice, lba_i, pi_type);
                        if let Some(sc_byte) = check.to_sc() {
                            tracing::warn!(
                                nsid,
                                lba = lba_i,
                                ?check,
                                "K4c multi-LBA PI verify FAIL"
                            );
                            self.stat_num_err_log_entries += 1;
                            self.push_error_log(
                                sq_id,
                                cid,
                                sc::sf_of(sc::status(sc_byte, sc::SCT_MEDIA_DATA_INTEGRITY)),
                                lba_i,
                                nsid,
                            );
                            return Some(Cqe::error(
                                cid,
                                sq_id,
                                sq_head,
                                phase,
                                sc::status(sc_byte, sc::SCT_MEDIA_DATA_INTEGRITY),
                            ));
                        }
                        data_only.extend_from_slice(data_slice);
                    }
                    // 三档 DMA-write 到 host PRP（与 plain READ 同分流）
                    let payload_bytes = data_only.len() as u64;
                    if payload_bytes <= NVME_PAGE_SIZE {
                        let tok = ctx.dma_write(prp1, data_only);
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
                    } else if payload_bytes <= 2 * NVME_PAGE_SIZE {
                        let split = NVME_PAGE_SIZE as usize;
                        let part1 = data_only[..split].to_vec();
                        let part2 = data_only[split..].to_vec();
                        let tok1 = ctx.dma_write(prp1, part1);
                        let tok2 = ctx.dma_write(prp2, part2);
                        // tok1 dispatch sibling-half placeholder；tok2 处理
                        // success CQE + counter（与 plain dual-PRP Read 一致）
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
                                op: PendingOp::NvmReadPiDmaWrite { num_blocks: nlb },
                            },
                        );
                    } else {
                        // **Phase K4c-list** — PI Read PRP-list 路径。data_only
                        // 已 verified + extracted；按 PRP 三档 + per-page DMA-write。
                        // PRP1 = page 0；PRP2 → list 页（u64 array of subsequent
                        // page GPAs）。dispatch list 页 fetch 后，per-entry
                        // dma_write 各页。
                        let op_id = self.alloc_op_id();
                        let pages_total = payload_bytes.div_ceil(NVME_PAGE_SIZE) as u32;
                        self.pi_reads.insert(
                            op_id,
                            PiReadAccum {
                                nsid,
                                num_blocks: nlb,
                                data_only,
                                pages_done: 0,
                                pages_total,
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
                            },
                        );
                        // 先 dispatch page 0 → PRP1
                        let accum = self.pi_reads.get(&op_id).unwrap();
                        let first = accum.data_only[..NVME_PAGE_SIZE as usize].to_vec();
                        let tok0 = ctx.dma_write(prp1, first);
                        self.pending_ios.insert(
                            tok0,
                            PendingIo {
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
                                nsid,
                                op: PendingOp::NvmReadPiListData { op_id, page_idx: 0 },
                            },
                        );
                        // 然后 fetch PRP2 list 页
                        let list_tok = ctx.dma_read(prp2, NVME_PAGE_SIZE as u32);
                        self.pending_ios.insert(
                            list_tok,
                            PendingIo {
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
                                nsid,
                                op: PendingOp::NvmReadPiListFetch { op_id },
                            },
                        );
                        tracing::info!(
                            nsid,
                            nlb,
                            payload_bytes,
                            pages_total,
                            "K4c-list PI Read: PRP1 sent + list fetch dispatched"
                        );
                    }
                    return None;
                }
                let total_lba = ns.total_lba;
                // H4：checked_add 防 slba + nlb 溢出（driver bug / 恶意输入）。
                match slba.checked_add(nlb as u64) {
                    Some(end) if end <= total_lba => {}
                    _ => {
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE));
                    }
                }
                // **Reviewer H-1** — ZNS Read：Offline zone 拒绝（其他 state 允许读）
                if let Some(cqe) = check_zns_read(ns, slba, cid, sq_id, sq_head, phase) {
                    return Some(cqe);
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
                    // **Phase M2** — read_at 走 mmap 零拷贝
                    if let Err(e) = ns_mut.read_at(&mut block_buf, slba * block_bytes as u64) {
                        tracing::warn!(error = %e, nsid, slba, "PI READ: backing read failed");
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
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
                        self.push_error_log(
                            sq_id,
                            cid,
                            sc::sf_of(sc::status(sc, sc::SCT_MEDIA_DATA_INTEGRITY)),
                            slba,
                            nsid,
                        );
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::status(sc, sc::SCT_MEDIA_DATA_INTEGRITY),
                        ));
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
                // **2026-06-09 纯 4K** — plain 路径按 per-NS 扇区字节重算传输大小
                // + 偏移（512B → 512 / 纯 4K → 4096）；并以真实大小复查 MDTS。
                let bytes = nlb as u64 * sector_bytes;
                if bytes > MDTS_MAX_BYTES {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                let mut buf = vec![0u8; bytes as usize];
                let ns_mut = self.ns_mut(nsid).unwrap();
                // **Phase M2** — read_at 走 mmap 零拷贝
                if let Err(e) = ns_mut.read_at(&mut buf, slba * sector_bytes) {
                    tracing::warn!(error = %e, nsid, slba, nlb, "READ: backing file read failed");
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::DATA_TRANSFER_ERROR,
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
                let nsid = sqe.nsid;
                let slba = cdw10 as u64 | ((cdw11 as u64) << 32);
                let nlb = (cdw12 & 0xffff) as u32 + 1;
                // **Phase R1/R2** — PSDT dispatch（同 READ 路径，参 resolve_data_pointers）
                let (prp1, prp2, is_sgl) = match resolve_data_pointers(&sqe) {
                    Ok(DataPointer::Prp { prp1, prp2 }) => (prp1, prp2, false),
                    Ok(DataPointer::SglSegment) => (0, 0, true),
                    Err(sc_byte) => {
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc_byte));
                    }
                };
                // **Phase Q1** — PRACT bit 29，参 READ 注释。PRACT=1 在 PI
                // NS 上等价 PI K4 path（controller generate tuple on write）。
                // 非 PI NS 上 PRACT=1 = INVALID_PROTECTION_INFO（NS 校验在
                // is_pi_path 检查后处理）。这里只 capture flag，下面与 ns
                // 一起处理。
                let pract = (cdw12 >> 29) & 0x1 != 0;
                // **2026-06-09**：同 READ —— ns 未取前按 512B 下界早退/日志，
                // 纯 4K 真实大小在取 ns 后用 sector_bytes 重算并复查 MDTS。
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
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                // **Phase H4** — NSID 校验
                let Some(ns) = self.ns(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                    ));
                };
                // **Phase S1** — Namespace Write Protection 拒写
                if let Some(cqe) = check_ns_write_protection(ns, cid, sq_id, sq_head, phase) {
                    return Some(cqe);
                }
                // **Phase Q1 + 12轮 H-Q1**:
                // - PRACT=1 + 非 PI NS = INVALID_PROTECTION_INFO（spec § 8.3.1）
                // - PRACT=0 + PI NS = INVALID_PROTECTION_INFO（K4 路径不支持
                //   driver-supplied inline tuple；driver 必须用 PRACT=1）
                let is_pi_capable = ns.lbads == 12 && ns.meta_size == 8 && ns.pi_type == 1;
                // **2026-06-09 纯 4K** — plain 格式 = 无 meta + 无 PI + lbads∈{9,12}。
                let is_plain =
                    ns.meta_size == 0 && !ns.pi_enabled() && (ns.lbads == 9 || ns.lbads == 12);
                let sector_bytes = 1u64 << ns.lbads;
                let meta_inline = ns.meta_inline; // B6b-2：separate(false) 走 MPTR 路径
                if pract && !is_pi_capable {
                    tracing::warn!(nsid, "WRITE PRACT=1 on non-PI NS → INVALID_PROTECTION_INFO");
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_PROTECTION_INFO,
                    ));
                }
                if !pract && is_pi_capable {
                    // **B6b-2（separate metadata，PRACT=0）** — host 供 PI tuple via MPTR。
                    // 仅 separate NS（meta_inline=false）：data 经 PRP、PI 经 MPTR 两条 DMA-read，
                    // 到齐后 verify host PI vs data → interleave 存盘。inline NS（extended LBA）
                    // 的 PRACT=0（host inline tuple）仍未实现 → 保持拒绝。单 LBA 起步。
                    if meta_inline {
                        tracing::warn!(
                            nsid,
                            "WRITE PRACT=0 on inline(extended-LBA) PI NS unsupported (用 PRACT=1)"
                        );
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::INVALID_PROTECTION_INFO,
                        ));
                    }
                    if nlb != 1 {
                        // 多 LBA separate WRITE = B6b-2 后续增量；当前单 LBA。
                        tracing::warn!(nlb, "separate-meta WRITE 暂仅支持单 LBA（多 LBA 待续）");
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                    }
                    if sqe.mptr == 0 {
                        tracing::warn!(nsid, "separate-meta WRITE 需 MPTR（host PI buffer）");
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                    }
                    let mptr = sqe.mptr;
                    let op_id = self.alloc_op_id();
                    let tok_d = ctx.dma_read(prp1, sector_bytes as u32);
                    self.pending_ios.insert(
                        tok_d,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::SepMetaWriteData { op_id },
                        },
                    );
                    let tok_m = ctx.dma_read(mptr, 8);
                    self.pending_ios.insert(
                        tok_m,
                        PendingIo {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            op: PendingOp::SepMetaWriteMeta { op_id },
                        },
                    );
                    self.sep_meta_writes.insert(
                        op_id,
                        crate::controller::SepMetaWriteAccum {
                            sq_id,
                            cid,
                            sq_head,
                            cq_id,
                            nsid,
                            lba: slba,
                            data: None,
                            meta: None,
                        },
                    );
                    return None;
                }
                // **Phase R2** — SGL Segment（PSDT=10）走平行 gather 路径。
                // 仅 plain NS（无 PI/meta）；NS Write Protection 已在上方校验。
                if is_sgl {
                    if !is_plain {
                        tracing::warn!(nsid, "SGL WRITE on non-plain NS unsupported (R2)");
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::INVALID_PROTECTION_INFO,
                        ));
                    }
                    return self.dispatch_sgl_write(
                        ctx,
                        &sqe,
                        sq_id,
                        cid,
                        sq_head,
                        cq_id,
                        nsid,
                        slba,
                        nlb,
                        sector_bytes,
                        phase,
                    );
                }
                if !is_plain {
                    // **Phase K4a** — PI 单 LBA Write 路径
                    let is_pi_path = ns.lbads == 12 && ns.meta_size == 8 && ns.pi_type == 1;
                    if !is_pi_path {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::INVALID_PROTECTION_INFO,
                        ));
                    }
                    // **Phase L1f** — PI + ZNS 组合。WRITE PI 在 ZNS NS 上
                    // 需走 check_zns_write 校验 SWR + state + 边界（与 plain
                    // WRITE 一致）；完成路径在 advance_zns_wp 推进 WP。
                    // 之前 7th-round H-4 暂拒，K4c-list 完成后这条解禁。
                    if ns.zns.is_some()
                        && let Some(cqe) =
                            check_zns_write(ns, slba, nlb, cid, sq_id, sq_head, phase)
                    {
                        return Some(cqe);
                    }
                    if nlb != 1 {
                        // **Phase K4c** — 多 LBA PI Write：data 体积
                        // nlb * 4096，可能跨多个 PRP 页。简化教学路径：
                        // 复用 plain Write 三档 PRP 机制（dual-PRP / PRP-list）
                        // 读 host data 到 raw_data 累积器；完成时按 LBA 切片
                        // compute PI tuple + interleave 写到 backing
                        // （每 LBA 占 4104 字节）。
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
                                ));
                            }
                        }
                        // **C1① MDTS 计入 inline metadata**：MDTS 门按 extended-LBA
                        // 真实传输大小 block_bytes(data+meta) 判（spec § 8.x）。
                        // **关键**：PRACT=1 时 host PRP 只传 data（N×4096），controller
                        // 自动 generate PI tuple → 实际 DMA / PRP 路由 / buffer 仍用
                        // data_bytes_total(data-only)；MDTS 门与传输大小**分开算**，
                        // 不可混用（否则多 LBA 写 buffer/PRP 路由错乱 → hang/corruption）。
                        if ns.block_bytes() * nlb as u64 > MDTS_MAX_BYTES {
                            return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                        }
                        let data_bytes_total = ns.data_bytes() * nlb as u64;
                        let op_id = self.alloc_op_id();
                        let pages_total = data_bytes_total.div_ceil(NVME_PAGE_SIZE) as u32;
                        let prp_list_pending = data_bytes_total > 2 * NVME_PAGE_SIZE;
                        self.pi_writes.insert(
                            op_id,
                            PiWriteAccum {
                                nsid,
                                slba,
                                num_blocks: nlb,
                                data_bytes_total: data_bytes_total as usize,
                                received: vec![0u8; data_bytes_total as usize],
                                pages_done: 0,
                                pages_total,
                                sq_id,
                                cid,
                                sq_head,
                                cq_id,
                                prp_list_pending,
                            },
                        );
                        // 三档：单 PRP / dual-PRP / PRP-list
                        if data_bytes_total <= NVME_PAGE_SIZE {
                            let tok = ctx.dma_read(prp1, data_bytes_total as u32);
                            self.pending_ios.insert(
                                tok,
                                PendingIo {
                                    sq_id,
                                    cid,
                                    sq_head,
                                    cq_id,
                                    nsid,
                                    op: PendingOp::NvmWritePiMulti { op_id, page_idx: 0 },
                                },
                            );
                        } else if data_bytes_total <= 2 * NVME_PAGE_SIZE {
                            let part1 = NVME_PAGE_SIZE as u32;
                            let part2 = (data_bytes_total - NVME_PAGE_SIZE) as u32;
                            let tok1 = ctx.dma_read(prp1, part1);
                            let tok2 = ctx.dma_read(prp2, part2);
                            self.pending_ios.insert(
                                tok1,
                                PendingIo {
                                    sq_id,
                                    cid,
                                    sq_head,
                                    cq_id,
                                    nsid,
                                    op: PendingOp::NvmWritePiMulti { op_id, page_idx: 0 },
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
                                    op: PendingOp::NvmWritePiMulti { op_id, page_idx: 1 },
                                },
                            );
                        } else {
                            // **Phase K4c-list** — PRP-list 路径。spec § 4.4：
                            // PRP1 仍指向第一页数据，PRP2 指向 PRP-list 页
                            // （4 KiB array of u64 entries，每 entry = 后续
                            // 数据页 GPA）。教学限制：PRP-list 单页 = 512 entry
                            // = 2 MiB；MDTS=5 把 transfer 上限钉在 128 KiB，
                            // 远小于 single list 页容量，所以不需 chained list。
                            //
                            // 流程：DMA-read PRP1 第一页 + DMA-read PRP2 list 页
                            // → on_dma_complete NvmWritePiListFetch 解析 entries
                            // → 逐 entry DMA-read 各页数据。
                            let first_tok = ctx.dma_read(prp1, NVME_PAGE_SIZE as u32);
                            self.pending_ios.insert(
                                first_tok,
                                PendingIo {
                                    sq_id,
                                    cid,
                                    sq_head,
                                    cq_id,
                                    nsid,
                                    op: PendingOp::NvmWritePiMulti { op_id, page_idx: 0 },
                                },
                            );
                            // Fetch PRP-list 页本身（4 KiB u64 array）
                            let list_tok = ctx.dma_read(prp2, NVME_PAGE_SIZE as u32);
                            self.pending_ios.insert(
                                list_tok,
                                PendingIo {
                                    sq_id,
                                    cid,
                                    sq_head,
                                    cq_id,
                                    nsid,
                                    op: PendingOp::NvmWritePiListFetch { op_id },
                                },
                            );
                            tracing::info!(
                                nsid,
                                nlb,
                                data_bytes_total,
                                pages_total,
                                "K4c-list PI Write: PRP1 + list fetch dispatched"
                            );
                        }
                        return None;
                    }
                    let total_lba = ns.total_lba;
                    if slba >= total_lba {
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE));
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
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE));
                    }
                }
                // **Reviewer M-2 + H-1** — 普通 NVM WRITE 落到 ZNS NS 时必须
                // 强制 SWR + 状态 + 边界检查（共享 check_zns_write helper）。
                if let Some(cqe) = check_zns_write(ns, slba, nlb, cid, sq_id, sq_head, phase) {
                    return Some(cqe);
                }
                // **2026-06-09 纯 4K** — plain 路径按 per-NS 扇区字节重算传输大小
                // + 复查 MDTS（dma_read 拉的数据量、后续 completion 写盘偏移据此）。
                let bytes = nlb as u64 * sector_bytes;
                if bytes > MDTS_MAX_BYTES {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
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
                    self.namespaces.keys().collect()
                } else if self.namespaces.contains_key(&nsid) {
                    vec![nsid]
                } else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                    ));
                };
                for target in targets {
                    let ns = self.namespaces.get_mut(&target).unwrap();
                    // **Phase M2** — 走 ns.flush()：mmap 路径 → msync；否则
                    // file.sync_all()。Flush 是 NVMe driver 拿持久化承诺
                    // 的唯一同步点（VWC=1 让 driver 主动发）。
                    if let Err(e) = ns.flush() {
                        tracing::warn!(error = %e, nsid = target, "FLUSH failed");
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
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
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
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
                    ));
                };
                // **Phase S1** — NS Write Protection gate（WRITE_ZEROES）
                if let Some(cqe) = check_ns_write_protection(ns, cid, sq_id, sq_head, phase) {
                    return Some(cqe);
                }
                let is_pi_path = ns.lbads == 12 && ns.meta_size == 8 && ns.pi_type == 1;
                // **2026-06-09**：plain 含 512B(LBAF[0]) 与纯-4K(LBAF[2])，按
                // 每-LBA sector_bytes = 1<<lbads 计算偏移；is_pi_path 走 PI 分支。
                let is_plain =
                    ns.meta_size == 0 && !ns.pi_enabled() && (ns.lbads == 9 || ns.lbads == 12);
                let sector_bytes = 1u64 << ns.lbads;
                if !is_pi_path && !is_plain {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                let total_lba = ns.total_lba;
                // **H4 修复**：用 checked_add 防 slba + nlb 溢出。
                match slba.checked_add(nlb as u64) {
                    Some(end) if end <= total_lba => {} // 范围合法
                    _ => {
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE));
                    }
                }
                // **Reviewer H-1** — WRITE_ZEROES 在 ZNS NS 上同样守 SWR
                if let Some(cqe) = check_zns_write(ns, slba, nlb, cid, sq_id, sq_head, phase) {
                    return Some(cqe);
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
                            ));
                        }
                    }
                    return Some(Cqe::success(cid, sq_id, sq_head, phase));
                }
                // **C2 修复**：分块写复用 4 KiB 零 buffer，避免大 nlb 时
                // 一次性分配 32 MiB+ Vec OOM。每块独立 write，spec 允许
                // controller 在中途因 error abort（我们这里若中途失败直接返）。
                // **Phase M2** — write_at 走 mmap 零拷贝（chunk-by-chunk
                // 是历史 file IO 分块；mmap 路径下其实可以一次性 copy，但
                // 保留 chunked 让 fallback file IO 路径不一次写 32 MiB）。
                const CHUNK: usize = 4096;
                let zero_buf = [0u8; CHUNK];
                // **2026-06-09**：plain 4K 时按 sector_bytes 重算字节数/偏移。
                let bytes = nlb as u64 * sector_bytes;
                let mut remaining = bytes as usize;
                let mut off = slba * sector_bytes;
                let ns = self.ns_mut(nsid).unwrap();
                while remaining > 0 {
                    let n = remaining.min(CHUNK);
                    if let Err(e) = ns.write_at(&zero_buf[..n], off) {
                        tracing::warn!(error = %e, slba, nlb, off, "WRITE ZEROES chunk failed");
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::DATA_TRANSFER_ERROR,
                        ));
                    }
                    remaining -= n;
                    off += n as u64;
                }
                // **Reviewer H-1** — 同 plain WRITE：ZNS NS 上 WZ 成功后推进 WP。
                advance_zns_wp(self.ns_mut(nsid).unwrap(), slba, nlb);
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
                let nsid = sqe.nsid;
                let Some(ns) = self.ns(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                    ));
                };
                // **Phase S1** — NS Write Protection gate（DSM Deallocate 等效 write）
                if let Some(cqe) = check_ns_write_protection(ns, cid, sq_id, sq_head, phase) {
                    return Some(cqe);
                }
                let nr = (sqe.cdw10 & 0xff) as u32 + 1;
                let ad = sqe.cdw11 & 0x4 != 0;
                tracing::debug!(nr, ad, "DSM Dataset Management (no-op success)");
                Some(Cqe::success(cid, sq_id, sq_head, phase))
            }
            nvm_opc::COPY => {
                // **Phase O1** — Simple Copy (NVMe 2.0 NVM CS § 3.3.5)。
                //
                // 教学价值：演示 controller-side 数据搬运 — driver 提供
                // source range list，controller 自己读 backing + 写到
                // SDLBA。host 端无 data PRP，节省 host↔guest DMA 双程。
                //
                // 字段：
                //   CDW10/11 = SDLBA (destination start LBA)
                //   CDW12 bits 7:0 = NR (number of source ranges - 1, 0-based)
                //   CDW12 bits 15:8 = Source Range Format (我们只支持 0x00 = Format 0)
                //   PRP1 = source range list buffer，每条 32 byte：
                //     bytes 0..8  = SLBA
                //     bytes 16..18 = NLB (0-based)
                //     其余字段（ELBT/EATM/...）= PI / 高级特性，本路径忽略
                //
                // 我们走 DMA-read PRP1 → 完成回调 per-range copy backing。
                let nsid = sqe.nsid;
                let sdlba = sqe.cdw10 as u64 | ((sqe.cdw11 as u64) << 32);
                let nr = ((sqe.cdw12 & 0xff) as u32) + 1; // 0-based → count
                let srf = ((sqe.cdw12 >> 8) & 0xff) as u8;
                if srf != 0 {
                    tracing::warn!(srf, "COPY: only Source Range Format 0 supported");
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                let Some(ns) = self.ns(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                    ));
                };
                // **Phase S1** — NS Write Protection gate（COPY 写目的 NS）
                if let Some(cqe) = check_ns_write_protection(ns, cid, sq_id, sq_head, phase) {
                    return Some(cqe);
                }
                if ns.pi_enabled() {
                    // PI + Copy 组合需要 per-range PI tuple verify/regen，
                    // 教学路径未实现，返 INVALID_PROTECTION_INFO 让 driver 知道。
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_PROTECTION_INFO,
                    ));
                }
                // ZNS：Copy 目标 zone 需走 SWR 校验，简化先拒
                if ns.zns.is_some() {
                    tracing::warn!(nsid, "COPY on ZNS NS not supported");
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_OPCODE));
                }
                let range_list_bytes = nr as u32 * 32;
                if range_list_bytes as u64 > NVME_PAGE_SIZE {
                    // 教学：range list > 1 page 需 PRP-list 取，本路径暂限 1 page = 128 ranges
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                let tok = ctx.dma_read(sqe.prp1, range_list_bytes);
                self.pending_ios.insert(
                    tok,
                    PendingIo {
                        sq_id,
                        cid,
                        sq_head,
                        cq_id,
                        nsid,
                        op: PendingOp::NvmCopyFetchRanges {
                            sdlba,
                            num_ranges: nr,
                        },
                    },
                );
                None
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
                let Some(ns) = self.ns(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                    ));
                };
                // **2026-06-09**：plain 含 512B(LBAF[0]) 与纯-4K(LBAF[2])；DMA
                // 字节数与 backing 偏移均按 sector_bytes = 1<<lbads 计算。
                let is_plain =
                    ns.meta_size == 0 && !ns.pi_enabled() && (ns.lbads == 9 || ns.lbads == 12);
                if !is_plain {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                let sector_bytes = 1u64 << ns.lbads;
                let total_lba = ns.total_lba;
                let bytes = nlb as u64 * sector_bytes;
                tracing::debug!(nsid, slba, nlb, bytes, "NVM COMPARE");
                if bytes > MDTS_MAX_BYTES {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                match slba.checked_add(nlb as u64) {
                    Some(end) if end <= total_lba => {}
                    _ => {
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE));
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
                    ));
                };
                let is_pi_path = ns.lbads == 12 && ns.meta_size == 8 && ns.pi_type == 1;
                // **2026-06-09**：plain 含 512B 与纯-4K(LBAF[2])。plain NS 无 PI
                // 元数据可校验，VERIFY 直接成功；4K 在此与 512B 同样放行。
                let is_plain =
                    ns.meta_size == 0 && !ns.pi_enabled() && (ns.lbads == 9 || ns.lbads == 12);
                if !is_pi_path && !is_plain {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                let total_lba = ns.total_lba;
                match slba.checked_add(nlb as u64) {
                    Some(end) if end <= total_lba => {}
                    _ => {
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE));
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
                            self.push_error_log(
                                sq_id,
                                cid,
                                sc::sf_of(sc::status(sc_code, sc::SCT_MEDIA_DATA_INTEGRITY)),
                                lba,
                                nsid,
                            );
                            return Some(Cqe::error(
                                cid,
                                sq_id,
                                sq_head,
                                phase,
                                sc::status(sc_code, sc::SCT_MEDIA_DATA_INTEGRITY),
                            ));
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
                    ));
                };
                // **Phase S1** — NS Write Protection gate（Zone Mgmt Send 含
                // Reset/Open/Finish/Close/Offline 均改变 zone 状态，对 protect
                // NS 一律拒绝）
                if let Some(cqe) = check_ns_write_protection(ns, cid, sq_id, sq_head, phase) {
                    return Some(cqe);
                }
                let Some(zns) = ns.zns.as_mut() else {
                    tracing::warn!(nsid, "Zone Mgmt Send: not a ZNS NS");
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_OPCODE));
                };
                // 构造要处理的 zone 索引列表（select-all → 全部；否则只一个）
                let zone_indices: Vec<usize> = if select_all {
                    (0..zns.zones.len()).collect()
                } else {
                    let zone_idx = (slba / zns.zone_size) as usize;
                    if zone_idx >= zns.zones.len() {
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE));
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
                        return Some(Cqe::error(cid, sq_id, sq_head, phase, sc_byte));
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
                        ));
                    }
                    if max_active > 0 && cur_active + new_active > max_active {
                        return Some(Cqe::error(
                            cid,
                            sq_id,
                            sq_head,
                            phase,
                            sc::TOO_MANY_ACTIVE_ZONES,
                        ));
                    }
                }
                // 第二遍：apply transitions — 委托 apply_zsa 纯函数（H-1 修复
                // 后逻辑唯一来源，spec 表中 `-` no-op 不改写）。
                let zone_capacity = zns.zone_capacity;
                for i in zone_indices {
                    let zone = &mut zns.zones[i];
                    apply_zsa(zone, zsa, zone_capacity);
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
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                };
                let bytes = numd as usize * 4;
                // **review M-1（2026-06-10）** — host 控的 NUMD→bytes 无上限会让 build_*
                // 分配巨量内存（最坏 ~16 GiB DoS）。比照 Get Log Page 卡 2 MiB（单 PRP-list
                // 页可服务范围）；超过 INVALID_FIELD 让 driver 分块。
                if bytes > 2 * 1024 * 1024 {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                let zra = (sqe.cdw13 & 0xff) as u8;
                let Some(ns) = self.ns(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                    ));
                };
                let Some(zns) = ns.zns.as_ref() else {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_OPCODE));
                };
                if zra != 0 {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                let start_zone_idx = (slba / zns.zone_size) as usize;
                let buf = build_zone_report(zns, start_zone_idx, bytes);
                self.dma_write_then_complete(
                    ctx, sqe.prp1, sqe.prp2, buf, cid, sq_id, sq_head, cq_id,
                );
                None
            }
            nvm_opc::ZONE_APPEND => {
                // **Phase L1 + reviewer H1/H8 + Phase Q2** — Zone Append
                // (ZNS CS § 4.3)。CDW10/11 = ZSLBA (zone 起点)；driver 不知 WP，
                // controller 把数据写在当前 WP 处，把实际 LBA 写回 CQE.dw0/dw1。
                // CDW12 bits 15:0 = NLB - 1。
                //
                // **Q2 解禁 PI**: 之前 H8 拒 PI NS（因 SECTOR_SIZE 常量限制）。
                // 现用 ns.data_bytes() (host data 单位) + ns.block_bytes()
                // (backing 单位含 PI tuple) 自适应；completion 路径按 is_pi
                // 分支决定是否插入 PI tuple。教学限制仍：单 PRP only
                // (data_bytes ≤ NVME_PAGE_SIZE)。
                let nsid = sqe.nsid;
                let prp1 = sqe.prp1;
                let zslba = sqe.cdw10 as u64 | ((sqe.cdw11 as u64) << 32);
                let nlb = (sqe.cdw12 & 0xffff) as u32 + 1;
                let Some(ns) = self.ns(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                    ));
                };
                // **Phase S1** — NS Write Protection gate（ZONE_APPEND 写入路径）
                if let Some(cqe) = check_ns_write_protection(ns, cid, sq_id, sq_head, phase) {
                    return Some(cqe);
                }
                // is_pi_path / block_bytes 在 completion 路径按 ns 字段查；
                // dispatch 只需 host_bytes (driver PRP data size)。
                let bytes_per_lba_host = ns.data_bytes(); // 512 or 4096
                let host_bytes = nlb as u64 * bytes_per_lba_host;
                let Some(zns) = ns.zns.as_ref() else {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_OPCODE));
                };
                if host_bytes > NVME_PAGE_SIZE {
                    // 单 PRP only for ZNS Append (教学简化)
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                let zone_idx = (zslba / zns.zone_size) as usize;
                if zone_idx >= zns.zones.len() {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::LBA_OUT_OF_RANGE));
                }
                let zone = zns.zones[zone_idx];
                if matches!(zone.state, ZoneState::ReadOnly) {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::ZONE_IS_READ_ONLY,
                    ));
                }
                if matches!(zone.state, ZoneState::Offline) {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::ZONE_IS_OFFLINE));
                }
                if matches!(zone.state, ZoneState::Full) {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::ZONE_IS_FULL));
                }
                if zone.write_pointer + nlb as u64 > zns.zone_capacity {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::ZONE_BOUNDARY_ERR,
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
                            matches!(z.state, ZoneState::ImplicitOpen | ZoneState::ExplicitOpen)
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
                // DMA-read data (host_bytes = N × data_bytes_per_lba) →
                // 完成回调 NvmZoneAppend 负责：write_at(assigned_lba * block_bytes)
                // 写文件（PI NS 时 per-LBA compute tuple + interleave，否则
                // 直接 write data）+ success(CQE dw0/dw1=assigned_lba) 或
                // failure(rollback + error CQE)。
                let tok = ctx.dma_read(prp1, host_bytes as u32);
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
                Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_OPCODE))
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
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                };
                let eds = sqe.cdw11 & 0x1 != 0;
                if eds {
                    tracing::warn!(nsid, "Reservation Report EDS=1 not yet supported");
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                let bytes = numd as usize * 4;
                // **review M-1（2026-06-10）** — host 控的 NUMD→bytes 无上限会让 build_*
                // 分配巨量内存（最坏 ~16 GiB DoS）。比照 Get Log Page 卡 2 MiB（单 PRP-list
                // 页可服务范围）；超过 INVALID_FIELD 让 driver 分块。
                if bytes > 2 * 1024 * 1024 {
                    return Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD));
                }
                let Some(ns) = self.ns(nsid) else {
                    return Some(Cqe::error(
                        cid,
                        sq_id,
                        sq_head,
                        phase,
                        sc::INVALID_NAMESPACE,
                    ));
                };
                let buf = super::reservation::build_reservation_report(ns, bytes);
                self.dma_write_then_complete(
                    ctx, sqe.prp1, sqe.prp2, buf, cid, sq_id, sq_head, cq_id,
                );
                None
            }
            opc => {
                tracing::warn!(opc, "unsupported NVM opcode");
                Some(Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_OPCODE))
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
            ));
        }
        let action = (sqe.cdw10 & 0x7) as u8;
        let rtype = ((sqe.cdw10 >> 8) & 0xff) as u8;
        // **Phase P1** — CPTPL only applicable to Register（spec § 6.13）；
        // 其它命令该字段保留。00=no change / 01=clear PTPL / 11=set PTPL；
        // 02=reserved。
        let cptpl = ((sqe.cdw10 >> 30) & 0x3) as u8;
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
                    cptpl,
                },
            },
        );
        None
    }
}
