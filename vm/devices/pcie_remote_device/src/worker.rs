// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 后台 worker：通过 Transport 收发 protobuf 帧。
//!
//! 设计要点（spec §3.3 / §3.5 / §3.8）：
//! - worker 拥有 transport、in-flight map、dead-man、SharedState
//! - 进 Lost 时同步 drain in-flight (complete_error NoResponse)
//! - 关闭信号通过 mesh::Receiver<()> 接收

use crate::deadman::DeadMan;
use crate::state::DeviceState;
use crate::state::SharedState;
use chipset_device::io::IoError;
use chipset_device::io::deferred::DeferredRead;
use chipset_device::io::deferred::DeferredWrite;
use futures::FutureExt;
use futures::StreamExt;
use futures::io::AsyncRead;
use futures::io::AsyncWrite;
use futures::select_biased;
use mesh::Receiver;
use pcie_remote_protocol::ToHost;
use pcie_remote_protocol::ToOpenhcl;
use pcie_remote_protocol::codec;
use std::collections::HashMap;

/// Pending request stored against a sequence number.
pub enum InFlight {
    /// MMIO read 等 host 回 MmioReadResult。
    Read {
        /// DeferredRead 由 device shim 创建并通过 DeviceRequest 移交进来。
        token: DeferredRead,
        /// 实际访问字节数（1/2/4/8），用于 complete 截断。
        access_size: usize,
    },
    /// MMIO write 等 host ack（v1 暂未严格 ack；保留接口）。
    Write {
        /// DeferredWrite，由 device shim 创建。
        token: DeferredWrite,
    },
}

/// 由 device shim 发给 worker 的请求。
pub struct DeviceRequest {
    /// 序号（也是协议帧的 seq）。
    pub seq: u64,
    /// 要发给 host 的帧。
    pub frame: ToHost,
    /// 可选：等 host 回应的 DeferredRead/DeferredWrite。
    /// 如果是 fire-and-forget（如 cfg_write_side_effect）则为 None。
    pub pending: Option<InFlight>,
}

/// Worker —— 后台 task 持有它，跑 `run()`。
pub struct Worker<T> {
    transport: T,
    state: SharedState,
    in_flight: HashMap<u64, InFlight>,
    _deadman: DeadMan,
    from_device: Receiver<DeviceRequest>,
}

impl<T> Worker<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    /// 构造 Worker。
    pub fn new(
        transport: T,
        state: SharedState,
        from_device: Receiver<DeviceRequest>,
    ) -> Self {
        Self {
            transport,
            state,
            in_flight: HashMap::new(),
            _deadman: DeadMan::new(),
            from_device,
        }
    }

    /// 主循环：直到 shutdown 信号或 transport 出错。
    pub async fn run(mut self, mut shutdown: Receiver<()>) {
        loop {
            select_biased! {
                _ = shutdown.next().fuse() => {
                    tracing::info!("pcie_remote worker shutdown signal");
                    break;
                }
                req = self.from_device.next().fuse() => {
                    let Some(req) = req else {
                        // sender 全 drop，退出
                        break;
                    };
                    if let Some(pending) = req.pending {
                        self.in_flight.insert(req.seq, pending);
                    }
                    if let Err(e) = codec::write_frame(&mut self.transport, &req.frame).await {
                        tracing::warn!(error = %e, "write_frame failed; going Lost");
                        break;
                    }
                }
                inbound = codec::read_frame::<_, ToOpenhcl>(&mut self.transport).fuse() => {
                    match inbound {
                        Ok(m) => self.dispatch_inbound(m),
                        Err(e) => {
                            tracing::warn!(error = %e, "read_frame failed; going Lost");
                            break;
                        }
                    }
                }
            }
        }
        self.drain_in_flight();
        self.state.store(DeviceState::Lost);
    }

    fn dispatch_inbound(&mut self, msg: ToOpenhcl) {
        use pcie_remote_protocol::to_openhcl::Body;
        let seq = msg.seq;
        match msg.body {
            Some(Body::MmioReadResult(r)) => {
                if let Some(InFlight::Read { token, access_size }) =
                    self.in_flight.remove(&seq)
                {
                    let bytes = r.value.to_le_bytes();
                    let n = access_size.min(8);
                    token.complete(&bytes[..n]);
                }
            }
            Some(Body::ReadGpa(_) | Body::WriteGpa(_)) => {
                tracing::warn!("DMA messages not yet implemented (Phase 5+)");
            }
            Some(Body::InterruptFire(_)) => {
                tracing::warn!("InterruptFire delivery lands when device.rs holds Vec<Interrupt>");
            }
            None => {
                tracing::warn!("ToOpenhcl missing body");
            }
        }
    }

    fn drain_in_flight(&mut self) {
        for (_, inflight) in self.in_flight.drain() {
            match inflight {
                InFlight::Read { token, .. } => token.complete_error(IoError::NoResponse),
                InFlight::Write { token } => token.complete_error(IoError::NoResponse),
            }
        }
    }
}
