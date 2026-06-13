// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **回归 case（fuzz#2 副产 finding，跨会话协调归属：ivory-vole 守外部 robustness、
//! silver-heron 接 completion.rs 修 + in-crate SC 断言）。**
//!
//! coverage-guided fuzz target `fuzz_prp_list_chain` 喂「**契约允许的** ok=true 短
//! PRP-list 页」跑出 `completion.rs:2474` 的 truncated-read 硬化缺口：
//!   - debug build：`debug_assert_eq!(list.len()+1, total_pages)` → panic；
//!   - release：assert 编掉 → `list` 欠填 → 下游 scatter 按 `list.len()` 欠写 →
//!     **静默短 guest READ**（data-integrity）。
//!
//! **为什么是契约允许而非 harness 假象**（见 memory `fuzz-the-contract-not-current-impl`）：
//! `pcie_device_core::Transport` trait 契约**显式允许** backend 截断 DMA read
//! （doc：「`len` ≤ MAX_DMA_BYTES；超限由 backend 自行拒绝 / 截断」），controller 注释也
//! 自称已处理截断（`completion.rs:2427-2438` 的 `iter().take()`）——故 device 必须对短读
//! robust 才是 conforming。现 vfio `dma_read_sync`(dma.rs:35) 偶然强制 `reply.len==请求 len`
//! → 短读变 ok=false → **当前掩盖**；但 NVMe-oF / 未来后端 / 不合规 peer 不保证。
//!
//! **本 test 是外部 robustness 边界回归闸**：喂 ok=true 短 list 页，断言 controller
//! **不 panic** 且链 **drain 收尾**（silver-heron 的 fix：underfill → `DATA_TRANSFER_ERROR`
//! 而非 assert/欠写）。精确 SC 级断言归 silver-heron 的 in-crate test（有 CQE/capture access）。
//!
//! **当前 `#[ignore]`**：silver-heron 的 `completion.rs:2474` 硬化修**未落地前**本 test 会
//! panic（debug_assert）。修落地后 **un-ignore** 即成 standing 回归守卫。

//! **un-ignore（2026-06-14）**：silver-heron 的 `completion.rs` truncated-read 硬化已落地
//! （commit `ea6cad268`：`NvmReadPrpListFetch` 短 list 页欠填 → 先于 scatter 精确
//! `DATA_TRANSFER_ERROR`，不再 debug_assert panic / 静默短 scatter）。本 test 转为
//! standing 回归守卫：契约允许的 ok=true 短 list 页 → no-panic + 链优雅 drain。
//! 精确 SC 级断言归 silver-heron in-crate（`prp_list_short_read_fails_clean_not_panic_or_underfill`）；
//! 本 test 守**外部 robustness 边界**（no-panic + drain）。

use nvme_firmware::NvmeController;
use nvme_firmware::cmd::Sqe;
use pcie_device_core::{CaptureTransport, DeviceCtx, PcieDevice, TransportEvent};

const NVME_PAGE_SIZE: u64 = 4096;

struct TmpImg(std::path::PathBuf);
impl Drop for TmpImg {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn open_tmp_ctrl() -> (NvmeController, TmpImg) {
    let path = std::env::temp_dir().join(format!("nvme_regr_shortread_{}.img", std::process::id()));
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(1024 * 1024).unwrap(); // 1 MiB → 2048 LBA（512B/sector）
    drop(f);
    let c = NvmeController::open(&[path.to_str().unwrap().to_string()], 0x1414, 0, &[])
        .expect("open tmp controller");
    (c, TmpImg(path))
}

/// **truncated PRP-list read 不得 panic / 不得静默欠写。**
///
/// 触发：plain READ（PSDT=00）nlb=17 → total_bytes=8704 → total_pages=3 → needed=2 个
/// data-page GPA。controller `guest_read(prp2, 4096)` 发首张 list 页 DmaRead；我们喂回一个
/// **只含 1 个 entry（8 字节）的短页**（ok=true，契约允许的截断）。
///   - pre-fix：`acc.extend(take(2))` 实得 1 → `acc.len()=1 < needed=2` → `completion.rs:2474`
///     debug_assert(`1+1 != 3`) panic。
///   - post-fix（silver-heron）：underfill → `DATA_TRANSFER_ERROR` CQE，链优雅收尾、不 panic。
#[test]
fn prp_list_truncated_read_completes_without_panic() {
    let (mut c, _img) = open_tmp_ctrl();
    let mut cap = CaptureTransport::with_start_token(0x100);

    const PRP1_GPA: u64 = 0x10_0000; // 页对齐 data 页
    const PRP2_GPA: u64 = 0x20_0000; // 页对齐 list 页
    let nlb: u32 = 17; // 17×512 = 8704 > 2 page → List tier；total_pages=3、needed=2

    let sqe = Sqe {
        cdw0: 0x02u32 | ((0x40u32) << 16), // READ | PSDT=00 | cid=0x40
        nsid: 1,
        cdw2: 0,
        cdw3: 0,
        mptr: 0,
        prp1: PRP1_GPA,
        prp2: PRP2_GPA,
        cdw10: 0, // slba=0
        cdw11: 0,
        cdw12: (nlb - 1) & 0xffff,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
    };

    {
        let mut ctx = DeviceCtx::new(&mut cap);
        let _ = c.nvme_io_dispatch(&mut ctx, /*sq_id*/ 1, sqe, 0x40, /*cq_id*/ 1);
    }

    // 驱动循环：对首张 list-页 fetch 喂回**短页**（1 entry = 8B，ok=true）。
    let mut serviced = std::collections::HashSet::new();
    let mut guard = 0u32;
    loop {
        let next = cap.events().iter().find_map(|e| match e {
            TransportEvent::DmaRead { token, .. } if !serviced.contains(token) => Some(*token),
            _ => None,
        });
        let Some(token) = next else { break };
        serviced.insert(token);
        guard += 1;
        assert!(guard < 1024, "链未收尾（守卫失效）");

        // 短 list 页：单 entry（页对齐占位 GPA），故意 < needed=2 → 触发 underfill 路径。
        let short_page = (NVME_PAGE_SIZE).to_le_bytes().to_vec(); // 8 字节 = 1 entry

        let mut ctx = DeviceCtx::new(&mut cap);
        // ok=true 短读：契约允许的截断。pre-fix 在此 panic；post-fix 优雅收尾。
        c.on_dma_complete(&mut ctx, token, true, short_page);
    }
    // 到这里 = 未 panic 且链 drain 收尾（post-fix 预期）。
}
