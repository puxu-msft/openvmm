// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W0-3: access 模块已迁移到 `vfio_user_wire`. re-export 兼容.
//!
//! 注: 原 `pub(crate)` 3 项 (MAX_CHUNK_MMIO / MAX_CHUNK_CONFIG /
//! register_chunks) 在 wire crate 提为 pub (因为 wire crate 整体就是
//! 内部协议层, pub(crate) 反而是误约束), 这里通过 `pub use` 通配吸收;
//! 原 crate 内对 `crate::access::register_chunks` 等调用一行不改.
pub use vfio_user_wire::access::*;
