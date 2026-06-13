# #4c-b：PI 路径 PRP1 页内偏移 —— 统一 PRP-segment 抽象（方案 B，全 tier 零例外）

> 裁判轴：**长远正确 + 设计良好 + 有意义的完整，不认 ROI/成本/回归面/"少见就拒"。**
> spec-legal 的 PRP1 偏移情形**无一例外**都要正确支持（可分阶段，绝不砍）。
> 本文件持久化 architect 两轮细化后的 execution-ready 设计，供本会话/跨会话执行。

## 背景与现状

#4 系列让 controller 支持 PRP1 任意页内偏移（spec NVMe Base §4.1.1：仅 PRP1 可带页内
偏移 O；PRP2/list-entry 须页对齐；传输 >2 页时 PRP2 是 PRP-list 指针）。已提交：
- `prp.rs` 几何内核：`prp1_offset / first_seg_len / total_pages / page_size / tier`
  （O=0≡legacy 有 `offset_zero_matches_legacy` 守；`inline_block_4104_tiers` 已覆盖
  O>4088→List corner）。
- #4c-a：inline nlb≥2、separate N>2 —— PrpListOp + reassemble-then-resplit finalize，
  **恒 List 档**，已 offset-aware（commit c48b3f095 / f14ba3c97）。

## 执行进度（2026-06-13，交接锚点）

**已提交（均经 rust-reviewer APPROVE 0 C/H、173+ non-cmb tests、clippy/fmt clean、index/filtered-patch 提交）：**
- ✅ **P0** `54f18713d` — prp.rs 统一段抽象地基：`Prp2Role`/`prp2_role`、`SegLens`、
  `DispatchSegs`/`dispatch_segs`（后两者仍 `#[allow(dead_code)]`，**P3 消费后移除**）。
- ✅ **P1** `6a1883197` — plain READ/WRITE 接入 `validate_prp2`（io.rs free fn，consumes
  `prp2_role`）。行为逐字节不变。
- ✅ **P2-a** `6f0119efe` — 两个 PRP-list PI WRITE finalize 合并为 `prp_pi_write_finalize`
  （completion.rs）；删 `SepMetaPrp.data_bytes_total` 死字段；separate meta 用 take()+move。
- ✅ **P2-b** `c2c55865a` — `PrpListOp.{sep_meta,inline_pi}` → 单 `pi: Option<PiFinalize>`；
  新增 `PiFinalize{pi_type,pi_first,data_bytes,block_bytes,prchk,layout}` +
  `PiLayout{Inline | Separate{mptr,meta,meta_pending}}`（mod.rs）；7 个 helper
  （`is_inline_pi`/`is_separate_pi`/`meta_done`/`meta_ready`/`set_separate_meta`/
  `take_separate_meta_for_scatter`/`clear_separate_meta_pending`）把 layout 派发收敛一处。

**当前代码状态（P3 起点）：**
- `prp.rs`：几何内核 + `dispatch_segs(prp1,prp2,total)->DispatchSegs{Single|Dual|List}` +
  `SegLens`（段长迭代器）+ `prp2_role`/`validate_prp2`（io.rs）全部就绪、有单测。
- `PrpListOp.pi: Option<PiFinalize>` 单字段 + helper 已就位；统一 `prp_pi_write_finalize`
  按 `PiLayout` 分派（inline 原样存 / separate interleave 存），**reassemble-then-resplit
  offset-aware**。P3 的新构造点直接填 `pi: Some(PiFinalize{layout})`、READ 走 List scatter。
- **nlb≤2 PI 路径仍是旧 bespoke dual**（未改）：inline nlb=1 走 `InlineMetaWriteSeg`/
  `InlineMetaWriteAccum`（io.rs ~2084-2176 WRITE、~977-1097 READ，硬编码 (4096,8)、
  `M-2` 强拒非页对齐 PRP1）；separate nlb=1/2 走 `SepMetaWriteAccum`/`SepMetaReadAccum`
  （io.rs ~2332-2391 WRITE、~1282-1403 READ，`data_prps=[prp1,prp2]` 整页、无 first_seg_len）。
  **这就是 P3 要消灭/补全的对象。**

## 共享树多会话纪律（本任务实测，交接必读）
- 本 crate 有并行 CMB 会话（silver-lynx 等）同改 io.rs/mod.rs/completion.rs。用 neighbors
  MCP（`neighbors_list`/`neighbors_status`/`neighbors_send`，from 填本会话别名）对齐区域。
- 提交隔离（见 `[[git-commit-shared-index-multisession]]` / `[[shared-worktree-fmt-hazard-index-commit]]`）：
  **不同文件** `git commit -- pathspec`；**同一文件与别会话 hunk 共存**→ filtered-patch
  （`git diff HEAD file > /tmp/x.diff`，删别人 hunk，`git apply --cached` 自己的 → 裸 commit）；
  **绝不** `git add -A`；fmt 走 /tmp 重建不在共享树跑。每步落绿即提交，不让红工作树挡别人 build。



**散落问题（B 要统一）**：PRP1 偏移处理散在 6+ 条 dispatch 路径，nlb≤2 的 PI 路径
（inline nlb=1、separate nlb=1/2）走 bespoke dual-PRP，**硬编码 4096/8、强拒非页对齐
PRP1**，无法吃下 Dual 的 split-点左移与 Dual→List 迁移。4 个 finalize（plain/sep/inline
各 WRITE + READ scatter）中 N>2 两个已是统一范式，nlb≤2 是其未统一的退化特例。

## 设计（B）：correct-by-construction 的统一段抽象

### 段几何两层（prp.rs，零分配借用）
1. **`SegLens`**（纯几何长度序列迭代器，dispatch + finalize 共用）：包装 `total_pages`/
   `page_size`，O(1) 借用，yield 每段逻辑字节数。
2. **`dispatch_segs(prp1, prp2, total) -> DispatchSegs`**（dispatch 期已知段）：
   - `Single{prp1,len=total}`
   - `Dual{prp1,len0=first_seg, prp2,len1=total-first}`
   - `List{prp1,first_len}`（余段经 list 页 fetch，接现有 offset-aware List 机件）

### PRP2 语义单点（消灭 8 处各自判）
`prp2_role(offset,total) -> Prp2Role{Unused|DataPage|ListPage}`（= tier 的语义映射）。
所有 dispatch 入口统一：`Unused`→不校验 prp2；`DataPage|ListPage`→prp2!=0 且页对齐
（非对齐→`PRP_OFFSET_INVALID`，0→`INVALID_FIELD`）。
注：`total` 对 inline = `nlb*block_bytes`，separate/plain = `nlb*data_bytes`。

### finalize 收敛（4→1）
`PrpListOp.{sep_meta,inline_pi}` → 单 `pi: Option<PiFinalize{layout, pi_type, pi_first,
data_bytes, block_bytes, prchk, mptr}>`，`PiLayout{Inline | Separate{meta}}`。
统一 `prp_pi_write_finalize`：reassemble host 段→连续流→按逻辑单元 resplit→verify-all→
原子 store-all（inline 原样写；separate 逐块 interleave + tuple 来自 meta）。
READ 侧 `prp_pi_read_scatter`：盘上 verify-all→按 SegLens 切 data_pages→走 List scatter
机件（+ separate MPTR meta 写）。dual accum（`SepMetaReadAccum`/`InlineMetaReadAccum`）退役。

## 分阶段（每阶段=语义单元=单 commit，含 reviewer gate + revert-verify）

- **P0 ✅ `54f18713d`**（地基）／**P1 ✅ `6a1883197`**（plain 接入 validate_prp2）／
  **P2-a ✅ `6f0119efe`**（finalize 合一）／**P2-b ✅ `c2c55865a`**（单 pi 字段 + 7 helper）。
  详见上「执行进度」。

- **P3（B 核心，execution-ready，下一步做）：消灭 nlb=1/2 PI 路径 4096/8 硬编码 +
  补全 Dual→List，全路径全 tier 零 spec-legal 例外。** 逐路径（每条 READ+WRITE 对称）：

  - **separate nlb=2（最先做，最省、复用已证机件）**：当前 `nlb==2` 走 dual-PRP
    `SepMetaWriteAccum`/`SepMetaReadAccum`（io.rs ~2336 WRITE / ~1286 READ）。tier 分析：
    total=8192，O=0→Dual、**O>0→List(3 页)**。做法：把分流判据从 `nlb>2` 改为
    **`prp::tier(prp1_offset(prp1), nlb*data_bytes)==List`**（让 tier 当唯一裁判，
    不要 `nlb==2&&O>0` 这种脆弱耦合），List 时路由进**已 offset-aware 的 N>2 PrpListOp
    路径**（构造 `pi: Some(PiFinalize{layout: Separate{...}})`，total_pages=prp::total_pages）；
    `tier==Dual` 仍走旧 dual。注意保留 `nlb==2&&prp2==0→INVALID_FIELD`（O=0 缺 PRP2）。
  - **separate nlb=1**：total=4096，O=0→Single（1 条 PRP1 sub-DMA）/ O>0→Dual
    `(4096−O@PRP1, O@PRP2)`（**永不 List**）。1 个 LBA 在 O>0 跨 2 host 段 → 用
    `dispatch_segs`/`SegLens` 算两段 sub-DMA，finalize 拼回（**2 段 trivial 顺序拼接，
    不上 PrpListOp**——复用 `SepMeta*Accum`，把"per-LBA 一页"语义改成"per-host-segment"，
    同 `prp_pi_write_finalize` 的拼回-重切但 N=1 段=2）。
  - **inline nlb=1**：block=4104，当前 `InlineMetaWriteSeg`(io.rs ~2131/2146 读固定 4096+8、
    READ ~1059 切固定 4096/8) + `M-2` 强拒非页对齐 PRP1。tier：O=0→Dual(4096,8) /
    O∈(0,4088]→Dual split 左移 `(4096−O, 8+O)` / **O∈(4088,4095]→List(3 段，真做不拒)**。
    Dual：把 `InlineMetaWriteAccum`/`finalize`（completion.rs:208 的 `prp1.len()!=4096||
    prp2.len()!=8` **硬编码长度校验是最高危回归点，必须改成 `(4096−O, 8+O)`**）+
    `InlineMetaReadDone` 的 split 全改用 `dispatch_segs`（**WRITE 拼 / READ 切用同一段表达式，
    否则 silent PI mismatch**）；List：路由进 inline-PI PrpListOp 路径（`pi:Some(layout:Inline)`）。
  - **PRP2 校验**：所有路径 dispatch 入口统一调 `validate_prp2(prp2, prp1_offset(prp1),
    total)`（P1 已建），删各路径手写的页对齐/非零判定。
  - 每路径 O=0 回归（与改造前逐字节一致）+ O>0 新档差分 oracle 全绿；
    `dispatch_segs`/`SegLens` 在此被消费 → 移除其 `#[allow(dead_code)]`。

- **P4（roadmap，需详化 gate）**：评估 plain Dual 是否并入统一 List dispatch。
  建议裁定：plain Dual 保留直发（热路径零拷贝）但用 `dispatch_segs` 算长度，
  不强并 List（无正确性缺口）。进入前确认无 spec-legal plain 路径被遗漏。


## 测试矩阵（差分 oracle 四件套）

每 case：① backing/PiTuple::compute 期望；② **显式断每条 DmaRead/DmaWrite 的 (gpa,len)**
（抓 4096/8 硬编码盲点）；③ **断走哪个 tier 分支**（Dual vs List 选择）；④ revert-verify
两向（write→read 回逐字节；revert 目标针对该路径真实盲点如"4096/8 硬编码"非"page0 固定4096"）。

**必测临界**：
- inline nlb=1：O=0/100/4088（Dual）、**O=4090/4095（List，corner 真跑）**。
- separate nlb=1：O=0（Single）、O=100（Dual）。
- separate nlb=2：**O=0（Dual）vs O=1（List）边界两侧断分支**。
- Dual→List 边界两侧：inline O=4088 vs 4089；separate nlb=2 O=0 vs 1。
- 回归：plain/inline≥2/separate N>2 在 O=0/1/4095 逐字节。
- proptest：任意 (path,tier,O,nlb) 断"段长和==预期 host 字节"+"首段==first_seg_len(O)"
  +"中段全整页"（独立 oracle）。

## 回归保障（已证 5 路径 O=0 逐字节断言点）
plain RW（W6c 真机）、inline nlb=1 RW、separate nlb≤2 RW、N>2 —— 每阶段改完跑全套→
临时 revert 该路径→确认旧测试同样绿（证 O=0 行为未漂移）。

## 显式标注的边界（正交扩展，非 PRP 偏移问题，不在 #4c-b）
1. 标准 PI NS 限定（block=4104）：非标准 lbads+meta 布局是 #3 子系统级，正交 backlog。
2. plain Dual 直发（P4）：性能取舍，非 spec 例外，所有 O 都正确。
3. SGL×PI 组合：正交，独立 backlog。

## 承重假设需 POC
P3 "inline nlb=1 O>4088 升 List"：spec-legal 必须支持。P3 入前写 known-answer 单测
直构造该 SQE 验 List 机件正确接管（不依赖真 driver 触发）= 设计期最廉价 oracle。

## 源码锚点
- prp.rs（P0 落点）；io.rs 6 条 dispatch（P1/P3）；completion.rs 4 finalize→1（P2）+
  List gather/scatter 机件（已 offset-aware 复用）；mod.rs `PrpListOp.{sep_meta,inline_pi}`
  →单 `pi`（P2）+ dual accum 退役。
