// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase W2** — 中立测试 transport：把 device 经 [`crate::DeviceCtx`] 发出的
//! outbound 原语（interrupt / dma_read / dma_write）记录成 [`TransportEvent`]，
//! 让下游 crate 的单测**无需任何 wire 编码 / live socket** 即可断言 device 行为。
//!
//! 取代 Phase T 的 `DeviceCtx::for_testing`（耦合 openhcl `ToOpenhcl` wire 类型）。
//! core 不依赖任何 wire crate，所以测试断言改为对中立事件，而非 protobuf body。

use crate::Transport;

/// device 经 `DeviceCtx` 发出的一条 outbound 原语（中立，无 wire 编码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEvent {
    /// `fire_interrupt(msix_index)`。
    FireInterrupt {
        /// MSI-X 向量 index。
        msix_index: u32,
    },
    /// `dma_read(gpa, len)` — 返回的 token 一并记录。
    DmaRead {
        /// 分配给本次 DMA 的 token。
        token: u64,
        /// guest 物理地址。
        gpa: u64,
        /// 字节数。
        len: u32,
    },
    /// `dma_write(gpa, data)` — 返回的 token 一并记录。
    DmaWrite {
        /// 分配给本次 DMA 的 token。
        token: u64,
        /// guest 物理地址。
        gpa: u64,
        /// 写入的字节。
        data: Vec<u8>,
    },
}

/// 记录所有 outbound 原语的测试 [`Transport`]。
///
/// 用法：`let mut cap = CaptureTransport::new(); let mut ctx = DeviceCtx::new(&mut cap);`
/// 驱动 device 后读 [`CaptureTransport::events`] 断言。
pub struct CaptureTransport {
    events: Vec<TransportEvent>,
    next_token: u64,
}

impl CaptureTransport {
    /// 默认 token 起点 `1<<40`（与 `PcieRemoteTransport::new` 的 owned 默认
    /// 一致，便于日志区分；token 值不保证跨 transport 可比）。
    pub fn new() -> Self {
        Self::with_start_token(1u64 << 40)
    }

    /// 自定 token 起点 —— 测试需要断言具体 token 值时用。
    pub fn with_start_token(start: u64) -> Self {
        Self {
            events: Vec::new(),
            next_token: start,
        }
    }

    /// 已记录的 outbound 事件（按发生顺序）。
    pub fn events(&self) -> &[TransportEvent] {
        &self.events
    }

    /// 清空已记录事件（token 计数器保留单调，便于分段断言）。
    pub fn clear(&mut self) {
        self.events.clear();
    }

    /// 分配一个单调递增 token。
    fn alloc(&mut self) -> u64 {
        let t = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);
        t
    }
}

impl Default for CaptureTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for CaptureTransport {
    fn fire_interrupt(&mut self, msix_index: u32) {
        self.events
            .push(TransportEvent::FireInterrupt { msix_index });
    }

    fn dma_read(&mut self, gpa: u64, len: u32) -> u64 {
        let token = self.alloc();
        self.events
            .push(TransportEvent::DmaRead { token, gpa, len });
        token
    }

    fn dma_write(&mut self, gpa: u64, data: Vec<u8>) -> u64 {
        let token = self.alloc();
        self.events
            .push(TransportEvent::DmaWrite { token, gpa, data });
        token
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeviceCtx;

    /// object-safe：必须能装进 `&mut dyn Transport`（否则 DeviceCtx 无法持有）。
    #[test]
    fn capture_is_object_safe() {
        let mut t = CaptureTransport::new();
        let _dyn: &mut dyn Transport = &mut t;
    }

    /// 经 DeviceCtx 发原语 → 按序记录成中立事件，token 单调递增。
    #[test]
    fn records_events_via_ctx_in_order() {
        let mut cap = CaptureTransport::with_start_token(0x1000);
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            ctx.fire_interrupt(7);
            let t1 = ctx.dma_read(0xCAFE, 4096);
            let t2 = ctx.dma_write(0xBEEF, vec![0xAB; 16]);
            assert_eq!(t1, 0x1000);
            assert_eq!(t2, 0x1001);
        }
        assert_eq!(
            cap.events(),
            &[
                TransportEvent::FireInterrupt { msix_index: 7 },
                TransportEvent::DmaRead {
                    token: 0x1000,
                    gpa: 0xCAFE,
                    len: 4096,
                },
                TransportEvent::DmaWrite {
                    token: 0x1001,
                    gpa: 0xBEEF,
                    data: vec![0xAB; 16],
                },
            ]
        );
    }

    /// fire-and-forget 变体也记录（token 内部分配，不暴露给 caller）。
    #[test]
    fn fire_and_forget_still_recorded() {
        let mut cap = CaptureTransport::new();
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            ctx.dma_read_fire_and_forget(0x10, 8);
            ctx.dma_write_fire_and_forget(0x20, vec![1, 2, 3]);
        }
        assert_eq!(cap.events().len(), 2);
        assert!(matches!(
            cap.events()[0],
            TransportEvent::DmaRead {
                gpa: 0x10,
                len: 8,
                ..
            }
        ));
        assert!(matches!(
            cap.events()[1],
            TransportEvent::DmaWrite { gpa: 0x20, .. }
        ));
    }

    /// token wrap 不 panic（防御边界）。
    #[test]
    fn token_wraps_without_panic() {
        let mut cap = CaptureTransport::with_start_token(u64::MAX);
        let t0 = cap.dma_read(0, 1);
        let t1 = cap.dma_read(0, 1);
        assert_eq!(t0, u64::MAX);
        assert_eq!(t1, 0);
    }
}
