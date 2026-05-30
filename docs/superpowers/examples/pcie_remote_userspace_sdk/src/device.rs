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
/// - `tick(ctx)` 每 5 秒一次（可配置），用于驱动 DMA / interrupt / 后台 IO
///   等不由 guest MMIO 触发的工作。
pub trait PcieDevice: 'static {
    /// 设备描述：vendor/device ID、class、BAR 布局、MSI-X 向量数等。
    /// 在 handshake 阶段调用一次。
    fn describe(&self) -> DeviceDescribe;

    /// guest 对 cfg space 中带 side-effect 的 offset 做了一次 32 位写。
    /// `cfg_write_side_effect_offsets` 在 `describe()` 中声明的子集。
    ///
    /// 用户不需要也**不能**回值：cfg 写在 OpenHCL 一侧已 `ConfigSpaceType0Emulator`
    /// 处理；这只是 host-side 观察事件。
    fn cfg_write_side_effect(&mut self, offset: u32, value: u32) {
        let _ = (offset, value);
    }

    /// guest 读 BAR 的 MMIO 区域。size ∈ {1, 2, 4, 8}。返回值在低 `size*8` 位有效。
    fn mmio_read(&mut self, bar: u32, offset: u64, size: u32) -> u64;

    /// guest 写 BAR 的 MMIO 区域。`value` 在低 `size*8` 位有效。
    fn mmio_write(&mut self, bar: u32, offset: u64, size: u32, value: u64);

    /// guest 触发了 PCIe 复位（kind 区分 FLR / WARM / COLD 等，目前协议都
    /// 是 1 = generic reset）。用户实现应清零所有 controller-state machines。
    fn reset(&mut self, kind: u32) {
        let _ = kind;
    }

    /// 周期性 tick。SDK 默认 5s 调一次；用户可在这里通过 `ctx` 主动发
    /// `fire_interrupt` / `dma_read` / `dma_write` 推动 IO。
    fn tick(&mut self, ctx: &mut DeviceCtx<'_>) {
        let _ = ctx;
    }
}

/// 设备实现可调用的上下文。挂载在 `mmio_read` / `mmio_write` / `tick` 等
/// 调用栈上，允许 device 反过来给 guest 触发中断 / 启动 DMA。
///
/// **设计选择**：DMA 是 async 的（需要 RTT 到 OpenHCL），但 PcieDevice 方法
/// 是同步的。所以 DeviceCtx 的 dma 方法走"入队 → 后续异步 flush"模型：
/// 调用 `dma_read` 把请求入队 + 返回 token；用户在后续 tick 或 mmio
/// 回调中通过 `ctx.poll_completions()` 取异步完成的结果。
///
/// 简化起见，**v1 的 SDK 只支持 fire-and-forget DMA**（用户 push 完不查
/// 结果）。如果 NVMe / virtio-blk 等需要等 DMA 完成才发 interrupt，
/// 可走 "DMA → 收到 DmaCompletion → 主循环回调 PcieDevice::on_dma_complete"
/// 模式 — v2 扩展。
pub struct DeviceCtx<'a> {
    pub(crate) outbound: &'a mut Vec<pcie_remote_protocol::ToOpenhcl>,
    pub(crate) next_seq: &'a mut u64,
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

    /// 异步从 guest memory 读 `len` 字节。fire-and-forget；不返回数据。
    /// 用于"我只想触发 OpenHCL DMA 路径"的诊断模式 — NVMe 等真用 DMA
    /// 的场景应走 v2 完成回调 API。
    pub fn dma_read_fire_and_forget(&mut self, gpa: u64, len: u32) {
        use pcie_remote_protocol::ReadGpaRequest;
        use pcie_remote_protocol::ToOpenhcl;
        use pcie_remote_protocol::to_openhcl::Body;

        let seq = self.alloc_seq();
        let token = seq; // token 在 v1 SDK 中复用 seq 便于排查
        self.outbound.push(ToOpenhcl {
            seq,
            body: Some(Body::ReadGpa(ReadGpaRequest { token, gpa, len })),
        });
    }

    /// 异步写 `data` 到 guest memory @ `gpa`。fire-and-forget。
    pub fn dma_write_fire_and_forget(&mut self, gpa: u64, data: Vec<u8>) {
        use pcie_remote_protocol::ToOpenhcl;
        use pcie_remote_protocol::WriteGpaRequest;
        use pcie_remote_protocol::to_openhcl::Body;

        let seq = self.alloc_seq();
        let token = seq;
        self.outbound.push(ToOpenhcl {
            seq,
            body: Some(Body::WriteGpa(WriteGpaRequest { token, gpa, data })),
        });
    }

    fn alloc_seq(&mut self) -> u64 {
        let s = *self.next_seq;
        *self.next_seq = self.next_seq.wrapping_add(1);
        s
    }
}
