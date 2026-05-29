// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Handshake 完成后的产物。

use crate::worker::DeviceRequest;
use mesh::Receiver;
use mesh::Sender;
use pcie_remote_protocol::DeviceDescribe;

/// Per-instance handshake 完成后的产物。
///
/// `to_worker` 给 device shim 用；`worker_inbox` 由调用方 take 后喂给 Worker::new。
/// transport 单独由调用方持有（不放进 Prepared，避免类型擦除困难）。
pub struct PreparedPcieRemoteDevice {
    /// host 在 HelloAck 中描述的设备 schema。
    pub describe: DeviceDescribe,
    /// device shim 用来发请求给 worker 的 sender。
    pub to_worker: Sender<DeviceRequest>,
    /// worker_inbox 的 receiver 端；调用方 `take()` 一次。
    pub worker_inbox: Option<Receiver<DeviceRequest>>,
}

impl PreparedPcieRemoteDevice {
    /// 从 prepared 中取走 worker 的 receiver。再次调用 panic。
    pub fn take_worker_inbox(&mut self) -> Receiver<DeviceRequest> {
        self.worker_inbox
            .take()
            .expect("worker_inbox already taken")
    }
}
