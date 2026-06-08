# Phase V-followup-prp-list — Lift V5_NLB_MAX 上限 via PRP-list path

> **⚠️ SUPERSEDED — 不按本文档做 (2026-06-06 audit)** — 本文档原计划走 session 端 multi-sentinel decode (改 controller PRP-list path)，最终改走 **session-level chunking** (16→256 LBA 透明分片) 路径，0 controller 改动；交付为 commits `2a4d734b` + `619f8d44`，5 anchor tests + Python io_size_sweep 12 场景 byte-equal。原 multi-sentinel 路径作为"future production PRP-list"留 TODO，需要时回看本文档的 §3.1+。当前路径成本 ≈ 200 LOC，本文档原方案 ≈ 500 LOC，**未来若做真 controller PRP-list path，本文档仍可作为起点**。

> 日期：2026-06-06
> 前置 HEAD：8b68c4cd (V-interop-7 全段完成)
> 目标：让 NVMe-oF target 单 IO 突破 V5_NLB_MAX=16 LBA (8 KiB)，支持 ≥ 128 KiB
> 单 IO，与 MDTS=5 (= 128 KiB) 真正对齐

---

## 1. 当前状态

- **Controller 层 (nvme_firmware)** 已**完整实现** PRP-list path
  (Phase E): `controller/io.rs::dispatch_io` 在 `bytes > 2 * NVME_PAGE_SIZE`
  时进 PRP-list 分支，分配 `op_id` + 入 `prp_list_ops` HashMap + dma_read
  PRP list page → dma_write data pages
- **NVMe-oF target 层 (nvme_of_tcp_target)** 在 `session.rs` / `async_session.rs`
  cap 在 `V5_NLB_MAX=16` (`nlb_real > V5_NLB_MAX` → reject SC=0x18)
- **MDTS=1** (V-interop-3 fix) 让 host 不发 > 8 KiB IO

## 2. 为什么之前 cap 在 16 LBA？

session 端 PRP1+PRP2 sentinel 模型 (V5e-2) 只识别两段 dma_write/dma_read。
> 2 page 时 controller 走 PRP-list path 会发更多 sentinel (一段 list page +
N 段 data page)，session 之前没改 capture decode 逻辑识别多 sentinel 区间。

## 3. 设计

### 3.1 Sentinel 扩展

```rust
pub const PRP1_SENTINEL: u64 = 0x1000_0000;
pub const PRP2_SENTINEL: u64 = 0x2000_0000;

// V-followup-prp-list 新增：
pub const PRP_LIST_SENTINEL: u64 = 0x3000_0000;       // PRP list 自身 page
pub const PRP_DATA_PAGE_BASE: u64 = 0x4000_0000;      // 第 N data page 的 GPA
pub const PRP_DATA_PAGE_STRIDE: u64 = NVME_PAGE_SIZE; // 各 page 间距 4 KiB
```

session 为 nlb > 16 IO 命令构造 sentinel：
- prp1 = PRP1_SENTINEL (第 0 page 的 data)
- prp2 = PRP_LIST_SENTINEL (PRP list page，含后续 N 个 page 的 GPA)
- list 内容 = [PRP_DATA_PAGE_BASE, PRP_DATA_PAGE_BASE + 4 KiB, ...]

### 3.2 session 端 capture decode 升级

`run_post_dispatch_async` 收 captured tcp_t.writes 后按 gpa 排序:
- gpa = CQ_BASE_GPA → CQE bytes (现有逻辑)
- gpa = PRP1_SENTINEL → 第 0 data page
- gpa ∈ [PRP_DATA_PAGE_BASE, +N*4KiB) → 第 (gpa - BASE) / 4KiB data page
- gpa = PRP2_SENTINEL → V5e-2 兼容 (单 IO ≤ 16 LBA 路径)

### 3.3 Read 路径

- N 个 dma_write captures (data) + 1 个 CQE write
- session 按 gpa 排序拼成 full data → 发单个 C2HData (≤ 64 KiB per MAXH2CDATA)
  或多个 C2HData 分片

### 3.4 Write 路径

- N 个 dma_read captures (data) + 1 个 CQE write
- session 按 gpa 排序确定 controller 要的页序 → 发 N 个 R2T 串行
- 每个 R2T 4 KiB → host H2CData → session 调 dma_read_complete

### 3.5 MDTS 同步升

新 V5_NLB_MAX = 256 (= 128 KiB / LBADS=9):
- MDTS = 5 (= 32 page = 128 KiB)
- 这是 V-interop-3 fix 之前的值；现可恢复

## 4. 测试矩阵

```
v_prp_list_anchor_session_sentinels: PRP1/PRP2/LIST/DATA_PAGE_BASE 不冲突
v_prp_list_io_read_3_pages_via_list: 12 KiB Read (3 page) byte-equal
v_prp_list_io_write_3_pages_via_list: 12 KiB Write (3 page) byte-equal
v_prp_list_io_read_128k_full_mdts:   128 KiB single IO (MDTS=5 边界) byte-equal
v_prp_list_mdts_anchored_in_identify: MDTS=5 in wire bytes
```

## 5. LOC 估算 + 风险

- session.rs / async_session.rs PRP-list path: ~200 LOC + ~150 LOC test
- regs.rs MDTS_PAGES_LOG2 = 5 (恢复)
- 风险：
  - R-1: capture order — controller 发 dma_write order 未必与 gpa 升序，
         session 必须按 gpa sort
  - R-2: 多 R2T per Write — V-followup-v4c 已支持 (实测 8 KiB 双 R2T)，复用即可
  - R-3: 单 C2HData > 64 KiB MAXH2CDATA 时拆分；MAXH2CDATA 已是 64 KiB
         (V8e-7-followup byte-identical gate fix)，128 KiB IO 需要 2 个 C2HData
  - R-4: 与 V-interop-3 (MDTS=1) 不向后兼容 — 必须先 PRP-list e2e 真互通才能
         lift MDTS，否则 host 按 5 发 > 8 KiB 仍被 V5_NLB_MAX=16 reject

## 6. 推荐实施顺序

1. session.rs 加 sentinel 常量 + anchor test
2. async_session.rs `run_post_dispatch_async` 收 captures 按 gpa sort + decode
   PRP list page 路径
3. Read e2e (3 page → 32 page) byte-equal
4. Write e2e (3 page → 32 page) byte-equal
5. MDTS=5 升回 + V-interop-3 anchor test 调整 (允许 1 ≤ MDTS ≤ 5)
6. Linux nvme-cli 真互通 (大 IO 验稳定)

## 7. 后续

完成后 V-followup-prp-list-2 可考虑：
- MAXH2CDATA 协商 (host advertise → 我们 cap)
- N-page IO 与 PI / Compare / Fused 组合
- 真生产 nvme-cli `dd bs=128k` + fio 多 jobs 压测

---

**注**: 此 plan 是 honest scoping — 估算 ~350 LOC + 5 regression test + 多轮
Linux nvme-cli 真互通验证。当前 V-followup-interop 套件已 263 lib test +
14 Python scenarios 真生产通过；V-followup-prp-list 在大 IO 价值显著但
工程量大，应作为独立 phase 实施。
