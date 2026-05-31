// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 用户实现的 trait + 调用上下文。

use pcie_remote_protocol::DeviceDescribe;

/// 用户实现此 trait 即可让 SDK 把它呈现为一个完整 PCIe 设备给 guest。
///
/// 所有方法都在 SDK 的 `run` 主循环单线程内调用，**不需要 `Send + Sync`**，
/// 用户实现可自由用 `RefCell` 等。需要后台任务时由用户自行 spawn。
///
/// # 时序保证
///
/// - `describe` 在 handshake 阶段调用一次。
/// - `cfg_write_side_effect` / `mmio_read` / `mmio_write` 由 guest 触发，
///   可乱序到达；同步返回值即设备响应。
/// - `reset` 是 PCIe FLR / DEVICE 等复位事件；用户应清零 controller state。
/// - `tick(ctx)` 周期性调用，用于驱动 device-initiated 工作（如 DMA / 中断）。
/// - `on_dma_complete(token, ok, data, ctx)` 是先前 `ctx.dma_read` / `dma_write`
///   的异步完成回调，token 与 issue 时返回值匹配，便于 device state machine
///   关联请求。
pub trait PcieDevice: 'static {
    /// 设备描述：vendor/device ID、class、BAR 布局、MSI-X 向量数等。
    /// 在 handshake 阶段调用一次。
    fn describe(&self) -> DeviceDescribe;

    /// guest 对 cfg space 中带 side-effect 的 offset 做了一次 32 位写。
    /// `cfg_write_side_effect_offsets` 在 `describe()` 中声明的子集。
    fn cfg_write_side_effect(&mut self, offset: u32, value: u32) {
        let _ = (offset, value);
    }

    /// guest 读 BAR 的 MMIO 区域。size ∈ {1, 2, 4, 8}。返回值在低 `size*8` 位有效。
    fn mmio_read(&mut self, bar: u32, offset: u64, size: u32) -> u64;

    /// guest 写 BAR 的 MMIO 区域。`value` 在低 `size*8` 位有效。
    ///
    /// 可通过 `ctx` 主动发 `fire_interrupt` / 启动 DMA。
    fn mmio_write(&mut self, ctx: &mut DeviceCtx<'_>, bar: u32, offset: u64, size: u32, value: u64);

    /// guest 触发了 PCIe 复位。用户实现应清零所有 controller-state machines。
    fn reset(&mut self, kind: u32) {
        let _ = kind;
    }

    /// 周期性 tick（由 `RunOptions::tick_interval` 控制）。
    fn tick(&mut self, ctx: &mut DeviceCtx<'_>) {
        let _ = ctx;
    }

    /// 异步 DMA 完成回调。`token` 与发起 `ctx.dma_read` / `ctx.dma_write` 时
    /// 返回值一致，便于 device state machine 关联请求。
    ///
    /// - 对 `dma_read`：`ok=true` 时 `data` 包含从 guest memory 读出的字节；
    ///   `ok=false` 时 `data` 为空（read 失败 / gpa 越界 / rate limit 拒绝）。
    /// - 对 `dma_write`：`data` 始终为空；`ok` 表示写入成功。
    ///
    /// 默认实现 no-op；只发 `dma_*_fire_and_forget` 的设备无需 override。
    fn on_dma_complete(&mut self, ctx: &mut DeviceCtx<'_>, token: u64, ok: bool, data: Vec<u8>) {
        let _ = (ctx, token, ok, data);
    }
}

/// 设备实现可调用的上下文。挂载在 `mmio_write` / `tick` / `on_dma_complete`
/// 调用栈上，允许 device 反过来给 guest 触发中断 / 启动 DMA。
///
/// **设计**：DMA 通过 token 关联请求-响应。`dma_read` / `dma_write` 同步
/// 返回 token（u64），device state machine 把 token 与"这是什么操作"映射
/// （如 PRP fetch / SQ entry fetch / CQ write）；future 由 SDK 主循环
/// 通过 `on_dma_complete` 回调返还。
///
/// 这样 device 实现保持纯同步，不需要 async/await。
pub struct DeviceCtx<'a> {
    pub(crate) outbound: &'a mut Vec<pcie_remote_protocol::ToOpenhcl>,
    pub(crate) next_seq: &'a mut u64,
    pub(crate) next_dma_token: &'a mut u64,
}

impl<'a> DeviceCtx<'a> {
    /// 给 guest 触发 MSI-X 中断（vector index）。fire-and-forget。
    pub fn fire_interrupt(&mut self, msix_index: u32) {
        use pcie_remote_protocol::InterruptFire;
        use pcie_remote_protocol::ToOpenhcl;
        use pcie_remote_protocol::to_openhcl::Body;

        let seq = self.alloc_seq();
        self.outbound.push(ToOpenhcl {
            seq,
            body: Some(Body::InterruptFire(InterruptFire { msix_index })),
        });
    }

    /// 异步从 guest memory 读 `len` 字节。返回 token；SDK 主循环会通过
    /// `PcieDevice::on_dma_complete(token, ok, data, ...)` 回调返回结果。
    ///
    /// `len` ≤ `MAX_DMA_BYTES`（64 KB）；超限的请求 OpenHCL 会以 ok=false 拒绝。
    pub fn dma_read(&mut self, gpa: u64, len: u32) -> u64 {
        use pcie_remote_protocol::ReadGpaRequest;
        use pcie_remote_protocol::ToOpenhcl;
        use pcie_remote_protocol::to_openhcl::Body;

        let token = self.alloc_dma_token();
        let seq = self.alloc_seq();
        self.outbound.push(ToOpenhcl {
            seq,
            body: Some(Body::ReadGpa(ReadGpaRequest { token, gpa, len })),
        });
        token
    }

    /// 异步写 `data` 到 guest memory @ `gpa`。返回 token；完成通过
    /// `on_dma_complete(token, ok, data=空, ...)` 回调（data 为空）。
    pub fn dma_write(&mut self, gpa: u64, data: Vec<u8>) -> u64 {
        use pcie_remote_protocol::ToOpenhcl;
        use pcie_remote_protocol::WriteGpaRequest;
        use pcie_remote_protocol::to_openhcl::Body;

        let token = self.alloc_dma_token();
        let seq = self.alloc_seq();
        self.outbound.push(ToOpenhcl {
            seq,
            body: Some(Body::WriteGpa(WriteGpaRequest { token, gpa, data })),
        });
        token
    }

    /// fire-and-forget 模式：用户**不关心**完成结果。同 `dma_read` 但不
    /// 返回 token（其实仍分配一个走 protocol，仅不暴露给用户）。
    pub fn dma_read_fire_and_forget(&mut self, gpa: u64, len: u32) {
        let _ = self.dma_read(gpa, len);
    }

    /// fire-and-forget 写。
    pub fn dma_write_fire_and_forget(&mut self, gpa: u64, data: Vec<u8>) {
        let _ = self.dma_write(gpa, data);
    }

    fn alloc_seq(&mut self) -> u64 {
        let s = *self.next_seq;
        *self.next_seq = self.next_seq.wrapping_add(1);
        s
    }

    fn alloc_dma_token(&mut self) -> u64 {
        let t = *self.next_dma_token;
        *self.next_dma_token = self.next_dma_token.wrapping_add(1);
        t
    }
}
