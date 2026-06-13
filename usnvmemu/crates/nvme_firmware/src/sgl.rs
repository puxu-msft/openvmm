// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe SGL (Scatter Gather List) descriptor 解析 (spec § 4.4)。
//!
//! SGL 是 PRP 的替代寻址机制，driver 通过 SQE.cdw0.PSDT=01/10 选择。
//! 与 PRP 区别：
//! - PRP 每条目寻址 1 个 page（4 KiB 对齐）
//! - SGL 每条目可任意 length（不需 page 对齐）+ 支持 chain / discard
//!
//! ## Descriptor 布局（16 byte 固定）
//!
//! ```text
//! offset 0..8   Address (GPA for Data Block; pointer for Segment)
//! offset 8..12  Length (u32 字节数)
//! offset 12..15 reserved
//! offset 15     SGL Identifier
//!               high nibble (bits 7:4) = SGL Descriptor Type:
//!                 0x0 = Data Block
//!                 0x1 = Bit Bucket（discard 写 / 返 0 读，spec § 4.4.1.2）
//!                 0x2 = Segment（指向后续 SGL list 内存段）
//!                 0x3 = Last Segment（同 Segment，但保证是最后一段）
//!                 0x4 = Keyed Data Block (NVMe-oF)
//!                 0x5 = Transport-specific (NVMe-oF)
//!               low nibble (bits 3:0) = Sub Type:
//!                 0x0 = Address (本地 host 内存)
//!                 0x1 = Offset (CMB-relative)
//!                 0x2-0xF = reserved / vendor
//! ```
//!
//! ## 教学限制（R1，2026-06-10 校正 advertise⟺implement）
//!
//! `io.rs::resolve_data_pointers` 当前**只**接受 PSDT=01 的 **inline 单 Data Block
//! (Type 0)、sub_type=0 (Address)、length ≤ 1 page**，映射成单 PRP 复用 PRP 路径。
//! 其余一律返 SGL_DESCRIPTOR_TYPE_INVALID：
//! - Bit Bucket (Type 1) / Segment / Last Segment (Type 2/3)：**未 wire**
//!   （`parse_sgl_list` + `flatten_data_blocks` 是 R2 scaffolding，dead-code 待接）。
//!   SGLS 也已不 advertise Bit Bucket（一致性，见 `cmd.rs` sgls）。
//! - Keyed Data Block / Transport-specific：NVMe-oF 专属，本地 PCIe 不用。
//! - PSDT=10 (Segment pointer)：留 R2，见
//!   `docs/plans/2026-06-10-sgl-r2-segment-chains-detailed.md`。
//!
//! ## CMB-P2（2026-06-12，CMB-relative SGL 放行）
//!
//! 上述 R1 限制中的"只接受 sub_type=0"已被 **CMB-P2** 放宽：当 controller 的 CMB
//! **已启用**（CMBMSC.CMSE=1）时，sub_type=1（Offset/CMB-relative）也合法——其 address
//! 字段是**相对 CMB 起点（CMBMSC.CBA）的偏移**（spec § 4.4 SGL Offset sub-type），经
//! [`resolve_sgl_address`] rebase 成实际 GPA `cba+offset` 后交 `guest_*`（命中 CMB
//! backing）。CMB **未启用**时 sub_type=1 仍返 `SGL_INVALID_USE_OF_CMB` (0x12)（现状）。
//! 三条 SGL 路径（inline / segment 指针 / segment 页 Data Block）共用 [`subtype_to_sc`]
//! 的 `cmb_enabled` 入参 + [`resolve_sgl_address`] 的 rebase 规则，判据自动一致。
//!
//! **SGLS 位（Identify Controller offset 536）裁定（P5 已落地）**：NVMe Base 2.0
//! § 5.1.13.2 的 SGLS 字段 **bit 20 = "SGL Address Field Specifies an Offset"（SAOS，
//! CMB-relative 支持；Linux 内核 `NVME_CTRL_SGLS_SAOS` = `1<<20` 作独立 oracle）**。
//! **CMB-P5** 已把它**条件化** advertise：`cmd.rs::build_v2_bytes_with_cmb` 在 CMB 启用
//! （`controller/admin.rs` 传 `self.cmb.is_some()`）时置 bit20，CMB off 时不置——满足本仓库
//! advertise⟺implement 纪律。注意门控用 `cmb.is_some()`（CMB 已 advertise）而非 CMSE：
//! driver 先读 Identify 看 SAOS + CMBLOC/CMBSZ，**之后**才编程 CMBMSC.CMSE（鸡生蛋，见
//! admin.rs 注释）。CMB-relative SGL 的**功能正确性**仍由本文件 classifier（`subtype_to_sc`）
//! 兜底，不依赖该 advertise 位。

/// SGL Descriptor Type (high nibble of byte 15)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SglType {
    DataBlock = 0x0,
    BitBucket = 0x1,
    Segment = 0x2,
    LastSegment = 0x3,
    KeyedDataBlock = 0x4,
    TransportSpecific = 0x5,
}

impl SglType {
    pub(crate) fn from_byte(b: u8) -> Option<Self> {
        match b >> 4 {
            0x0 => Some(Self::DataBlock),
            0x1 => Some(Self::BitBucket),
            0x2 => Some(Self::Segment),
            0x3 => Some(Self::LastSegment),
            0x4 => Some(Self::KeyedDataBlock),
            0x5 => Some(Self::TransportSpecific),
            _ => None,
        }
    }
}

/// 解码 16-byte SGL descriptor。
#[derive(Debug, Clone, Copy)]
pub(crate) struct SglDescriptor {
    pub(crate) address: u64,
    pub(crate) length: u32,
    pub(crate) sgl_type: SglType,
    /// Sub type（低 nibble）。当前我们只接受 0x0 Address。
    pub(crate) sub_type: u8,
}

impl SglDescriptor {
    /// Parse 16 byte SGL descriptor。返 None 表示 type 未识别（caller 应
    /// 返 SGL_DESCRIPTOR_TYPE_INVALID）。
    pub(crate) fn parse(buf: &[u8; 16]) -> Option<Self> {
        let address = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let length = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        let id = buf[15];
        let sgl_type = SglType::from_byte(id)?;
        let sub_type = id & 0x0F;
        Some(Self {
            address,
            length,
            sgl_type,
            sub_type,
        })
    }
}

/// **CMB-SGL spec-completeness 共享 classifier** — 把一个 SGL descriptor 的
/// sub_type（byte 15 低 nibble）+ **CMB 当前是否启用**映射到"是否非法 + 精确
/// NVMe Status Code"。
///
/// 三条 SGL 路径共用本判据，使**同一非法 sub_type 跨路径返同一 SC**，不再漂移：
/// - `parse_sgl_list`（PSDT=10 segment 页里的 Data Block descriptor）
/// - `controller/io.rs::resolve_data_pointers`（PSDT=01 inline 单 Data Block）
/// - `controller/io.rs::validate_segment_pointer`（PSDT=10 SGL1 / chain continuation 指针）
///
/// **CMB-P2（CMB-relative 放行）**：`cmb_enabled` = CMB 是否启用（CMBMSC.CMSE=1）。
/// sub_type=1（Offset/CMB-relative）的合法性**取决于运行时是否有可用 CMB**——三路径
/// 统一从 controller 的 CMB 状态取同一个 `cmb_enabled` 入参，故放行/拒绝判据跨三路径
/// 自动一致（classifier 集中化纪律）。
///
/// 映射（spec § 4.4 SGL Descriptor sub-type；NVMe Base 2.0 § 4.4 "Address Field
/// Specifies an Offset"）：
/// - `0`（Address，host 内存）→ `None`：合法，放行（与 CMB 无关）。
/// - `1`（Offset，CMB-relative）：
///   * `cmb_enabled == true` → `None`：放行。其 address 字段是**相对 CMB 起点
///     （CMBMSC.CBA）的偏移**，调用方经 [`resolve_sgl_address`] 解析成实际 GPA
///     `cba + offset` 后交 `guest_read`/`guest_write`（自然命中 CMB backing）。
///     偏移越界由 `guest_*` 的 `cmb_hit`（straddle 判定）兜底。
///   * `cmb_enabled == false` → `Some(SGL_INVALID_USE_OF_CMB)` (0x12)：无可用 CMB
///     （Identify/CMBLOC/CMBSZ 全 0 或 CMSE 未置），偏移无从解析为 GPA，放行会被误当
///     GPA 解引用 → 必须以 generic status 0x12 拒（spec generic status 0x12 "SGL
///     Invalid Use of CMB"）。
/// - `≥2`（reserved/vendor）→ `Some(SGL_DESCRIPTOR_TYPE_INVALID)` (0x11)：本教学实现
///   一律以 Descriptor Type Invalid 拒（与 CMB 无关）。
pub(crate) fn subtype_to_sc(sub_type: u8, cmb_enabled: bool) -> Option<u16> {
    use crate::cmd::sc;
    match sub_type {
        0 => None,
        1 if cmb_enabled => None,
        1 => Some(sc::SGL_INVALID_USE_OF_CMB),
        _ => Some(sc::SGL_DESCRIPTOR_TYPE_INVALID),
    }
}

/// **CMB-P2 / P5** — 把一个 SGL descriptor 的 address 字段按 sub_type 解析成实际
/// guest 物理地址（GPA），供 `guest_read`/`guest_write` 使用。三条 SGL 路径
/// （inline / segment 指针 / segment 页内 Data Block）**共用本一处** rebase 规则，
/// 与 [`subtype_to_sc`] 同样集中，避免某路径忘了 rebase 而把 CMB 偏移误当 GPA。
///
/// 返 `Result<u64, u16>`：`Ok(gpa)` 是可喂 `guest_*` 的实际 GPA；`Err(sc)` 是精确
/// NVMe Status Code，caller 经 `?` 透传给 CQE（三路径已全返 `Result<_, u16>`）。
///
/// 规则（spec § 4.4 SGL Offset sub-type）：
/// - sub_type=0（Address）→ 原样返回 `Ok(address)`（address 本就是 GPA，不受 CMB 约束）。
/// - sub_type=1（Offset/CMB-relative）→ `cba + offset`（CMB 在 guest 地址空间的实际
///   位置 = CMBMSC.CBA + descriptor 内偏移）。
///   * `cmb` 为 `Some((cba, size))`：
///     - **`offset < size`**（落在 CMB 窗口 `[0, size)` 内）→ `Ok(cba + offset)`。
///     - **`offset ≥ size`**（起点已出窗口）→ `Err(SGL_OFFSET_INVALID)` (0x16)。
///       **CMB-P5 严格化**：取代旧行为（rebase 成 `cba+offset` 出窗 → `cmb_hit` Miss →
///       lenient 走 DMA）。spec § 4.4 要求 SGL Offset 落在 CMB 内，越界即非法字段，应以
///       generic status 0x16 拒，而非把 CMB-relative 访问悄悄当真实 GPA 发 DMA。
///       （注：`offset < size` 但 `offset + len` 越尾的 straddle，由 `cmb_hit` 在 dispatch
///       期判 `ok=false`——本函数只见 address 不见 len，故越尾 straddle 不在此拦。）
///   * `cmb` 为 `None`（CMB 未启用）时**不该到达**（`subtype_to_sc` 已先拒 sub_type=1），
///     防御性地原样返回 `Ok(offset)`（后续 `cmb_hit` Miss → 走 DMA，行为可预测、不 panic）。
///
/// **注意**：本函数只处理 sub_type∈{0,1}；sub_type≥2 由 `subtype_to_sc` 在更早处拒，
/// 不会带着非法 sub_type 走到这里。
pub(crate) fn resolve_sgl_address(
    sub_type: u8,
    address: u64,
    cmb: Option<(u64, u64)>,
) -> Result<u64, u16> {
    use crate::cmd::sc;
    match (sub_type, cmb) {
        // CMB-relative：偏移必须落在 CMB 窗口 [0, size) 内。
        (1, Some((cba, size))) => {
            if address >= size {
                // 起点已出窗口 → SGL Offset Invalid（CMB-P5 严格化，取代旧 lenient DMA）。
                Err(sc::SGL_OFFSET_INVALID)
            } else {
                // offset < size：rebase 成实际 GPA。`address < size` 且 size 为有界
                // CMBSZ，`cba` 为 4 KiB 对齐 CBA → `cba + address` 不溢出（cmb_hit
                // 的 win_end checked_add 已是窗口上界守卫的真相源）。
                Ok(cba + address)
            }
        }
        // Address（sub_type=0）或防御性 fallback（None）：原样。
        _ => Ok(address),
    }
}

/// 解析一段 SGL list（已通过 DMA-read 拿到 raw bytes），按 16-byte 切片
/// 返回所有 descriptor。caller 负责处理 Segment chain（递归 DMA-read 下
/// 一段）+ Last Segment 终止 + bit bucket skip。
///
/// 返 `Err(u16)` 时，u16 是**精确的 NVMe Status Code**（含 SCT 高字节），caller
/// 直接透传给 `finish_sgl_error` → CQE，无需在调用点再手挑 SC：
/// - 字节数非 16 倍数 → `INVALID_SGL_SEGMENT_DESCRIPTOR` (0x0d)
/// - `SglDescriptor::parse` 返 None（type 高 nibble 未识别）→ `SGL_DESCRIPTOR_TYPE_INVALID` (0x11)
/// - sub_type=1（Offset / CMB-relative）→ **CMB 启用时放行并 rebase**（见下）；CMB 未启用
///   时 `SGL_INVALID_USE_OF_CMB` (0x12)
/// - sub_type≥2（reserved/vendor）→ `SGL_DESCRIPTOR_TYPE_INVALID` (0x11)
///
/// **CMB-P2**：`cmb` = controller 当前 CMB 窗口 `Some((cba, size))`（CMSE=1 时）/ `None`。
/// 经共享 [`subtype_to_sc`]`(sub_type, cmb.is_some())` 判合法性，并对放行的 sub_type=1
/// descriptor 用 [`resolve_sgl_address`] **就地把偏移 rebase 成实际 GPA**（`cba + offset`），
/// 使 caller（completion.rs 的 fragment walk）拿到的 `address` 直接是可喂 `guest_*` 的 GPA，
/// 自然命中 CMB backing。三路径同一 rebase 规则。
///
/// **Phase R2a** 起已 wire：`controller/completion.rs::NvmSglFetch` 用本函数
/// 解析 PSDT=10 segment 页里的 descriptor 数组。
pub(crate) fn parse_sgl_list(
    buf: &[u8],
    cmb: Option<(u64, u64)>,
) -> Result<Vec<SglDescriptor>, u16> {
    // 错误现在直接返**精确 NVMe SC**（u16，含 SCT 高字节），caller 透传给 CQE，
    // 不再在调用点手填一个粗粒度 SC——与 cmd.rs sc 模块"以 spec 源为准"同一纪律。
    use crate::cmd::sc;
    if !buf.len().is_multiple_of(16) {
        return Err(sc::INVALID_SGL_SEGMENT_DESCRIPTOR);
    }
    let mut out = Vec::with_capacity(buf.len() / 16);
    for chunk in buf.chunks_exact(16) {
        let arr: [u8; 16] = chunk.try_into().unwrap();
        let mut desc = SglDescriptor::parse(&arr).ok_or(sc::SGL_DESCRIPTOR_TYPE_INVALID)?;
        // ── 逐 descriptor sub_type 校验（spec § 4.4 SGL Descriptor sub-type）──
        // 经共享 `subtype_to_sc` classifier 判（三条 SGL 路径同判据，见其 doc）：
        // sub_type=0 (Address) 放行；sub_type=1 (Offset, CMB-relative) → CMB 启用时
        // 放行 + rebase（cba+offset），未启用时 generic status 0x12 = SGL Invalid Use
        // of CMB；sub_type≥2 (reserved/vendor) → Descriptor Type Invalid (0x11)。
        if let Some(err_sc) = subtype_to_sc(desc.sub_type, cmb.is_some()) {
            return Err(err_sc);
        }
        // **CMB-P2 / P5** — 放行的 CMB-relative descriptor：就地把偏移 rebase 成实际 GPA，
        // 使下游 fragment walk 拿到的 address 直接可喂 `guest_*`（命中 CMB backing）。
        // offset ≥ CMB size（越界）→ `resolve_sgl_address` 返 `Err(0x16)`，经 `?` 透传。
        desc.address = resolve_sgl_address(desc.sub_type, desc.address, cmb)?;
        out.push(desc);
    }
    Ok(out)
}

/// SGL Walker — 把一组 descriptor 展开成 (address, length) 数据片段 list，
/// 跳过 Bit Bucket（caller 用 None 表示），返扁平 transfer plan。
/// Segment / Last Segment 不在此 list 中（caller 必须先递归 fetch 展开）。
///
/// 教学版只支持 sub_type=0 (Address)；其它走 INVALID。
///
/// 当前 R1 仅 inline single-Data-Block；本函数留给 R2 Segment chain 使用。
#[allow(dead_code)]
pub(crate) enum SglFragment {
    Data { address: u64, length: u32 },
    BitBucket { length: u32 }, // 跳过这段（write 丢弃，read 填 0）
}

#[allow(dead_code)]
pub(crate) fn flatten_data_blocks(
    descriptors: &[SglDescriptor],
) -> Result<Vec<SglFragment>, &'static str> {
    let mut out = Vec::with_capacity(descriptors.len());
    for d in descriptors {
        // **CMB-P2 一致性锚（dead-code R2 scaffolding）** — 本函数当前**无生产调用点**
        // （仅测试引用）。此处 `sub_type != 0` 是 R1 遗留的**本地**判据，**未**走集中
        // classifier `subtype_to_sc`，故对 CMB-relative（sub_type=1）会一律 Err——与三条
        // live SGL 路径（parse_sgl_list / resolve_data_pointers / validate_segment_pointer
        // 已 CMB-aware）**不一致**。若将来 R2 wiring 采用本函数，**必须**改为经
        // `subtype_to_sc(d.sub_type, cmb_enabled)` 判 + `resolve_sgl_address(.., cmb)` rebase
        // （即把 `cmb` 窗口穿进来），否则会悄悄重新引入 CMB-relative 误拒。保留现状仅因
        // 它不在任何 live 路径上（classifier 集中化纪律：单一裁决点）。
        if d.sub_type != 0 {
            return Err("SGL sub_type != 0 (only Address subtype supported)");
        }
        match d.sgl_type {
            SglType::DataBlock => out.push(SglFragment::Data {
                address: d.address,
                length: d.length,
            }),
            SglType::BitBucket => out.push(SglFragment::BitBucket { length: d.length }),
            SglType::Segment | SglType::LastSegment => {
                return Err("Segment/LastSegment must be expanded by caller before flatten");
            }
            SglType::KeyedDataBlock | SglType::TransportSpecific => {
                return Err("Keyed/Transport SGL not supported (NVMe-oF only)");
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_data_block() {
        let mut buf = [0u8; 16];
        buf[0..8].copy_from_slice(&0xDEAD_BEEF_u64.to_le_bytes());
        buf[8..12].copy_from_slice(&4096u32.to_le_bytes());
        buf[15] = 0x00; // Type=0 Data Block, Sub=0 Address
        let d = SglDescriptor::parse(&buf).unwrap();
        assert_eq!(d.address, 0xDEAD_BEEF);
        assert_eq!(d.length, 4096);
        assert_eq!(d.sgl_type, SglType::DataBlock);
        assert_eq!(d.sub_type, 0);
    }

    #[test]
    fn parse_bit_bucket() {
        let mut buf = [0u8; 16];
        buf[8..12].copy_from_slice(&512u32.to_le_bytes());
        buf[15] = 0x10; // Type=1 Bit Bucket
        let d = SglDescriptor::parse(&buf).unwrap();
        assert_eq!(d.sgl_type, SglType::BitBucket);
        assert_eq!(d.length, 512);
    }

    #[test]
    fn parse_segment_and_last() {
        let mut buf = [0u8; 16];
        buf[15] = 0x20;
        assert_eq!(
            SglDescriptor::parse(&buf).unwrap().sgl_type,
            SglType::Segment
        );
        buf[15] = 0x30;
        assert_eq!(
            SglDescriptor::parse(&buf).unwrap().sgl_type,
            SglType::LastSegment
        );
    }

    #[test]
    fn reject_unknown_type() {
        let mut buf = [0u8; 16];
        buf[15] = 0xF0; // unknown
        assert!(SglDescriptor::parse(&buf).is_none());
    }

    #[test]
    fn flatten_data_and_bit_bucket() {
        let descs = vec![
            SglDescriptor {
                address: 0x1000,
                length: 512,
                sgl_type: SglType::DataBlock,
                sub_type: 0,
            },
            SglDescriptor {
                address: 0,
                length: 128,
                sgl_type: SglType::BitBucket,
                sub_type: 0,
            },
            SglDescriptor {
                address: 0x2000,
                length: 512,
                sgl_type: SglType::DataBlock,
                sub_type: 0,
            },
        ];
        let frags = flatten_data_blocks(&descs).unwrap();
        assert_eq!(frags.len(), 3);
        match &frags[1] {
            SglFragment::BitBucket { length } => assert_eq!(*length, 128),
            _ => panic!("expected BitBucket"),
        }
    }

    #[test]
    fn flatten_rejects_segment() {
        let descs = vec![SglDescriptor {
            address: 0x1000,
            length: 512,
            sgl_type: SglType::Segment,
            sub_type: 0,
        }];
        assert!(flatten_data_blocks(&descs).is_err());
    }

    #[test]
    fn flatten_rejects_keyed_and_transport() {
        let descs = vec![SglDescriptor {
            address: 0,
            length: 0,
            sgl_type: SglType::KeyedDataBlock,
            sub_type: 0,
        }];
        assert!(flatten_data_blocks(&descs).is_err());
    }

    /// **mutant-kill** — `from_byte` 的 Keyed(0x4)/Transport(0x5) 臂此前无测试
    /// （cargo-mutants 删这两臂存活）。逐 high-nibble 锁死 type 解码。
    #[test]
    fn from_byte_decodes_all_types() {
        assert_eq!(SglType::from_byte(0x00), Some(SglType::DataBlock));
        assert_eq!(SglType::from_byte(0x1f), Some(SglType::BitBucket)); // low nibble 忽略
        assert_eq!(SglType::from_byte(0x20), Some(SglType::Segment));
        assert_eq!(SglType::from_byte(0x30), Some(SglType::LastSegment));
        assert_eq!(SglType::from_byte(0x40), Some(SglType::KeyedDataBlock));
        assert_eq!(SglType::from_byte(0x50), Some(SglType::TransportSpecific));
        assert_eq!(SglType::from_byte(0x60), None);
        assert_eq!(SglType::from_byte(0xf0), None);
    }

    /// **mutant-kill** — `parse_sgl_list` 的 16-倍数守卫 + descriptor 计数此前
    /// 无直测（cargo-mutants 把守卫取反 / 返 `Ok(vec![])` 均存活）。
    #[test]
    fn parse_sgl_list_count_and_alignment() {
        // 非 16 倍数 → Err（守卫取反会让本断言红）。
        assert!(parse_sgl_list(&[0u8; 17], None).is_err());
        assert!(parse_sgl_list(&[0u8; 15], None).is_err());
        // 3 个合法 Data Block descriptor（48 字节）→ Ok 且 count==3（返 Ok(vec![])
        // 的 mutant 会让 len 断言红）。
        let mut buf = vec![0u8; 48];
        buf[8..12].copy_from_slice(&512u32.to_le_bytes()); // desc0 length
        buf[16 + 8..16 + 12].copy_from_slice(&1024u32.to_le_bytes()); // desc1 length
        let descs = parse_sgl_list(&buf, None).unwrap();
        assert_eq!(descs.len(), 3);
        assert_eq!(descs[0].length, 512);
        assert_eq!(descs[1].length, 1024);
        // 含未知 type（high nibble 0x6）→ Err。
        let mut bad = vec![0u8; 16];
        bad[15] = 0x60;
        assert!(parse_sgl_list(&bad, None).is_err());
    }

    /// **共享 classifier 单测** — `subtype_to_sc` 是三条 SGL 路径（parse_sgl_list /
    /// resolve_data_pointers / validate_segment_pointer）共用的 sub_type→SC 判据；
    /// 直锁其映射，防三处漂移。值经 `cmd::sc`（已 M1-anchor 到 nvme_spec）间接锚定。
    /// **CMB-P2**：无 CMB（`cmb_enabled=false`）时 sub_type=1 → 0x12（现状）。
    #[test]
    fn subtype_to_sc_maps_each_subtype() {
        use crate::cmd::sc;
        // 0 = Address（host 内存）→ 合法，None。
        assert_eq!(subtype_to_sc(0, false), None);
        // 1 = Offset（CMB-relative），CMB 未启用 → SGL_INVALID_USE_OF_CMB (0x12)。
        assert_eq!(subtype_to_sc(1, false), Some(sc::SGL_INVALID_USE_OF_CMB));
        // ≥2 = reserved/vendor → SGL_DESCRIPTOR_TYPE_INVALID (0x11)。逐值锁，
        // 防"只 match 2、漏 15"之类的部分实现（CMB 状态不影响 reserved 判定）。
        for st in 2u8..=15 {
            assert_eq!(
                subtype_to_sc(st, false),
                Some(sc::SGL_DESCRIPTOR_TYPE_INVALID),
                "sub_type={st} 应返 Descriptor Type Invalid"
            );
            assert_eq!(
                subtype_to_sc(st, true),
                Some(sc::SGL_DESCRIPTOR_TYPE_INVALID),
                "sub_type={st} 即使 CMB 启用仍 Descriptor Type Invalid"
            );
        }
    }

    /// **CMB-P2 classifier 放行 + rebase** — CMB 启用（`cmb_enabled=true`）时
    /// sub_type=1（Offset/CMB-relative）放行（`None`，非 0x12）；sub_type=0/≥2 不受 CMB
    /// 状态影响。`resolve_sgl_address` 把放行的 CMB-relative 偏移 rebase 成 `cba+offset`。
    #[test]
    fn cmb_relative_subtype_allowed_and_rebased_when_cmb_enabled() {
        use crate::cmd::sc;
        // 放行：CMB 启用 → sub_type=1 合法。
        assert_eq!(subtype_to_sc(1, true), None, "CMB 启用 → sub_type=1 放行");
        // sub_type=0 (Address) 与 CMB 无关，恒放行。
        assert_eq!(subtype_to_sc(0, true), None);
        assert_eq!(subtype_to_sc(0, false), None);

        // rebase：sub_type=1 + CMB 窗口 (cba=0x8000_0000, size=2 MiB) → cba+offset。
        let cba = 0x8000_0000u64;
        let cmb = Some((cba, 2 * 1024 * 1024u64));
        assert_eq!(
            resolve_sgl_address(1, 0x1000, cmb),
            Ok(cba + 0x1000),
            "CMB-relative 偏移 rebase 成 cba+offset"
        );
        // sub_type=0：address 本就是 GPA，原样（即使传了 cmb 窗口）。
        assert_eq!(resolve_sgl_address(0, 0xDEAD_0000, cmb), Ok(0xDEAD_0000));
        // CMB 未启用（None）：防御性原样返回偏移（subtype_to_sc 已先拒，不该到达）。
        assert_eq!(resolve_sgl_address(1, 0x1000, None), Ok(0x1000));
    }

    /// **CMB-P5（越界严格化）** — CMB-relative（sub_type=1）的 offset ≥ CMB size
    /// （起点已出窗口）必须返 `Err(SGL_OFFSET_INVALID)` (0x16)，**不再** rebase 成
    /// `cba+offset`（旧 lenient 行为：rebase 出窗 → cmb_hit Miss → 走 DMA）。
    /// 边界值 offset == size 也越界（窗口是半开区间 `[cba, cba+size)`）。
    #[test]
    fn resolve_sgl_address_rejects_offset_out_of_cmb_window() {
        use crate::cmd::sc;
        let cba = 0x8000_0000u64;
        let size = 2 * 1024 * 1024u64;
        let cmb = Some((cba, size));
        // offset < size：合法，rebase。
        assert_eq!(resolve_sgl_address(1, size - 1, cmb), Ok(cba + size - 1));
        // offset == size：越界（半开区间）→ 0x16。
        assert_eq!(
            resolve_sgl_address(1, size, cmb),
            Err(sc::SGL_OFFSET_INVALID),
            "offset == size 已出窗口 → SGL_OFFSET_INVALID"
        );
        // offset > size：越界 → 0x16。
        assert_eq!(
            resolve_sgl_address(1, size + 0x1000, cmb),
            Err(sc::SGL_OFFSET_INVALID),
        );
        // 恶意巨偏移：越界 → 0x16（不 panic、不 saturating rebase）。
        assert_eq!(
            resolve_sgl_address(1, u64::MAX, cmb),
            Err(sc::SGL_OFFSET_INVALID),
        );
        // sub_type=0 不受 CMB 窗口约束（address 是 GPA），即使 ≥ size 也原样放行。
        assert_eq!(
            resolve_sgl_address(0, cba + size + 1, cmb),
            Ok(cba + size + 1)
        );
    }

    /// **CMB-SGL spec-completeness（unit anchor）** — 本 controller 无 CMB
    /// （CMBLOC/CMBSZ=0），故 sub_type=1 (Offset, CMB-relative) 的 SGL Data Block
    /// 永远非法 → `parse_sgl_list` 必返精确 `SGL_INVALID_USE_OF_CMB` (0x12)，而非
    /// 粗粒度的 Descriptor Type Invalid。sub_type=0 (Address) 仍 Ok。sub_type≥2
    /// （reserved）走 Descriptor Type Invalid。
    ///
    /// 这是新行为的单元级锚点；e2e `openhcl_sgl_cmb_relative_offset_rejected` 在真
    /// wire 上验同一 SC（独立 oracle = firmware CQE status 0x0012）。
    #[test]
    fn parse_sgl_list_rejects_cmb_relative_subtype() {
        use crate::cmd::sc;
        // sub_type=1（id_byte 0x01 = type 0 Data Block + sub 1 Offset），CMB 未启用
        // （None）→ 0x12。
        let mut cmb = [0u8; 16];
        cmb[8..12].copy_from_slice(&4096u32.to_le_bytes());
        cmb[15] = 0x01;
        assert_eq!(
            parse_sgl_list(&cmb, None).unwrap_err(),
            sc::SGL_INVALID_USE_OF_CMB,
            "sub_type=1 (CMB-relative) + 无 CMB 必返 SGL_INVALID_USE_OF_CMB (0x12)"
        );
        // sub_type=0（id_byte 0x00 = type 0 Data Block + sub 0 Address）→ Ok。
        let mut addr = [0u8; 16];
        addr[8..12].copy_from_slice(&4096u32.to_le_bytes());
        addr[15] = 0x00;
        let ok = parse_sgl_list(&addr, None).expect("sub_type=0 (Address) 应 Ok");
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].sub_type, 0);
        // sub_type=2（id_byte 0x02 = type 0 Data Block + sub 2 reserved）→ 0x11。
        let mut rsvd = [0u8; 16];
        rsvd[15] = 0x02;
        assert_eq!(
            parse_sgl_list(&rsvd, None).unwrap_err(),
            sc::SGL_DESCRIPTOR_TYPE_INVALID,
            "sub_type≥2 (reserved) 必返 SGL_DESCRIPTOR_TYPE_INVALID (0x11)"
        );
    }

    /// **CMB-P2** — CMB 启用时 `parse_sgl_list` 放行 sub_type=1 并就地 rebase
    /// descriptor.address 成 `cba+offset`（segment-页 Data Block 路径）。
    #[test]
    fn parse_sgl_list_rebases_cmb_relative_when_enabled() {
        let cba = 0x8000_0000u64;
        let cmb = Some((cba, 2 * 1024 * 1024u64));
        // 单 Data Block：offset=0x1000、length=4096、sub_type=1（CMB-relative）。
        let mut d = [0u8; 16];
        d[0..8].copy_from_slice(&0x1000u64.to_le_bytes());
        d[8..12].copy_from_slice(&4096u32.to_le_bytes());
        d[15] = 0x01;
        let descs = parse_sgl_list(&d, cmb).expect("CMB 启用 → sub_type=1 放行");
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].sub_type, 1, "sub_type 保留（供下游辨识）");
        assert_eq!(
            descs[0].address,
            cba + 0x1000,
            "address 已 rebase 成 cba+offset（下游直接喂 guest_*）"
        );
    }
}
