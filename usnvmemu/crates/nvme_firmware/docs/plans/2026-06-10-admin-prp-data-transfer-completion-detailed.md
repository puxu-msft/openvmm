# Plan — Admin 数据 DMA 的完整 PRP 处理（教学简化 → spec-complete）

> 状态：**P1 ✅ SHIPPED**（commit `5a87cf08`，2026-06-10，含 revert-verify + rust-reviewer
> 0 C/H/M）/ **P2 📋 待续**（>2 page PRP list，去 Get Log Page 8KiB 上限）。
> 类型：silent-corruption-class firmware core 手术 —— 必须走完整严谨循环
> （设计 → 实施 → unit + harness 测试 → **revert-verify** → rust-reviewer → commit）。

## What（精确缺口）

`controller/mod.rs::dma_write_then_complete(ctx, prp1, buf, ...)` 把**整个 `buf`
连续 DMA-write 到单个 `prp1` GPA**，完全无视 PRP2 与 PRP list。被 **7 个 admin
命令**复用（Identify / Get Log Page / Get Features 等，`grep dma_write_then_complete
src/controller/admin.rs`）。

后果（NVMe spec § 4.1.1 Physical Region Page）：
- **≤ 1 page (≤4 KiB)**：正确（单 PRP1 页）。Identify(4K)/SMART(512B) 等多数 admin 数据走这。
- **4–8 KiB**：**latent silent corruption** —— 把 page 1 连续写到 `prp1 + 4096`，
  但 spec 要求 page 1 落在 **PRP2** 指向的 GPA。host 的 PRP1/PRP2 页**不保证 GPA
  连续**（Linux/Windows 驱动常给非连续页）→ 数据写错地方。当前被 `bytes_req > 8192
  → INVALID_FIELD`（`admin.rs:755`）挡住 > 8 KiB，但 4–8 KiB 没挡 → 真 bug。
- **> 8 KiB**：直接拒（`nvme telemetry-log` / 大 Persistent Event Log 等取不到）。

作者已知（`admin.rs:750` 注释 "我们目前没实现 PRP list（Phase E TODO）"）。这是
README "未实现（按优先级排序）" 的 admin 侧数据通路欠债，属用户要的"教学替代 → spec 补全"。

## Why（值得做）

- **修真 bug**：4–8 KiB admin 数据在非连续 PRP 的真 host 上会 silent corrupt。
- **解锁真 driver 功能**：大 log（Telemetry 0x07/0x08、Persistent Event 0x0d）。
- **教学 = spec-complete 非玩具**（PRINCIPLES / [[teaching-means-rigor-not-toy]]）：
  admin 数据通路与 IO 通路在 PRP 处理上不该分叉。
- **可复用**：IO **read** 路径的 device→host PRP 写已实现且 reviewer-clean，是精确参考。

## 复用参考（关键 —— 不重造轮子）

IO read 把读出的数据经 PRP1+PRP2+list 写回 host，与 admin Get Log Page 的 device→host
写**同向**。现成机件：
- `controller/mod.rs::parse_prp_list(data) -> Vec<u64>`（解析 list 页）。
- `PendingOp::NvmReadDualPrpSiblingHalf`（2 页 sync：prp1 半 + prp2 半，一个负责 CQE）。
- `PendingOp::NvmReadPrpListFetch{op_id}` / `NvmReadPrpListData{op_id, page_idx}`
  （> 2 页 async：先 DMA-read list 页 → parse → per-page DMA-write → 全到齐 post CQE）。
- `completion.rs` 已有这些 arm 的处理（read 路径）。

实施时**优先抽出**一个中立 `write_buf_to_host_via_prp(ctx, prp1, prp2, buf, cid, sq_id,
sq_head, cq_id)`，IO read 的 device→host 写与 admin 都调它（DRY），而非给 admin 复制一套。
若抽取风险高，退而为 admin 加平行的 `AdminWriteDualPrp` / `AdminWritePrpList*` 变体。

## 子阶段

- **P1（≤2 页，sync）✅ SHIPPED `5a87cf08`**：`dma_write_then_complete` 加 `prp2` 参数 +
  按 buf size 分流：≤1 页单 PRP1（字节级不变）/ 2 页 page0→PRP1(`NvmReadDualPrpSiblingHalf`)
  + page1→PRP2(`NvmReadDmaWrite` completer)。**实施中发现 helper 非 admin-only**——io.rs 的
  Zone Report / Reservation Report 也用它且可 > 8 KiB，故 **> 2 页加了回退分支**（PRP2 是
  list 指针时回退旧连续写，留 P2），8 个 call site（6 admin + 2 io）全传 `sqe.prp2`。harness
  test `openhcl_get_log_page_noncontiguous_prp`（非连续 PRP + sentinel）revert-verified。
- **P2（> 2 页，async，PRP list）📋 待续**：把 P1 的 > 2 页**回退分支**换成真 list 处理。
  **复用路径已确认**：io.rs read > 2 页（`io.rs:880-918`）的 device→host list 写机件
  `PrpListOp{is_write:false, data_pages}` + `alloc_op_id` + `prp_list_ops` + 完成 arm
  `NvmReadPrpListFetch`/`NvmReadPrpListData`（completion.rs:1779/1852）可直接套——在
  `dma_write_then_complete` 的 > 2 页分支里：split buf 成 data_pages → insert PrpListOp
  (num_blocks:0, nsid:0) → `dma_read(prp2, PAGE)` 取 list 页。**起手须核**完成 arm 是否对
  num_blocks=0 跳过 SMART 计数（NvmReadDmaWrite 有 `if num_blocks>0` 守卫，list arm 须同样）。
  删 `admin.rs` 的 `bytes_req > 8192 → INVALID_FIELD` 上限。测试：> 8 KiB Get Log Page +
  guest-mem 放真 PRP list 页 + 非连续 entry，断言跨 list 正确落地 + revert-verify。
- **P3（清理）**：审 caller buf 上界；README "未实现" 删该条 / ROADMAP 记 SHIPPED。

## 测试计划（**用本会话新建的 OpenHCL harness** + 单元）

- **harness e2e**（`tests/openhcl_pcie_remote_e2e.rs`，guest-mem 可服务任意 ReadGpa/WriteGpa
  + PRP list 页）：
  1. P1：请求一个 6 KiB Get Log Page（如 Telemetry header+），PRP1 与 PRP2 设**故意
     非连续**的 GPA（如 prp1=0xA0000，prp2=0xB0000），断言 firmware 把 page 0 写到
     0xA0000、page 1 写到 0xB0000（而非 0xA1000）—— **直接抓 latent 非连续 bug**。
  2. P2：请求 > 8 KiB log，在 guest-mem 放一个 PRP list 页（prp2 指向它，含 page 2.. 的
     GPA），断言完整 log 跨 list 正确落地。
- **单元**（`controller/tests.rs`）：`CaptureTransport` 验 DMA write 落在正确 GPA。
- **revert-verify（强制）**：P1 修好后，临时把 prp2 写回退成 "连续写 prp1+4096"，确认
  harness 测试 FAIL（page 1 落错 GPA）；还原后 pass。否则测试没牙（[[review-not-optional-self-consistent-trap]]）。

## 风险 / 注意

- **改 shared helper 影响 7 命令**：P1 必须保证 ≤1 页路径**字节级不变**（Identify/SMART
  等回归）。先跑全量 firmware lib test（当前应 ~97 + 测）确认无回归。
- **多 token 完成顺序**：DMA completion 不保证按 issue 序到达 → 用计数器/op 结构追踪
  "还差几个 write"，全 0 才 post CQE（沿用 IO read 的 op 结构模式，勿假设顺序）。
- **error 路径**：某页 DMA-write 失败 → post error CQE 且**不**让 sibling 后到时再 post
  success 覆盖（completion.rs 已有此 retain 模式，照搬）。
- **prp2 语义**：≤2 页时 prp2 = page 1 的 GPA；> 2 页时 prp2 = PRP list 页指针。按
  `buf.len()` 分流（spec § 4.1.1）。

## Acceptance

- 7 call site 全过 `sqe.prp2`；helper 按 buf size 正确分流 1/2/list 页。
- harness 抓非连续 PRP（revert-verified）+ > 8 KiB log 跨 list 正确。
- 全量 firmware lib test + harness 4 test + clippy 0 warning；rust-reviewer 0 C/H/M。
- `admin.rs` 删 > 8 KiB 上限；README "未实现" 删该条 / ROADMAP 记 SHIPPED。

## 起手提示（新 context）

read 本 plan + [[openhcl-pcie-remote-e2e-harness]]（测试载体）+ `git show <dma_write_then_complete>`
现状。先 P1（含 revert-verify）单独 commit，再 P2。改 data-path 每步必 rust-reviewer。
