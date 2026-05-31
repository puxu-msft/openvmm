// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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

/// 计算 T10 DIF CRC16（polynomial 0x8BB7，init 0，no reflect，no xor-out）。
pub(crate) fn crc16_t10dif(data: &[u8]) -> u16 {
    const POLY: u16 = 0x8BB7;
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ POLY;
            } else {
                crc <<= 1;
            }
        }
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
    /// 校验 PI tuple 与 data + LBA 一致。Type 3 不查 RefTag。
    pub(crate) fn verify(self, data: &[u8], lba: u64, pi_type: u8) -> PiCheck {
        let expected = Self::compute(data, lba, pi_type);
        if self.guard != expected.guard {
            return PiCheck::GuardFail;
        }
        if (pi_type == 1 || pi_type == 2) && self.ref_tag != expected.ref_tag {
            return PiCheck::RefTagFail;
        }
        // AppTag 我们不强校验（spec 允许 driver 决定语义）
        PiCheck::Ok
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
    #[allow(dead_code)] // Phase K4 真 PI Read 路径会用
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
        assert_eq!(pi.verify(&data, lba, 1), PiCheck::Ok);
        // 改 data → guard fail
        let mut bad = data.clone();
        bad[0] ^= 0xff;
        assert_eq!(pi.verify(&bad, lba, 1), PiCheck::GuardFail);
        // 改 lba → reftag fail
        assert_eq!(pi.verify(&data, lba + 1, 1), PiCheck::RefTagFail);
        // Type 3 不查 reftag
        assert_eq!(pi.verify(&data, lba + 1, 3), PiCheck::Ok);
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
