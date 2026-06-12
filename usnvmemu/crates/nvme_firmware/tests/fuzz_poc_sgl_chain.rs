// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **POC（承重假设验证，非正式 fuzz target）** —— 验证「外部 crate 能否经
//! **公共 API** + `CaptureTransport` 同步驱动 controller 的 SGL segment chain
//! walker，并喂入任意（fuzzer-controlled）字节」。
//!
//! 这是把 coverage-guided fuzz 提议落到 `xtask fuzz` crate 之前的**最小冒烟**：
//! 一个真正的 fuzz target 是**独立 crate**，只能用 `pub` API（不能像 in-crate
//! `controller/tests.rs::drive()` 那样碰 `pub(super)` 的 `dispatch_io`/
//! `on_dma_complete_impl`/`pending_ios`）。本 POC 故意放在 `tests/`（外部 crate
//! 可见性 == fuzz crate 可见性），用**纯公共面**复刻 device-side 同步驱动循环：
//!
//!   `NvmeController::open` → `nvme_io_dispatch`(PSDT=10 SGL READ)
//!     → 观察 `CaptureTransport.events()` 里的 `DmaRead{token,gpa,len}`
//!     → `PcieDevice::on_dma_complete(token, ok=true, <fuzzer 字节>)` 喂回 segment 页
//!     → continuation 触发下一个 `DmaRead` → 循环，直到排空或 hop guard 截断。
//!
//! 验证三件事（架构复核降级后的真正风险点）：
//! 1. **公共 API 可达**：无需任何 fuzz-only hook，纯 pub 面即可驱动整条链。
//! 2. **同步、无 async runtime**：fuzz_target body 内可同步跑完 dispatch→complete。
//! 3. **hop guard 在对抗输入下封顶**：构造 Segment **自环**，断言 `MAX_SGL_SEGMENTS`
//!    截断 → DmaRead 次数有界（不无限 fetch、不挂死）。
//!
//! 安全 oracle（无 golden，与真 fuzz target 一致）= **不 panic + 迭代有界**。

use nvme_firmware::NvmeController;
use nvme_firmware::cmd::Sqe;
use pcie_device_core::{CaptureTransport, DeviceCtx, TransportEvent};

/// 与 `controller::MAX_SGL_SEGMENTS` 同值（该常量 `pub(super)`，外部不可见 ——
/// 这本身是给 plan 的一个 finding：fuzz crate 要断言 hop 上限需一个 pub 出口）。
const MAX_SGL_SEGMENTS: u32 = 64;

/// fuzz_target body 内每轮迭代的安全上限：远超 `MAX_SGL_SEGMENTS`，用来 catch
/// 「hop guard 失效 → 无限 fetch」。真挂死则测试超时（也算 fail）。
const ITER_SAFETY_CAP: u32 = 4096;

/// tmpfile RAII 守卫：`Drop` 删文件，**panic（含 assert 失败）也清理**，不漏 /tmp。
/// （rust-reviewer MEDIUM-1：手工 remove_file 在 assert 失败路径漏文件。）
struct TmpImg(std::path::PathBuf);
impl Drop for TmpImg {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 建一个 tmpfile-backed controller（NS=1, lbads=9/512B, plain）。
///
/// blocker #3（架构复核 LOW）实证点：fuzz crate 每轮重建 vs 复用的文件策略。
/// 本 POC 采「每个 case 新建、RAII 守卫保证 panic 也删」——证明重建路径无句柄泄漏
/// 致命问题；真 fuzz target 倾向**一次性建、跨轮复用**。
fn open_tmp_ctrl(nonce: u64) -> (NvmeController, TmpImg) {
    let path = std::env::temp_dir().join(format!(
        "fuzz_poc_sgl_{}_{}.img",
        std::process::id(),
        nonce
    ));
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(1024 * 1024).unwrap(); // 1 MiB → 2048 LBA
    drop(f);
    let c = NvmeController::open(&[path.to_str().unwrap().to_string()], 0x1414, 0, &[])
        .expect("open tmp controller");
    (c, TmpImg(path))
}

/// 编一条 PSDT=10 SGL READ SQE：embedded SGL1 = Segment/LastSegment 指针。
///
/// embedded_sgl_bytes 布局：buf[0..8]=prp1=seg_addr；buf[8..12]=length=prp2 低 32 位；
/// buf[15]=SGL id 字节=prp2 最高字节（high nibble=type、low nibble=sub_type）。
fn sgl_read_sqe(cid: u16, slba: u64, nlb: u32, seg_addr: u64, seg_len: u32, id_byte: u8) -> Sqe {
    // Sqe 字段 pub，直接 struct 字面量构造（不引 zerocopy 依赖到外部 test crate）。
    Sqe {
        // cdw0: opcode(0x02 READ) | psdt(0b10)<<14 | cid<<16
        cdw0: 0x02u32 | (0b10u32 << 14) | ((cid as u32) << 16),
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

/// 16-byte SGL descriptor: address(8) + length(4) + rsvd(3) + id(1)。
fn sgl_desc(address: u64, length: u32, id_byte: u8) -> [u8; 16] {
    let mut d = [0u8; 16];
    d[0..8].copy_from_slice(&address.to_le_bytes());
    d[8..12].copy_from_slice(&length.to_le_bytes());
    d[15] = id_byte;
    d
}

/// **核心驱动循环** —— 这正是未来 fuzz_target! body 的同步内核。
///
/// `feed(gpa, len) -> Vec<u8>`：给定 controller 发出的 segment-页 DmaRead，
/// 返回喂回的字节（真 fuzz target 这里来自 `arbitrary::Unstructured`；POC 用
/// 闭包注入确定性 / 对抗输入）。
///
/// 返回 `(dma_read_count, finished)`：dma_read_count = 喂回的 segment fetch 次数
/// （即 hop 数），finished = 是否在安全上限内排空（false 表示疑似无限 fetch）。
fn drive_sgl_chain(
    c: &mut NvmeController,
    cap: &mut CaptureTransport,
    sqe: Sqe,
    cid: u16,
    mut feed: impl FnMut(u64, u32) -> Vec<u8>,
) -> (u32, bool) {
    let mut ctx = DeviceCtx::new(cap);
    // dispatch：异步 → 返 None，并发出首个 segment 页 DmaRead。
    let _ = c.nvme_io_dispatch(&mut ctx, /*sq_id*/ 1, sqe, cid, /*cq_id*/ 1);

    let mut serviced = std::collections::HashSet::new();
    let mut dma_reads = 0u32;
    loop {
        // 找下一个未服务的 DmaRead（segment fetch / WRITE 方向 fragment gather）。
        // READ 方向的 fragment scatter 是 DmaWrite，无需喂回。
        // 依赖不变量：`guest_read` 每次 mint 唯一 token、`CaptureTransport` 不回收 token，
        // 故按 token 去重安全（token 复用会让本循环漏喂 → 假 finished）。
        let next = cap.events().iter().find_map(|e| match e {
            TransportEvent::DmaRead { token, gpa, len } if !serviced.contains(token) => {
                Some((*token, *gpa, *len))
            }
            _ => None,
        });
        let Some((token, gpa, len)) = next else {
            return (dma_reads, true); // 排空：链正常收尾（成功或错误 CQE）。
        };
        serviced.insert(token);
        dma_reads += 1;
        if dma_reads > ITER_SAFETY_CAP {
            return (dma_reads, false); // 疑似无限 fetch（hop guard 失效）。
        }
        let bytes = feed(gpa, len);
        let mut ctx = DeviceCtx::new(cap);
        // 公共完成入口（PcieDevice trait method）—— data = fuzzer 字节。
        use pcie_device_core::PcieDevice;
        c.on_dma_complete(&mut ctx, token, true, bytes);
    }
}

/// **对抗 case：Segment 自环必被 hop guard 截断（不无限 fetch、不 panic）。**
///
/// SGL1 = Segment(0x20，非 last) → SEG_GPA(len 16 = 1 descriptor)；
/// 喂回的 segment 页 = 单条 Segment 描述符指回 SEG_GPA 自己 → 死循环拓扑。
/// 期望：walk_segments 累加到 MAX_SGL_SEGMENTS 后 finish_sgl_error 截断。
#[test]
fn sgl_segment_self_loop_is_bounded_by_hop_guard() {
    const SEG_GPA: u64 = 0xA_0000;
    let (mut c, _img) = open_tmp_ctrl(1);
    let mut cap = CaptureTransport::with_start_token(0x100);

    // SGL1 = Segment(high nibble 0x2, sub 0) → SEG_GPA, len=16。
    let sqe = sgl_read_sqe(0x40, /*slba*/ 0, /*nlb*/ 8, SEG_GPA, 16, 0x20);
    // 每次 segment fetch 都喂「指回自己的 Segment 描述符」→ 自环。
    let (hops, finished) = drive_sgl_chain(&mut c, &mut cap, sqe, 0x40, |_gpa, _len| {
        sgl_desc(SEG_GPA, 16, 0x20).to_vec()
    });

    assert!(finished, "自环必须被 hop guard 截断，而非无限 fetch（hops={hops}）");
    // 精确 oracle（revert-verify 强度）：walk_segments 1..=64 放行 + 第 65 次 >64 触发
    // finish_sgl_error，故恰好喂回 65 次 segment fetch。若是其它早错路径会 != 65。
    assert_eq!(
        hops,
        MAX_SGL_SEGMENTS + 1,
        "自环应恰好 fetch MAX_SGL_SEGMENTS+1({}) 次后被 hop guard 截断，实测 {hops}",
        MAX_SGL_SEGMENTS + 1
    );
    // _img 出作用域时 Drop 删 tmpfile（panic 也清理）。
}

/// **fuzzer-shaped case：任意字节喂回 segment 页，只验不 panic + 有界。**
///
/// 这模拟真 fuzz target：把一段种子字节按 16B 切成「随机 descriptor」喂回每次
/// fetch（随机 type/sub_type/length/address/对齐）。不关心结果正确，只验安全网。
fn run_fuzz_shaped(seed: &[u8]) {
    const SEG_GPA: u64 = 0xA_0000;
    let (mut c, _img) = open_tmp_ctrl(0xF000 + seed.len() as u64);
    let mut cap = CaptureTransport::with_start_token(0x100);

    // 用种子前 16B 当 SGL1 segment 描述符的 (len, id) 来源；地址固定 SEG_GPA
    // （保证首个 fetch 一定打到我们会喂字节的地方）。
    let seg_len = if seed.len() >= 4 {
        // 限幅到合理页内大小并 16 对齐，避免 len 巨大触发分配（真 fuzz 由 entry 上限兜底，
        // POC 这里只想驱动到 parse 路径）。末尾再 `.max(16)` 兜 0 → 至少 1 descriptor。
        u32::from_le_bytes(seed[0..4].try_into().unwrap()) % 256 & !0xF
    } else {
        16
    };
    let id_byte = seed.first().map(|b| b & 0x30).unwrap_or(0x20); // type∈{0,1,2,3}, sub 0
    let sqe = sgl_read_sqe(0x41, 0, 8, SEG_GPA, seg_len.max(16), id_byte);

    // 喂回：从种子循环取 `len` 字节（不足补 0），模拟 Unstructured 的字节供给。
    let mut cursor = 16usize;
    let (hops, finished) = drive_sgl_chain(&mut c, &mut cap, sqe, 0x41, |_gpa, len| {
        let mut out = vec![0u8; len as usize];
        for b in out.iter_mut() {
            if !seed.is_empty() {
                *b = seed[cursor % seed.len()];
                cursor = cursor.wrapping_add(1);
            }
        }
        out
    });
    assert!(
        finished && hops <= ITER_SAFETY_CAP,
        "任意字节驱动也必须有界收尾（hops={hops}, finished={finished}）"
    );
    // _img Drop 删 tmpfile。
}

/// 跑一组确定性种子 —— 证明同一 controller 驱动流程对各种字节都不 panic、有界。
/// （真 fuzz 由 libfuzzer 喂海量种子；POC 用手选种子覆盖几类形状。）
#[test]
fn fuzz_shaped_seeds_no_panic_and_bounded() {
    let seeds: &[&[u8]] = &[
        &[],
        &[0x00; 16],                                  // 全 0：Data Block len0 sub0
        &[0xFF; 64],                                  // 全 F：未知 type → 解析 Err
        &[0x20, 0, 0, 0, 0, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0x20], // 像 Segment 链
        &[0x01; 48],                                  // sub_type=1（CMB-relative，无 CMB → 0x12）
        b"arbitrary nvme sgl fuzz seed bytes \x00\x10\x20\x30",
    ];
    for s in seeds {
        run_fuzz_shaped(s);
    }
}
