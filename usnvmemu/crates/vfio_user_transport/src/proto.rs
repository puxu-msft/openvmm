// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W0-2: proto 模块已迁移到 sans-IO crate `vfio_user_wire`. 本文件保留为
//! 兼容性 re-export, 保住所有现有 `crate::proto::*` / `vfio_user_transport::proto::*`
//! 引用路径不破.
//!
//! 新代码应直接 `use vfio_user_wire::proto::*;`.

pub use vfio_user_wire::proto::*;
