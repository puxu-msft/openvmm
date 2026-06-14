// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **nvme_firmware fuzz 共享 harness 工具**（A 类存活性-DoS target 复用）。
//!
//! 三件套（消重 + 统一正确性）：
//!   - `TmpImg`：进程内一次性 tmpfile-backed image 的 RAII 清理 guard。
//!   - `open_tmpfile_controller`：建 1 MiB tmpfile + `NvmeController::open` + `CaptureTransport`。
//!   - `drive_dma_drain`：**O(N) cursor+worklist** DMA-completion 驱动循环（统一替换各 A 类 target
//!     早期 `events().find_map` 每轮全扫的 O(N²)——见 FUZZING.md §1.6）。
//!
//! 每个 A 类 bin 经 `mod common;` 各自编译本文件（cargo-fuzz bin = 独立 crate 根，sibling 文件按
//! 模块系统解析）；故 `#![allow(dead_code)]`（某 bin 可能不用全部项）。

#![allow(dead_code)]

use nvme_firmware::NvmeController;
use pcie_device_core::{CaptureTransport, DeviceCtx, PcieDevice, TransportEvent};
use std::collections::VecDeque;
use std::path::PathBuf;

/// 进程内一次性 tmpfile-backed image 的 RAII 清理 guard（drop 时删文件）。
pub struct TmpImg(pub PathBuf);

impl Drop for TmpImg {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 建 1 MiB tmpfile-backed image + 打开 controller（vendor `0x1414`）+ 新建 `CaptureTransport`
/// （`start_token=0x100`）。返回 `(controller, cap, img-guard)`；任一步失败返 `None`（caller
/// `return` 跳过本输入）。`tag` 进文件名避免并发 target 撞名。
pub fn open_tmpfile_controller(tag: &str) -> Option<(NvmeController, CaptureTransport, TmpImg)> {
    // 进程内一次性 tmpfile-backed controller。libfuzzer 每个输入调一次。
    let path = std::env::temp_dir().join(format!("nvme_fuzz_{}_{}.img", tag, std::process::id()));
    let f = std::fs::File::create(&path).ok()?;
    f.set_len(1024 * 1024).ok()?;
    drop(f);
    let img = TmpImg(path.clone());
    let c = NvmeController::open(&[path.to_str()?.to_string()], 0x1414, 0, &[]).ok()?;
    let cap = CaptureTransport::with_start_token(0x100);
    Some((c, cap, img))
}

/// **O(N) cursor+worklist** DMA-completion 驱动循环：服务**所有**未服务 DMA 直到链排空。
///
/// - **DmaWrite** → 一律 `on_dma_complete(token, true, [])`（data scatter / CQE-post）。服务 write
///   是 op 表 drain oracle 的前提——只服务 read 会留 in-flight write → op 永不收尾 → 误判泄漏。
/// - **DmaRead** → 调 `feed(token, len, read_idx)` 取 `(ok, bytes)`。`read_idx` 从 0 单调递增、
///   **只在 read 上递增**（== 各 target 原 `fetch_idx`/`dma_reads`，故同一 idx 同时索引输入页 +
///   `fail_mask` bit）。
/// - 服务数超 `safety_cap` → `panic!(cap_msg)`：守卫失效 backstop，**非**正常 oracle；阈值须
///   ≥ SUT 合法最坏（见 FUZZING.md §1.5，例：admin Get Log Page 不受 MDTS 限）。
///
/// **O(N) 正确性判据**：`CaptureTransport` 事件 append-only、token 单调唯一不回收 → cursor 只扫新
/// 事件入队、每 token 服务一次。FIFO worklist 顺序 == 原 `find_map` 每轮取最早未服务 → **行为等价**
/// （`find_map` 匹配 read+write 二者、按 events() 创建序取首个未服务 = FIFO）。返回服务的 DMA 总数。
pub fn drive_dma_drain<F>(
    c: &mut NvmeController,
    cap: &mut CaptureTransport,
    safety_cap: u32,
    cap_msg: &str,
    mut feed: F,
) -> u32
where
    F: FnMut(u64, u32, u32) -> (bool, Vec<u8>),
{
    let mut cursor = 0usize;
    let mut work: VecDeque<(u64, Option<u32>)> = VecDeque::new();
    let mut read_idx = 0u32;
    let mut total = 0u32;
    loop {
        // cursor 只扫新增事件入队（append-only → 不重扫已入队的）。
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
            break; // 全 drain：链收尾（成功 scatter+CQE / 精确错误 CQE / 守卫截断）。
        };
        total += 1;
        if total > safety_cap {
            panic!("{}", cap_msg);
        }
        // feed 只读 input、不触 cap → 先算 (ok,bytes) 再借 cap 建 ctx，避免借用重叠。
        let (ok, bytes) = match read_len {
            Some(len) => {
                let r = feed(token, len, read_idx);
                read_idx += 1;
                r
            }
            None => (true, Vec::new()),
        };
        let mut ctx = DeviceCtx::new(cap);
        c.on_dma_complete(&mut ctx, token, ok, bytes);
    }
    total
}
