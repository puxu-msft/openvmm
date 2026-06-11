// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `NoopTransport` —— panic 桩 [`Transport`]：任何方法被调用即 panic。
//!
//! 用途：device 实现需要一个 `Transport` ctx，但当前路径**保证不调用**
//! `ctx.dma_*` / `ctx.fire_interrupt`（如 nvme_of_tcp_target V2 的 Property
//! Set / CC.EN 寄存器写路径——只改寄存器状态，不发起 DMA / 中断）。这种
//! 场景用本桩占位。
//!
//! **为何 panic 而非 silent**：本桩曾在 nvme_of_tcp_target 被误接到 IO 路径，
//! 导致 guest doorbell 的 DMA 被静默丢弃（极难发现的 bug）。silent return 0
//! 会抹掉诊断信号；panic 让任何"本不该调用却调用了"的误用立即暴露。合法
//! 用法（从不调用）零开销。
//!
//! 生产数据路径**不**用它：真 Transport 由具体 transport crate 实现
//! （vfio_user_transport 的 `VfioUserSession` 走 vfio-user wire；nvme_of_tcp
//! 的 fabric session 走 R2T/H2CData）。
//!
//! 从 vfio_user_transport 迁来（W0.5，2026-06-11）——解除 nvme_of_tcp_target
//! 仅为 1 个 ZST 拉整个 vfio_user_transport 的反向依赖。panic 而非 tracing
//! 以保持 pcie_device_core 零依赖。

use crate::Transport;

/// panic 桩 [`Transport`]：任何方法被调用即 panic。仅用于"device 从不调用
/// `ctx.dma_*` / `fire_*`"的路径占位 / 单测 / 示例；误接到真 IO 路径会 loud fail。
pub struct NoopTransport;

impl Transport for NoopTransport {
    fn fire_interrupt(&mut self, _msix_index: u32) {
        panic!(
            "NoopTransport.fire_interrupt called — 本桩仅用于从不发起中断的路径；\
             误用了请接真 Transport"
        );
    }
    fn dma_read(&mut self, _gpa: u64, _len: u32) -> u64 {
        panic!(
            "NoopTransport.dma_read called — 本桩仅用于从不发起 DMA 的路径；\
             误用了请接真 Transport（历史上此处 silent-drop 过 guest doorbell DMA）"
        );
    }
    fn dma_write(&mut self, _gpa: u64, _data: Vec<u8>) -> u64 {
        panic!(
            "NoopTransport.dma_write called — 本桩仅用于从不发起 DMA 的路径；\
             误用了请接真 Transport"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_is_object_safe() {
        let mut t = NoopTransport;
        let _dyn_ref: &mut dyn crate::Transport = &mut t;
    }

    #[test]
    #[should_panic(expected = "NoopTransport.dma_read called")]
    fn noop_dma_read_panics_to_catch_misuse() {
        let mut t = NoopTransport;
        let _ = t.dma_read(0xCAFE, 4096);
    }

    #[test]
    #[should_panic(expected = "NoopTransport.fire_interrupt called")]
    fn noop_fire_interrupt_panics_to_catch_misuse() {
        let mut t = NoopTransport;
        t.fire_interrupt(7);
    }
}
