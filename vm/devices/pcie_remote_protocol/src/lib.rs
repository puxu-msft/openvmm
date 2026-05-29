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

pub mod codec;

/// Generated protobuf types. Generated code does not conform to our lint
/// configuration; silence the relevant lints inside this submodule only.
#[expect(missing_docs)]
#[expect(clippy::allow_attributes)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/openhcl.pcie_remote.v1.rs"));
}

pub use proto::*;
