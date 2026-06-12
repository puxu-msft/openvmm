// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 设备状态机（照抄 `pcie_remote_device::state`）。
//!
//! Connecting → Live ⇄ Lost。**Lost 非终态**：C-3 reconnect 成功后 worker 把状态
//! 重新 `store(Live)`（见 `worker.rs`），故 Lost↔Live 可反复跃迁（后端 usnvmemu
//! 停了又起）。device shim 与 async worker 经 [`SharedState`] 共用一个 `AtomicU8`。
//!
//! **热插拔（Layer C）扩展**：`SharedState` 可挂一个可选的「边沿通知」
//! [`mesh::Sender<DeviceState>`]。每当状态**真正发生跃迁**（值改变）时，向该
//! sender fire-and-forget 投递新状态。underhill 侧的 vfio_user 热插拔 reconcile
//! （`emuplat/vfio_user_hotplug.rs`）持对应 `Receiver`，据此驱动 guest 侧 VpciBus：
//! 首个 Live→add 通道（盘出现）；**Lost→保设备在位**（Option B transient-stall 模型：
//! 不 hot-remove，靠 C-3 reconnect 透明恢复——Windows guest 无法被 VSP 迫使重枚举，
//! 故不走 remove/re-add，详见 `vfio_user_hotplug.rs` `process` 的 Lost 分支注释）。
//! - sender 缺省 `None`（resolver / 单元测试路径）→ **零行为变化**，不触任何通知。
//! - 仅热插拔路径在装配 device shim 时经 [`SharedState::with_edge_notifier`] 注入。
//! - 状态跃迁罕见（connect/disconnect，**非** per-IO），故通知开销可忽略；
//!   `mesh::Sender::send` 本身是非阻塞 fire-and-forget。

use inspect::Inspect;
use mesh::Sender;
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
    /// 后端连接丢失（usnvmemu 停）：cfg read 返全 1（guest 视为设备无响应），MMIO 返 Err。
    /// **非终态**——C-3 reconnect 成功后回到 [`Live`](Self::Live)；短暂 Lost 窗口内 guest
    /// 的 IO 超时重试，重连后透明恢复（如真硬件 controller 短暂 reset）。
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
///
/// 内部 = 一个 `AtomicU8`（状态值）+ 一个可选的边沿通知 sender（C0 热插拔用）。
/// `Clone` 复制 `Arc`，故所有副本共享同一状态 + 同一通知 sink。
#[derive(Clone)]
pub struct SharedState(Arc<SharedStateInner>);

struct SharedStateInner {
    /// 离散状态值（编码进 `AtomicU8`）。
    value: AtomicU8,
    /// 可选边沿通知：状态**真正改变**时投递新状态（fire-and-forget）。
    /// `None` 时（resolver / 测试路径）所有跃迁静默，零行为变化。
    edge_tx: Option<Sender<DeviceState>>,
}

impl Inspect for SharedState {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.value(self.load().as_str());
    }
}

impl SharedState {
    /// 构造一个状态共享器（无边沿通知）。
    pub fn new(initial: DeviceState) -> Self {
        Self(Arc::new(SharedStateInner {
            value: AtomicU8::new(initial as u8),
            edge_tx: None,
        }))
    }

    /// 构造一个带边沿通知的状态共享器（C0 热插拔）。
    ///
    /// 每当状态**真正改变**（`store` 写入不同值，或 `try_transition` CAS 成功）时，
    /// 向 `edge_tx` fire-and-forget 投递**新**状态。reconcile 据此 add/remove bus。
    /// `edge_tx` 的接收端被 drop 后，`send` 自动降级为无害空操作。
    pub fn with_edge_notifier(initial: DeviceState, edge_tx: Sender<DeviceState>) -> Self {
        Self(Arc::new(SharedStateInner {
            value: AtomicU8::new(initial as u8),
            edge_tx: Some(edge_tx),
        }))
    }

    /// 读取当前状态。
    pub fn load(&self) -> DeviceState {
        DeviceState::from(self.0.value.load(Ordering::Acquire))
    }

    /// 投递边沿通知（内部）。仅在确有 sender 时发；不持锁、非阻塞。
    fn notify_edge(&self, new_state: DeviceState) {
        if let Some(tx) = &self.0.edge_tx {
            tx.send(new_state);
        }
    }

    /// CAS 切换；成功返回 prior，失败返回当前真实值。
    ///
    /// CAS 成功（即发生真正跃迁）时投递一次边沿通知。
    pub fn try_transition(
        &self,
        from: DeviceState,
        to: DeviceState,
    ) -> Result<DeviceState, DeviceState> {
        let r = self
            .0
            .value
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .map(DeviceState::from)
            .map_err(DeviceState::from);
        if r.is_ok() {
            // CAS 成功意味着 from != to 时状态确实改变（from == to 的退化 CAS 不应被
            // 调用方用作跃迁，且即便发生也只是冗余通知，reconcile 端幂等处理）。
            self.notify_edge(to);
        }
        r
    }

    /// 无条件写入（用于强制进 Lost）。
    ///
    /// 仅当写入值与旧值**不同**（真正跃迁）时才投递边沿通知，避免重复 store 同值
    /// 引发的冗余通知（如 `go_lost` 兜底再 store(Lost)）。
    pub fn store(&self, s: DeviceState) {
        let prev = self.0.value.swap(s as u8, Ordering::AcqRel);
        if prev != s as u8 {
            self.notify_edge(s);
        }
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

    /// C0 边沿通知：每次**真正跃迁**恰好投递一次新状态；重复 store 同值不通知。
    #[test]
    fn edge_notifier_fires_on_real_transition_only() {
        let (tx, mut rx) = mesh::channel::<DeviceState>();
        let s = SharedState::with_edge_notifier(DeviceState::Connecting, tx);

        // Connecting → Live（CAS 成功）→ 通知 Live。
        s.try_transition(DeviceState::Connecting, DeviceState::Live)
            .unwrap();
        assert_eq!(rx.try_recv().unwrap(), DeviceState::Live);

        // 失败的 CAS（当前 Live，from=Connecting 不匹配）→ 不通知。
        assert!(
            s.try_transition(DeviceState::Connecting, DeviceState::Lost)
                .is_err()
        );
        assert!(rx.try_recv().is_err());

        // store(Lost)（值改变）→ 通知 Lost。
        s.store(DeviceState::Lost);
        assert_eq!(rx.try_recv().unwrap(), DeviceState::Lost);

        // 重复 store(Lost)（值不变）→ 不通知（避免 go_lost 兜底冗余通知）。
        s.store(DeviceState::Lost);
        assert!(rx.try_recv().is_err());
    }

    /// 无 edge sender（默认 `new`）路径：跃迁不 panic、不依赖通知（零行为变化）。
    #[test]
    fn no_edge_notifier_is_silent() {
        let s = SharedState::new(DeviceState::Connecting);
        s.try_transition(DeviceState::Connecting, DeviceState::Live)
            .unwrap();
        s.store(DeviceState::Lost);
        assert_eq!(s.load(), DeviceState::Lost);
    }
}
