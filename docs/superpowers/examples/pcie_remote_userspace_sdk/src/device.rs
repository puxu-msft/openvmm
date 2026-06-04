// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 用户实现的 trait + 调用上下文。
//!
//! **Phase T**：`DeviceCtx` 内部由直接持 `Vec<ToOpenhcl>` 改为持
//! `&mut dyn Transport`，让 NVMe controller 等设备实现透明跑在多个
//! backend 之上（pcie_remote vsock / vfio-user / NVMe-oF TCP …）。
//! 公开 API（`fire_interrupt` / `dma_read` / `dma_write` / `*_fire_and_forget`）
//! 完全保持向后兼容，所以 PcieDevice 实现 0 改动。

use pcie_remote_protocol::DeviceDescribe;

/// **Phase T** — 设备原语 trait：所有 `DeviceCtx` 方法最终落到的"后端"。
///
/// 一个 backend 实现 `Transport` 即可承载任意 [`PcieDevice`]。当前仓库内
/// 有 [`OpenhclVsockTransport`](crate::OpenhclVsockTransport)（pcie_remote
/// 协议）；Phase U/V 会新增 vfio-user 与 NVMe-oF TCP 的 impl。
///
/// 设计要点：
/// - **保持 object-safe**（无 `Self` 返回、无 generic method），允许
///   `DeviceCtx<'_>` 持 `&mut dyn Transport`，避免控制器代码全部加泛型
///   `<T: Transport>`（67 个测试 + ~50 个方法签名稳定）。
/// - DMA 维持 token 模型：`dma_read` / `dma_write` 同步返 `u64` token，
///   完成由主循环异步通过 `PcieDevice::on_dma_complete(token, …)` 回调。
///   token 分配权交给 Transport 实现 — 因为 vsock/vfio-user/TCP 各自有
///   不同的 msg_id ↔ token 映射需求。
pub trait Transport {
    /// 给 guest 触发 MSI-X 中断（向量 index）。fire-and-forget。
    fn fire_interrupt(&mut self, msix_index: u32);

    /// 异步从 guest memory 读 `len` 字节。返 token；完成由主循环
    /// 通过 `PcieDevice::on_dma_complete(token, ok, data)` 回调。
    ///
    /// `len` ≤ `MAX_DMA_BYTES`（64 KiB）；超限由 backend 自行拒绝 / 截断。
    fn dma_read(&mut self, gpa: u64, len: u32) -> u64;

    /// 异步写 `data` 到 guest memory @ `gpa`。返 token；完成通过
    /// `on_dma_complete(token, ok, data=空)` 回调。
    fn dma_write(&mut self, gpa: u64, data: Vec<u8>) -> u64;
}

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
///
/// **Phase T**：内部既可以 `Borrowed(&mut dyn Transport)`（生产路径，由
/// `DeviceCtx::new` 构造）也可以 `Owned(Box<dyn Transport + 'a>)`（仅
/// `for_testing` 路径，让 test 写法保持单行）。两者均通过 `transport()`
/// helper 借出 trait 对象。
pub struct DeviceCtx<'a> {
    inner: CtxInner<'a>,
}

enum CtxInner<'a> {
    // **review M1** — 显式 `+ 'a` 让 borrowed transport 也可承载非 'static
    // 内部借用（如 vfio-user/V backend 可能持有 borrowed fd table）。默认
    // 对象绑定是 'static，会让 Phase U/V 阶段需要时碰壁；现在加上不影响
    // 现有路径（OpenhclVsockTransport::new() 内部全 owned，满足任意 lifetime）。
    Borrowed(&'a mut (dyn Transport + 'a)),
    Owned(Box<dyn Transport + 'a>),
}

impl<'a> DeviceCtx<'a> {
    /// 包一个 [`Transport`] 实例成 `DeviceCtx`。SDK 主循环用；用户实现
    /// 一般通过 callback 拿到现成的 `&mut DeviceCtx<'_>`，不直接构造。
    pub fn new(transport: &'a mut (dyn Transport + 'a)) -> Self {
        Self {
            inner: CtxInner::Borrowed(transport),
        }
    }

    fn transport(&mut self) -> &mut dyn Transport {
        match &mut self.inner {
            CtxInner::Borrowed(t) => *t,
            CtxInner::Owned(t) => t.as_mut(),
        }
    }

    /// 给 guest 触发 MSI-X 中断（vector index）。fire-and-forget。
    pub fn fire_interrupt(&mut self, msix_index: u32) {
        self.transport().fire_interrupt(msix_index);
    }

    /// 异步从 guest memory 读 `len` 字节。返回 token；SDK 主循环会通过
    /// `PcieDevice::on_dma_complete(token, ok, data, ...)` 回调返回结果。
    ///
    /// `len` ≤ `MAX_DMA_BYTES`（64 KB）；超限的请求 OpenHCL 会以 ok=false 拒绝。
    pub fn dma_read(&mut self, gpa: u64, len: u32) -> u64 {
        self.transport().dma_read(gpa, len)
    }

    /// 异步写 `data` 到 guest memory @ `gpa`。返回 token；完成通过
    /// `on_dma_complete(token, ok, data=空, ...)` 回调（data 为空）。
    pub fn dma_write(&mut self, gpa: u64, data: Vec<u8>) -> u64 {
        self.transport().dma_write(gpa, data)
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

    /// **Phase Q10 + 12 轮 M-Q10**（Phase T 重构后）— Test-only 构造器。
    ///
    /// 让外部 crate 的单元测试能直接捕获 device 产生的 DMA/CQE/interrupt 包
    /// （无需 mock SDK transport / live vsock）。
    ///
    /// 三个 `&mut` 参数（outbound / next_seq / next_dma_token）原为
    /// `DeviceCtx` 内部字段；Phase T 后这些字段移到
    /// [`OpenhclVsockTransport`](crate::OpenhclVsockTransport)，但调用方式
    /// 与重构前一致 — 内部把 transport 装在 `CtxInner::Owned(Box<…>)`，
    /// 由 `DeviceCtx` drop 时一并销毁，不需要 caller 再多写一行 `let mut t =`。
    ///
    /// `#[doc(hidden)]` 让此 API 不出现在 cargo doc 公开页（production
    /// SDK 用户不应直接用）；保 `pub` 以便其他 crate 的 `#[cfg(test)]`
    /// 模块能访问。
    #[doc(hidden)]
    pub fn for_testing(
        outbound: &'a mut Vec<pcie_remote_protocol::ToOpenhcl>,
        next_seq: &'a mut u64,
        next_dma_token: &'a mut u64,
    ) -> Self {
        let transport =
            crate::OpenhclVsockTransport::with_buffers(outbound, next_seq, next_dma_token);
        Self {
            inner: CtxInner::Owned(Box::new(transport)),
        }
    }
}
