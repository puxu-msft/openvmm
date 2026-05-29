// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! pcie_remote_device 错误类型。

use thiserror::Error;

/// 设备相关错误。
#[derive(Debug, Error)]
pub enum Error {
    /// Resolver 找不到对应 instance 的 handshake 结果。
    #[error("handshake not prepared for instance {0}")]
    HandshakeNotPrepared(guid::Guid),
    /// BAR schema 非法。
    #[error("bar layout invalid: {0}")]
    BarLayout(String),
    /// Capability blob 非法。
    #[error("capability blob invalid: {0}")]
    CapabilityBlob(String),
    /// msix_count 超限。
    #[error("msix count {0} exceeds limit 2048")]
    MsixCountTooLarge(u32),
    /// I/O 错误。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// 协议编解码错误。
    #[error("codec error: {0}")]
    Codec(#[from] pcie_remote_protocol::codec::CodecError),
}
