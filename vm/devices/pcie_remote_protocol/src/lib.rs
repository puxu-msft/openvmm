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

#[cfg(test)]
mod mesh_payload_compat {
    //! K-15: 验证 prost-generated message 都 derive 了 mesh::MeshPayload
    //! 且 nested enum (BarInfo.Kind) 通过 i32 字段透明传递 — mesh 不需
    //! 直接 derive 该 enum。
    use super::*;
    use mesh::MeshPayload;
    use mesh::payload::Protobuf;

    fn _assert_meshpayload<T: MeshPayload>() {}
    fn _assert_protobuf<T: Protobuf>() {}

    #[test]
    fn all_messages_are_meshpayload() {
        _assert_meshpayload::<Hello>();
        _assert_meshpayload::<HelloAck>();
        _assert_meshpayload::<DeviceDescribe>();
        _assert_meshpayload::<BarInfo>();
        _assert_meshpayload::<CapabilityBlob>();
        _assert_meshpayload::<MmioAccess>();
        _assert_meshpayload::<MmioReadResult>();
        _assert_meshpayload::<CfgAccess>();
        _assert_meshpayload::<InterruptFire>();
        _assert_meshpayload::<ReadGpaRequest>();
        _assert_meshpayload::<WriteGpaRequest>();
        _assert_meshpayload::<DmaCompletion>();
        _assert_meshpayload::<Reset>();
        _assert_meshpayload::<ToHost>();
        _assert_meshpayload::<ToOpenhcl>();
    }

    #[test]
    fn bar_info_kind_is_i32_in_struct() {
        // BarInfo.kind 字段类型是 i32 (prost enum -> i32)，不是 Kind 类型。
        // 这样 mesh 不必为 Kind enum 单独 derive MeshPayload。
        let b = BarInfo {
            index: 0,
            size: 4096,
            kind: bar_info::Kind::Mmio32 as i32,
            prefetchable: false,
        };
        // 字段类型断言（compile-time）
        let _check: i32 = b.kind;
    }
}
