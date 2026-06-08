// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase U3 占位** — `NoopTransport`：实现 [`Transport`] 但所有方法返
//! 空 / 0，仅给 `mmio_write` 等需要 `DeviceCtx<'_>` 的回调用临时占位。
//!
//! Phase U4 加 `VfioUserTransport` 真实现：dma_read/write 经 DMA_READ/WRITE
//! S→C 命令拿数据；fire_interrupt 写 eventfd。

use pcie_device_core::Transport;

/// 不做任何事的 [`Transport`]。任何 dma_read/dma_write 返 token=0；
/// fire_interrupt 静默丢弃。
///
/// **教学注意**：U3 阶段 session 的 `mmio_write` handler 给设备的 DeviceCtx
/// 用这个 stub 是 *安全* 的，因为单测 MockDev 不调 `ctx.dma_*` / `fire_*`；
/// 真 NVMe controller 在 U4/U5 接通后会用 `VfioUserTransport` 替代。
pub struct NoopTransport;

impl Transport for NoopTransport {
    fn fire_interrupt(&mut self, msix_index: u32) {
        tracing::warn!(
            msix_index,
            "NoopTransport.fire_interrupt called — Phase U5 will replace with eventfd write"
        );
    }
    fn dma_read(&mut self, gpa: u64, len: u32) -> u64 {
        tracing::warn!(
            gpa = format_args!("{gpa:#x}"),
            len,
            "NoopTransport.dma_read called — Phase U4 will replace with vfio-user DMA_READ"
        );
        0
    }
    fn dma_write(&mut self, gpa: u64, _data: Vec<u8>) -> u64 {
        tracing::warn!(
            gpa = format_args!("{gpa:#x}"),
            "NoopTransport.dma_write called — Phase U4 will replace with vfio-user DMA_WRITE"
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
