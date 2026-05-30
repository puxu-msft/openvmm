// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Boot-time handshake spawner（v2 重构）。
//!
//! 为每个 instance 启动一个 listener task：
//!   1. bind transport (TCP 或 vsock)
//!   2. accept；任意 accept 出错 / handshake fail → 100ms backoff 后重试
//!   3. 总超时由 `CancelContext::with_timeout` 控制
//!   4. 成功 handshake → 把 `PreparedPcieRemoteDevice { describe, transport }`
//!      插入 prepared_map
//!
//! **v2 关键变化：本任务不再 spawn worker**。worker 在 resolver
//! `assemble_device` 阶段才 spawn（彼时拿到 `msi_target / register_mmio /
//! guest_memory`，可构造完整 MSI-X / BAR / DMA 闭环）。
//!
//! 因此本任务返回 `Vec<Task<()>>` 仅包含 listener task；listener task
//! 在 handshake 成功后自然结束，prepared 在 map 中等 resolver 取走。

use crate::handshake::handshake;
use crate::prepared::PreparedPcieRemoteDevice;
use crate::prepared::box_transport;
use crate::resolver::PreparedMap;
use cvm_tracing::CVM_ALLOWED;
use mesh::CancelContext;
use pal_async::driver::Driver;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use std::time::Duration;

/// K-11: Per-instance accept attempts cap. 拒绝恶意 host 用大 timeout 把 vm
/// boot 期间拉到无限重试。32 是仓库 ttrpc / pipette 等设施的常见值。
const MAX_ACCEPT_ATTEMPTS: u32 = 32;

/// Listener 类型擦除：抽离 listener 接收 + accept + 返回 boxed transport 的能力。
///
/// 返回 Some(prepared) 表示 handshake 成功；None 表示超时 / 重试耗尽。
async fn accept_and_handshake<L>(
    driver: impl Driver + Clone,
    listener: L,
    instance_id: guid::Guid,
    handshake_timeout: Duration,
) -> Option<PreparedPcieRemoteDevice>
where
    L: pal_async::socket::Listener + 'static,
    L::Socket: 'static + Send,
    PolledSocket<L::Socket>: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin + Send,
{
    let mut polled_listener = match PolledSocket::new(&driver, listener) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(
                CVM_ALLOWED,
                %instance_id,
                error = %e,
                "pcie_remote: PolledSocket::new(listener) failed"
            );
            return None;
        }
    };

    let mut ctx = CancelContext::new().with_timeout(handshake_timeout);
    let driver_for_backoff = driver.clone();
    let outcome = ctx
        .until_cancelled(async move {
            let mut attempts: u32 = 0;
            loop {
                if attempts >= MAX_ACCEPT_ATTEMPTS {
                    tracing::error!(
                        CVM_ALLOWED,
                        %instance_id,
                        attempts,
                        "pcie_remote: per-instance accept attempt cap reached"
                    );
                    return None;
                }
                attempts += 1;
                let (stream, _addr) = match polled_listener.accept().await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            CVM_ALLOWED,
                            %instance_id,
                            error = %e,
                            "pcie_remote: accept failed; backing off"
                        );
                        PolledTimer::new(&driver_for_backoff)
                            .sleep(Duration::from_millis(100))
                            .await;
                        continue;
                    }
                };
                let polled = match PolledSocket::new(&driver_for_backoff, stream) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            CVM_ALLOWED,
                            %instance_id,
                            error = %e,
                            "pcie_remote: PolledSocket::new(stream) failed"
                        );
                        continue;
                    }
                };
                match handshake(polled, instance_id).await {
                    Ok((describe, polled)) => {
                        return Some(PreparedPcieRemoteDevice {
                            describe,
                            transport: Some(box_transport(polled)),
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            CVM_ALLOWED,
                            %instance_id,
                            error = %e,
                            "pcie_remote: handshake failed; listener kept open"
                        );
                        PolledTimer::new(&driver_for_backoff)
                            .sleep(Duration::from_millis(100))
                            .await;
                        continue;
                    }
                }
            }
        })
        .await;
    outcome.ok().flatten()
}

/// 为一组 TCP-based instance 启动 listener 任务，结果填入 `prepared`。
///
/// 返回 listener tasks（调用方持有 Vec 不被 drop）。
pub fn spawn_tcp_handshakes(
    driver: impl Driver + Clone + 'static,
    spawner: impl Spawn + Clone + 'static,
    instances: Vec<(guid::Guid, String, Duration)>,
    prepared: PreparedMap,
) -> Vec<Task<()>> {
    let mut tasks = Vec::new();
    for (id, addr, timeout) in instances {
        let driver = driver.clone();
        let prepared = prepared.clone();
        let _spawner_keep = spawner.clone();
        let _ = _spawner_keep; // v2: 不再 spawn worker，spawner 不必再持
        let task = spawner.spawn(format!("pcie_remote_listen_{id}"), async move {
            let listener = match std::net::TcpListener::bind(&addr) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(
                        CVM_ALLOWED,
                        %id, addr,
                        error = %e,
                        "pcie_remote: TCP bind failed"
                    );
                    return;
                }
            };
            if let Err(e) = listener.set_nonblocking(true) {
                tracing::error!(CVM_ALLOWED, %id, error = %e, "pcie_remote: set_nonblocking failed");
                return;
            }
            let Some(prep) =
                accept_and_handshake(driver.clone(), listener, id, timeout).await
            else {
                tracing::warn!(CVM_ALLOWED, %id, "pcie_remote: TCP handshake timeout; device absent");
                return;
            };
            prepared.lock().insert(id, prep);
            tracing::info!(CVM_ALLOWED, %id, "pcie_remote: TCP handshake ok, prepared inserted");
        });
        tasks.push(task);
    }
    tasks
}

/// 为一组 vsock-based instance 启动 listener 任务（OpenHCL 路径）。
pub fn spawn_vsock_handshakes(
    driver: impl Driver + Clone + 'static,
    spawner: impl Spawn + Clone + 'static,
    instances: Vec<(guid::Guid, u32, Duration)>,
    prepared: PreparedMap,
) -> Vec<Task<()>> {
    let mut tasks = Vec::new();
    for (id, port, timeout) in instances {
        let driver = driver.clone();
        let prepared = prepared.clone();
        let task = spawner.spawn(format!("pcie_remote_vsock_{id}"), async move {
            let listener = match vmsocket::VmListener::bind(vmsocket::VmAddress::vsock_any(port)) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(
                        CVM_ALLOWED,
                        %id, port,
                        error = %e,
                        "pcie_remote: vsock bind failed"
                    );
                    return;
                }
            };
            let Some(prep) =
                accept_and_handshake(driver.clone(), listener, id, timeout).await
            else {
                tracing::warn!(CVM_ALLOWED, %id, "pcie_remote: vsock handshake timeout; device absent");
                return;
            };
            prepared.lock().insert(id, prep);
            tracing::info!(CVM_ALLOWED, %id, "pcie_remote: vsock handshake ok, prepared inserted");
        });
        tasks.push(task);
    }
    tasks
}
