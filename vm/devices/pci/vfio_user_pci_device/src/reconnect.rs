// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 持久重连引擎（W6b A2.2）。
//!
//! 把 W6b 从「一次性 connect」升级为「持久重连」：operator 可在 runtime 反复
//! 起停/重启 usnvmemu backend，设备据此在 Lost↔Live 间切换。
//!
//! [`reconnect_loop`] 是一个长生命周期 async task（resolver spawn 进 worker_tasks
//! 保活）。它与 [`crate::worker::Worker`] 通过两条 channel 协作：
//! - **`reconnect_tx`**（connector → worker）：每次连接成功，把全新的一对全双工
//!   读写半经 [`ReconnectEvent::Connected`] **一条消息**交给 worker（C-1：两半原子
//!   换上）。
//! - **`lost_rx`**（worker → connector）：worker 每次检测到掉线（边沿触发）就发一次
//!   信号；connector 收到后发起下一轮重连。
//!
//! 一轮重连的完整步骤（任一步 Err 都 backoff 后重来，期间设备保持非 Live）：
//!   1. `connect` 到 firmware 的 AF_UNIX socket（err → backoff）
//!   2. `handshake`（err → backoff）
//!   3. 读真几何：`get_region_info(BAR0).size` + `get_irq_info(MSIX).count`（err → backoff）
//!   4. [`validate_identity`]：actual ≤ declared 才放行；`Exceeds` → 错误日志 + backoff，
//!      **保持非 Live**（防 guest 拿到半映射控制器，决策 b）
//!   5. **C-3**：用**持久 eventfd**（`eventfds`，owned `pal_event::Event` clone）重新
//!      `set_irqs`——每次重连都是全新 client/socket，必须在 `into_channel` 之前重新把
//!      同一批 eventfd 配给 firmware，否则 revive 后 MSI-X 形同虚设（err → backoff）
//!   6. `into_channel` 拆全双工读写半
//!   7. `reconnect_tx.send(Connected{writer,reader})`（Err = worker 已退出 → 整个
//!      loop return）
//!   8. 重置 backoff
//!   9. `lost_rx.next().await` 等下次掉线（`None` = worker 已退出 → return）→ 回到 1
//!
//! **backoff**：指数退避 100ms → 2s（每次失败翻倍，封顶 2s）；连接成功后重置回 100ms。
//!
//! 本模块纯 safe（无 SCM_RIGHTS 裸 fd 操作——`set_irqs` 的 fd 传递封在 W6a
//! `vfio_user_device` 内）。

#![forbid(unsafe_code)]

use crate::identity::ActualGeometry;
use crate::identity::DeclaredGeometry;
use crate::identity::IdentityCheck;
use crate::identity::validate_identity;
use crate::worker::ReconnectEvent;
use cvm_tracing::CVM_ALLOWED;
use futures::StreamExt as _;
use mesh::Receiver;
use mesh::Sender;
use pal_async::driver::Driver;
use pal_async::timer::PolledTimer;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use std::time::Duration;
use vfio_user_device::VfioUserClient;
use vfio_user_wire::proto::pci_irq;
use vfio_user_wire::proto::pci_region;

/// backoff 起始值（首次失败后等待）。
const BACKOFF_MIN: Duration = Duration::from_millis(100);
/// backoff 封顶值（指数退避不超过此值）。
const BACKOFF_MAX: Duration = Duration::from_secs(2);

/// connector ↔ worker 的两条 channel 端点（connector 持有的一侧）。
///
/// `reconnect_tx`：connector → worker，投递新连接（[`ReconnectEvent::Connected`]）。
/// `lost_rx`：worker → connector，掉线边沿信号（驱动下一轮重连）。
pub struct ReconnectChannels {
    /// 投递新连接（writer+reader 对）给 worker。
    pub reconnect_tx: Sender<ReconnectEvent>,
    /// 接收 worker 的掉线边沿信号。
    pub lost_rx: Receiver<()>,
}

/// 下一个 backoff 值：当前值翻倍，封顶 [`BACKOFF_MAX`]。
fn next_backoff(cur: Duration) -> Duration {
    (cur * 2).min(BACKOFF_MAX)
}

/// 持久重连循环（A2.2）。详见模块文档。
///
/// - `driver`：用于 connect / set_irqs 的 async I/O + backoff sleep（`Clone` 以便
///   `connect` 与 `PolledTimer` 各取一份引用）。
/// - `unix_path`：firmware vfio-user server 的 AF_UNIX socket 路径。
/// - `declared`：声明几何（identity 校验上界）。
/// - `eventfds`：**持久** MSI-X eventfd（owned `pal_event::Event` clone，与 irq task
///   共享同一批底层 fd）；每次重连 C-3 重新 `set_irqs` 用。空 vec = 无 MSI-X，跳过
///   set_irqs。
/// - `ch`：connector 侧 channel 端点。
pub async fn reconnect_loop(
    driver: impl Driver + Clone,
    unix_path: String,
    declared: DeclaredGeometry,
    eventfds: Vec<pal_event::Event>,
    ch: ReconnectChannels,
) {
    let ReconnectChannels {
        reconnect_tx,
        mut lost_rx,
    } = ch;
    let mut backoff = BACKOFF_MIN;

    loop {
        // ── 1. connect ──
        let mut client = match VfioUserClient::connect(&driver, &unix_path).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    CVM_ALLOWED,
                    path = unix_path.as_str(),
                    error = %e,
                    backoff_ms = backoff.as_millis() as u64,
                    "vfio_user_pci reconnect: connect failed; backing off"
                );
                PolledTimer::new(&driver).sleep(backoff).await;
                backoff = next_backoff(backoff);
                continue;
            }
        };

        // ── 2. handshake ──
        if let Err(e) = client.handshake().await {
            tracing::warn!(
                CVM_ALLOWED,
                error = %e,
                backoff_ms = backoff.as_millis() as u64,
                "vfio_user_pci reconnect: handshake failed; backing off"
            );
            PolledTimer::new(&driver).sleep(backoff).await;
            backoff = next_backoff(backoff);
            continue;
        }

        // ── 3. 读真几何（BAR0 size + MSI-X count）──
        let actual = match read_actual_geometry(&mut client).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(
                    CVM_ALLOWED,
                    error = %e,
                    backoff_ms = backoff.as_millis() as u64,
                    "vfio_user_pci reconnect: read geometry failed; backing off"
                );
                PolledTimer::new(&driver).sleep(backoff).await;
                backoff = next_backoff(backoff);
                continue;
            }
        };

        // ── 4. identity 校验（actual ≤ declared 才放行）──
        // 注：spec 曾提「vendor/class drift → log+继续 Live」，但本实现**有意**把
        // guest-facing 身份固定为 `declared_hardware_ids()`，只校验 BAR0 size / MSI-X
        // count（决策 b）。故 vendor/class drift **不被观测/上报**——这是刻意为之
        // （固定 guest 身份更安全，且与 pci_core construct-once 一致），**非疏漏**。
        if let IdentityCheck::Exceeds(why) = validate_identity(&declared, &actual) {
            tracing::error!(
                CVM_ALLOWED,
                why,
                actual_bar0 = actual.bar0_size,
                declared_bar0 = declared.bar0_size,
                actual_msix = actual.msix_count,
                declared_msix = declared.msix_count,
                backoff_ms = backoff.as_millis() as u64,
                "vfio_user_pci reconnect: actual geometry exceeds declared; refusing (stay non-Live)"
            );
            PolledTimer::new(&driver).sleep(backoff).await;
            backoff = next_backoff(backoff);
            continue;
        }

        // ── 5. C-3：重新 set_irqs（持久 eventfd），必须在 into_channel 之前 ──
        if !eventfds.is_empty() {
            let fds: Vec<BorrowedFd<'_>> = eventfds.iter().map(|e| e.as_fd()).collect();
            if let Err(e) = client.set_irqs(pci_irq::MSIX, 0, &fds).await {
                tracing::warn!(
                    CVM_ALLOWED,
                    error = %e,
                    backoff_ms = backoff.as_millis() as u64,
                    "vfio_user_pci reconnect: set_irqs failed; backing off"
                );
                PolledTimer::new(&driver).sleep(backoff).await;
                backoff = next_backoff(backoff);
                continue;
            }
        }

        // ── 6. 拆全双工读写半（注意顺序：set_irqs 借 &mut client 在前，
        //       into_channel consume client 在后）──
        let (writer, reader) = client.into_channel();

        // ── 7. 投递新连接给 worker（C-1：两半同一条消息）──
        // mesh `Sender::send` 是 fire-and-forget（返 `()`）；用 `is_closed()` 检测
        // worker 是否已退出（其 `reconnect_rx` 被 drop）。已退出 → connector 收工。
        reconnect_tx.send(ReconnectEvent::Connected { writer, reader });
        if reconnect_tx.is_closed() {
            tracing::info!(
                CVM_ALLOWED,
                "vfio_user_pci reconnect: worker gone (reconnect channel closed); connector exiting"
            );
            return;
        }
        tracing::info!(
            CVM_ALLOWED,
            path = unix_path.as_str(),
            "vfio_user_pci reconnect: connected, handed transport to worker"
        );

        // ── 8. 连接成功，重置 backoff ──
        backoff = BACKOFF_MIN;

        // ── 9. 等下次掉线边沿信号（None = worker 退出 → 收工）──
        if lost_rx.next().await.is_none() {
            tracing::info!(
                CVM_ALLOWED,
                "vfio_user_pci reconnect: lost channel closed (worker gone); connector exiting"
            );
            return;
        }
        tracing::info!(
            CVM_ALLOWED,
            "vfio_user_pci reconnect: worker reported Lost; reconnecting"
        );
        // 回到循环顶部重连。
    }
}

/// 读 firmware 真几何：BAR0 size + MSI-X count。
///
/// packed `RegionInfoPayload` / `IrqInfoPayload` 字段先 copy 到本地（`#[repr(C,packed)]`
/// 下直接取字段是 E0793）。
async fn read_actual_geometry(client: &mut VfioUserClient) -> anyhow::Result<ActualGeometry> {
    let bar0 = client.get_region_info(pci_region::BAR0).await?;
    let bar0_size = bar0.size; // packed copy

    let irq = client.get_irq_info(pci_irq::MSIX).await?;
    let msix_count = irq.count as u16; // packed copy

    Ok(ActualGeometry {
        bar0_size,
        msix_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// backoff 指数退避：100ms → 200 → 400 → 800 → 1600 → 2000（封顶）。
    #[test]
    fn backoff_grows_then_caps() {
        let mut b = BACKOFF_MIN;
        assert_eq!(b, Duration::from_millis(100));
        b = next_backoff(b);
        assert_eq!(b, Duration::from_millis(200));
        b = next_backoff(b);
        assert_eq!(b, Duration::from_millis(400));
        b = next_backoff(b);
        assert_eq!(b, Duration::from_millis(800));
        b = next_backoff(b);
        assert_eq!(b, Duration::from_millis(1600));
        b = next_backoff(b);
        // 3200 → 封顶 2000。
        assert_eq!(b, Duration::from_secs(2));
        b = next_backoff(b);
        // 已封顶，保持 2000。
        assert_eq!(b, Duration::from_secs(2));
    }
}
