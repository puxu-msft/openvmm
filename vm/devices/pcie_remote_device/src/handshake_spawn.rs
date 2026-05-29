// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Boot-time handshake spawner.
//!
//! 为每个 instance 启动一个 listener task：
//!   1. bind transport (TCP 或 vsock)
//!   2. accept；任意 accept 出错 / handshake fail → 100ms backoff 后重试
//!   3. 总超时由 `CancelContext::with_timeout` 控制
//!   4. 成功 handshake → push 到 `PreparedMap` + spawn worker task
//!
//! 调用方需持有返回的 `Vec<Task<()>>`（不被 drop 则 listener task 一直运行）。

use crate::handshake::handshake;
use crate::resolver::PreparedMap;
use crate::state::DeviceState;
use crate::state::SharedState;
use crate::worker::Worker;
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

/// Listener 类型擦除：抽离 listener 接收 + accept + 返回 polled stream 的能力。
async fn accept_and_handshake<L>(
    driver: impl Driver + Clone,
    listener: L,
    instance_id: guid::Guid,
    handshake_timeout: Duration,
) -> Option<(crate::PreparedPcieRemoteDevice, PolledSocket<L::Socket>)>
where
    L: pal_async::socket::Listener + 'static,
    L::Socket: 'static + Send,
    PolledSocket<L::Socket>: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin,
{
    let mut polled_listener = match PolledSocket::new(&driver, listener) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(
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
                            %instance_id,
                            error = %e,
                            "pcie_remote: PolledSocket::new(stream) failed"
                        );
                        continue;
                    }
                };
                match handshake(polled, instance_id).await {
                    Ok((prep, polled)) => return Some((prep, polled)),
                    Err(e) => {
                        tracing::warn!(
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
        let spawner_inner = spawner.clone();
        let task = spawner.spawn(format!("pcie_remote_listen_{id}"), async move {
            let listener = match std::net::TcpListener::bind(&addr) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(
                        %id, addr,
                        error = %e,
                        "pcie_remote: TCP bind failed"
                    );
                    return;
                }
            };
            if let Err(e) = listener.set_nonblocking(true) {
                tracing::error!(%id, error = %e, "pcie_remote: set_nonblocking failed");
                return;
            }
            let Some((mut prep, polled_stream)) =
                accept_and_handshake(driver.clone(), listener, id, timeout).await
            else {
                tracing::warn!(%id, "pcie_remote: TCP handshake timeout; device absent");
                return;
            };
            let inbox = prep.take_worker_inbox();
            let state = SharedState::new(DeviceState::Live);
            prepared.lock().insert(id, prep);
            // shutdown 通道：v1 不主动关，靠 transport EOF / dead-man 进 Lost。
            // 但我们需要 sender 不被立即 drop（drop 会让 worker.next() Ok(None) 立刻退出）。
            // 用 mesh::OneshotSender + Receiver<()>，让 sender 永远不发也不 drop（leak 到进程结束）。
            let (shutdown_tx, shutdown_rx) = mesh::channel::<()>();
            std::mem::forget(shutdown_tx);
            let worker = Worker::new(polled_stream, state, inbox);
            spawner_inner
                .spawn(
                    format!("pcie_remote_worker_{id}"),
                    worker.run(shutdown_rx),
                )
                .detach();
            tracing::info!(%id, "pcie_remote: TCP handshake ok, worker spawned");
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
        let spawner_inner = spawner.clone();
        let task = spawner.spawn(format!("pcie_remote_vsock_{id}"), async move {
            let listener = match vmsocket::VmListener::bind(vmsocket::VmAddress::vsock_any(port)) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(
                        %id, port,
                        error = %e,
                        "pcie_remote: vsock bind failed"
                    );
                    return;
                }
            };
            let Some((mut prep, polled_stream)) =
                accept_and_handshake(driver.clone(), listener, id, timeout).await
            else {
                tracing::warn!(%id, "pcie_remote: vsock handshake timeout; device absent");
                return;
            };
            let inbox = prep.take_worker_inbox();
            let state = SharedState::new(DeviceState::Live);
            prepared.lock().insert(id, prep);
            let (shutdown_tx, shutdown_rx) = mesh::channel::<()>();
            std::mem::forget(shutdown_tx);
            let worker = Worker::new(polled_stream, state, inbox);
            spawner_inner
                .spawn(
                    format!("pcie_remote_vsock_worker_{id}"),
                    worker.run(shutdown_rx),
                )
                .detach();
            tracing::info!(%id, "pcie_remote: vsock handshake ok, worker spawned");
        });
        tasks.push(task);
    }
    tasks
}
