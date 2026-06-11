// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 设备状态机（照抄 `pcie_remote_device::state`）。
//!
//! Connecting → Live → Lost（Lost 为终态）。device shim 与 async worker 经
//! [`SharedState`] 共用一个 `AtomicU8`。

use inspect::Inspect;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

/// 离散状态值（编码进 AtomicU8）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DeviceState {
    /// 启动期，等握手 / connect 完成。
    Connecting = 0,
    /// 已就绪，cfg/MMIO 走正常路径。
    Live = 1,
    /// 终态：cfg read 返全 1（guest 视为设备消失），MMIO 返 Err。
    Lost = 2,
}

impl DeviceState {
    /// 用于 ohcldiag-dev inspect 显示的稳定字符串名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceState::Connecting => "Connecting",
            DeviceState::Live => "Live",
            DeviceState::Lost => "Lost",
        }
    }
}

impl From<u8> for DeviceState {
    fn from(v: u8) -> Self {
        match v {
            0 => DeviceState::Connecting,
            1 => DeviceState::Live,
            _ => DeviceState::Lost,
        }
    }
}

/// 共享状态：device shim 与 worker task 共用。
#[derive(Clone)]
pub struct SharedState(Arc<AtomicU8>);

impl Inspect for SharedState {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.value(self.load().as_str());
    }
}

impl SharedState {
    /// 构造一个状态共享器。
    pub fn new(initial: DeviceState) -> Self {
        Self(Arc::new(AtomicU8::new(initial as u8)))
    }

    /// 读取当前状态。
    pub fn load(&self) -> DeviceState {
        DeviceState::from(self.0.load(Ordering::Acquire))
    }

    /// CAS 切换；成功返回 prior，失败返回当前真实值。
    pub fn try_transition(
        &self,
        from: DeviceState,
        to: DeviceState,
    ) -> Result<DeviceState, DeviceState> {
        self.0
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .map(DeviceState::from)
            .map_err(DeviceState::from)
    }

    /// 无条件写入（用于强制进 Lost）。
    pub fn store(&self, s: DeviceState) {
        self.0.store(s as u8, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle() {
        let s = SharedState::new(DeviceState::Connecting);
        assert_eq!(s.load(), DeviceState::Connecting);
        s.try_transition(DeviceState::Connecting, DeviceState::Live)
            .unwrap();
        assert_eq!(s.load(), DeviceState::Live);
        s.store(DeviceState::Lost);
        assert_eq!(s.load(), DeviceState::Lost);
        assert!(
            s.try_transition(DeviceState::Live, DeviceState::Live)
                .is_err()
        );
    }

    /// `DeviceState::as_str` 给 `SharedState` 的 Inspect 实现使用；保证
    /// ohcldiag-dev 看到的是 "Live/Lost/Connecting" 字符串而非数字。
    #[test]
    fn device_state_as_str_stable() {
        assert_eq!(DeviceState::Connecting.as_str(), "Connecting");
        assert_eq!(DeviceState::Live.as_str(), "Live");
        assert_eq!(DeviceState::Lost.as_str(), "Lost");
    }
}
