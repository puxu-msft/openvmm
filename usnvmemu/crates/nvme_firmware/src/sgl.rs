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

/// 解析一段 SGL list（已通过 DMA-read 拿到 raw bytes），按 16-byte 切片
/// 返回所有 descriptor。caller 负责处理 Segment chain（递归 DMA-read 下
/// 一段）+ Last Segment 终止 + bit bucket skip。
///
/// 返 Err 描述哪里出错；调用方按 sc::SGL_* 转 CQE。
///
/// **Phase R2a** 起已 wire：`controller/completion.rs::NvmSglFetch` 用本函数
/// 解析 PSDT=10 segment 页里的 descriptor 数组。
pub(crate) fn parse_sgl_list(buf: &[u8]) -> Result<Vec<SglDescriptor>, &'static str> {
    if !buf.len().is_multiple_of(16) {
        return Err("SGL list bytes not multiple of 16");
    }
    let mut out = Vec::with_capacity(buf.len() / 16);
    for chunk in buf.chunks_exact(16) {
        let arr: [u8; 16] = chunk.try_into().unwrap();
        let desc = SglDescriptor::parse(&arr).ok_or("SGL descriptor type unknown")?;
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
        assert!(parse_sgl_list(&[0u8; 17]).is_err());
        assert!(parse_sgl_list(&[0u8; 15]).is_err());
        // 3 个合法 Data Block descriptor（48 字节）→ Ok 且 count==3（返 Ok(vec![])
        // 的 mutant 会让 len 断言红）。
        let mut buf = vec![0u8; 48];
        buf[8..12].copy_from_slice(&512u32.to_le_bytes()); // desc0 length
        buf[16 + 8..16 + 12].copy_from_slice(&1024u32.to_le_bytes()); // desc1 length
        let descs = parse_sgl_list(&buf).unwrap();
        assert_eq!(descs.len(), 3);
        assert_eq!(descs[0].length, 512);
        assert_eq!(descs[1].length, 1024);
        // 含未知 type（high nibble 0x6）→ Err。
        let mut bad = vec![0u8; 16];
        bad[15] = 0x60;
        assert!(parse_sgl_list(&bad).is_err());
    }
}
