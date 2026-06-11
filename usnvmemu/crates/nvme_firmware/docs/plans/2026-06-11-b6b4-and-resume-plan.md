# 新会话续作计划 — B6b-4 多 LBA separate-metadata + 后续路线

> 写于 2026-06-11，承接一个超长会话（A1→D + B6b 单 LBA 闭环，十余个 reviewer-clean commit）。
> **本文档是新会话的起手依据**：照它就能无缝接续，不必回读旧会话。

---

## 0. 当前状态（green baseline）

- **分支**：`feat/usnvmemu`
- **最后绿基线 commit**：`04c76b22`（B6b-3 READ）。`cargo test --lib --no-default-features` = **148 passed**，clippy/fmt clean。
- **本会话已交付**（全 reviewer APPROVE 0 C/H）：

| 项 | commit | 一句话 |
|----|--------|--------|
| A1 Abort | `af9e72a2` + `6d832a38` | no-op stub→真中止 in-flight 命令；follow-up 修多-DMA partial-abort 泄漏 |
| B1 Reservation Preempt | `7020a2e8` | SPC-3/§6.11 完整 Preempt（registrant 注销/PRKEY=0 all-reg/conflict）|
| B6a MSET/FLBAS 极性 | `6df43815` | 修内联 metadata 错报 separate 的极性 bug |
| C1① MDTS | `0aabc8fc` | 计入 inline metadata + 全 opcode 适用矩阵审计 |
| C1② PRP-list chaining | `c8a1bb2b` | device→host >2 MiB 跟 chain pointer（forward scaffolding，激活待 MDTS 抬高）|
| D 教学边界 | `434d70b8` | persistent features(Save) + CSTS.CFS on flush 失败；③④记正当边界 |
| B6b-1 | `1950f033` | Format 接受 MSET=0（separate）+ `ns.meta_inline` 贯穿 + FLBAS 修正 |
| B6b-2 WRITE | `90536c52` | PRACT=0 separate：host 经 MPTR 供 PI→verify-before-store→interleave 落盘（单 LBA）|
| B6b-3 READ | `04c76b22` | PRACT=0 separate：读 interleaved→verify→data 回 PRP + PI 回 MPTR（单 LBA）|

**单 LBA separate-buffer WRITE+READ 闭环 spec-complete**。剩 **B6b-4 多 LBA**。

---

## 1. B6b-4 任务：多 LBA separate-metadata（WRITE+READ）

### 目标
把单 LBA separate 路径推广到 **N≤2**（dual-PRP data + MPTR 装 N×8 PI），逐 LBA verify。
**N>2（PRP-list data separate）显式 reject + 文档化为最终扩展**（复用 `PrpListOp` 机件，量大）。

### 设计（已部分起手，被本会话回退到 baseline；按此重做）

**数据模型（mod.rs）**：
- `SepMetaWriteAccum` 推广：删 `data: Option<Vec<u8>>`，加
  - `num_blocks: u32`
  - `data_pages: Vec<Option<Vec<u8>>>`（len=num_blocks，按 page_idx 填）
  - `data_remaining: u32`（初值 num_blocks）
  - 保留 `meta: Option<Vec<u8>>`（num_blocks×8）。
- `PendingOp::SepMetaWriteData { op_id, page_idx: u32 }`（加 page_idx）。
- `SepMetaReadAccum` **不用改**（它只是 `remaining: u32` 计数器；多 LBA 时 dispatch 多发几条
  DMA-write、remaining = N(data) + 1(meta) 即可）。

**WRITE dispatch（io.rs，`!pract && is_pi_capable && !meta_inline` 分支）**：
- 当前单 LBA：`nlb != 1 → INVALID_FIELD`。改为 `nlb > 2 → INVALID_FIELD`（N>2 PRP-list 待续）。
- N=1：data DMA-read prp1（page_idx 0）。N=2：prp1（page_idx 0）+ prp2（page_idx 1）。
- meta DMA-read mptr，长度 = `nlb * 8`。
- accum：`data_pages = vec![None; nlb]`，`data_remaining = nlb`，`meta = None`。

**WRITE finalize（completion.rs `sep_meta_write_finalize`）**：
- 触发：`data_remaining == 0 && meta.is_some()`（每条 SepMetaWriteData 到达 data_remaining-=1）。
- 先 **verify 全部 N 块**（block i：meta[i*8..i*8+8] vs data_pages[i] at lba+i，per pi_type）；
  **任一失败 → 错误 CQE（首个失败的 SC，经 to_sc()），不存任何块**（原子）。
- 全 OK → interleave 存 N 块（每块 [tuple][data] 或 [data][tuple] per pi_first），逐块 write_at。

**READ dispatch（io.rs，`!pract && is_pi_path && !meta_inline` 分支）**：
- `nlb > 2 → INVALID_FIELD`。读 N 个 interleaved block（slba..slba+N），拆 N data + N tuple。
- **verify 全部 N 块**（stored PI；失败 → Media SCT=2 错误，**首个失败类型** via to_sc()，同步返回，不 DMA-write）。
- 全 OK → DMA-write data page i 到（i==0 ? prp1 : prp2）+ meta(N×8 concat) 到 mptr。
  `remaining = nlb + 1`。

**READ completion（`SepMetaReadDone`）**：不用改（remaining 递减到 0 → success CQE）。

**清理 / abort**：accum 已接入 disable() / Format-inflight / A1 `try_abort_inflight`（B6b-2/3 已做）；
data_pages 的多条子-DMA 由 `pending_ios` sweep 清，无需额外改 A1。

### 测试（differential oracle）
- `b6b_separate_meta_write_multi`（N=2）：dispatch WRITE PRACT=0 nlb=2，feed 2 data + 1 meta（正确 PI）
  → backing 2 块 round-trip；篡改 block 1 的 host PI → 错误 + **两块都不落盘**（原子）。
- `b6b_separate_meta_read_multi`（N=2）：seed 2 interleaved 块 → READ nlb=2 → data 2 页 + tuple(16) 回送；
  block 1 stored PI 损坏 → Media 错误 + 无 host DMA-write。
- 每个都 **revert-verify**（注入 bug 确认转红）。

### 纪律（务必照旧）
implement → 差分 oracle 测试 → **revert-verify**（注 bug 确认红，再恢复）→ `cargo test --lib`
+ clippy + fmt 全绿 → **ecc:rust-reviewer subagent APPROVE**（0 C/H 才提交）→ **中文 conventional commit**。
spec 值（SC/offset 等）一律对 `nvme_spec` / 真 wire 核，**不手算、不信自家注释**。

---

## 2. 并发会话共存（重要）

本仓有**另一会话并行**改 `sgl.rs` / `pcie_device_*` / `experiments/` / `docs/DECISIONS.md` /
类型重命名（已完成 `OpenhclVsockTransport→PcieRemoteTransport`）。规矩：
- **只 `git add` 自己改的文件**，逐个显式 stage，**绝不 `git add -A`**。
- 不碰 `sgl.rs`、`pcie_device_core/`、`pcie_device_sdk/`、`experiments/`、`docs/DECISIONS.md`、
  `docs/plans/2026-06-11-scaffolding-sc-*.md`（都是对方的）。
- 若 `cargo test` 因对方未提交改动编译失败（如 sgl.rs），**不是你的 bug**——`cargo build --lib`
  验自己生产代码，等对方收尾或 sleep 等待。
- 提交前 `git status --short` 确认只有自己的文件。

---

## 3. 后续路线（B6b-4 之后，按优先级）

1. **B6b-4 N>2**（PRP-list-data separate）：data 走 `PrpListOp` 机件 + MPTR PI。量同当年 K4c。
2. **C1② chaining 真激活**：抬高 MDTS（IO Read >2 MiB）让 device→host chaining 走生产路径
   （注意 nvme-of transport 的 nlb cap 独立，需协调）。
3. **inline NS 的 PRACT=0**（host inline tuple，extended LBA）：另一条 host-PI 路径。
4. **PRCHK 逐项门控**（cdw12 bits 28:26）：当前一律 verify（over-strict）；真做时按 bit 解析。
5. 见 `docs/TEST_COVERAGE_PROGRESS.md` 的 follow-up + 早先功能缺口清单里 B 类剩余
   （Reservation Report EDS=1 + ptpls、Security/Virt 真实现等——多数偏离教学核心，按需）。

---

## 4. 如何开启新会话

1. `cd /home/xp/refs/openvmm`，确认在 `feat/usnvmemu`，`git log --oneline -12` 看到上表 commit。
2. **先读这三份**：本文件 → `usnvmemu/crates/nvme_firmware/docs/TEST_COVERAGE_PROGRESS.md`
   → `usnvmemu/docs/LESSONS.md`（§20 self-consistent 陷阱 / §23·26 无牙 / §30 scaffolding）。
3. 验绿基线：`cd usnvmemu/crates/nvme_firmware && cargo test --lib --no-default-features`
   应 ≥148 passed（对方可能又加了几个）。
4. 起手提示词建议：
   > 接续 B6b-4 多 LBA separate-metadata（按 docs/plans/2026-06-11-b6b4-and-resume-plan.md）。
   > 先做 N≤2（dual-PRP）的 WRITE+READ，逐 LBA verify，全段走 implement→test→revert-verify→
   > ecc:rust-reviewer→中文 commit 纪律；与并发会话选择性 staging 共存。
5. 记忆：相关条目已在 `memory/`（usnvmemu 结构 / spec-aligned-field-order / teaching=rigor /
   review-not-optional 等）；新会话会自动加载 MEMORY.md。
