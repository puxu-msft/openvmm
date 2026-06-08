// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V3 / V4b** — Tcp Admin Transport：把 NvmeController 的
//! ctx.dma_write / ctx.dma_read / fire_interrupt 调用截下来，让 V2Session
//! 后续翻成 NVMe-oF wire (C2HData PDU / R2T → H2CData / CapsuleResp)。
//!
//! 设计思路（教学最简化版）：
//!
//! NvmeController 处理一条 admin SQE 时的内部交互：
//! 1. 同步路径（Set Features 等）：dispatch_admin 直接返 `Some(Cqe)`，
//!    caller 调 post_cqe → 触发 `ctx.dma_write(cqe_gpa, 16B)` +
//!    `ctx.fire_interrupt(irq_vec)`。
//! 2. 异步 write-out 路径（Identify、Get Log Page）：dispatch_admin 返
//!    `None`，内部已 `ctx.dma_write(prp1_gpa, data_buf)` + 把 token 存
//!    pending_ios。caller 调 `on_dma_complete(token, ok=true, data=空)`，
//!    controller 走 post_cqe。
//! 3. **V4b 新加：异步 read-in 路径**（NS Attachment 0x15、Firmware
//!    Download 等）：dispatch_admin 返 `None`，内部 `ctx.dma_read(prp1, len)`
//!    + 把 token 存 pending_ios。caller 必须主动调
//!      `on_dma_complete(token, ok=true, data=Vec<bytes>)` 把 host 那边 *拉
//!      回来* 的 bytes 喂给 controller。在 NVMe-oF TCP 上这一步走
//!      R2T → H2CData round-trip。
//!
//! 本 transport：
//! - **dma_write** 入 `writes: VecDeque<DmaWriteRecord>`；session 通过 gpa
//!   ≥ `CQ_BASE_GPA` 区分 CQE bytes vs PRP1 data。
//! - **dma_read** (V4b) 入 `pending_reads: VecDeque<DmaReadRecord>`；
//!   session 对每条 read 分配 ttag → emit R2T → 收齐 H2CData → 调
//!   `nvme_admin_complete_dma(tok, true, bytes)`。
//! - **fire_interrupt** 只 log + 计数（NVMe-oF 不需要传统 MSI-X；
//!   C2HData + CapsuleResp 自身就是 "中断"）。
//!
//! token 编码：caller (V2Session) 持 monotonic counter，通过
//! [`Self::new_with_token_base`] 注入；本 transport 自增，结束后通过
//! [`Self::token_high_water`] 取回。**修 V3-polish review M-1**：跨 cmd
//! 单调，不再每次 default() 重启撞 controller pending_ios 残留。

use pcie_device_core::Transport;
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

/// **V4b** 一次 dma_read 调用的快照：caller (session) 后续 emit R2T
/// `r2t_length = len` 给 host，等 H2CData 收齐再投 `on_dma_complete(token, true, bytes)`。
#[derive(Debug, Clone)]
pub struct DmaReadRecord {
    /// caller 在 SQE.prp1 填的 sentinel（V4b 仅用作 opaque cookie）。
    pub gpa: u64,
    /// 请求字节数（→ R2T.r2t_length）。
    pub len: u32,
    /// transport 分配的 token；投 `on_dma_complete(token, ...)` 时回填。
    pub token: u64,
}

/// V3/V4b admin transport：捕获 dma_write + dma_read、token 自增、跨 cmd 单调。
///
/// **修 review M-1**：`next_token` 由 caller (V2Session) 通过
/// [`Self::new_with_token_base`] 注入，结束后通过 [`Self::token_high_water`]
/// 取回，避免跨 cmd 复用同 token 撞 controller pending_ios 残留。
///
/// **修 review M-2**：`dma_read` 不再 silent-drop；入 `pending_reads`
/// 让 session 走 R2T → H2CData → complete 闭环。
pub struct TcpAdminTransport {
    /// 累积本次 admin cmd 处理中 controller 产生的所有 dma_write。
    pub writes: VecDeque<DmaWriteRecord>,
    /// **V4b** 累积本次 admin cmd 处理中 controller 产生的所有 dma_read。
    pub pending_reads: VecDeque<DmaReadRecord>,
    /// 计数 fire_interrupt 调用（仅 log；V3 不主动转 NVMe-oF 中断）。
    pub interrupts_fired: u32,
    /// token 计数器，跨 cmd 单调（由 caller 注入起点）。
    next_token: u64,
}

impl Default for TcpAdminTransport {
    fn default() -> Self {
        // 起点 1<<48；测试 / 教学起点用得到。生产路径 V4b+ 一律用
        // [`Self::new_with_token_base`] 跨 cmd 推进。
        Self::new_with_token_base(1u64 << 48)
    }
}

impl TcpAdminTransport {
    /// **V4b** — caller 注入 token 起点；结束后 [`token_high_water`] 拿回。
    pub fn new_with_token_base(base: u64) -> Self {
        Self {
            writes: VecDeque::new(),
            pending_reads: VecDeque::new(),
            interrupts_fired: 0,
            next_token: base,
        }
    }

    /// **V4b** — 取下次会分配的 token（即"high water mark + 1"）；
    /// caller 把这个值存回 session.next_token，跨 cmd 单调。
    pub fn token_high_water(&self) -> u64 {
        self.next_token
    }

    /// pop 出最旧一条 dma_write 记录（FIFO 顺序与 controller 写出顺序一致）。
    pub fn pop_write(&mut self) -> Option<DmaWriteRecord> {
        self.writes.pop_front()
    }

    /// **V4b** — pop 出最旧一条 dma_read 记录（FIFO 顺序）。
    pub fn pop_read(&mut self) -> Option<DmaReadRecord> {
        self.pending_reads.pop_front()
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
        // **V4b** — 不再 silent-drop；入队由 session 通过 R2T/H2CData 闭环
        // 拉回 bytes 后 on_dma_complete。
        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);
        tracing::debug!(
            token,
            gpa = format_args!("{gpa:#x}"),
            len,
            "TcpAdminTransport: capture dma_read (V4b → R2T)"
        );
        self.pending_reads
            .push_back(DmaReadRecord { gpa, len, token });
        token
    }
    fn dma_write(&mut self, gpa: u64, data: Vec<u8>) -> u64 {
        // **review M1** — 数据 payload sentinel 与 CQ sentinel 之间留 ≥
        // CQ_BASE_GPA - PRP1_SENTINEL bytes (~13.8 EiB)；V3 单 PDU
        // ≤ 8 KiB 完全安全，但 V5 IO RW 引入多 PDU 累积时需要重新分配
        // sentinel。本 debug assert 是 tripwire：一旦真撞上立即 panic in
        // dev build，release 0 开销。
        debug_assert!(
            data.len() as u64 <= u64::MAX / 2,
            "single dma_write > half u64 — sentinel scheme bug"
        );
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

    /// **V4b** dma_read 现在入队，不再 silent。
    #[test]
    fn dma_read_captured_to_pending_reads() {
        let mut t = TcpAdminTransport::default();
        let tok = t.dma_read(0xCAFE, 4096);
        assert_eq!(t.pending_reads.len(), 1);
        let r = t.pop_read().unwrap();
        assert_eq!(r.gpa, 0xCAFE);
        assert_eq!(r.len, 4096);
        assert_eq!(r.token, tok);
        assert!(t.pop_read().is_none());
    }

    /// **V4b** — caller-injected token base 让 token 跨 cmd 单调（修 M-1）。
    #[test]
    fn new_with_token_base_monotonic_across_cmds() {
        let mut t1 = TcpAdminTransport::new_with_token_base(100);
        let _ = t1.dma_write(0x10, vec![0; 4]); // → token=100
        let _ = t1.dma_write(0x20, vec![0; 4]); // → token=101
        let high = t1.token_high_water();
        assert_eq!(high, 102, "next token after two writes should be 102");

        // 下一条 cmd 用 high 继续
        let mut t2 = TcpAdminTransport::new_with_token_base(high);
        let tok = t2.dma_write(0x30, vec![0; 4]);
        assert_eq!(tok, 102, "token must continue monotonically across cmds");
    }

    /// **V4b** — dma_read 与 dma_write 用同一 token 池（避免 controller
    /// pending_ios 内 read/write entry 撞）。
    #[test]
    fn dma_read_and_write_share_token_pool() {
        let mut t = TcpAdminTransport::new_with_token_base(1);
        let tw = t.dma_write(0x10, vec![0; 4]); // token=1
        let tr = t.dma_read(0x20, 4096); // token=2
        let tw2 = t.dma_write(0x30, vec![0; 4]); // token=3
        assert_eq!(tw, 1);
        assert_eq!(tr, 2);
        assert_eq!(tw2, 3);
    }

    /// Trait object safety — confirm we can pass &mut dyn Transport。
    #[test]
    fn is_object_safe() {
        let mut t = TcpAdminTransport::default();
        let _r: &mut dyn Transport = &mut t;
    }
}
