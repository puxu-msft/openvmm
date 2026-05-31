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
    // dma_token 同理，独立空间便于 device state machine 关联请求-响应。
    let mut next_seq: u64 = 1u64 << 32;
    let mut next_dma_token: u64 = 1u64 << 40;
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
                dispatch_inbound(
                    &mut device,
                    req,
                    &mut outbound,
                    &mut next_seq,
                    &mut next_dma_token,
                )?;
            }
            Some(Err(e)) => return Err(anyhow!("transport read failed: {e}")),
            None => {
                let mut ctx = DeviceCtx {
                    outbound: &mut outbound,
                    next_seq: &mut next_seq,
                    next_dma_token: &mut next_dma_token,
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
    next_dma_token: &mut u64,
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
            let mut ctx = DeviceCtx {
                outbound,
                next_seq,
                next_dma_token,
            };
            device.mmio_write(&mut ctx, m.bar, m.offset, m.size, m.value);
        }
        Some(HostBody::CfgWriteSideEffect(c)) => {
            device.cfg_write_side_effect(c.offset, c.value);
        }
        Some(HostBody::Reset(r)) => {
            device.reset(r.kind);
        }
        Some(HostBody::DmaCompletion(d)) => {
            let mut ctx = DeviceCtx {
                outbound,
                next_seq,
                next_dma_token,
            };
            device.on_dma_complete(&mut ctx, d.token, d.ok, d.data);
        }
        None => {
            tracing::warn!(seq, "ToHost missing body; ignored");
        }
    }
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
    use pcie_remote_protocol::to_openhcl::Body;

    #[test]
    fn mask_value_clears_high_bits() {
        assert_eq!(mask_value(0xdead_beef_cafe_babe, 1), 0xbe);
        assert_eq!(mask_value(0xdead_beef_cafe_babe, 2), 0xbabe);
        assert_eq!(mask_value(0xdead_beef_cafe_babe, 4), 0xcafe_babe);
        assert_eq!(mask_value(0xdead_beef_cafe_babe, 8), 0xdead_beef_cafe_babe);
    }

    /// **Phase N1** — DeviceCtx 基础 API：每个动作往 outbound 队列加一条
    /// protobuf message，seq 单调递增。
    #[test]
    fn device_ctx_outbound_seq_monotonic() {
        let mut outbound = Vec::new();
        let mut seq = 0u64;
        let mut tok = 0u64;
        let mut ctx = crate::DeviceCtx {
            outbound: &mut outbound,
            next_seq: &mut seq,
            next_dma_token: &mut tok,
        };
        ctx.fire_interrupt(0);
        let t1 = ctx.dma_read(0x1000, 4096);
        let t2 = ctx.dma_write(0x2000, vec![0xab; 256]);
        assert_eq!(outbound.len(), 3);
        // seq 从 0 起，alloc 返回当前再 +1
        assert_eq!(outbound[0].seq, 0);
        assert_eq!(outbound[1].seq, 1);
        assert_eq!(outbound[2].seq, 2);
        // 同样 tokens 从 0 起
        assert_eq!(t1, 0);
        assert_eq!(t2, 1);
    }

    /// **Phase N1** — fire_interrupt 生成 InterruptFire body，msix_index 透传。
    #[test]
    fn device_ctx_fire_interrupt_body() {
        let mut outbound = Vec::new();
        let mut seq = 0u64;
        let mut tok = 0u64;
        let mut ctx = crate::DeviceCtx {
            outbound: &mut outbound,
            next_seq: &mut seq,
            next_dma_token: &mut tok,
        };
        ctx.fire_interrupt(3);
        let msg = &outbound[0];
        match msg.body.as_ref().unwrap() {
            Body::InterruptFire(ifire) => assert_eq!(ifire.msix_index, 3),
            _ => panic!("expected InterruptFire body"),
        }
    }

    /// **Phase N1** — dma_read 生成 ReadGpaRequest with token + gpa + len 一致。
    #[test]
    fn device_ctx_dma_read_body() {
        let mut outbound = Vec::new();
        let mut seq = 0u64;
        let mut tok = 0u64;
        let mut ctx = crate::DeviceCtx {
            outbound: &mut outbound,
            next_seq: &mut seq,
            next_dma_token: &mut tok,
        };
        let t = ctx.dma_read(0xdead_beef, 8192);
        match outbound[0].body.as_ref().unwrap() {
            Body::ReadGpa(r) => {
                assert_eq!(r.token, t);
                assert_eq!(r.gpa, 0xdead_beef);
                assert_eq!(r.len, 8192);
            }
            _ => panic!("expected ReadGpa body"),
        }
    }

    /// **Phase N1** — dma_write 生成 WriteGpaRequest with data 透传。
    #[test]
    fn device_ctx_dma_write_body() {
        let mut outbound = Vec::new();
        let mut seq = 0u64;
        let mut tok = 0u64;
        let mut ctx = crate::DeviceCtx {
            outbound: &mut outbound,
            next_seq: &mut seq,
            next_dma_token: &mut tok,
        };
        let payload = vec![1, 2, 3, 4, 5];
        let t = ctx.dma_write(0xcafe_babe, payload.clone());
        match outbound[0].body.as_ref().unwrap() {
            Body::WriteGpa(w) => {
                assert_eq!(w.token, t);
                assert_eq!(w.gpa, 0xcafe_babe);
                assert_eq!(w.data, payload);
            }
            _ => panic!("expected WriteGpa body"),
        }
    }

    /// **Phase N1b** — Test harness: capture device callbacks for inbound assertions.
    #[derive(Default)]
    struct CaptureDevice {
        last_mmio_read: Option<(u32, u64, u32)>,
        mmio_read_return: u64,
        last_mmio_write: Option<(u32, u64, u32, u64)>,
        last_cfg: Option<(u32, u32)>,
        last_reset: Option<u32>,
        last_dma: Option<(u64, bool, Vec<u8>)>,
    }

    impl crate::PcieDevice for CaptureDevice {
        fn describe(&self) -> pcie_remote_protocol::DeviceDescribe {
            pcie_remote_protocol::DeviceDescribe::default()
        }
        fn mmio_read(&mut self, bar: u32, offset: u64, size: u32) -> u64 {
            self.last_mmio_read = Some((bar, offset, size));
            self.mmio_read_return
        }
        fn mmio_write(
            &mut self,
            _ctx: &mut crate::DeviceCtx<'_>,
            bar: u32,
            offset: u64,
            size: u32,
            value: u64,
        ) {
            self.last_mmio_write = Some((bar, offset, size, value));
        }
        fn cfg_write_side_effect(&mut self, offset: u32, value: u32) {
            self.last_cfg = Some((offset, value));
        }
        fn reset(&mut self, kind: u32) {
            self.last_reset = Some(kind);
        }
        fn on_dma_complete(
            &mut self,
            _ctx: &mut crate::DeviceCtx<'_>,
            token: u64,
            ok: bool,
            data: Vec<u8>,
        ) {
            self.last_dma = Some((token, ok, data));
        }
    }

    fn make_req(seq: u64, body: HostBody) -> ToHost {
        ToHost {
            seq,
            body: Some(body),
        }
    }

    /// **Phase N1b** — MmioRead inbound 经过 mask 后返回；调用 device.mmio_read。
    #[test]
    fn dispatch_inbound_mmio_read_masks_value() {
        let mut dev = CaptureDevice::default();
        dev.mmio_read_return = 0xDEAD_BEEF_CAFE_BABE;
        let mut out = Vec::new();
        let mut seq = 0u64;
        let mut tok = 0u64;
        let req = make_req(
            42,
            HostBody::MmioRead(pcie_remote_protocol::MmioAccess {
                bar: 0,
                offset: 0x10,
                size: 2, // 16-bit
                value: 0,
            }),
        );
        super::dispatch_inbound(&mut dev, req, &mut out, &mut seq, &mut tok).unwrap();
        // 设备被调用且参数透传
        assert_eq!(dev.last_mmio_read, Some((0, 0x10, 2)));
        assert_eq!(out.len(), 1);
        // outbound seq = 入站 seq
        assert_eq!(out[0].seq, 42);
        match out[0].body.as_ref().unwrap() {
            Body::MmioReadResult(r) => assert_eq!(r.value, 0xBABE), // mask 到低 16 位
            _ => panic!("expected MmioReadResult"),
        }
    }

    /// **Phase N1b** — MmioRead 非法 size 触发 SDK 防御性拒绝（dispatch 返 Err）。
    #[test]
    fn dispatch_inbound_mmio_read_rejects_bad_size() {
        let mut dev = CaptureDevice::default();
        let mut out = Vec::new();
        let mut seq = 0u64;
        let mut tok = 0u64;
        let req = make_req(
            1,
            HostBody::MmioRead(pcie_remote_protocol::MmioAccess {
                bar: 0,
                offset: 0,
                size: 3, // 非法（only 1/2/4/8）
                value: 0,
            }),
        );
        let r = super::dispatch_inbound(&mut dev, req, &mut out, &mut seq, &mut tok);
        assert!(r.is_err());
        // 设备未被调用
        assert_eq!(dev.last_mmio_read, None);
        assert!(out.is_empty());
    }

    /// **Phase N1b** — MmioWrite 入站只调用 device.mmio_write，无 outbound。
    #[test]
    fn dispatch_inbound_mmio_write_no_outbound() {
        let mut dev = CaptureDevice::default();
        let mut out = Vec::new();
        let mut seq = 0u64;
        let mut tok = 0u64;
        let req = make_req(
            7,
            HostBody::MmioWrite(pcie_remote_protocol::MmioAccess {
                bar: 1,
                offset: 0x1000,
                size: 4,
                value: 0xCAFEBABE,
            }),
        );
        super::dispatch_inbound(&mut dev, req, &mut out, &mut seq, &mut tok).unwrap();
        assert_eq!(dev.last_mmio_write, Some((1, 0x1000, 4, 0xCAFEBABE)));
        assert!(out.is_empty()); // MMIO Write 无 response
    }

    /// **Phase N1b** — CfgWriteSideEffect 路由到 device。
    #[test]
    fn dispatch_inbound_cfg_write_side_effect() {
        let mut dev = CaptureDevice::default();
        let mut out = Vec::new();
        let mut seq = 0u64;
        let mut tok = 0u64;
        let req = make_req(
            5,
            HostBody::CfgWriteSideEffect(pcie_remote_protocol::CfgAccess {
                offset: 0x04,
                size: 4,
                value: 0x0006_0000, // PCI_COMMAND
            }),
        );
        super::dispatch_inbound(&mut dev, req, &mut out, &mut seq, &mut tok).unwrap();
        assert_eq!(dev.last_cfg, Some((0x04, 0x0006_0000)));
    }

    /// **Phase N1b** — Reset 路由 kind 给 device。
    #[test]
    fn dispatch_inbound_reset() {
        let mut dev = CaptureDevice::default();
        let mut out = Vec::new();
        let mut seq = 0u64;
        let mut tok = 0u64;
        let req = make_req(9, HostBody::Reset(pcie_remote_protocol::Reset { kind: 2 }));
        super::dispatch_inbound(&mut dev, req, &mut out, &mut seq, &mut tok).unwrap();
        assert_eq!(dev.last_reset, Some(2));
    }

    /// **Phase N1b** — DmaCompletion 透传 (token, ok, data) 给 device。
    #[test]
    fn dispatch_inbound_dma_completion() {
        let mut dev = CaptureDevice::default();
        let mut out = Vec::new();
        let mut seq = 0u64;
        let mut tok = 0u64;
        let req = make_req(
            11,
            HostBody::DmaCompletion(pcie_remote_protocol::DmaCompletion {
                token: 0xAA,
                ok: true,
                data: vec![0x11, 0x22, 0x33],
            }),
        );
        super::dispatch_inbound(&mut dev, req, &mut out, &mut seq, &mut tok).unwrap();
        assert_eq!(dev.last_dma, Some((0xAA, true, vec![0x11, 0x22, 0x33])));
    }

    /// **Phase N1b** — DmaCompletion ok=false 时 data 也应原样透传（教学：
    /// device 可能用 data 长度或 magic byte 判别）。
    #[test]
    fn dispatch_inbound_dma_completion_failure() {
        let mut dev = CaptureDevice::default();
        let mut out = Vec::new();
        let mut seq = 0u64;
        let mut tok = 0u64;
        let req = make_req(
            13,
            HostBody::DmaCompletion(pcie_remote_protocol::DmaCompletion {
                token: 0xBB,
                ok: false,
                data: vec![],
            }),
        );
        super::dispatch_inbound(&mut dev, req, &mut out, &mut seq, &mut tok).unwrap();
        assert_eq!(dev.last_dma, Some((0xBB, false, vec![])));
    }

    /// **Phase N1b** — req.body=None 不 crash，warn + 跳过。
    #[test]
    fn dispatch_inbound_empty_body_is_ignored() {
        let mut dev = CaptureDevice::default();
        let mut out = Vec::new();
        let mut seq = 0u64;
        let mut tok = 0u64;
        let req = ToHost { seq: 0, body: None };
        let r = super::dispatch_inbound(&mut dev, req, &mut out, &mut seq, &mut tok);
        assert!(r.is_ok());
        assert!(out.is_empty());
    }
}
