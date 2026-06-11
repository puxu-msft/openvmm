// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `NoopTransport` —— no-op [`Transport`] 桩：所有方法返空 / 0。
//!
//! **现状**：生产路径**不**用它。vfio-user 的真 Transport 由
//! [`VfioUserSession`](crate::VfioUserSession) 自身实现 —— `dma_read`/`dma_write`
//! 走 DMA_READ/WRITE S→C wire（`dma::dma_read_sync` / `dma_write_sync`），
//! `fire_interrupt` 写 MSI-X eventfd（`irq::IrqVectors::fire`）。最初设想的
//! 独立 `VfioUserTransport` 类型并未引入。
//!
//! 本桩仅保留给"设备实现从不调用 `ctx.dma_*` / `fire_*`"的单测 / 示例。

use pcie_device_core::Transport;

/// 不做任何事的 [`Transport`]：`dma_read`/`dma_write` 返 token=0，
/// `fire_interrupt` 静默丢弃。
///
/// 仅用于"设备实现从不调用 `ctx.dma_*` / `fire_*`"的单测 / 示例。生产
/// 路径用 [`VfioUserSession`](crate::VfioUserSession)（它自身 `impl Transport`，
/// DMA/IRQ 走真 wire）。命中本桩的 dma/irq 调用会打 `warn!` 提示用错了
/// Transport。
pub struct NoopTransport;

impl Transport for NoopTransport {
    fn fire_interrupt(&mut self, msix_index: u32) {
        tracing::warn!(
            msix_index,
            "NoopTransport.fire_interrupt called — no-op stub; 生产路径应是 \
             VfioUserSession（写 MSI-X eventfd）"
        );
    }
    fn dma_read(&mut self, gpa: u64, len: u32) -> u64 {
        tracing::warn!(
            gpa = format_args!("{gpa:#x}"),
            len,
            "NoopTransport.dma_read called — no-op stub; 生产路径 VfioUserSession \
             走 vfio-user DMA_READ"
        );
        0
    }
    fn dma_write(&mut self, gpa: u64, _data: Vec<u8>) -> u64 {
        tracing::warn!(
            gpa = format_args!("{gpa:#x}"),
            "NoopTransport.dma_write called — no-op stub; 生产路径 VfioUserSession \
             走 vfio-user DMA_WRITE"
        );
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_returns_zero_token_and_does_not_panic() {
        let mut t: Box<dyn pcie_device_core::Transport> = Box::new(NoopTransport);
        t.fire_interrupt(7);
        assert_eq!(t.dma_read(0xCAFE, 4096), 0);
        assert_eq!(t.dma_write(0xBEEF, vec![1, 2, 3]), 0);
    }

    #[test]
    fn noop_is_object_safe() {
        let mut t = NoopTransport;
        let _dyn_ref: &mut dyn pcie_device_core::Transport = &mut t;
    }
}
