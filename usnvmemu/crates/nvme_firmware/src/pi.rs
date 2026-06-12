// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Reviewer M7：之前 K4 未接入时全模块 #![allow(dead_code)]，K4a/b/c 已上
// 路；移除 blanket allow，让未来新加 PI helper 真不用时能被 unused-code
// lint 抓到。

//! T10 DIF (Data Integrity Field) CRC16 + PI tuple 序列化 / 校验。
//!
//! NVMe spec § 8.3 Protection Information。Phase K1 实现。
//!
//! ## T10 DIF tuple 布局 (8 byte)
//!
//! ```text
//! offset 0..2  Guard Tag       (CRC16 over data)
//! offset 2..4  Application Tag (user-defined; we don't enforce)
//! offset 4..8  Reference Tag   (LBA-derived sequence number)
//! ```
//!
//! ## PI Type 差异 (spec § 8.3.1)
//!
//! - **Type 1**: RefTag = lower 32 bits of LBA；each LBA strictly +1
//! - **Type 2**: 与 Type 1 类似但 driver 自定义 RefTag 起始值
//! - **Type 3**: 不校验 RefTag（仅 Guard + AppTag）
//!
//! ## CRC16 polynomial
//!
//! T10 DIF 用 `x^16 + x^15 + x^11 + x^9 + x^8 + x^7 + x^5 + x^4 + x^2 + x + 1`
//! = 0x18BB7 (= 0x8BB7 17-bit 含最高位)。初始值 0，无 reflection，无 XOR-out。
//! 也叫 CRC-16-T10-DIF。Linux kernel `crc-t10dif` 同款。
//!
//! 实现：table-free bit-serial（教学清晰；perf 临界路径可换 table 或
//! `crc-catalog::CRC_16_T10_DIF`）。

/// T10 DIF CRC16 多项式 0x8BB7（init 0，no reflect，no xor-out）。
const CRC16_T10DIF_POLY: u16 = 0x8BB7;

/// 编译期预计算的 256 项 CRC16 查找表（MSB-first，每项 = 单字节 `i` 经 8 bit 处理的余式）。
/// 取代 bit-serial 内层 8 次迭代——CRC 是**每个 PI IO 每 data 字节都跑**的热路径
/// （[[hot-path-perf-first]]），table-driven 每字节 1 次查表 + 移位异或，~8× 减少内层运算。
/// `const fn` 编译期构建，零运行时初始化成本。
const CRC16_T10DIF_TABLE: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ CRC16_T10DIF_POLY
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// 计算 T10 DIF CRC16（polynomial 0x8BB7，init 0，no reflect，no xor-out）。
/// **table-driven 热路径**（见 `CRC16_T10DIF_TABLE`）：与原 bit-serial 数学等价，由
/// known-answer（crc16_zeros / crc16_known_vector=0xD0DB）+ proptest vs `crc` crate 守正确。
pub(crate) fn crc16_t10dif(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        let idx = (((crc >> 8) ^ b as u16) & 0xff) as usize;
        crc = (crc << 8) ^ CRC16_T10DIF_TABLE[idx];
    }
    crc
}

/// PI tuple (8 byte)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PiTuple {
    pub(crate) guard: u16,
    pub(crate) app_tag: u16,
    pub(crate) ref_tag: u32,
}

impl PiTuple {
    pub(crate) fn from_bytes(b: &[u8; 8]) -> Self {
        Self {
            guard: u16::from_be_bytes([b[0], b[1]]),
            app_tag: u16::from_be_bytes([b[2], b[3]]),
            ref_tag: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        }
    }
    pub(crate) fn to_bytes(self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0..2].copy_from_slice(&self.guard.to_be_bytes());
        out[2..4].copy_from_slice(&self.app_tag.to_be_bytes());
        out[4..8].copy_from_slice(&self.ref_tag.to_be_bytes());
        out
    }
    /// 计算 PI tuple based on data block + LBA + pi_type。
    pub(crate) fn compute(data: &[u8], lba: u64, pi_type: u8) -> Self {
        let guard = crc16_t10dif(data);
        let ref_tag = match pi_type {
            1 | 2 => (lba & 0xFFFF_FFFF) as u32, // Type 1/2: lower 32 bits of LBA
            _ => 0,                              // Type 3: not checked
        };
        Self {
            guard,
            app_tag: 0,
            ref_tag,
        }
    }
    /// 校验 PI tuple 与 data + LBA 一致，按 PRCHK 逐项门控（spec § 8.3.1 + NVM CS
    /// Cdw12ReadWrite.prinfo）。`prchk` 指示是否校验各字段：
    /// - `prchk.guard`：校验 Guard（CRC16）；
    /// - `prchk.ref_tag`：校验 Reference Tag（仅 Type 1/2，Type 3 本就不查）；
    /// - `prchk.app_tag`：App Tag 校验**未实现**（需 cdw15 App Tag Mask 机件），
    ///   即便置位也不校验（教学边界，host app_tag 原样保留）。
    ///
    /// PRCHK=0（无任何校验位）→ 一律 `Ok`：driver 显式 opt-out PI 校验（spec 允许，
    /// controller 仍按 PRACT strip/insert，只是不验证）。这取代了早期"over-strict 一律
    /// 校验 Guard+RefTag"的教学边界——现按 spec 真门控。
    pub(crate) fn verify(self, data: &[u8], lba: u64, pi_type: u8, prchk: PrChk) -> PiCheck {
        let expected = Self::compute(data, lba, pi_type);
        if prchk.guard && self.guard != expected.guard {
            return PiCheck::GuardFail;
        }
        if prchk.ref_tag && (pi_type == 1 || pi_type == 2) && self.ref_tag != expected.ref_tag {
            return PiCheck::RefTagFail;
        }
        // AppTag：本实现未接 App Tag Mask（cdw15），即便 prchk.app_tag 置位也不校验
        // （诚实标注的教学边界；host 的 app_tag 原样存盘/回送）。
        PiCheck::Ok
    }
}

/// PRCHK（Protection Information Check）逐项校验门控。
///
/// 源自 cdw12 PRINFO 字段（spec NVM CS `Cdw12ReadWrite.prinfo`，4 bit @ bits 29:26）：
/// `[bit29 PRACT][bit28 Guard][bit27 AppTag][bit26 RefTag]`（PRINFO[3..0]）。本结构只承载
/// 3 个 PRCHK 位（PRACT 在调用方单独解析）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrChk {
    pub(crate) guard: bool,
    pub(crate) app_tag: bool,
    pub(crate) ref_tag: bool,
}

impl PrChk {
    /// 从 cdw12 解析 PRCHK 3 位（Guard@28 / AppTag@27 / RefTag@26）。
    pub(crate) fn from_cdw12(cdw12: u32) -> Self {
        Self {
            guard: (cdw12 >> 28) & 1 != 0,
            app_tag: (cdw12 >> 27) & 1 != 0,
            ref_tag: (cdw12 >> 26) & 1 != 0,
        }
    }
    /// 全开（Guard+AppTag+RefTag 都校验）。供内部/测试用。
    #[cfg(test)]
    pub(crate) fn all() -> Self {
        Self {
            guard: true,
            app_tag: true,
            ref_tag: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PiCheck {
    Ok,
    GuardFail,
    RefTagFail,
    #[allow(dead_code)] // 未来 AppTag 强校验路径
    AppTagFail,
}

impl PiCheck {
    /// 失败时映射到 NVMe SC：spec § 4.6.1 Media/Data Integrity (SCT=0x02)。
    /// （已在 B6b/B6c/K4c PI verify 多条生产路径使用，无需 dead_code 豁免。）
    pub(crate) fn to_sc(self) -> Option<u8> {
        match self {
            PiCheck::Ok => None,
            PiCheck::GuardFail => Some(0x82),  // GUARD_CHECK_ERR
            PiCheck::AppTagFail => Some(0x83), // APP_TAG_CHECK_ERR
            PiCheck::RefTagFail => Some(0x84), // REF_TAG_CHECK_ERR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 已知 vector：全 0 数据的 T10 DIF CRC = 0。
    #[test]
    fn crc16_zeros() {
        assert_eq!(crc16_t10dif(&[0u8; 512]), 0);
    }

    /// Linux kernel `crc_t10dif("123456789")` 已知值 = 0xD0DB。
    #[test]
    fn crc16_known_vector() {
        let v = crc16_t10dif(b"123456789");
        assert_eq!(v, 0xD0DB, "T10 DIF CRC16 of '123456789' = 0xD0DB");
    }

    #[test]
    fn pi_round_trip() {
        let data = b"hello world hello world hello wo".repeat(16); // 512 byte
        let lba = 42u64;
        let pi = PiTuple::compute(&data, lba, 1);
        assert_eq!(pi.verify(&data, lba, 1, PrChk::all()), PiCheck::Ok);
        // 改 data → guard fail
        let mut bad = data.clone();
        bad[0] ^= 0xff;
        assert_eq!(pi.verify(&bad, lba, 1, PrChk::all()), PiCheck::GuardFail);
        // 改 lba → reftag fail
        assert_eq!(
            pi.verify(&data, lba + 1, 1, PrChk::all()),
            PiCheck::RefTagFail
        );
        // Type 3 不查 reftag
        assert_eq!(pi.verify(&data, lba + 1, 3, PrChk::all()), PiCheck::Ok);
    }

    /// PRCHK 逐项门控（spec NVM CS PRINFO bits 28:26）：PRCHK=0 → 即便 data/lba 改坏
    /// 也一律 Ok（driver opt-out 校验）；单独开 Guard / RefTag 各自精确门控。
    #[test]
    fn pi_prchk_gating() {
        let data = b"abcdefgh".repeat(64); // 512 byte
        let lba = 7u64;
        let pi = PiTuple::compute(&data, lba, 1);
        let mut bad = data.clone();
        bad[0] ^= 0xff; // 破坏 guard
        let none = PrChk {
            guard: false,
            app_tag: false,
            ref_tag: false,
        };
        // PRCHK=0：坏 guard + 错 lba 都放过。
        assert_eq!(
            pi.verify(&bad, lba, 1, none),
            PiCheck::Ok,
            "PRCHK=0 不校验 guard"
        );
        assert_eq!(
            pi.verify(&data, lba + 1, 1, none),
            PiCheck::Ok,
            "PRCHK=0 不校验 reftag"
        );
        // 仅开 Guard：坏 guard 被抓，但错 lba（reftag）放过。
        let guard_only = PrChk {
            guard: true,
            app_tag: false,
            ref_tag: false,
        };
        assert_eq!(pi.verify(&bad, lba, 1, guard_only), PiCheck::GuardFail);
        assert_eq!(
            pi.verify(&data, lba + 1, 1, guard_only),
            PiCheck::Ok,
            "Guard-only 不查 reftag"
        );
        // 仅开 RefTag：错 lba 被抓，但坏 guard 放过。
        let ref_only = PrChk {
            guard: false,
            app_tag: false,
            ref_tag: true,
        };
        assert_eq!(pi.verify(&data, lba + 1, 1, ref_only), PiCheck::RefTagFail);
        assert_eq!(
            pi.verify(&bad, lba, 1, ref_only),
            PiCheck::Ok,
            "RefTag-only 不查 guard"
        );
    }

    /// from_cdw12 锚定 PRINFO 位布局（PRACT@29 不属 PRCHK；Guard@28/AppTag@27/RefTag@26）。
    #[test]
    fn prchk_from_cdw12_bit_layout() {
        assert_eq!(
            PrChk::from_cdw12(1 << 28),
            PrChk {
                guard: true,
                app_tag: false,
                ref_tag: false
            }
        );
        assert_eq!(
            PrChk::from_cdw12(1 << 27),
            PrChk {
                guard: false,
                app_tag: true,
                ref_tag: false
            }
        );
        assert_eq!(
            PrChk::from_cdw12(1 << 26),
            PrChk {
                guard: false,
                app_tag: false,
                ref_tag: true
            }
        );
        // PRACT(bit29) 不应被解析进任何 PRCHK 位。
        assert_eq!(
            PrChk::from_cdw12(1 << 29),
            PrChk {
                guard: false,
                app_tag: false,
                ref_tag: false
            }
        );
    }

    #[test]
    fn pi_bytes_round_trip() {
        let p = PiTuple {
            guard: 0xABCD,
            app_tag: 0x1234,
            ref_tag: 0xDEAD_BEEF,
        };
        let b = p.to_bytes();
        assert_eq!(PiTuple::from_bytes(&b), p);
    }
}
