// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! vfio-user NVMe PCIe 设备（W6b）：把 W6a 的 async `VfioUserClient` 集成进
//! underhill_core，向 guest 呈现一个可枚举并驱动的 NVMe 设备。
//!
//! 照抄 `vm/devices/pcie_remote_device` 的结构（ChipsetDevice shim + async worker +
//! resolver + connect spawner），换掉 wire 层（protobuf → vfio_user_device typed async）。

#![forbid(unsafe_code)]

pub mod absent;
pub mod device;
pub mod irq;
pub mod prepared;
pub mod resolver;
pub mod spawn;
pub mod state;
pub mod worker;

pub use absent::AbsentPcieDevice;
pub use device::MSIX_BAR_INDEX;
pub use device::VfioUserPciDevice;
pub use irq::irq_wait_loop;
pub use prepared::PreparedMap;
pub use prepared::PreparedVfioUserDevice;
pub use resolver::VfioUserPciResolver;
pub use resolver::WorkerTasks;
pub use spawn::derive_hardware_ids_from_cfg;
pub use spawn::spawn_vfio_user_connects;
pub use state::DeviceState;
pub use state::SharedState;
pub use worker::DeviceRequest;
pub use worker::ReqKind;
pub use worker::SharedWorkerStats;
pub use worker::Worker;
pub use worker::WorkerStats;
