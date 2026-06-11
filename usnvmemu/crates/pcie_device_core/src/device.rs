// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 用户实现的 trait + 调用上下文。
//!
//! **Phase T**：`DeviceCtx` 内部由直接持 `Vec<ToOpenhcl>` 改为持
//! `&mut dyn Transport`，让 NVMe controller 等设备实现透明跑在多个
//! backend 之上（pcie_remote vsock / vfio-user / NVMe-oF TCP …）。
//! 公开 API（`fire_interrupt` / `dma_read` / `dma_write` / `*_fire_and_forget`）
//! 完全保持向后兼容，所以 PcieDevice 实现 0 改动。

use crate::describe::DeviceDescribe;

/// **Phase T** — 设备原语 trait：所有 `DeviceCtx` 方法最终落到的"后端"。
///
/// 一个 backend 实现 `Transport` 即可承载任意 [`PcieDevice`]。当前仓库内
/// 有 `PcieRemoteTransport`（pcie_remote 协议，在 `pcie_device_sdk`）与
/// vfio-user / NVMe-oF TCP 的 impl。
///
/// **命名语义**：这里的 `Transport` 指**设备朝 guest 的原语层**（中断注入 +
/// guest 内存 DMA），对齐 NVMe "transport" 语义（controller↔host 的承载：
/// vsock / TCP / vfio-user）。**它不是 PCI bridge / 拓扑元件**。两类 impl
/// 兑现方式不同：
/// - *转发器*（pcie_remote vsock / NVMe-oF TCP）：`dma_read`/`fire_interrupt`
///   字面生成 wire 消息、经传输到另一端，由对端访问 guest 内存——名副其实。
/// - *访问器*（vfio-user）：自己访问 guest 内存（DMA_MAP 带 fd 时 mmap 零拷贝，
///   见 vfio-user backend 的 `DmaBacking`），mmap 命中时**不传输任何东西**；
///   此处 "transport" 取其引申义（设备 I/O 的承载层），非字面 wire 传输。
///
/// device 只依赖统一契约（见 `dma_read` / `on_dma_complete`），不感知自己跑在
/// 转发器还是访问器之上。
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
    ///
    /// **契约（review M-1）**：`describe()` 应在 device 生命周期内**静态**
    /// （多次调用返回等价结果）。adapter 可能缓存其结果（如 vfio-user 据此
    /// 懒构造 PCI config space）；运行期改变 BAR/MSI-X 布局不被支持。复位
    /// （FLR）会重置 config space base/Command 但布局不变。
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
    /// **语义契约（W4 / ADR-010）**：本回调是 *transport 内部的 token 完成通知*，
    /// **不是**跨 transport 对称的"入站事件"。不同 backend 产生它的方式不同：
    /// pcie_remote 是 host 收到一帧真异步 `DmaCompletion` wire 消息；vfio-user
    /// 的 DMA 是 *同步* 往返，adapter 在 wire round-trip 完成后 *合成* 一条完成
    /// 事件投递。device 只依赖"token 终会被回调一次"这个契约，不应假设其底层
    /// 是同步还是异步。
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
/// **Phase W2**：内部直接持 `&mut dyn Transport`（borrowed）。`+ 'a` 让 transport
/// 可承载非 'static 内部借用（如 vfio-user backend 持有 borrowed fd table）。
pub struct DeviceCtx<'a> {
    transport: &'a mut (dyn Transport + 'a),
}

impl<'a> DeviceCtx<'a> {
    /// 包一个 [`Transport`] 实例成 `DeviceCtx`。adapter 主循环用；device 实现
    /// 一般通过 callback 拿到现成的 `&mut DeviceCtx<'_>`，不直接构造。
    pub fn new(transport: &'a mut (dyn Transport + 'a)) -> Self {
        Self { transport }
    }

    fn transport(&mut self) -> &mut dyn Transport {
        self.transport
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
}
