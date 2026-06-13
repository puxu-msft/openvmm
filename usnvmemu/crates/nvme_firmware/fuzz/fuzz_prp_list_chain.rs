// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **target #2 `fuzz_prp_list_chain`** —— 结构感知 fuzz：读方向 PRP-list 链 walker。
//!
//! 打 `nvme_firmware` controller 的 **device→host PRP-list** 指针追逐核心
//! （`completion.rs::NvmReadPrpListFetch` arm 的 walk + chain-pointer 跟随 +
//! `MAX_PRP_LIST_PAGES`(=16) 自环/超长链上限）。镜像 `fuzz_sgl_chain.rs` 已证的
//! **同步 device-side 驱动循环**（公共 API，无 async runtime、无 fuzz-only hook）：
//!
//!   `nvme_io_dispatch`(plain READ，> 2 page) → controller `guest_read(prp2, 4096)`
//!     发出首张 PRP-list 页的 `DmaRead{token,gpa,len}`
//!     → `PcieDevice::on_dma_complete(token, ok, <fuzzer list-页字节>)`
//!     → 完成 arm parse u64 entry 数组 + 满页时跟随末位 chain pointer → 下一张
//!       list 页 `DmaRead` → 循环，直到链收尾 / `MAX_PRP_LIST_PAGES` 截断。
//!
//! **触发形状**（先读 io.rs:1740-1866 plain READ List-tier 路径 + completion.rs:2375
//! 完成 arm 后定型）：
//!   - opcode 0x02 READ、nsid=1、PSDT=00（plain PRP，**非** SGL）；
//!   - `bytes = nlb × 512` 必须 **> 2 page**（8 KiB）才进 List tier → nlb ≥ 17，即
//!     cdw12 低 16 位（= nlb-1）≥ 16；
//!   - prp1 **页对齐**（保 prp_off=0、tier 计算简单）；
//!   - prp2 **页对齐**（否则被 `validate_prp2` 的 `PRP_OFFSET_INVALID` 提前拒，
//!     见 io.rs:285-306 / 1769）；
//!   - `bytes ≤ MDTS_MAX_BYTES`（用 nlb 上限兜底，见下 `MAX_NLB`）。
//!
//! **喂回字节 = fuzzer 控制的 PRP-list 页内容**：`parse_prp_list`（mod.rs:4407）把页
//! 按 `chunks_exact(8)` 解析成 LE u64 entry 数组。满页（512 entry）时 entry[0..511]
//! 是 data-page GPA、entry[511] 是指向下一 list 页的 **chain pointer**（spec § 4.1.2）。
//! fuzzer 控 entry 数 / 值 / chain 拓扑（自环 / 超长链）。input.list_pages 耗尽后回退
//! 喂一个「entry[511] 指回 prp2 自身」的自环页，压 `MAX_PRP_LIST_PAGES` 守卫。
//!
//! **oracle（observable-only，无 golden）**：libfuzzer 自动 catch panic / 超时（守卫
//! 失效 → 无限 fetch）；本 harness 另设 `ITER_SAFETY_CAP` 兜底超限即 `panic!`。
//! READ 方向的 data scatter 是 `DmaWrite`（device→host），**不喂**；只对未服务的
//! `DmaRead`（= list-页 fetch）喂回。**已开 `fuzzing` feature**：drain 后经
//! `__fuzz_invariants()` 断言 op 表清空（无 op_id 泄漏，CFS 例外）。

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use nvme_firmware::NvmeController;
use nvme_firmware::cmd::Sqe;
use pcie_device_core::{CaptureTransport, DeviceCtx, PcieDevice, TransportEvent};
use xtask_fuzz::fuzz_target;

/// NVMe 4 KiB host page；PRP-list 页 = 512 个 u64 entry。
const NVME_PAGE_SIZE: u64 = 4096;
/// 一张满 PRP-list 页的 entry 数（4096/8）。
const ENTRIES_PER_PAGE: usize = (NVME_PAGE_SIZE / 8) as usize; // 512

/// 与 `controller::MAX_PRP_LIST_PAGES`(=16) 同思路的安全上限：远超之，用来 catch
/// 「chain depth guard 失效 → 无限 list-页 fetch」。真挂死则 libfuzzer 超时（也算 fail）。
const ITER_SAFETY_CAP: u32 = 4096;

/// 输入 Vec 限幅（W-2）：防 `Arbitrary` 生成任意长向量炸内存/时间。
const MAX_LIST_PAGES: usize = 256;
const MAX_ENTRIES_PER_FED_PAGE: usize = ENTRIES_PER_PAGE; // 单页最多 512 entry

/// 1 MiB tmpfile → 2048 LBA（512B/sector）。nlb 跨 > 2 page → List tier。
/// 上限 = MDTS 兜底（远小于 MDTS_MAX_BYTES；2048 LBA = 1 MiB 也在范围内）。
const MAX_NLB: u32 = 2048;

/// 一张喂回的 PRP-list 页 = fuzzer 控的一组 u64 entry。
#[derive(Arbitrary, Debug)]
struct ListPage {
    entries: Vec<u64>,
}

/// 结构感知 fuzz 输入。
#[derive(Arbitrary, Debug)]
struct PrpListChainInput {
    /// 起始 LBA（截断到容量内，避免被 LBA_OUT_OF_RANGE 提前拒——非目标路径）。
    slba: u16,
    /// nlb-1 的低位来源；最终 nlb 夹到 [17, MAX_NLB] 保证进 List tier。
    nlb_minus_1: u16,
    /// 每次 list-页 fetch 按序喂回的页序列；耗尽后回退到 chain→prp2 自身的自环页。
    list_pages: Vec<ListPage>,
    /// 部分 DMA 完成报失败（`ok=false`），驱动错误清理路径。
    fail_mask: u32,
}

/// 把一组 u64 entry 序列化成 PRP-list 页字节（每条 8B LE）。caller 再 resize 到
/// DmaRead 请求的精确长度（见驱动循环）——**模型真 transport**：`dma_read_sync`
/// (vfio_user_transport/src/dma.rs:35) 强制 `reply.len == 请求 len`，短读会变 ok=false，
/// 故 ok=true 的 list 页恒满 4096B。fuzzer 仍控全部 512 entry 值/chain 拓扑。
/// （注：「ok=true 短读触发 completion.rs:2474 debug_assert」是一个**已报给 silver-heron
/// 的 latent 硬化缺口**——Transport 契约允许截断、controller 注释自称已处理但不完整；
/// 现 transport 强制长度故未活化。此 harness padding 到请求长度以探索 chain/entry 空间，
/// 不反复撞那一个已知点。）
fn serialize_entries(entries: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * 8);
    for e in entries {
        out.extend_from_slice(&e.to_le_bytes());
    }
    out
}

/// 编一条 plain READ SQE（PSDT=00），prp2 指向 PRP-list 页（页对齐）。
fn prp_list_read_sqe(cid: u16, slba: u64, nlb: u32, prp1: u64, prp2: u64) -> Sqe {
    Sqe {
        // cdw0: opcode(0x02 READ) | psdt(00) | cid<<16
        cdw0: 0x02u32 | ((cid as u32) << 16),
        nsid: 1,
        cdw2: 0,
        cdw3: 0,
        mptr: 0,
        prp1,
        prp2,
        cdw10: slba as u32,
        cdw11: (slba >> 32) as u32,
        cdw12: nlb.saturating_sub(1) & 0xffff,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
    }
}

/// 回退「自环页」：满页 data GPA 占位 + entry[511] = chain→prp2 自身 → 无限 chain
/// 拓扑,用来压 `MAX_PRP_LIST_PAGES` 深度守卫（input.list_pages 耗尽后每次 fetch 都喂它）。
fn self_loop_list_page(prp2_gpa: u64) -> Vec<u8> {
    // 满页：entry[0..511] = 任意 data GPA（用页对齐占位）、entry[511] = chain→prp2 自身。
    let mut entries = vec![NVME_PAGE_SIZE; ENTRIES_PER_PAGE]; // data GPA 占位（页对齐）
    entries[ENTRIES_PER_PAGE - 1] = prp2_gpa; // chain pointer 指回自己 → 自环
    serialize_entries(&entries)
}

/// tmpfile RAII 守卫（panic 也删，不漏 /tmp；同 fuzz_sgl_chain）。
struct TmpImg(std::path::PathBuf);
impl Drop for TmpImg {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn run(input: PrpListChainInput) {
    // 进程内一次性 tmpfile-backed controller。libfuzzer 每个输入调一次 run()。
    let path = std::env::temp_dir().join(format!("nvme_fuzz_prplist_{}.img", std::process::id()));
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

    // prp2 = PRP-list 页 GPA（**页对齐**，否则 validate_prp2 提前拒）。
    const PRP1_GPA: u64 = 0x10_0000; // 页对齐 data 页
    const PRP2_GPA: u64 = 0x20_0000; // 页对齐 list 页

    // nlb 夹到 [17, MAX_NLB]：17×512 = 8704 > 8192(2 page) → 进 List tier。
    // 同时 slba 截断到容量内，保证 end ≤ total_lba（否则 LBA_OUT_OF_RANGE 提前拒）。
    let nlb = ((input.nlb_minus_1 as u32) + 1).clamp(17, MAX_NLB);
    let max_slba = 2048u64.saturating_sub(nlb as u64);
    let slba = if max_slba == 0 {
        0
    } else {
        (input.slba as u64) % (max_slba + 1)
    };

    let sqe = prp_list_read_sqe(0x40, slba, nlb, PRP1_GPA, PRP2_GPA);

    // dispatch（异步 → None，发首张 list-页 DmaRead 在 prp2）。
    {
        let mut ctx = DeviceCtx::new(&mut cap);
        let _ = c.nvme_io_dispatch(&mut ctx, /*sq_id*/ 1, sqe, 0x40, /*cq_id*/ 1);
    }

    // 同步驱动循环：服务**所有**未服务 DMA——DmaRead（= list-页 fetch，喂 fuzzer list 页）
    // 与 DmaWrite（= data scatter / CQE-post，喂空）。服务 write 是 op 表 drain oracle 的前提
    // （只服务 read 会留 in-flight write → op 永不收尾 → 误判泄漏）。
    // 依赖不变量：`guest_read`/`guest_write` 每次 mint 唯一 token、`CaptureTransport` 不回收。
    let mut serviced = std::collections::HashSet::new();
    let mut fetch_idx = 0usize;
    let mut dma_reads = 0u32;
    let mut total_dmas = 0u32;
    loop {
        // 优先未服务 DmaRead（带 len）；其次未服务 DmaWrite（None = 喂空 ok=true）。
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
            break; // 全 drain：链收尾（成功 scatter+CQE / 精确错误 CQE / 截断）。
        };
        serviced.insert(token);
        total_dmas += 1;
        if total_dmas > ITER_SAFETY_CAP {
            panic!(
                "PRP-list chain 超 ITER_SAFETY_CAP 仍未排空（疑似 MAX_PRP_LIST_PAGES 守卫失效/无限 fetch）"
            );
        }

        let mut ctx = DeviceCtx::new(&mut cap);
        match read_len {
            Some(len) => {
                // list-页 fetch：耗尽 input.list_pages 后回退到 chain→prp2 自身自环页
                // （压 MAX_PRP_LIST_PAGES 深度守卫）。
                let mut bytes = if fetch_idx < input.list_pages.len() {
                    let page = &input.list_pages[fetch_idx];
                    let n = page.entries.len().min(MAX_ENTRIES_PER_FED_PAGE);
                    serialize_entries(&page.entries[..n])
                } else {
                    self_loop_list_page(PRP2_GPA)
                };
                // 模型真 transport（dma_read_sync 强制 reply.len == 请求 len）：补/截到请求长度。
                bytes.resize(len as usize, 0);
                // DMA-read 完成 ok 标志：fail_mask 对应 bit（覆盖读失败清理路径）。`& 31` 防溢出。
                let ok = (input.fail_mask >> (dma_reads & 31)) & 1 == 0;
                fetch_idx += 1;
                dma_reads += 1;
                c.on_dma_complete(&mut ctx, token, ok, bytes);
            }
            None => {
                // DmaWrite 完成：data 空、ok=true（正常落盘 / CQE-post）。
                c.on_dma_complete(&mut ctx, token, true, Vec::new());
            }
        }
    }

    // ── 内部不变式 oracle（`fuzzing` feature，见 DMA_COMPLETION_INVARIANTS.md）──
    // 命令全 drain 后 op 表必清空（无 op_id 泄漏）。**CFS 例外**（I2）：CFS 置位时入口短路、
    // 表由 disable()/reset 回收而非 drain（本 harness 不驱 doorbell 故 cfs 恒 false；守未来）。
    let inv = c.__fuzz_invariants();
    assert!(
        inv.cfs || inv.prp_list_ops == 0,
        "PRP-list op 泄漏：drain 后 prp_list_ops={}（cfs={}）",
        inv.prp_list_ops,
        inv.cfs
    );
    assert!(
        inv.cfs || inv.pending_ios == 0,
        "pending_ios 泄漏：drain 后={}（cfs={}）",
        inv.pending_ios,
        inv.cfs
    );
}

fuzz_target!(|input: PrpListChainInput| {
    xtask_fuzz::init_tracing_if_repro();
    // W-2 限幅：截断过长输入向量（Arbitrary 可生成任意长）。
    let mut input = input;
    if input.list_pages.len() > MAX_LIST_PAGES {
        input.list_pages.truncate(MAX_LIST_PAGES);
    }
    run(input);
});
