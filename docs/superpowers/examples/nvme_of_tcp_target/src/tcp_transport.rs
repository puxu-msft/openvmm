// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V3** — Tcp Admin Transport：把 NvmeController 的 ctx.dma_write/
//! fire_interrupt 调用截下来，让 V2Session 后续转成 NVMe-oF C2HData PDU +
//! CapsuleResp。
//!
//! 设计思路（教学最简化版）：
//!
//! NvmeController 处理一条 admin SQE 时的内部交互：
//! 1. 同步路径（Set Features 等）：dispatch_admin 直接返 `Some(Cqe)`，
//!    caller 调 post_cqe → 触发 `ctx.dma_write(cqe_gpa, 16B)` +
//!    `ctx.fire_interrupt(irq_vec)`。
//! 2. 异步路径（Identify、Get Log Page）：dispatch_admin 返 `None`，
//!    内部已 `ctx.dma_write(prp1_gpa, data_buf)` + 把 token 存
//!    pending_ios。然后必须由 caller 主动调
//!    `device.on_dma_complete(token, ok=true, data=空)`，controller 才会
//!    继续 post_cqe（再次 dma_write + fire_interrupt）。
//!
//! 本 transport：
//! - 把所有 `dma_write` 写入按 gpa 分类记录（gpa 是 caller — 即 V2Session —
//!   构造 SQE 时填的 sentinel，session 通过比对 gpa 知道是 PRP1 data
//!   还是 CQ entry）。
//! - 对 `dma_read` 返 0 + 即时投递 ok=false 的 completion（V3 不处理
//!   controller 主动 read host mem 的 admin cmd，比如 NS Attachment 的
//!   controller list、Set Features Host Identifier — 这些 V4 SGL R2 加）。
//! - `fire_interrupt` 只 log + 计数（NVMe-oF 不需要传统 MSI-X；C2HData
//!   + CapsuleResp 自身就是 "中断"）。
//!
//! token 编码：直接用 caller 自增计数器（low bit 区分 data write vs
//! cqe write 是 caller 的责任，本 transport 不解释）。

use pcie_remote_userspace_sdk::Transport;
use std::collections::VecDeque;

/// 一次 dma_write 调用的快照：caller 通过 (gpa, data) 判断这是 PRP1 data
/// 还是 CQ entry write。
#[derive(Debug, Clone)]
pub struct DmaWriteRecord {
    /// caller 在 SQE.prp1 / CQ base 等位置填的"GPA"sentinel。
    pub gpa: u64,
    /// 写入字节。
    pub data: Vec<u8>,
    /// 我们分配的 token（返给 NvmeController，让它存 pending_ios）。
    pub token: u64,
}

/// V3 admin transport：捕获 dma_write、token 自增、ctx.dma_read 走错路径。
pub struct TcpAdminTransport {
    /// 累积本次 admin cmd 处理中 controller 产生的所有 dma_write。
    pub writes: VecDeque<DmaWriteRecord>,
    /// 计数 fire_interrupt 调用（仅 log；V3 不主动转 NVMe-oF 中断）。
    pub interrupts_fired: u32,
    /// token 计数器（顶位置 1 便于日志区分）。
    next_token: u64,
}

impl Default for TcpAdminTransport {
    fn default() -> Self {
        Self {
            writes: VecDeque::new(),
            interrupts_fired: 0,
            next_token: 1u64 << 48,
        }
    }
}

impl TcpAdminTransport {
    /// 当前 token 计数器（供测试观测）。
    pub fn peek_next_token(&self) -> u64 {
        self.next_token
    }
    /// pop 出最旧一条 dma_write 记录（FIFO 顺序与 controller 写出顺序一致）。
    pub fn pop_write(&mut self) -> Option<DmaWriteRecord> {
        self.writes.pop_front()
    }
}

impl Transport for TcpAdminTransport {
    fn fire_interrupt(&mut self, msix_index: u32) {
        self.interrupts_fired = self.interrupts_fired.saturating_add(1);
        tracing::debug!(
            msix_index,
            "TcpAdminTransport: fire_interrupt captured (no-op)"
        );
    }
    fn dma_read(&mut self, gpa: u64, len: u32) -> u64 {
        // V3 不处理 controller 主动读 host mem 的 admin 路径。
        // 返非零 token；caller 应在 dispatch 后立即给 on_dma_complete(
        // token, ok=false, vec![]) 让 controller 走 IO-error 清理。
        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);
        tracing::warn!(
            token,
            gpa = format_args!("{gpa:#x}"),
            len,
            "TcpAdminTransport.dma_read: V3 不支持 controller-initiated read; \
             返 token + 等 caller 投 ok=false completion"
        );
        token
    }
    fn dma_write(&mut self, gpa: u64, data: Vec<u8>) -> u64 {
        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);
        tracing::debug!(
            token,
            gpa = format_args!("{gpa:#x}"),
            bytes = data.len(),
            "TcpAdminTransport: capture dma_write"
        );
        self.writes.push_back(DmaWriteRecord { gpa, data, token });
        token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dma_write_records_in_fifo_order() {
        let mut t = TcpAdminTransport::default();
        let tok1 = t.dma_write(0x1000, vec![0xAA; 4]);
        let tok2 = t.dma_write(0x2000, vec![0xBB; 16]);
        assert_eq!(tok2, tok1 + 1);
        let r1 = t.pop_write().unwrap();
        assert_eq!(r1.gpa, 0x1000);
        assert_eq!(r1.data.len(), 4);
        assert_eq!(r1.token, tok1);
        let r2 = t.pop_write().unwrap();
        assert_eq!(r2.gpa, 0x2000);
        assert_eq!(r2.data.len(), 16);
        assert!(t.pop_write().is_none());
    }

    #[test]
    fn fire_interrupt_only_counts() {
        let mut t = TcpAdminTransport::default();
        t.fire_interrupt(0);
        t.fire_interrupt(3);
        t.fire_interrupt(3);
        assert_eq!(t.interrupts_fired, 3);
        assert!(t.writes.is_empty());
    }

    #[test]
    fn dma_read_returns_token_but_logs_warn() {
        let mut t = TcpAdminTransport::default();
        let tok = t.dma_read(0xCAFE, 4096);
        assert_eq!(tok, 1u64 << 48);
        // 不写入 writes
        assert!(t.writes.is_empty());
    }

    /// Trait object safety — confirm we can pass &mut dyn Transport。
    #[test]
    fn is_object_safe() {
        let mut t = TcpAdminTransport::default();
        let _r: &mut dyn Transport = &mut t;
    }
}
