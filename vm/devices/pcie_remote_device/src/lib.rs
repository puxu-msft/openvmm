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
pub mod device;
pub mod dma;
pub mod error;
pub mod handshake;
pub mod handshake_spawn;
pub mod prepared;
pub mod resolver;
pub mod state;
pub mod transport;
pub mod worker;

pub use absent::AbsentPcieDevice;
pub use device::PcieRemoteDevice;
pub use error::Error;
pub use prepared::PreparedPcieRemoteDevice;
pub use resolver::PcieRemoteTcpResolver;
pub use resolver::PcieRemoteVmbusResolver;
pub use resolver::PreparedMap;
pub use resolver::WorkerTasks;
pub use worker::SharedWorkerStats;
pub use worker::WorkerStats;
pub use state::DeviceState;
pub use state::SharedState;
