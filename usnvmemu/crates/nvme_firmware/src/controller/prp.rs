// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **#4 PRP 非页对齐布局** —— PRP1 页内偏移感知的传输分段（spec NVMe Base § 4.1.1）。
//!
//! NVMe PRP 规则：一条命令的数据传输 L 字节由 PRP1（+ PRP2 作第二数据指针或 PRP-list
//! 指针）描述。**只有 PRP1 可带页内偏移 O**；其后所有段（PRP2 作数据页 / PRP-list 内
//! 各 entry）页内偏移必须为 0（spec 强制）。故：
//! - 首段（PRP1）逻辑字节 = `min(L, page - O)`；
//! - 其后整页；末段 partial = 余量。
//!
//! 本模块把"偏移→分段"逻辑集中成 **O(1) 纯函数**（无 per-segment Vec 分配，hot-path
//! 友好），供 plain / PI 各 dual-PRP / PRP-list gather·scatter 统一使用。
//!
//! **回归不变量**：当 O=0 时本模块产出与改造前"假设页对齐、首页满 page"行为**逐字节
//! 一致**（见单测 `offset_zero_matches_legacy`），故既有测试 + e2e 守住 plain 路径不回归。

use crate::regs::NVME_PAGE_SIZE;

/// PRP 三档布局（spec § 4.4）。档位选择随 PRP1 偏移 O 变化：
/// single 当 `L ≤ page-O`；dual 当 `L ≤ (page-O) + page`；否则 list。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrpTier {
    /// 单 PRP1（含偏移）即可覆盖。
    Single,
    /// PRP1（含偏移）+ PRP2 作第二数据页。
    Dual,
    /// PRP1（含偏移）+ PRP2 指向 PRP-list 页。
    List,
}

/// PRP1 的页内偏移 O = `prp1 & (page-1)`。
#[inline]
pub(crate) fn prp1_offset(prp1: u64) -> u64 {
    prp1 & (NVME_PAGE_SIZE - 1)
}

/// 首段（PRP1）逻辑字节数 = `min(total_len, page - O)`。
#[inline]
pub(crate) fn first_seg_len(offset: u64, total_len: u64) -> u64 {
    (NVME_PAGE_SIZE - offset).min(total_len)
}

/// 总 PRP 数据页数（含 PRP1）。O=0 时 == `ceil(total_len / page)`（legacy 一致）。
#[inline]
pub(crate) fn total_pages(offset: u64, total_len: u64) -> u32 {
    if total_len == 0 {
        return 0;
    }
    let first = first_seg_len(offset, total_len);
    if total_len <= first {
        1
    } else {
        1 + (total_len - first).div_ceil(NVME_PAGE_SIZE) as u32
    }
}

/// 第 `page_idx`（0-based）个 PRP 数据页应承载的逻辑字节数。
/// - page 0（PRP1）= `min(total_len, page - O)`；
/// - page i≥1 = `min(page, total_len - first - (i-1)*page)`（末页 partial）。
///
/// O(1)，无分配。`page_idx` 越界（≥ total_pages）返回 0。
#[inline]
pub(crate) fn page_size(page_idx: u32, offset: u64, total_len: u64) -> u32 {
    let first = first_seg_len(offset, total_len);
    if page_idx == 0 {
        return first as u32;
    }
    // page i≥1 在逻辑流里的起点。
    let start = first + (page_idx as u64 - 1) * NVME_PAGE_SIZE;
    if start >= total_len {
        return 0;
    }
    (total_len - start).min(NVME_PAGE_SIZE) as u32
}

/// 档位选择。
#[inline]
pub(crate) fn tier(offset: u64, total_len: u64) -> PrpTier {
    match total_pages(offset, total_len) {
        0 | 1 => PrpTier::Single,
        2 => PrpTier::Dual,
        _ => PrpTier::List,
    }
}

// ───────────────────────── #4c-b 统一段抽象（P0 地基）─────────────────────────
//
// 下列符号是 #4c-b（PI 全路径 PRP1 偏移）的统一消费层。**P1–P3 已全部接入**（io.rs：plain
// 三档判定 + PI separate/inline 的 dual/list dispatch 经 `validate_prp2`/`dispatch_segs`；
// completion.rs：finalize 收敛），故无 `#[allow(dead_code)]`。设计见
// docs/plans/2026-06-13-4cb-unified-prp-segment-abstraction.md。

/// PRP2 在某 tier 下的语义（spec § 4.1.1 / § 4.4）—— "tier → PRP2 解读"的**唯一裁决点**。
/// 供所有 dispatch 路径共用，消灭各路径各自 if/else 判 PRP2 对齐/非零的散落逻辑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prp2Role {
    /// Single 档：PRP2 未使用（可为 0，不校验）。
    Unused,
    /// Dual 档：PRP2 是第二个**数据页**指针（须非 0 且页对齐）。
    DataPage,
    /// List 档：PRP2 指向 **PRP-list 页**（须非 0 且页对齐）。
    ListPage,
}

/// 由 (PRP1 偏移, 总字节) 裁定 PRP2 语义。与 `tier` 一一对应，是其面向 dispatch 的语义视图。
/// caller 据此统一校验 PRP2（Unused 不校验；DataPage/ListPage 须非 0 且页对齐，否则
/// 非对齐→`PRP_OFFSET_INVALID`、0→`INVALID_FIELD`）。
#[inline]
pub(crate) fn prp2_role(offset: u64, total_len: u64) -> Prp2Role {
    match tier(offset, total_len) {
        PrpTier::Single => Prp2Role::Unused,
        PrpTier::Dual => Prp2Role::DataPage,
        PrpTier::List => Prp2Role::ListPage,
    }
}

/// dispatch 期已知的 host 段布局。Single/Dual 的全部段 GPA 即时可知（PRP1、PRP2 是直接
/// GPA）；List 仅首段（PRP1）即时可知，其余段 GPA 须等 PRP-list 页 DMA-read 回来（接现有
/// offset-aware List 机件，`page_size` 同一几何），故 List 变体只携首段长度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchSegs {
    /// 单段：data 全在 PRP1（含偏移）页内。
    Single { prp1: u64, len: u32 },
    /// 双段：PRP1（首段 page−O）+ PRP2（第二数据页，余量 ≤ 1 页）。
    Dual {
        prp1: u64,
        len0: u32,
        prp2: u64,
        len1: u32,
    },
    /// 列表：PRP1 首段即时可发；余段经 PRP-list 页 fetch（prp2 = list 页指针）。
    List { prp1: u64, first_len: u32 },
}

/// 按 (prp1 偏移, prp2, 总字节) 解析 dispatch 期 host 段。`prp2` 在 Dual 档是第二数据页
/// 指针、List 档是 list 页指针（caller 据返回的 `DispatchSegs` 变体区分用途，无需再判 tier）。
#[inline]
pub(crate) fn dispatch_segs(prp1: u64, prp2: u64, total_len: u64) -> DispatchSegs {
    let off = prp1_offset(prp1);
    match tier(off, total_len) {
        PrpTier::Single => DispatchSegs::Single {
            prp1,
            len: total_len as u32,
        },
        PrpTier::Dual => {
            let len0 = first_seg_len(off, total_len) as u32;
            DispatchSegs::Dual {
                prp1,
                len0,
                prp2,
                len1: (total_len - len0 as u64) as u32,
            }
        }
        PrpTier::List => DispatchSegs::List {
            prp1,
            first_len: first_seg_len(off, total_len) as u32,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **回归守卫**：O=0 时分段序列与 legacy「首页满 4096、其后整页、末页 partial」逐字节一致。
    #[test]
    fn offset_zero_matches_legacy() {
        let page = NVME_PAGE_SIZE;
        for &total in &[1u64, 8, 4095, 4096, 4097, 8192, 8208, 12288, 131072] {
            let legacy_pages = total.div_ceil(page) as u32;
            assert_eq!(
                total_pages(0, total),
                legacy_pages,
                "total_pages O=0 total={total}"
            );
            // 逐页 size：legacy 首页与中页 = page，末页 = total - (n-1)*page。
            let mut sum = 0u64;
            for i in 0..legacy_pages {
                let sz = page_size(i, 0, total) as u64;
                let legacy_sz = if i == legacy_pages - 1 {
                    total - (i as u64) * page
                } else {
                    page
                };
                assert_eq!(sz, legacy_sz, "page_size O=0 total={total} idx={i}");
                sum += sz;
            }
            assert_eq!(sum, total, "页 size 总和 == total（O=0 total={total}）");
        }
    }

    /// 偏移 O>0：首段缩短为 page-O，分段总和仍 == total。
    #[test]
    fn offset_nonzero_segments_sum_to_total() {
        let page = NVME_PAGE_SIZE;
        for &offset in &[1u64, 100, 2048, 4088, 4095] {
            for &total in &[1u64, 8, 4096, 8208, 12288, 100000] {
                let n = total_pages(offset, total);
                let mut sum = 0u64;
                for i in 0..n {
                    sum += page_size(i, offset, total) as u64;
                }
                assert_eq!(sum, total, "offset={offset} total={total}: 分段和 != total");
                // 首段 = min(total, page-offset)。
                assert_eq!(
                    page_size(0, offset, total) as u64,
                    (page - offset).min(total),
                    "首段 size offset={offset} total={total}"
                );
                // 中段（若有）= 整页。
                if n >= 3 {
                    assert_eq!(page_size(1, offset, total), page as u32, "中段须整页");
                }
            }
        }
    }

    /// 档位选择随偏移变化：偏移把 single/dual 阈值往下压。
    #[test]
    fn tier_selection() {
        let page = NVME_PAGE_SIZE;
        // O=0：≤page single；≤2page dual；>2page list。
        assert_eq!(tier(0, page), PrpTier::Single);
        assert_eq!(tier(0, page + 1), PrpTier::Dual);
        assert_eq!(tier(0, 2 * page), PrpTier::Dual);
        assert_eq!(tier(0, 2 * page + 1), PrpTier::List);
        // O=100：首段 page-100，阈值整体下移。
        assert_eq!(tier(100, page - 100), PrpTier::Single);
        assert_eq!(tier(100, page - 100 + 1), PrpTier::Dual);
        assert_eq!(tier(100, page - 100 + page), PrpTier::Dual);
        assert_eq!(tier(100, page - 100 + page + 1), PrpTier::List);
    }

    /// 4104-byte extended block（inline nlb=1）：O=0 → dual（4096+8）；
    /// O>0 → 仍 dual 当 8+O ≤ page（即 O ≤ 4088）。
    #[test]
    fn inline_block_4104_tiers() {
        assert_eq!(tier(0, 4104), PrpTier::Dual);
        assert_eq!(page_size(0, 0, 4104), 4096);
        assert_eq!(page_size(1, 0, 4104), 8);
        // 偏移 100：首段 3996，余 108 ≤ page → dual。
        assert_eq!(tier(100, 4104), PrpTier::Dual);
        assert_eq!(page_size(0, 100, 4104), 3996);
        assert_eq!(page_size(1, 100, 4104), 108);
        // 偏移 4090：首段 6，余 4098 > page → list（3 段：6 / 4096 / 2）。
        assert_eq!(tier(4090, 4104), PrpTier::List);
        assert_eq!(page_size(0, 4090, 4104), 6);
        assert_eq!(page_size(1, 4090, 4104), 4096);
        assert_eq!(page_size(2, 4090, 4104), 2);
    }

    // ───────────────── #4c-b P0：统一段抽象单测 ─────────────────

    /// `prp2_role` 与 `tier` 一一对应（语义视图等价）。
    #[test]
    fn prp2_role_matches_tier() {
        let page = NVME_PAGE_SIZE;
        for &offset in &[0u64, 1, 100, 2048, 4088, 4095] {
            for &total in &[1u64, 8, 4096, 4104, 8192, 8208, 12288, 100000] {
                let expect = match tier(offset, total) {
                    PrpTier::Single => Prp2Role::Unused,
                    PrpTier::Dual => Prp2Role::DataPage,
                    PrpTier::List => Prp2Role::ListPage,
                };
                assert_eq!(
                    prp2_role(offset, total),
                    expect,
                    "offset={offset} total={total}"
                );
            }
        }
        // 锚定档位边界（O=0）：≤page Unused；≤2page DataPage；>2page ListPage。
        assert_eq!(prp2_role(0, page), Prp2Role::Unused);
        assert_eq!(prp2_role(0, 2 * page), Prp2Role::DataPage);
        assert_eq!(prp2_role(0, 2 * page + 1), Prp2Role::ListPage);
    }

    /// `dispatch_segs` 三档：GPA + 段长正确，含偏移与 4104 corner。
    #[test]
    fn dispatch_segs_variants() {
        let page = NVME_PAGE_SIZE;
        // Single（O=0，data 全在 PRP1 页内）。
        assert_eq!(
            dispatch_segs(0x4000, 0x5000, 4096),
            DispatchSegs::Single {
                prp1: 0x4000,
                len: 4096
            }
        );
        // Dual（O=0，2 页）。
        assert_eq!(
            dispatch_segs(0x4000, 0x5000, 2 * page),
            DispatchSegs::Dual {
                prp1: 0x4000,
                len0: 4096,
                prp2: 0x5000,
                len1: 4096
            }
        );
        // Dual（偏移把 single→dual：total=4096 但 O>0 → 首段 page-O + 余 O@PRP2）。
        // 这是 separate nlb=1 在 O>0 的形态。
        let prp1 = 0x4000 + 100;
        assert_eq!(
            dispatch_segs(prp1, 0x5000, 4096),
            DispatchSegs::Dual {
                prp1,
                len0: (page - 100) as u32,
                prp2: 0x5000,
                len1: 100
            }
        );
        // inline nlb=1（4104）偏移 100：Dual，首段 3996 + 余 108。
        let prp1b = 0x4000 + 100;
        assert_eq!(
            dispatch_segs(prp1b, 0x9000, 4104),
            DispatchSegs::Dual {
                prp1: prp1b,
                len0: 3996,
                prp2: 0x9000,
                len1: 108
            }
        );
        // inline nlb=1（4104）偏移 4090：List corner（首段 6，余经 list 页）。
        let prp1c = 0x4000 + 4090;
        assert_eq!(
            dispatch_segs(prp1c, 0x9000, 4104),
            DispatchSegs::List {
                prp1: prp1c,
                first_len: 6
            }
        );
        // List（O=0，>2 页）。
        assert_eq!(
            dispatch_segs(0x4000, 0x9000, 2 * page + 1),
            DispatchSegs::List {
                prp1: 0x4000,
                first_len: 4096
            }
        );
    }
}
