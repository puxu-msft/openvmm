// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V4a** — Transfer Tag (TTAG) 分配器。
//!
//! R2T 携带一个 16-bit `ttag`；host 收到 R2T 后必须在 H2CData PDU 里 echo
//! 同一个 ttag，session 据此 demux 多个并发 R2T（V4 范围内单 R2T，但分配器
//! 留出 multi-pipeline 余地）。
//!
//! 约束（spec § 3.6.3）：
//! - `ttag == 0` 保留（不分配；host 若收到 ttag=0 必须 reject）
//! - 16-bit 空间循环复用；分配器 wrap 时跳过 0
//! - 同一 session 内 in-flight ttag 不可冲突 —— V4b 每条 cmd 分配 + 完成后
//!   隐式释放（V4 范围内一次只一条 R2T，简化）
//!
//! 教学版仅给单调计数器；真生产实现应配合 `BitSet` 跟踪 in-flight。

use std::num::Wrapping;

/// 单调递增 TTAG 分配器；wrap 跳过 0。
pub struct TtagAllocator {
    next: Wrapping<u16>,
}

impl Default for TtagAllocator {
    fn default() -> Self {
        // 从 1 开始（避开保留值 0）
        Self {
            next: Wrapping(1u16),
        }
    }
}

impl TtagAllocator {
    /// 返一个非零 ttag。每调一次推进；wrap 时跳过 0。
    pub fn alloc(&mut self) -> u16 {
        let t = self.next.0;
        // advance
        self.next += Wrapping(1u16);
        if self.next.0 == 0 {
            // wrap 跳过 0 → 强制为 1
            self.next = Wrapping(1u16);
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_starts_at_one() {
        let mut a = TtagAllocator::default();
        assert_eq!(a.alloc(), 1);
        assert_eq!(a.alloc(), 2);
        assert_eq!(a.alloc(), 3);
    }

    /// wrap 跳过 0：第 65535 次 alloc 返 0xFFFF，第 65536 次返 1（不返 0）。
    #[test]
    fn alloc_skips_zero_on_wrap() {
        let mut a = TtagAllocator::default();
        // 预先吃掉 0xFFFE 个：alloc 1..=0xFFFE
        for _ in 0..0xFFFEu32 {
            let _ = a.alloc();
        }
        // 第 0xFFFF 次 alloc 应返 0xFFFF
        assert_eq!(a.alloc(), 0xFFFF);
        // 下一次本应是 0，但要跳过 → 返 1
        let v = a.alloc();
        assert_ne!(v, 0, "alloc 不可返 0（spec reserved）");
        assert_eq!(v, 1);
    }
}
