// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **target #3 `fuzz_admin_dispatch`** —— 结构感知 fuzz：admin 命令分派。
//!
//! 打 `nvme_firmware` controller 的 admin 路径（`admin.rs::dispatch_admin`，经 pub
//! `nvme_admin_dispatch` 入口；`dispatch_admin` 本身 `pub(super)` 外部 crate 不可直接
//! 调）。fuzzer 控 admin SQE 的 opcode / cdw10..15 / nsid / prp，压：
//!   - **opcode 路由**（IDENTIFY / GET_LOG_PAGE / SET_FEATURES / GET_FEATURES /
//!     CREATE_IO_{CQ,SQ} / DELETE_IO_{CQ,SQ} / ABORT / KEEP_ALIVE / 各 vendor 槽）；
//!   - **Identify CNS 分支**（cdw10 低 8 位选 0x00/0x01/0x02/.../0x1c，见 admin.rs:127）；
//!   - **Get/Set Features FID**（cdw10 低 8 位）；
//!   - **Create/Delete IO Queue**（cdw10 qid + qsize、cdw11 pc/cqid）；
//!   - **Get Log Page**（cdw10 LID + NUMD，大 payload → device→host PRP DMA）。
//!
//! **同步驱动循环**（镜像 fuzz_sgl_chain.rs / fuzz_prp_list_chain.rs，公共 API、无
//! async runtime、无 fuzz-only hook）：
//!
//!   `nvme_admin_dispatch(sqe, cid, cq_id)`
//!     → `Some(Cqe)` = 同步完成（多数 admin：Set Features / Create Queue / …）；
//!     → `None` = 异步：controller 已 `guest_write`(payload≤1 page) / `guest_read`
//!       (Get Log Page >2 page 的 PRP-list 页) → 进 pending_ios。
//!     对任意产生的 **未服务 `DmaRead`**（= PRP-list 页 fetch，见 mod.rs:3862
//!     `dma_write_then_complete` 的 >2 page 分支）喂回 fuzzer 字节、循环到排空。
//!     device→host 的 `DmaWrite`（payload 写 host）**不喂**（不影响排空）。
//!
//! **触发要点**（先读 mod.rs:1873 `nvme_admin_dispatch` + mod.rs:2082
//! `nvme_admin_complete_dma` + admin.rs dispatch_admin opcode match 后定型）：
//!   - opcode 在 cdw0 低 8 位、cid 在 cdw0 高 16 位；
//!   - cq_id 传 0（admin CQ，`open` 已装；缺失时 dispatch 也容忍 `unwrap_or(1)`）；
//!   - Get Log Page 大 payload 的 prp2 须页对齐（否则 PRP-list fetch 前被拒——
//!     但 admin 路径的 dma_write_then_complete 不预检 PRP2 对齐，故大 payload 时
//!     我们让 prp2 页对齐以稳定触发 fetch；其余 opcode 不依赖此）。
//!
//! **oracle（observable-only，无 golden）**：libfuzzer 自动 catch panic / 超时；本
//! harness 另设 `ITER_SAFETY_CAP` 兜底超限即 `panic!`。**已开 `fuzzing` feature**：drain 后
//! 经 `__fuzz_invariants()` 断言 op 表清空（无 op_id 泄漏，CFS 例外）。

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use nvme_firmware::NvmeController;
use nvme_firmware::cmd::Sqe;
use pcie_device_core::{CaptureTransport, DeviceCtx, PcieDevice, TransportEvent};
use xtask_fuzz::fuzz_target;

/// NVMe 4 KiB host page；满 PRP-list 页 = 512 entry。
const NVME_PAGE_SIZE: u64 = 4096;
const ENTRIES_PER_PAGE: usize = (NVME_PAGE_SIZE / 8) as usize; // 512

/// 安全上限：远超任何合法 admin DMA 链深度（Get Log Page chain ≤ MAX_PRP_LIST_PAGES=16），
/// 用来 catch「守卫失效 → 无限 fetch」。真挂死则 libfuzzer 超时（也算 fail）。
const ITER_SAFETY_CAP: u32 = 4096;

/// 输入 Vec 限幅（W-2）。
const MAX_FED_PAGES: usize = 64;
const MAX_ENTRIES_PER_FED_PAGE: usize = ENTRIES_PER_PAGE;

/// 一次 admin 命令的 fuzzer 控制面（直接喂 8 个 cdw + nsid + prp）。
#[derive(Arbitrary, Debug)]
struct AdminCmd {
    /// admin opcode（cdw0 低 8 位）。
    opcode: u8,
    nsid: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
    /// 是否让 prp 页对齐（影响大 payload 的 PRP-list fetch 稳定触发）。
    prp_aligned: bool,
    /// prp1/prp2 的低位扰动（页对齐时仅取高位、否则原样带偏移）。
    prp1_seed: u16,
    prp2_seed: u16,
}

/// 结构感知 fuzz 输入：一条 admin 命令 + 喂回的 PRP-list 页序列。
#[derive(Arbitrary, Debug)]
struct AdminDispatchInput {
    cmd: AdminCmd,
    /// 对每次 PRP-list 页 fetch 按序喂回的页内容；耗尽后回退到「最终页」（无 chain）。
    fed_pages: Vec<ListPage>,
    /// 部分 DMA 完成报失败（`ok=false`）。
    fail_mask: u32,
}

/// 一张喂回的 PRP-list 页（device→host 大 payload 的 list 页）。
#[derive(Arbitrary, Debug)]
struct ListPage {
    entries: Vec<u64>,
}

/// 把一组 u64 entry 序列化成 PRP-list 页字节（每条 8B LE）。
fn serialize_entries(entries: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * 8);
    for e in entries {
        out.extend_from_slice(&e.to_le_bytes());
    }
    out
}

/// 「最终页」：全 data GPA、无 chain pointer（让 walk 正常收尾）。
fn final_list_page() -> Vec<u8> {
    serialize_entries(&vec![NVME_PAGE_SIZE; ENTRIES_PER_PAGE])
}

/// 从 fuzzer 控制面编一条 admin SQE。
fn admin_sqe(cmd: &AdminCmd, cid: u16) -> Sqe {
    // prp 基址：页对齐时用一个固定页 + seed 高位扰动；否则带页内偏移。
    let (prp1, prp2) = if cmd.prp_aligned {
        (
            0x10_0000 + ((cmd.prp1_seed as u64) << 12),
            0x80_0000 + ((cmd.prp2_seed as u64) << 12),
        )
    } else {
        (
            0x10_0000 + (cmd.prp1_seed as u64),
            0x80_0000 + (cmd.prp2_seed as u64),
        )
    };
    Sqe {
        cdw0: (cmd.opcode as u32) | ((cid as u32) << 16),
        nsid: cmd.nsid,
        cdw2: 0,
        cdw3: 0,
        mptr: 0,
        prp1,
        prp2,
        cdw10: cmd.cdw10,
        cdw11: cmd.cdw11,
        cdw12: cmd.cdw12,
        cdw13: cmd.cdw13,
        cdw14: cmd.cdw14,
        cdw15: cmd.cdw15,
    }
}

/// tmpfile RAII 守卫（panic 也删；同 fuzz_sgl_chain）。
struct TmpImg(std::path::PathBuf);
impl Drop for TmpImg {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn run(input: AdminDispatchInput) {
    let path = std::env::temp_dir().join(format!("nvme_fuzz_admin_{}.img", std::process::id()));
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

    let sqe = admin_sqe(&input.cmd, 0x40);

    // dispatch（cq_id=0 admin CQ）。同步完成返 Some(Cqe)（丢弃，observable oracle
    // 不校验内容）；异步返 None 并发出 DmaWrite/DmaRead。
    {
        let mut ctx = DeviceCtx::new(&mut cap);
        let _ = c.nvme_admin_dispatch(&mut ctx, sqe, 0x40, /*cq_id*/ 0);
    }

    // 同步驱动循环：服务**所有**未服务 DMA——DmaRead（= Get Log Page 大 payload 的 PRP-list
    // 页 fetch，喂 fuzzer list 页）与 DmaWrite（= payload 写 host / CQE-post，喂空 ok=true）。
    // 服务 write 是 op 表 drain oracle 的前提（否则留 in-flight write → 误判泄漏）。
    let mut serviced = std::collections::HashSet::new();
    let mut fetch_idx = 0usize;
    let mut dma_reads = 0u32;
    let mut total_dmas = 0u32;
    loop {
        let next = cap.events().iter().find_map(|e| match e {
            TransportEvent::DmaRead { token, len, .. } if !serviced.contains(token) => {
                Some((*token, Some(*len)))
            }
            TransportEvent::DmaWrite { token, .. } if !serviced.contains(token) => {
                Some((*token, None))
            }
            _ => None,
        });
        let Some((token, read_len)) = next else {
            break; // 全 drain：admin 命令收尾（同步早返 / 异步 DMA 链结束 / 守卫截断）。
        };
        serviced.insert(token);
        total_dmas += 1;
        if total_dmas > ITER_SAFETY_CAP {
            panic!(
                "admin PRP-list fetch 超 ITER_SAFETY_CAP 仍未排空（疑似 MAX_PRP_LIST_PAGES 守卫失效/无限 fetch）"
            );
        }

        let mut ctx = DeviceCtx::new(&mut cap);
        match read_len {
            Some(len) => {
                let mut bytes = if fetch_idx < input.fed_pages.len() {
                    let page = &input.fed_pages[fetch_idx];
                    let n = page.entries.len().min(MAX_ENTRIES_PER_FED_PAGE);
                    serialize_entries(&page.entries[..n])
                } else {
                    final_list_page()
                };
                // 模型真 transport（dma_read_sync 强制 reply.len == 请求 len）：补/截到请求长度。
                bytes.resize(len as usize, 0);
                let ok = (input.fail_mask >> (dma_reads & 31)) & 1 == 0;
                fetch_idx += 1;
                dma_reads += 1;
                c.on_dma_complete(&mut ctx, token, ok, bytes);
            }
            None => {
                // DmaWrite 完成：data 空、ok=true（payload 落 host / CQE-post）。
                c.on_dma_complete(&mut ctx, token, true, Vec::new());
            }
        }
    }

    // ── 内部不变式 oracle（`fuzzing` feature，见 DMA_COMPLETION_INVARIANTS.md）──
    // admin 命令全 drain 后异步 op 表必清空（无 op_id 泄漏）。AER（0x0c）等 pending 由独立
    // aen_pending 跟踪、不入 pending_ios，故本断言不被合法 AER 误伤。**CFS 例外**（I2）：CFS
    // 置位时入口短路、表由 reset 回收而非 drain（本 harness 不驱 doorbell 故 cfs 恒 false；守未来）。
    let inv = c.__fuzz_invariants();
    assert!(
        inv.cfs || inv.prp_list_ops == 0,
        "admin PRP-list op 泄漏：drain 后 prp_list_ops={}（cfs={}）",
        inv.prp_list_ops,
        inv.cfs
    );
    assert!(
        inv.cfs || inv.pending_ios == 0,
        "admin pending_ios 泄漏：drain 后={}（cfs={}）",
        inv.pending_ios,
        inv.cfs
    );
}

fuzz_target!(|input: AdminDispatchInput| {
    xtask_fuzz::init_tracing_if_repro();
    let mut input = input;
    if input.fed_pages.len() > MAX_FED_PAGES {
        input.fed_pages.truncate(MAX_FED_PAGES);
    }
    run(input);
});
