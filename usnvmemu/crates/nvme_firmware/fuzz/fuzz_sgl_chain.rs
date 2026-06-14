// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **target #1 `fuzz_sgl_chain`** —— 结构感知 fuzz：PSDT=10 SGL segment 链 walker。
//!
//! 打 `nvme_firmware` controller 的指针追逐核心（`on_dma_complete` 里的 `NvmSglFetch`
//! 递归段 fetch + `MAX_SGL_SEGMENTS` 自环上限）。harness 内核 = `tests/fuzz_poc_sgl_chain.rs`
//! 已证的**同步 device-side 驱动循环**（公共 API，无 async runtime）：
//!
//!   `nvme_io_dispatch`(SGL READ) → 读 `CaptureTransport` 的 `DmaRead{token,gpa,len}`
//!     → `PcieDevice::on_dma_complete(token, ok, <fuzzer 段页字节>)` → continuation
//!     触发下一 `DmaRead` → 循环直到排空 / 守卫截断。
//!
//! **结构感知**（reviewer 核心洞见）：fuzzer 不喂裸字节，而是经 `arbitrary` 生成有结构的
//! **多步序列**——SGL1 指针 + 每次 fetch 喂回的 segment 页（每页若干 16B descriptor，
//! fuzzer 控 type/sub_type/length/address/对齐 + continuation 拓扑：自环 / 超长链 / 空段 /
//! 非 16 倍数）。比 POC 多覆盖：DMA 失败路径（`ok=false`）+ address-dependent 段页。
//!
//! **oracle**：libfuzzer 自动 catch panic / 超时（无限 fetch）；本 harness 另设
//! `ITER_SAFETY_CAP` 兜底，超限即 `panic!`（守卫失效信号）。**已开 `fuzzing` feature**：
//! drain 后经 `__fuzz_invariants()` 断言 op 表清空（无 op_id 泄漏，CFS 例外）。

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use nvme_firmware::cmd::Sqe;
use pcie_device_core::DeviceCtx;
use xtask_fuzz::fuzz_target;

/// 与 `controller::MAX_SGL_SEGMENTS`(=64) 同思路的安全上限：远超之，用来 catch
/// 「hop guard 失效 → 无限 fetch」。真挂死则 libfuzzer 超时（也算 fail）。
const ITER_SAFETY_CAP: u32 = 4096;

/// 输入 Vec 限幅（W-2）：防 `Arbitrary` 生成任意长向量炸内存/时间。
const MAX_SEGMENTS: usize = 256;
const MAX_DESCS_PER_PAGE: usize = 256;

/// 一个 16B SGL descriptor 的 fuzzer 控制面。
#[derive(Arbitrary, Debug)]
struct RawDesc {
    address: u64,
    length: u32,
    /// byte 15：high nibble = type、low nibble = sub_type。
    id_byte: u8,
}

/// 每次 segment fetch 喂回的「段页」= 若干 descriptor。
#[derive(Arbitrary, Debug)]
struct SegPage {
    descs: Vec<RawDesc>,
}

/// 结构感知 fuzz 输入。
#[derive(Arbitrary, Debug)]
struct SglChainInput {
    slba: u16,
    nlb: u8,
    /// embedded SGL1 segment 指针的 length（指向首段）。
    sgl1_len: u16,
    /// SGL1 的 id 字节（type/sub_type）。
    sgl1_id: u8,
    /// 每次 fetch 按序喂回的段页序列；耗尽后回退到一个 cont→自身的自环页。
    segments: Vec<SegPage>,
    /// 部分 DMA 完成报失败（`ok=false`），驱动错误清理路径。
    fail_mask: u32,
}

/// 把一组 `RawDesc` 序列化成 segment-页字节（每条 16B）。
fn serialize_descs(descs: &[RawDesc]) -> Vec<u8> {
    let mut out = Vec::with_capacity(descs.len() * 16);
    for d in descs {
        out.extend_from_slice(&d.address.to_le_bytes());
        out.extend_from_slice(&d.length.to_le_bytes());
        out.extend_from_slice(&[0u8; 3]); // reserved
        out.push(d.id_byte);
    }
    out
}

/// 编一条 PSDT=10 SGL READ SQE（embedded SGL1 = segment 指针，见 POC 注释）。
fn sgl_read_sqe(cid: u16, slba: u64, nlb: u32, seg_addr: u64, seg_len: u32, id_byte: u8) -> Sqe {
    Sqe {
        cdw0: 0x02u32 | (0b10u32 << 14) | ((cid as u32) << 16), // READ | PSDT=10 | cid
        nsid: 1,
        cdw2: 0,
        cdw3: 0,
        mptr: 0,
        prp1: seg_addr,
        prp2: (seg_len as u64) | ((id_byte as u64) << 56),
        cdw10: slba as u32,
        cdw11: (slba >> 32) as u32,
        cdw12: nlb.saturating_sub(1) & 0xffff,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
    }
}

mod common;

fn run(input: SglChainInput) {
    // 进程内一次性 tmpfile-backed controller（建 1 MiB image + open + cap，见 common.rs）。
    let (mut c, mut cap, _img) = match common::open_tmpfile_controller("sgl") {
        Some(t) => t,
        None => return,
    };

    const SEG_GPA: u64 = 0xA_0000;
    let sgl1_len = (input.sgl1_len as u32) & !0xF; // 16 对齐
    let sqe = sgl_read_sqe(
        0x40,
        input.slba as u64,
        (input.nlb as u32).max(1),
        SEG_GPA,
        sgl1_len.max(16),
        input.sgl1_id,
    );

    // dispatch（异步 → None，发首个 segment-页 DmaRead）。
    {
        let mut ctx = DeviceCtx::new(&mut cap);
        let _ = c.nvme_io_dispatch(&mut ctx, /*sq_id*/ 1, sqe, 0x40, /*cq_id*/ 1);
    }

    // O(N) DMA-drain（见 common::drive_dma_drain）：服务所有 DMA——DmaRead = segment-页 fetch、
    // DmaWrite = data scatter/CQE-post（喂空 ok=true，由 helper 自动；服务 write 是 op 表 drain
    // oracle 前提，只服务 read 会留 in-flight write 致误判泄漏）。read 的 fetch 逻辑见 feed 闭包。
    common::drive_dma_drain(
        &mut c,
        &mut cap,
        ITER_SAFETY_CAP,
        "SGL chain 超 ITER_SAFETY_CAP 仍未排空（疑似 hop guard 失效/无限 fetch）",
        |_token, _len, idx| {
            let i = idx as usize;
            // segment-页 fetch：耗尽 input.segments 后回退到 cont→自身自环页（压 hop guard）。
            let bytes = if i < input.segments.len() {
                let page = &input.segments[i];
                let n = page.descs.len().min(MAX_DESCS_PER_PAGE);
                serialize_descs(&page.descs[..n])
            } else {
                // 自环：单条 Segment(0x20) 指回 SEG_GPA。
                let mut d = [0u8; 16];
                d[0..8].copy_from_slice(&SEG_GPA.to_le_bytes());
                d[8..12].copy_from_slice(&16u32.to_le_bytes());
                d[15] = 0x20;
                d.to_vec()
            };
            // DMA-read 完成 ok 标志：fail_mask 对应 bit（覆盖读失败清理路径）。`& 31` 防溢出
            // （>32 hop 后失败 pattern 周期复用，coverage nit 非 bug）。
            let ok = (input.fail_mask >> (idx & 31)) & 1 == 0;
            (ok, bytes)
        },
    );

    // ── 内部不变式 oracle（`fuzzing` feature，见 DMA_COMPLETION_INVARIANTS.md）──
    // 命令全 drain 后 SGL op 表必清空（无 op_id 泄漏）。**CFS 例外**（I2）：CFS 置位时
    // `on_dma_complete_impl` 入口短路、表由 disable()/reset 回收而非 drain，故 cfs 下跳过
    // 泄漏检查（本 harness 不驱 doorbell/DBBUF 故 cfs 恒 false；`inv.cfs ||` 守未来 harness）。
    let inv = c.__fuzz_invariants();
    assert!(
        inv.cfs || inv.sgl_ops == 0,
        "SGL op 泄漏：drain 后 sgl_ops={}（cfs={}）",
        inv.sgl_ops,
        inv.cfs
    );
    assert!(
        inv.cfs || inv.pending_ios == 0,
        "pending_ios 泄漏：drain 后={}（cfs={}）",
        inv.pending_ios,
        inv.cfs
    );
}

fuzz_target!(|input: SglChainInput| {
    xtask_fuzz::init_tracing_if_repro();
    // W-2 限幅：截断过长输入向量（Arbitrary 可生成任意长）。
    let mut input = input;
    if input.segments.len() > MAX_SEGMENTS {
        input.segments.truncate(MAX_SEGMENTS);
    }
    run(input);
});
