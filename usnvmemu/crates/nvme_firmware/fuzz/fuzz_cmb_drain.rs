// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **target #5 `fuzz_cmb_drain`** —— 结构感知 fuzz：CMB（Controller Memory Buffer）数据路径
//! + cascade drain（a 类路径 #4，台账 `docs/DMA_COMPLETION_INVARIANTS.md`）。
//!
//! 打 `nvme_firmware` controller 的 **CMB 命中 → 合成完成 → `drain_cmb_completions` cascade**
//! （`cmb.rs`）：IO 命令的 PRP/SGL 落进 CMB 窗口时，`guest_read`/`guest_write` 退化为**同步**
//! backing 访问 + 合成一条 `CmbCompletion` 入队，由顶层栈尾的 `drain_cmb_completions` 喂回
//! `on_dma_complete_impl`（其处理可能再发 CMB 访问 → cascade，受 `MAX_CMB_DRAIN_ITERS`(=1<<20)
//! 循环上限 + **非重入铁律** `cmb_in_access_guest` 哨兵）。
//!
//! **为什么不驱到 1M cap**：`MAX_CMB_DRAIN_ITERS`≈100 万，组织性 fuzz 驱到 cap 不现实（且
//! cap-firing 已由确定性 `cmb_drain_iters_cap_fires_and_sets_cfs` 守）。CMB cascade 的**真风险面
//! 是结构正确性**：① 非重入（`on_dma_complete_impl` 入口 `debug_assert!(!cmb_in_access_guest)`
//! ——若任何路径把 completion 拽进 `access_guest` 栈 → debug build 即 panic = libfuzzer catch）；
//! ② cmb_hit 边界数学（Contained / Straddle / Miss，含越界/跨界 PRP）；③ PRP/SGL 链元素落 CMB
//! 时的 gpa→backing 偏移 + cascade ordering；④ drain 后无 op 泄漏。
//!
//! **触发形状**（纯公共 API）：
//!   1. `enable_cmb(64 KiB, bir=1)` 装 VecRamRegion backing；
//!   2. `mmio_write` enable（AQA/ASQ/ACQ/CC）；
//!   3. `mmio_write(CMBMSC=0x50, CBA|CMSE|CRE)` 激活窗口 → `cmb_window()=Some([CBA, CBA+size))`；
//!   4. `nvme_admin_dispatch` 建 IO CQ/SQ（qid=1）；
//!   5. `nvme_io_dispatch` 一条 fuzzer IO 命令，**PRP1/PRP2 落在 CMB 窗口内外**（覆盖
//!      Contained/Straddle/Miss）→ CMB 命中触 cascade。
//!
//! **oracle**：observable（no panic——含非重入 debug_assert + cmb_hit 越界）+ 内部不变式
//! （`fuzzing` feature）：drain 后 `cmb_completions`/`pending_ios` 清空（CFS 例外）。
//! **I2 搭车（守未来，非本 target 活跃 oracle）**：本 harness 只 dispatch **一条**命令、不驱
//! doorbell，唯一可达 CFS 源是 `MAX_CMB_DRAIN_ITERS`≈1M 撞顶（doc 明言不驱到）→ 故本 target
//! `cfs` **恒 false**，`reads_after_cfs==0` 与 CFS-例外分支是守未来的 vacuous guard（CFS 经
//! shadow-poll/queue-create 的真行使在 `fuzz_shadow_poll` #3 那里）。

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use nvme_firmware::NvmeController;
use nvme_firmware::cmd::Sqe;
use pcie_device_core::{CaptureTransport, DeviceCtx, PcieDevice, TransportEvent};
use xtask_fuzz::fuzz_target;

// ── NVMe BAR0 寄存器偏移（spec § 3.1；同 fuzz_shadow_poll / tests）──
const REG_CC: u64 = 0x14;
const REG_AQA: u64 = 0x24;
const REG_ASQ: u64 = 0x28;
const REG_ACQ: u64 = 0x30;
const REG_CMBMSC: u64 = 0x50; // CRE(bit0) | CMSE(bit1) | CBA(bits63:12)

// ── 固定 GPA 布局（页对齐，互不重叠；CMB 窗口在高地址）──
const ASQ_GPA: u64 = 0x1_0000;
const ACQ_GPA: u64 = 0x2_0000;
const IO_CQ_GPA: u64 = 0x3_0000;
const IO_SQ_GPA: u64 = 0x4_0000;
/// CMB 窗口基址（4 KiB 对齐，CBA bits63:12）。
const CMB_CBA: u64 = 0x8000_0000;
/// CMB 大小：64 KiB = 16 页（4 KiB 非零倍 + 2 的幂）。
const CMB_SIZE: u64 = 0x1_0000;

const ADMIN_Q_DEPTH: u16 = 8;
const IO_Q_DEPTH: u16 = 8;
const IO_QID: u16 = 1;

const CREATE_IO_CQ: u8 = 0x05;
const CREATE_IO_SQ: u8 = 0x01;

/// 安全上限：catch「`MAX_CMB_DRAIN_ITERS`(=1<<20) / 链上限守卫失效 → 无限 cascade」。设在远
/// 超任何单命令合法 DMA 数之上（单命令 ≤ MDTS/PRP-list/SGL 链上限 × CMB 访问 ≪ 此值）。
const ITER_SAFETY_CAP: u32 = 100_000;

/// 输入 Vec 限幅（W-2）。
const MAX_FED_PAGES: usize = 64;

/// 一张喂回的 PRP-list / SGL segment 页（非 CMB 的 list 页 fetch）。
#[derive(Arbitrary, Debug)]
struct FedPage {
    bytes: Vec<u8>,
}

/// 结构感知 fuzz 输入：一条落 CMB 的 IO 命令 + 喂回的非-CMB list 页序列。
#[derive(Arbitrary, Debug)]
struct CmbDrainInput {
    /// IO opcode（cdw0 低 8；0x01 WRITE / 0x02 READ / 0x09 DSM / … fuzzer 探路由）。
    opcode: u8,
    /// nlb-1（传输长度；夹到 [0, MAX_NLB]）。
    nlb_minus_1: u16,
    /// 起始 LBA（截断到容量内）。
    slba: u16,
    /// PRP1 页偏移种子：CBA + (prp1_pg % 32)·4 KiB → 覆盖窗口内(0..15)+外(16..31)。
    prp1_pg: u8,
    /// PRP2 页偏移种子（同上）。
    prp2_pg: u8,
    /// PSDT（0=PRP / 1=SGL）选择位 + 杂项 cdw 扰动。
    use_sgl: bool,
    cdw12: u32,
    /// 对每次非-CMB list-页 fetch 按序喂回的页内容。
    fed_pages: Vec<FedPage>,
    /// 部分 DMA 完成报失败（`ok=false`）。
    fail_mask: u32,
}

const MAX_NLB: u32 = 2048;

/// tmpfile RAII 守卫（panic 也删；同 fuzz_sgl_chain）。
struct TmpImg(std::path::PathBuf);
impl Drop for TmpImg {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 编一条 admin SQE。
fn admin_sqe(opcode: u8, cid: u16, prp1: u64, cdw10: u32, cdw11: u32) -> Sqe {
    Sqe {
        cdw0: (opcode as u32) | ((cid as u32) << 16),
        nsid: 0,
        cdw2: 0,
        cdw3: 0,
        mptr: 0,
        prp1,
        prp2: 0,
        cdw10,
        cdw11,
        cdw12: 0,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
    }
}

/// PRP 页偏移种子 → CBA 邻域 GPA：[CBA, CBA + 32·4 KiB) 覆盖窗口内外（Contained/Miss/Straddle）。
fn cmb_neighborhood_gpa(pg_seed: u8) -> u64 {
    CMB_CBA + ((pg_seed as u64 % 32) << 12)
}

fn run(input: CmbDrainInput) {
    let path = std::env::temp_dir().join(format!("nvme_fuzz_cmb_{}.img", std::process::id()));
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
    // 装 CMB backing（64 KiB VecRamRegion，bir=1）。失败（不该）→ 放弃。
    if c.enable_cmb(CMB_SIZE, 1).is_err() {
        return;
    }
    let mut cap = CaptureTransport::with_start_token(0x100);

    // ── 确定性 setup：enable → 激活 CMBMSC → 建 IO 队列对 ──
    {
        let mut ctx = DeviceCtx::new(&mut cap);
        let aqa = (((ADMIN_Q_DEPTH - 1) as u64) << 16) | (ADMIN_Q_DEPTH - 1) as u64;
        c.mmio_write(&mut ctx, 0, REG_AQA, 4, aqa);
        c.mmio_write(&mut ctx, 0, REG_ASQ, 8, ASQ_GPA);
        c.mmio_write(&mut ctx, 0, REG_ACQ, 8, ACQ_GPA);
        c.mmio_write(&mut ctx, 0, REG_CC, 4, 1u64 | (6 << 16) | (4 << 20));
        // 激活 CMB 窗口：CMBMSC = CBA | CMSE(bit1) | CRE(bit0)。
        c.mmio_write(&mut ctx, 0, REG_CMBMSC, 8, CMB_CBA | 0b11);
        // 建 IO CQ(先) + IO SQ。
        let cdw10_q = (IO_QID as u32) | (((IO_Q_DEPTH - 1) as u32) << 16);
        let _ = c.nvme_admin_dispatch(
            &mut ctx,
            admin_sqe(CREATE_IO_CQ, 0x10, IO_CQ_GPA, cdw10_q, 0b11),
            0x10,
            0,
        );
        let _ = c.nvme_admin_dispatch(
            &mut ctx,
            admin_sqe(
                CREATE_IO_SQ,
                0x11,
                IO_SQ_GPA,
                cdw10_q,
                1 | ((IO_QID as u32) << 16),
            ),
            0x11,
            0,
        );
    }

    // ── dispatch 一条落 CMB 的 IO 命令 ──
    {
        let mut ctx = DeviceCtx::new(&mut cap);
        let nlb = ((input.nlb_minus_1 as u32) + 1).clamp(1, MAX_NLB);
        let max_slba = 2048u64.saturating_sub(nlb as u64);
        let slba = if max_slba == 0 {
            0
        } else {
            (input.slba as u64) % (max_slba + 1)
        };
        let psdt = if input.use_sgl { 1u32 << 14 } else { 0 }; // cdw0 bits15:14 = PSDT
        let sqe = Sqe {
            cdw0: (input.opcode as u32) | psdt | (0x40u32 << 16),
            nsid: 1,
            cdw2: 0,
            cdw3: 0,
            mptr: 0,
            prp1: cmb_neighborhood_gpa(input.prp1_pg),
            prp2: cmb_neighborhood_gpa(input.prp2_pg),
            cdw10: slba as u32,
            cdw11: (slba >> 32) as u32,
            cdw12: (nlb.saturating_sub(1) & 0xffff) | (input.cdw12 & 0xffff_0000),
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };
        let _ = c.nvme_io_dispatch(&mut ctx, IO_QID, sqe, 0x40, IO_QID);
    }

    // ── 同步驱动循环（O(N) cursor+worklist，同 fuzz_shadow_poll：cascade 可深，避 O(N²)）──
    // CMB 命中的访问是**内部同步**（不出 transport 事件）；这里只服务**非-CMB** DMA：
    // list/segment 页 fetch（DmaRead，喂 fuzzer 页字节）+ CQE-post / 非-CMB scatter（DmaWrite，喂空）。
    let mut cursor = 0usize;
    let mut work: std::collections::VecDeque<(u64, Option<u32>)> =
        std::collections::VecDeque::new();
    let mut fed_idx = 0usize;
    let mut dma_reads = 0u32;
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
            break;
        };
        total_dmas += 1;
        if total_dmas > ITER_SAFETY_CAP {
            panic!(
                "CMB cascade 超 ITER_SAFETY_CAP 仍未排空（疑似 MAX_CMB_DRAIN_ITERS / 链守卫失效）"
            );
        }
        if cfs_seen && read_len.is_some() {
            reads_after_cfs += 1;
        }
        {
            let mut ctx = DeviceCtx::new(&mut cap);
            match read_len {
                Some(len) => {
                    // 非-CMB list/segment 页 fetch：喂 fuzzer 页字节（补/截到请求长度）。
                    let mut bytes = if fed_idx < input.fed_pages.len() {
                        input.fed_pages[fed_idx].bytes.clone()
                    } else {
                        Vec::new()
                    };
                    bytes.resize(len as usize, 0);
                    let ok = (input.fail_mask >> (dma_reads & 31)) & 1 == 0;
                    fed_idx += 1;
                    dma_reads += 1;
                    c.on_dma_complete(&mut ctx, token, ok, bytes);
                }
                None => {
                    c.on_dma_complete(&mut ctx, token, true, Vec::new());
                }
            }
        }
        if !cfs_seen && c.__fuzz_invariants().cfs {
            cfs_seen = true;
        }
    }

    // ── oracle ──
    let inv = c.__fuzz_invariants();
    // (1) CMB cascade 全 drain 后无残留合成完成。
    assert!(
        inv.cfs || inv.cmb_completions == 0,
        "CMB cascade 残留：drain 后 cmb_completions={}（cfs={}）",
        inv.cmb_completions,
        inv.cfs
    );
    // (2) 无 op 泄漏（CFS 例外：表由 disable/reset 回收）。
    assert!(
        inv.cfs || inv.pending_ios == 0,
        "pending_ios 泄漏：drain 后={}（cfs={}）",
        inv.pending_ios,
        inv.cfs
    );
    // (3) I2 搭车：CFS 置位后入口短路 → 之后不再发新 DmaRead。
    assert_eq!(
        reads_after_cfs, 0,
        "I2 违反：CFS 置位后仍发出 {} 条新 DmaRead（on_dma_complete_impl 入口未短路）",
        reads_after_cfs
    );
}

fuzz_target!(|input: CmbDrainInput| {
    xtask_fuzz::init_tracing_if_repro();
    let mut input = input;
    if input.fed_pages.len() > MAX_FED_PAGES {
        input.fed_pages.truncate(MAX_FED_PAGES);
    }
    run(input);
});
