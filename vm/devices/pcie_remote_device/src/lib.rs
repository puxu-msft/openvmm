// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 远程 PCIe 实验设备。
//!
//! 架构说明（spec v3.1）：
//! - Resolver 自持 prepared_map；handle 仅 carry instance_id
//! - cfg 100% sync，MMIO IoResult::Defer
//! - state machine: Connecting → Live → Lost (terminal in v1)
//! - 不支持 CVM / save_restore / hotplug (v1)

#![forbid(unsafe_code)]

pub mod absent;
pub mod deadman;
pub mod error;
pub mod handshake;
pub mod prepared;
pub mod state;
pub mod worker;

pub use absent::AbsentPcieDevice;
pub use error::Error;
pub use prepared::PreparedPcieRemoteDevice;
pub use state::DeviceState;
pub use state::SharedState;
