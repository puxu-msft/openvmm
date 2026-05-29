// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wire protocol for the pcie_remote experimental device.
//! All multi-byte numeric fields are little-endian on the wire.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

// Crates referenced by generated code; explicit uses prevent automated tooling
// from removing them.
use inspect as _;
use mesh as _;
use prost as _;

pub const MAX_FRAME_BYTES: usize = 1 << 20;
pub const MAX_DMA_BYTES: usize = 64 << 10;
pub const PROTOCOL_MAGIC: u32 = 0x52504345;
pub const PROTOCOL_VERSION: u32 = 1;

/// K-18 / spec §3.5: MMIO 访问尺寸严格 ∈ {1,2,4,8}。
/// PCIe spec 不允许 3/5/6/7 byte 的 PIO/MMIO 访问。
pub const fn is_valid_mmio_size(size: u32) -> bool {
    matches!(size, 1 | 2 | 4 | 8)
}

pub mod codec;

/// Generated protobuf types. Generated code does not conform to our lint
/// configuration; silence the relevant lints inside this submodule only.
#[expect(missing_docs)]
#[expect(clippy::allow_attributes)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/openhcl.pcie_remote.v1.rs"));
}

pub use proto::*;

#[cfg(test)]
mod size_tests {
    use super::*;

    #[test]
    fn valid_mmio_sizes() {
        for sz in [1u32, 2, 4, 8] {
            assert!(is_valid_mmio_size(sz), "size {sz} must be valid");
        }
    }

    #[test]
    fn invalid_mmio_sizes() {
        // K-18: 3/5/6/7 必须拒绝；0 和 >8 也拒绝
        for sz in [0u32, 3, 5, 6, 7, 9, 16, 32, 64, 4096, u32::MAX] {
            assert!(!is_valid_mmio_size(sz), "size {sz} must be invalid");
        }
    }
}
