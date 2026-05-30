// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Handshake 完成后的产物。
//!
//! 设计要点（spec §3.3 完整版，v2 重构）：
//! - handshake 完成后 transport 不立即给 worker；存在 prepared 中
//! - worker 在 resolver `assemble_device` 阶段才 spawn（此时拿到 msi_target /
//!   register_mmio / guest_memory，可构造完整 MsixEmulator + Vec<Interrupt>
//!   + 路由 BAR window）
//! - 这样 worker 启动时就持有 `Vec<Interrupt>` 和 `GuestMemory`，
//!   `InterruptFire` 与 `ReadGpa/WriteGpa` DMA 都能闭环
//!
//! Vec<Interrupt> 不需要 `Arc<Mutex<...>>`：单次构造、单 worker 持有、
//! `Interrupt::deliver()` 通过 `&self` 调用 → 直接 by value 移交 worker
//! （遵循 nvme/pci.rs 范式）。

use futures::io::AsyncRead;
use futures::io::AsyncWrite;
use pal_async::socket::PolledSocket;
use pcie_remote_protocol::DeviceDescribe;

/// 类型擦除的双工 async transport。
///
/// `dyn` trait object 让 prepared 可以同时持 TCP（OpenVMM 路径）和 vsock
/// （OpenHCL 路径）— worker.rs 不再需要为每种 transport 单独 monomorphize。
/// 每帧 vtable dispatch ~1ns，相比每帧 protobuf encode + socket syscall
/// （微秒级）可忽略。
pub trait AsyncTransport: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + ?Sized> AsyncTransport for T {}

/// 已 polled、已握手的 transport。box 起来便于 prepared 跨类型存。
pub type BoxedTransport = Box<dyn AsyncTransport>;

/// 把 PolledSocket 包成 BoxedTransport。
pub fn box_transport<S>(polled: PolledSocket<S>) -> BoxedTransport
where
    PolledSocket<S>: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    Box::new(polled)
}

/// Per-instance handshake 完成后的产物。
///
/// resolver 在 `assemble_device` 阶段 take 走全部字段并消耗本结构。
pub struct PreparedPcieRemoteDevice {
    /// host 在 HelloAck 中描述的设备 schema。
    pub describe: DeviceDescribe,
    /// 已 polled、已握手的 transport，等 resolver 取走给 worker。
    pub transport: Option<BoxedTransport>,
}

impl PreparedPcieRemoteDevice {
    /// 从 prepared 中取走 transport。再次调用 panic。
    pub fn take_transport(&mut self) -> BoxedTransport {
        self.transport
            .take()
            .expect("transport already taken (assemble called twice?)")
    }
}
