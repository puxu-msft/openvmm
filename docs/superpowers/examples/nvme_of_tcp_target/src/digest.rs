// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V1** — CRC32C Castagnoli digest（NVMe TCP header / data digest）。
//!
//! Spec: NVMe TCP Transport § 3.4 — HDGST/DDGST 都是 CRC32C-Castagnoli
//! 多项式 `0x1EDC6F41`，4 byte little-endian。我们复用 crates.io `crc32c`
//! crate（提供 SSE4.2 加速 + 软件回退，纯 safe Rust）。
//!
//! API：
//! - [`crc32c(data)`] — 给一段字节计算 CRC32C，返 u32（host-endian）
//! - [`crc32c_le_bytes(data)`] — 直接给出 4-byte LE 字节切片，便于直接
//!   写入 wire
//! - [`verify_crc32c(data, expected_le_bytes)`] — 校验 4-byte LE 字节
//!   是否匹配 `data` 的实际 CRC

/// CRC32C Castagnoli (poly 0x1EDC6F41)。
pub fn crc32c(data: &[u8]) -> u32 {
    ::crc32c::crc32c(data)
}

/// 同 [`crc32c`] 但直接返 4-byte LE 字节，便于 wire encode。
pub fn crc32c_le_bytes(data: &[u8]) -> [u8; 4] {
    crc32c(data).to_le_bytes()
}

/// 校验 wire 上的 4-byte LE digest 与 `data` 实际 CRC 是否一致。
pub fn verify_crc32c(data: &[u8], expected_le: [u8; 4]) -> bool {
    crc32c(data).to_le_bytes() == expected_le
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 已知向量：CRC32C of "123456789" = 0xE3069283（iSCSI / SCTP / NVMe 共用，
    /// 与 ISO 9797-1 Annex B 标准向量一致）。
    #[test]
    fn known_vector_iscsi_check() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    /// 空切片 CRC32C = 0。
    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc32c(&[]), 0);
    }

    /// LE bytes 与 to_le_bytes 一致；verify 路径完整闭环。
    #[test]
    fn le_bytes_roundtrip_and_verify() {
        let data = b"NVMe TCP digest test";
        let le = crc32c_le_bytes(data);
        assert!(verify_crc32c(data, le));
        // 任意一 bit 翻 → mismatch
        let mut bad = le;
        bad[0] ^= 0x01;
        assert!(!verify_crc32c(data, bad));
    }
}
