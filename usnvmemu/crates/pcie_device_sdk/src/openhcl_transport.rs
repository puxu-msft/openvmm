// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase T** — pcie_remote 协议的 [`Transport`](crate::Transport) 实现。
//!
//! 持有出站帧 buffer + seq/token 分配器，把 [`crate::DeviceCtx`] 的
//! `fire_interrupt`/`dma_read`/`dma_write` 序列化成 protobuf
//! [`ToOpenhcl`](pcie_remote_protocol::ToOpenhcl) 消息。SDK 主循环
//! ([`crate::run`]) drain buffer 写到 wire；其他 backend（vfio-user /
//! NVMe-oF TCP）会有自己的 `impl Transport`。

use crate::Transport;
use pcie_remote_protocol::InterruptFire;
use pcie_remote_protocol::MmioReadResult;
use pcie_remote_protocol::ReadGpaRequest;
use pcie_remote_protocol::ToOpenhcl;
use pcie_remote_protocol::WriteGpaRequest;
use pcie_remote_protocol::to_openhcl::Body;

/// pcie_remote 协议 backend。两种构造方式：
///
/// 1. [`OpenhclVsockTransport::new`]：自带 owned buffer，SDK [`crate::run`]
///    主循环每 iter 调 [`Self::drain`] 把 buffer flush 到 wire。
/// 2. [`OpenhclVsockTransport::with_buffers`]：借用外部 buffer + seq/token
///    分配器，配 [`crate::DeviceCtx::new`] 用于 adapter 的 wire 单测 —— 调用方
///    在 ctx 借用结束后直接读 outbound vec 断言 protobuf 帧（见 run.rs 的
///    `device_ctx_*_body` 测试）。
pub struct OpenhclVsockTransport<'a> {
    outbound: BufferRef<'a>,
    next_seq: U64Ref<'a>,
    next_dma_token: U64Ref<'a>,
}

/// outbound 帧 buffer：可拥有也可借用（测试场景）。
enum BufferRef<'a> {
    Owned(Vec<ToOpenhcl>),
    Borrowed(&'a mut Vec<ToOpenhcl>),
}

impl BufferRef<'_> {
    fn push(&mut self, msg: ToOpenhcl) {
        match self {
            Self::Owned(v) => v.push(msg),
            Self::Borrowed(v) => v.push(msg),
        }
    }

    fn drain_into(&mut self, sink: &mut Vec<ToOpenhcl>) {
        match self {
            Self::Owned(v) => sink.append(v),
            Self::Borrowed(v) => sink.append(v),
        }
    }
}

/// `u64` 分配器：owned 用 inline 字段；borrowed 用外部 `&mut u64`。
enum U64Ref<'a> {
    Owned(u64),
    Borrowed(&'a mut u64),
}

impl U64Ref<'_> {
    /// 取当前值，自增 1，返回原值。
    fn alloc(&mut self) -> u64 {
        match self {
            Self::Owned(x) => {
                let cur = *x;
                *x = x.wrapping_add(1);
                cur
            }
            Self::Borrowed(x) => {
                let cur = **x;
                **x = x.wrapping_add(1);
                cur
            }
        }
    }
}

impl OpenhclVsockTransport<'_> {
    /// SDK 主循环用的构造：owned buffer + 默认 seq=1<<32 / dma_token=1<<40
    /// （避免与 OpenHCL 侧 seq 撞，便于日志区分）。
    pub fn new() -> Self {
        Self {
            outbound: BufferRef::Owned(Vec::with_capacity(16)),
            next_seq: U64Ref::Owned(1u64 << 32),
            next_dma_token: U64Ref::Owned(1u64 << 40),
        }
    }

    /// drain 当前 outbound buffer 进 `sink`，buffer **保留**自身 capacity
    /// （`Vec::append` 移动 elements 但保留分配），主循环逐 iter 复用，
    /// 避免每次 flush 都 realloc 16-cap Vec。
    pub fn drain(&mut self, sink: &mut Vec<ToOpenhcl>) {
        self.outbound.drain_into(sink);
    }

    /// 投递一条 `MmioReadResult`，**复用 inbound seq**（OpenHCL 侧按 seq
    /// 匹配请求 ↔ reply）。SDK 主循环 [`crate::run`] 处理 MmioRead 入站时
    /// 调用；其他场景不应使用 — `dma_*` / `fire_interrupt` 都会自动 alloc
    /// 新 seq。
    pub fn push_mmio_read_result(&mut self, inbound_seq: u64, result: MmioReadResult) {
        self.outbound.push(ToOpenhcl {
            seq: inbound_seq,
            body: Some(Body::MmioReadResult(result)),
        });
    }
}

impl Default for OpenhclVsockTransport<'_> {
    /// 同 [`Self::new`]。
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> OpenhclVsockTransport<'a> {
    /// 测试用构造：把 outbound buffer + seq/token 分配器外置，让
    /// 调用方读 outbound vec 做断言。
    pub fn with_buffers(
        outbound: &'a mut Vec<ToOpenhcl>,
        next_seq: &'a mut u64,
        next_dma_token: &'a mut u64,
    ) -> Self {
        Self {
            outbound: BufferRef::Borrowed(outbound),
            next_seq: U64Ref::Borrowed(next_seq),
            next_dma_token: U64Ref::Borrowed(next_dma_token),
        }
    }
}

impl Transport for OpenhclVsockTransport<'_> {
    fn fire_interrupt(&mut self, msix_index: u32) {
        let seq = self.next_seq.alloc();
        self.outbound.push(ToOpenhcl {
            seq,
            body: Some(Body::InterruptFire(InterruptFire { msix_index })),
        });
    }

    fn dma_read(&mut self, gpa: u64, len: u32) -> u64 {
        let token = self.next_dma_token.alloc();
        let seq = self.next_seq.alloc();
        self.outbound.push(ToOpenhcl {
            seq,
            body: Some(Body::ReadGpa(ReadGpaRequest { token, gpa, len })),
        });
        token
    }

    fn dma_write(&mut self, gpa: u64, data: Vec<u8>) -> u64 {
        let token = self.next_dma_token.alloc();
        let seq = self.next_seq.alloc();
        self.outbound.push(ToOpenhcl {
            seq,
            body: Some(Body::WriteGpa(WriteGpaRequest { token, gpa, data })),
        });
        token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Transport` 必须保持 object-safe —— 否则 `DeviceCtx` 的
    /// `&mut dyn Transport` 字段编译失败，整个 Phase T 抽象崩塌。
    #[test]
    fn transport_is_object_safe() {
        let mut t = OpenhclVsockTransport::new();
        let _dyn_ref: &mut dyn Transport = &mut t;
    }

    /// seq / token 分配单调递增，wrap_around 不 panic。
    #[test]
    fn alloc_monotonic_and_wraps() {
        let mut t = OpenhclVsockTransport::new();
        let s0 = t.next_seq.alloc();
        let s1 = t.next_seq.alloc();
        assert_eq!(s1, s0.wrapping_add(1));
        t.next_seq = U64Ref::Owned(u64::MAX);
        let s_max = t.next_seq.alloc();
        let s_wrap = t.next_seq.alloc();
        assert_eq!(s_max, u64::MAX);
        assert_eq!(s_wrap, 0);
    }

    /// `dma_read` push 一条 ReadGpa；token 与 alloc 顺序一致。
    #[test]
    fn dma_read_pushes_readgpa_with_alloc_token() {
        let mut outbound = Vec::new();
        let mut seq = 0u64;
        let mut tok = 0u64;
        let mut t = OpenhclVsockTransport::with_buffers(&mut outbound, &mut seq, &mut tok);
        let returned = t.dma_read(0xCAFE, 4096);
        assert_eq!(returned, 0);
        assert_eq!(outbound.len(), 1);
        match outbound[0].body.as_ref().unwrap() {
            Body::ReadGpa(r) => {
                assert_eq!(r.token, 0);
                assert_eq!(r.gpa, 0xCAFE);
                assert_eq!(r.len, 4096);
            }
            _ => panic!("expected ReadGpa"),
        }
    }

    /// **review M3** — owned (`new`) 与 borrowed (`with_buffers`) 两条构造
    /// 路径必须产出 byte-equal 序列；否则 OpenHCL 侧无法区分。给两边喂
    /// 一样的调用序列 + 用 `1<<32` / `1<<40` 起点，drain 后直接 assert_eq。
    #[test]
    fn owned_and_borrowed_paths_produce_identical_bytes() {
        // owned 路径：默认起点 (1<<32, 1<<40)
        let mut t_owned = OpenhclVsockTransport::new();
        t_owned.fire_interrupt(7);
        let _ = t_owned.dma_read(0xCAFE, 4096);
        let _ = t_owned.dma_write(0xBEEF, vec![0xab; 16]);
        let mut owned_out = Vec::new();
        t_owned.drain(&mut owned_out);

        // borrowed 路径：手动设同样起点
        let mut outbound = Vec::new();
        let mut seq = 1u64 << 32;
        let mut tok = 1u64 << 40;
        {
            let mut t_borrowed =
                OpenhclVsockTransport::with_buffers(&mut outbound, &mut seq, &mut tok);
            t_borrowed.fire_interrupt(7);
            let _ = t_borrowed.dma_read(0xCAFE, 4096);
            let _ = t_borrowed.dma_write(0xBEEF, vec![0xab; 16]);
        }

        assert_eq!(
            owned_out, outbound,
            "owned vs borrowed transport 必须产生 byte-equal 出站序列"
        );
    }

    /// **review L6** — `push_mmio_read_result` 复用 inbound seq，且不消耗
    /// `next_seq`（避免 alloc 与 inbound seq 撞）。
    #[test]
    fn push_mmio_read_result_reuses_inbound_seq() {
        let mut t = OpenhclVsockTransport::new();
        let seq_before = match &t.next_seq {
            U64Ref::Owned(s) => *s,
            U64Ref::Borrowed(_) => unreachable!(),
        };
        t.push_mmio_read_result(0xDEAD, MmioReadResult { value: 0xBABE });
        let seq_after = match &t.next_seq {
            U64Ref::Owned(s) => *s,
            U64Ref::Borrowed(_) => unreachable!(),
        };
        assert_eq!(seq_before, seq_after, "MmioReadResult 不应分配新 seq");
        let mut out = Vec::new();
        t.drain(&mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seq, 0xDEAD);
    }
}
