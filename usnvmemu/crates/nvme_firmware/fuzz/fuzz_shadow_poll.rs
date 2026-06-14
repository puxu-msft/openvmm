// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **target #4 `fuzz_shadow_poll`** —— 结构感知 fuzz：DBBUF shadow-doorbell 轮询状态机
//! （a 类路径 #3，台账 `docs/DMA_COMPLETION_INVARIANTS.md`）。
//!
//! 打 `nvme_firmware` controller 的 **DBBUF shadow-poll 自馈链**
//! （`mod.rs::start_shadow_sq_poll` → `issue_shadow_read` → `on_dma_complete` →
//! `handle_shadow_poll_complete` → 自续 re-read，受 `MAX_SHADOW_POLL_ITERS`(=1<<16) 上限）。
//! 这是 §29 历史上最 bug-prone 的一条（dropped-ring / wrap-saturation / vfio 自馈死锁三个
//! HIGH 全在它身上），确定性 `dbbuf_tests` 已守已知 bug，本 coverage-guided fuzz 是叠加层。
//!
//! **触发形状**（纯公共 API、无 fuzz-only 驱动 hook；shadow-poll 由 **doorbell ring** 触发，
//! 不经 `nvme_io_dispatch`——见 `on_sq_tail_doorbell` 的 DBBUF 分支）：
//!   1. **enable**：`mmio_write` AQA/ASQ/ACQ/CC(EN|IOSQES=6|IOCQES=4) → CSTS.RDY；
//!   2. **建 IO 队列对**：`nvme_admin_dispatch` Create IO CQ(0x05) → Create IO SQ(0x01)，qid=1
//!      （`on_sq_tail_doorbell` 要求 `sqs.contains_key(sq_id)`）；
//!   3. **激活 DBBUF**：`nvme_admin_dispatch` Doorbell Buffer Config(0x7c)，PRP1=shadow GPA、
//!      PRP2=event_idx GPA → `dbbuf_active()`；
//!   4. **ring IO SQ doorbell**：`mmio_write(0x1000+2·qid·4)` → `start_shadow_sq_poll` →
//!      `issue_shadow_read` 发 4 字节 `DmaRead`(shadow GPA)。
//!
//! **喂回字节 = fuzzer 控的 shadow tail 值**：对每次 4 字节 shadow-read `DmaRead`，喂回
//! `input.shadow_values` 的下一个 u32（LE）。耗尽后回退到**收敛值 0**（链 ≤2 读 settle）——
//! 不把每个输入推向 65536 iter-cap（那条 deep self-loop 由确定性 `dbbuf_tests` 守）；CFS 仍可
//! 被 fuzzer 喂的**越界** shadow（`>= size` → `handle_shadow_sq` 即时置 CFS）廉价触达。
//! 非 4 字节 `DmaRead`（settle 后的 SQE fetch 等）喂零。
//!
//! **oracle**：
//!   - **observable-only**：libfuzzer catch panic / 超时；`ITER_SAFETY_CAP` 设在真 cap
//!     (`MAX_SHADOW_POLL_ITERS`) **之上**，只为 catch「cap 失效 → 真无限 re-read」（**不**用
//!     数迭代当正常 oracle——cap 太大 65536，正常自环就要跑这么多次才置 CFS）。
//!   - **内部不变式**（`fuzzing` feature）：drain 后 `pending_shadow_polls`/`pending_ios` 清空
//!     （无链泄漏），**CFS 例外**（自环撞 cap → I2 入口短路 + 表由 reset 回收非 drain）。
//!   - **I2 搭车**：若自环驱到 CFS 置位，再喂一条**新 token 完成** → 断言 `cap.events()` **0 条
//!     新增**（CFS 后 `on_dma_complete_impl` 入口短路、不派生新 DMA）。#3/#4 是唯二能到达 CFS
//!     的 fuzz 路径（dispatch target 到不了），故 I2 可 fuzz 不变式天然搭在这里。

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use nvme_firmware::NvmeController;
use nvme_firmware::cmd::Sqe;
use pcie_device_core::{CaptureTransport, DeviceCtx, PcieDevice, TransportEvent};
use xtask_fuzz::fuzz_target;

// ── NVMe BAR0 寄存器偏移（spec § 3.1；同 tests/openhcl_pcie_remote_e2e.rs）──
const REG_CC: u64 = 0x14;
const REG_AQA: u64 = 0x24;
const REG_ASQ: u64 = 0x28;
const REG_ACQ: u64 = 0x30;
const DOORBELL_BASE: u64 = 0x1000;

// ── 固定 GPA 布局（页对齐，互不重叠）──
const ASQ_GPA: u64 = 0x1_0000;
const ACQ_GPA: u64 = 0x2_0000;
const IO_CQ_GPA: u64 = 0x3_0000;
const IO_SQ_GPA: u64 = 0x4_0000;
const SHADOW_GPA: u64 = 0xA_0000;
const EVENTIDX_GPA: u64 = 0xB_0000;

const ADMIN_Q_DEPTH: u16 = 8;
const IO_Q_DEPTH: u16 = 8;
const IO_QID: u16 = 1;

const CREATE_IO_CQ: u8 = 0x05;
const CREATE_IO_SQ: u8 = 0x01;
const DOORBELL_BUFFER_CONFIG: u8 = 0x7c;

/// SQ tail doorbell 偏移（DSTRD=0 → stride 4；SQ=偶 idx）：0x1000 + (2·qid)·4。
fn sq_db(qid: u16) -> u64 {
    DOORBELL_BASE + (2 * qid as u64) * 4
}

/// 安全上限：catch「`MAX_SHADOW_POLL_ITERS`(=1<<16=65536) 守卫失效 → 真无限 re-read」。
/// 设在真 cap **之上**（合法自环最坏 = 65536 次自续后置 CFS 收链）。真挂死则 libfuzzer 超时。
const ITER_SAFETY_CAP: u32 = 200_000;

/// 输入 Vec 限幅（W-2）：防 `Arbitrary` 生成任意长向量。
const MAX_SHADOW_VALUES: usize = 4096;

/// 结构感知 fuzz 输入。
#[derive(Arbitrary, Debug)]
struct ShadowPollInput {
    /// 每次 shadow-read 按序喂回的 SQ tail 值（驱动 poll 链：振荡/越界值压自续深度守卫 +
    /// 触越界 CFS）。耗尽后回退到收敛值 0（见 `fallback_shadow`）。
    shadow_values: Vec<u32>,
    /// 额外 doorbell ring 次数（每次可能重启链 / 置 `shadow_ring_pending`，压 HIGH-1 路径）。
    extra_rings: u8,
    /// 部分 shadow-read DMA 报失败（`ok=false`），覆盖读失败收链路径。
    fail_mask: u32,
}

/// 耗尽 `shadow_values` 后的**收敛**回退值：恒 0。链会在 ≤2 读内 settle（processed→0 后
/// `shadow==processed`），避免 fallback 把每个输入都推向 65536 iter-cap（那条 deep self-loop
/// 由确定性 `dbbuf_tests` 守；本 fuzz 重在逻辑广度 + 越界 CFS + ring/settle 交错）。CFS 仍可
/// 被 fuzzer 喂的**越界** shadow（`>= size` → `handle_shadow_sq` 即时置 CFS）廉价触达，
/// 从而行使 leak-exception + I2 搭车 oracle。
fn fallback_shadow() -> u32 {
    0
}

/// tmpfile RAII 守卫（panic 也删，不漏 /tmp；同 fuzz_sgl_chain）。
struct TmpImg(std::path::PathBuf);
impl Drop for TmpImg {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 编一条 admin SQE（opcode 在 cdw0 低 8、cid 在高 16）。
fn admin_sqe(opcode: u8, cid: u16, prp1: u64, prp2: u64, cdw10: u32, cdw11: u32) -> Sqe {
    Sqe {
        cdw0: (opcode as u32) | ((cid as u32) << 16),
        nsid: 0,
        cdw2: 0,
        cdw3: 0,
        mptr: 0,
        prp1,
        prp2,
        cdw10,
        cdw11,
        cdw12: 0,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
    }
}

fn run(input: ShadowPollInput) {
    let path = std::env::temp_dir().join(format!("nvme_fuzz_shadow_{}.img", std::process::id()));
    let f = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(_) => return,
    };
    if f.set_len(1024 * 1024).is_err() {
        return;
    }
    drop(f);
    let _img = TmpImg(path.clone());
    let mut c = match NvmeController::open(&[path.to_str().unwrap().to_string()], 0x1414, 0, &[]) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut cap = CaptureTransport::with_start_token(0x100);

    // ── 确定性 setup（非 fuzz 面）：enable → 建 IO 队列对 → 激活 DBBUF → ring SQ doorbell ──
    {
        let mut ctx = DeviceCtx::new(&mut cap);
        // 1) enable：AQA(0-based depth)、ASQ、ACQ、CC(EN|IOSQES=6|IOCQES=4)。
        let aqa = (((ADMIN_Q_DEPTH - 1) as u64) << 16) | (ADMIN_Q_DEPTH - 1) as u64;
        c.mmio_write(&mut ctx, 0, REG_AQA, 4, aqa);
        c.mmio_write(&mut ctx, 0, REG_ASQ, 8, ASQ_GPA);
        c.mmio_write(&mut ctx, 0, REG_ACQ, 8, ACQ_GPA);
        c.mmio_write(&mut ctx, 0, REG_CC, 4, 1u64 | (6 << 16) | (4 << 20));
        // 2) 建 IO CQ(先) + IO SQ：cdw10 = qid|(depth-1)<<16。
        let cdw10_q = (IO_QID as u32) | (((IO_Q_DEPTH - 1) as u32) << 16);
        let _ = c.nvme_admin_dispatch(
            &mut ctx,
            admin_sqe(CREATE_IO_CQ, 0x10, IO_CQ_GPA, 0, cdw10_q, 0b11), // PC|IEN|IV=0
            0x10,
            0,
        );
        let _ = c.nvme_admin_dispatch(
            &mut ctx,
            admin_sqe(
                CREATE_IO_SQ,
                0x11,
                IO_SQ_GPA,
                0,
                cdw10_q,
                1 | ((IO_QID as u32) << 16), // PC | CQID
            ),
            0x11,
            0,
        );
        // 3) 激活 DBBUF：PRP1=shadow、PRP2=event_idx。
        let _ = c.nvme_admin_dispatch(
            &mut ctx,
            admin_sqe(DOORBELL_BUFFER_CONFIG, 0x1d, SHADOW_GPA, EVENTIDX_GPA, 0, 0),
            0x1d,
            0,
        );
        // 4) ring IO SQ doorbell（首次 + extra_rings 次）→ start_shadow_sq_poll。
        let rings = 1 + (input.extra_rings as u32).min(8);
        for r in 0..rings {
            c.mmio_write(&mut ctx, 0, sq_db(IO_QID), 4, (r % 4) as u64);
        }
    }

    // ── 同步驱动循环：服务所有 DMA。shadow-read(len=4) 喂 fuzzer SQ tail 值；其余 DmaRead
    //    （settle 后 SQE fetch 等）喂零；DmaWrite（event_idx 写 / CQE-post）喂空 ok=true。
    //
    // **O(N) cursor+worklist**（**刻意不同于** fuzz_sgl/prp/admin 的 `events().find_map` 每轮全扫
    // O(N²)）：shadow-poll 自环可驱到 `MAX_SHADOW_POLL_ITERS`=65536，O(N²) 会让一次合法撞-cap
    // 看起来像 libfuzzer hang（假阳）。`CaptureTransport` 事件 append-only、token 单调唯一不回收，
    // 故按 cursor 只扫新事件入队、每 token 服务一次 = O(N)，撞 cap 也 ~ms。
    let mut cursor = 0usize;
    let mut work: std::collections::VecDeque<(u64, Option<u32>)> =
        std::collections::VecDeque::new();
    let mut shadow_idx = 0u32;
    let mut cfs_seen = false;
    let mut reads_after_cfs = 0u32;
    let mut total_dmas = 0u32;
    loop {
        {
            let events = cap.events();
            while cursor < events.len() {
                match &events[cursor] {
                    TransportEvent::DmaRead { token, len, .. } => {
                        work.push_back((*token, Some(*len)))
                    }
                    TransportEvent::DmaWrite { token, .. } => work.push_back((*token, None)),
                    _ => {}
                }
                cursor += 1;
            }
        }
        let Some((token, read_len)) = work.pop_front() else {
            break; // 全 drain：链 settle / 撞 cap 收链 / 截断。
        };
        total_dmas += 1;
        if total_dmas > ITER_SAFETY_CAP {
            panic!(
                "shadow-poll 超 ITER_SAFETY_CAP 仍未排空（疑似 MAX_SHADOW_POLL_ITERS 守卫失效/无限 re-read）"
            );
        }
        // I2：CFS 置位后**不应再发出新 DmaRead**（入口短路）。统计 cfs 后服务的 read 数。
        if cfs_seen && read_len.is_some() {
            reads_after_cfs += 1;
        }

        {
            let mut ctx = DeviceCtx::new(&mut cap);
            match read_len {
                Some(4) => {
                    // shadow-read：喂 fuzzer SQ tail（LE u32），耗尽回退收敛 0（链 settle）。
                    let val = if (shadow_idx as usize) < input.shadow_values.len() {
                        input.shadow_values[shadow_idx as usize]
                    } else {
                        fallback_shadow()
                    };
                    let ok = (input.fail_mask >> (shadow_idx & 31)) & 1 == 0;
                    shadow_idx += 1;
                    c.on_dma_complete(&mut ctx, token, ok, val.to_le_bytes().to_vec());
                }
                Some(len) => {
                    // 非 shadow read（settle 后 SQE fetch 等）：喂零（controller 安全处理 garbage）。
                    c.on_dma_complete(&mut ctx, token, true, vec![0u8; len as usize]);
                }
                None => {
                    // DmaWrite（event_idx / CQE-post）：喂空 ok=true。
                    c.on_dma_complete(&mut ctx, token, true, Vec::new());
                }
            }
        }
        // 检测 CFS 首次置位（自环撞 MAX_SHADOW_POLL_ITERS）；置位后停止重复查询。
        if !cfs_seen && c.__fuzz_invariants().cfs {
            cfs_seen = true;
        }
    }

    // ── oracle ──
    let inv = c.__fuzz_invariants();
    // (1) 无链泄漏（CFS 例外：表由 disable/reset 回收）。
    assert!(
        inv.cfs || inv.pending_shadow_polls == 0,
        "shadow-poll 链泄漏：drain 后 pending_shadow_polls={}（cfs={}）",
        inv.pending_shadow_polls,
        inv.cfs
    );
    assert!(
        inv.cfs || inv.pending_ios == 0,
        "pending_ios 泄漏：drain 后={}（cfs={}）",
        inv.pending_ios,
        inv.cfs
    );
    // (2) I2 搭车：CFS 置位后入口短路 → 之后不再发新 DmaRead。CFS 未达则 reads_after_cfs=0
    //     天然成立（#3 唯一能到达 CFS 的 fuzz 路径——自环撞 cap）。
    assert_eq!(
        reads_after_cfs, 0,
        "I2 违反：CFS 置位后仍发出 {} 条新 DmaRead（on_dma_complete_impl 入口未短路）",
        reads_after_cfs
    );
}

fuzz_target!(|input: ShadowPollInput| {
    xtask_fuzz::init_tracing_if_repro();
    let mut input = input;
    if input.shadow_values.len() > MAX_SHADOW_VALUES {
        input.shadow_values.truncate(MAX_SHADOW_VALUES);
    }
    run(input);
});
