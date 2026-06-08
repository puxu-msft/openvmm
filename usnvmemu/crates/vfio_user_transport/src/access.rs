// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **vfio-spec** — region 访问的寄存器粒度分块。
//!
//! vfio-user 的 `REGION_READ` / `REGION_WRITE` 允许任意长度（bulk）访问，例如
//! guest 一次性 dump 整个 config header，或写一段连续寄存器。但底层 PCI
//! config / MMIO 语义是**按寄存器宽度**的（dword 的 BAR base/size-probe、
//! doorbell 等），byte-by-byte 拆会丢掉寄存器宽度语义。
//!
//! [`register_chunks`] 把 `[offset, offset+count)` 拆成 `(abs_offset, size)`
//! chunk，`size` 取 `{max,…,4,2,1}` 中"既对齐 `abs_offset`、又 ≤ 剩余长度"的
//! 最大者：
//! - bulk 访问里落在某寄存器（BAR-probe / doorbell）上的对齐段会被**整体**
//!   access，命中寄存器宽度语义（如 BAR size-probe 要求 4 字节写）；
//! - 非对齐的起始或尾段自然退化到更小粒度。
//!
//! `max` 按 region 自然宽度选：**config space 是 dword（4）寄存器** —— 8 字节
//! 写不被 config 状态机识别（BAR-probe 仅认 4 字节），故 config 路径传
//! `max=4`；**MMIO/BAR 支持 8 字节访问**，传 `max=8`。read / write / config
//! 三条 bulk 路径共用同一分块器（DRY + 对称）。
//!
//! **护栏（未来扩展）**：bulk write 的分块是**逐段顺序投递、非原子**，跨寄存器
//! 无顺序/原子保证。当前教学 NVMe 设备无单次 bulk write 跨写多个有顺序依赖
//! 寄存器的用法，故无影响；但若未来某寄存器副作用依赖"整次 bulk 写完才生效"
//! 的原子语义，此分块模型需重新评估。

/// region 访问的最大寄存器粒度（字节）。
pub(crate) const MAX_CHUNK_MMIO: u32 = 8;
/// config space 的最大寄存器粒度 —— PCI config 是 dword 导向，写 > 4 字节不被
/// 状态机识别（BAR size-probe / Command RW 都按 ≤4 字节）。
pub(crate) const MAX_CHUNK_CONFIG: u32 = 4;

/// 把 `[offset, offset+count)` 拆成对齐感知的寄存器粒度 `(abs_offset, size)`。
///
/// `size` 取 `{max,…,4,2,1}`（`max` 须为 2 的幂、≤8）中"`abs_offset` 整除该
/// size 且剩余长度 ≥ size"的最大者。`count == 0` 返回空。
pub(crate) fn register_chunks(offset: u64, count: usize, max: u32) -> Vec<(u64, u32)> {
    debug_assert!(max.is_power_of_two() && max <= 8, "max 须为 ≤8 的 2 的幂");
    let mut chunks = Vec::new();
    let mut pos = 0usize;
    while pos < count {
        let abs = offset + pos as u64;
        let rem = count - pos;
        // 从大到小取第一个"对齐 + 装得下"的粒度；都不满足则 1 字节。
        let sz = [8u32, 4, 2]
            .into_iter()
            .find(|&cand| cand <= max && rem >= cand as usize && abs.is_multiple_of(cand as u64))
            .unwrap_or(1);
        chunks.push((abs, sz));
        pos += sz as usize;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_when_zero_count() {
        assert!(register_chunks(0, 0, MAX_CHUNK_MMIO).is_empty());
    }

    #[test]
    fn aligned_whole_access_is_single_chunk() {
        assert_eq!(register_chunks(0, 4, MAX_CHUNK_MMIO), vec![(0, 4)]);
        assert_eq!(register_chunks(0, 8, MAX_CHUNK_MMIO), vec![(0, 8)]);
        assert_eq!(register_chunks(8, 2, MAX_CHUNK_MMIO), vec![(8, 2)]);
        assert_eq!(register_chunks(0, 1, MAX_CHUNK_MMIO), vec![(0, 1)]);
    }

    #[test]
    fn non_power_of_two_count_splits_largest_first() {
        // offset 0 对齐：6 = 4 + 2；7 = 4 + 2 + 1；3 = 2 + 1。
        assert_eq!(register_chunks(0, 6, MAX_CHUNK_MMIO), vec![(0, 4), (4, 2)]);
        assert_eq!(
            register_chunks(0, 7, MAX_CHUNK_MMIO),
            vec![(0, 4), (4, 2), (6, 1)]
        );
        assert_eq!(register_chunks(0, 3, MAX_CHUNK_MMIO), vec![(0, 2), (2, 1)]);
    }

    #[test]
    fn unaligned_offset_degrades_until_aligned() {
        // 起始 abs=1（奇）→ 先 1 字节到 2，2 对齐 → 2 字节到 4，4 对齐 → 4 字节。
        assert_eq!(
            register_chunks(1, 7, MAX_CHUNK_MMIO),
            vec![(1, 1), (2, 2), (4, 4)]
        );
        // abs=4 是 4 对齐但非 8 对齐：count=8 → 4 + 4（不会误当单次 8 字节非对齐访问）。
        assert_eq!(register_chunks(4, 8, MAX_CHUNK_MMIO), vec![(4, 4), (8, 4)]);
        // abs=2 → 2 字节到 4，4 字节到 8，尾 2 字节。
        assert_eq!(
            register_chunks(2, 8, MAX_CHUNK_MMIO),
            vec![(2, 2), (4, 4), (8, 2)]
        );
    }

    #[test]
    fn config_max_caps_at_dword() {
        // config（max=4）：8 字节对齐访问也拆成 4 + 4，绝不出现 8-wide chunk
        // （否则 config 状态机的 BAR-probe / Command RW 不识别 > 4 字节写）。
        assert_eq!(
            register_chunks(0x10, 8, MAX_CHUNK_CONFIG),
            vec![(0x10, 4), (0x14, 4)]
        );
        // 对比 MMIO（max=8）：同样访问是单个 8-wide chunk。
        assert_eq!(register_chunks(0x10, 8, MAX_CHUNK_MMIO), vec![(0x10, 8)]);
    }
}
