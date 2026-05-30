// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 主循环：handshake → select(read inbound | timer tick) → 派发到 PcieDevice。
//!
//! 整个 SDK 的 wire-protocol 处理全在本文件 + transport.rs；用户实现完全
//! 不需要 import `pcie_remote_protocol` 的 ToHost/ToOpenhcl 等底层类型。

use crate::DeviceCtx;
use crate::PcieDevice;
use crate::Transport;
use anyhow::Result;
use anyhow::anyhow;
use futures::FutureExt;
use pal_async::driver::Driver;
use pal_async::timer::PolledTimer;
use pcie_remote_protocol::Hello;
use pcie_remote_protocol::HelloAck;
use pcie_remote_protocol::MmioReadResult;
use pcie_remote_protocol::PROTOCOL_MAGIC;
use pcie_remote_protocol::ToHost;
use pcie_remote_protocol::ToOpenhcl;
use pcie_remote_protocol::codec;
use pcie_remote_protocol::to_host::Body as HostBody;
use pcie_remote_protocol::to_openhcl::Body as OpenhclBody;
use std::time::Duration;

/// `run` 行为可调项。
pub struct RunOptions {
    /// 周期 tick 间隔（驱动 `PcieDevice::tick`）。默认 5s。NVMe 等不需要
    /// tick 的设备可设很大值（如 Duration::MAX）。
    pub tick_interval: Duration,
    /// inbound read 单次超时；超时即 tick 一次。一般 = `tick_interval`。
    pub read_timeout: Duration,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(5),
            read_timeout: Duration::from_secs(5),
        }
    }
}

/// 主循环。Handshake → 驱动 PcieDevice 直到 transport EOF / err。
///
/// 调用方应在外层 reconnect loop 中重复调用本函数 —— EOF/err 后重 connect
/// + 新 device instance + 再调 `run`。这对应 OpenHCL K-20 hotplug。
///
/// # Errors
///
/// - handshake 失败（OpenHCL 拒接 / 协议版本不匹配）
/// - transport read/write Err
/// - PcieDevice 返回的 MMIO size 非法（应自检，但 SDK 也兜底检查）
pub async fn run<D: PcieDevice>(
    driver: &impl Driver,
    mut transport: Transport,
    mut device: D,
    options: RunOptions,
) -> Result<()> {
    // ─── 1. Handshake ───
    let hello: Hello = codec::read_frame(&mut transport).await?;
    if hello.magic != PROTOCOL_MAGIC {
        return Err(anyhow!(
            "protocol magic mismatch: got {:#x} want {:#x}",
            hello.magic,
            PROTOCOL_MAGIC
        ));
    }
    tracing::info!(
        magic = format_args!("{:#x}", hello.magic),
        version = hello.version,
        "SDK received Hello"
    );

    let describe = device.describe();
    let ack = HelloAck {
        ok: true,
        reason: String::new(),
        device: Some(describe),
    };
    codec::write_frame(&mut transport, &ack).await?;
    tracing::info!("SDK sent HelloAck; entering main loop");

    // ─── 2. 主循环 ───
    //
    // 协议状态：seq 由 SDK 单调分配（高位区间避免与 OpenHCL 侧 seq 撞）。
    let mut next_seq: u64 = 1u64 << 32;
    // 出站帧缓冲：device 回调可能 push 多个；本循环串行 flush。
    let mut outbound: Vec<ToOpenhcl> = Vec::with_capacity(16);
    let mut timer = PolledTimer::new(driver);

    loop {
        // 2a. 等 inbound 或超时 tick。
        let inbound = {
            let read_fut = codec::read_frame::<_, ToHost>(&mut transport).fuse();
            let timer_fut = timer.sleep(options.read_timeout).fuse();
            futures::pin_mut!(read_fut, timer_fut);
            futures::select_biased! {
                req = read_fut => Some(req),
                _ = timer_fut => None,
            }
        };

        match inbound {
            Some(Ok(req)) => {
                dispatch_inbound(&mut device, req, &mut outbound, &mut next_seq)?;
            }
            Some(Err(e)) => return Err(anyhow!("transport read failed: {e}")),
            None => {
                let mut ctx = DeviceCtx {
                    outbound: &mut outbound,
                    next_seq: &mut next_seq,
                };
                device.tick(&mut ctx);
            }
        }

        // 2b. flush outbound（mmio_read 的 reply、interrupt、DMA 请求）。
        for frame in outbound.drain(..) {
            if let Err(e) = codec::write_frame(&mut transport, &frame).await {
                return Err(anyhow!("transport write failed: {e}"));
            }
        }
    }
}

/// 处理一帧 ToHost：派发到 PcieDevice + 把响应 push 到 outbound。
fn dispatch_inbound<D: PcieDevice>(
    device: &mut D,
    req: ToHost,
    outbound: &mut Vec<ToOpenhcl>,
    next_seq: &mut u64,
) -> Result<()> {
    let seq = req.seq;
    match req.body {
        Some(HostBody::MmioRead(m)) => {
            // 协议侧已校验 size ∈ {1,2,4,8}（OpenHCL worker.rs K-18），
            // 但 SDK 兜底再检一次防御性 — 用户实现可能误传。
            if !matches!(m.size, 1 | 2 | 4 | 8) {
                return Err(anyhow!(
                    "MmioRead: invalid size {} (must be 1/2/4/8)",
                    m.size
                ));
            }
            let value = device.mmio_read(m.bar, m.offset, m.size);
            // value 截到 size 字节（PCI 协议低位有效）；用 mask 防御 device
            // 实现误返高位 garbage。
            let masked = mask_value(value, m.size);
            outbound.push(ToOpenhcl {
                seq,
                body: Some(OpenhclBody::MmioReadResult(MmioReadResult {
                    value: masked,
                })),
            });
        }
        Some(HostBody::MmioWrite(m)) => {
            if !matches!(m.size, 1 | 2 | 4 | 8) {
                return Err(anyhow!(
                    "MmioWrite: invalid size {} (must be 1/2/4/8)",
                    m.size
                ));
            }
            device.mmio_write(m.bar, m.offset, m.size, m.value);
        }
        Some(HostBody::CfgWriteSideEffect(c)) => {
            device.cfg_write_side_effect(c.offset, c.value);
        }
        Some(HostBody::Reset(r)) => {
            device.reset(r.kind);
        }
        Some(HostBody::DmaCompletion(d)) => {
            // v1 SDK：fire-and-forget DMA，不回调用户。仅 trace 一行供调试。
            tracing::trace!(
                seq,
                token = d.token,
                ok = d.ok,
                data_len = d.data.len(),
                "DmaCompletion (fire-and-forget mode, dropping)"
            );
        }
        None => {
            tracing::warn!(seq, "ToHost missing body; ignored");
        }
    }
    let _ = next_seq; // 当前 dispatch path 不分配 seq；预留给未来 DMA 完成回调路径
    Ok(())
}

/// 把 `value` 的高于 `size` 字节的位清零。size ∈ {1,2,4,8}。
fn mask_value(value: u64, size: u32) -> u64 {
    match size {
        1 => value & 0xff,
        2 => value & 0xffff,
        4 => value & 0xffff_ffff,
        8 => value,
        _ => unreachable!("size validated before mask_value"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_value_clears_high_bits() {
        assert_eq!(mask_value(0xdead_beef_cafe_babe, 1), 0xbe);
        assert_eq!(mask_value(0xdead_beef_cafe_babe, 2), 0xbabe);
        assert_eq!(mask_value(0xdead_beef_cafe_babe, 4), 0xcafe_babe);
        assert_eq!(mask_value(0xdead_beef_cafe_babe, 8), 0xdead_beef_cafe_babe);
    }
}
