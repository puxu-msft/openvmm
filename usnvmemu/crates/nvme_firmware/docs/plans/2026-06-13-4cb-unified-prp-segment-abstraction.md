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

- **P0（地基，纯增量无行为变化）**：prp.rs 加 `Prp2Role`/`prp2_role`/`SegLens`/
  `dispatch_segs`/`DispatchSegs` + 单测。无 caller。revert-verify=删符号仍编译。
- **P1（plain READ/WRITE 接入，行为逐字节不变）**：plain 三档判定 + prp2 校验改用
  `prp2_role`+`dispatch_segs`。O=0/O>0 都已 offset-aware，仅换 API。revert-verify 两向。
- **P2（finalize 收敛 + READ scatter 收敛）**：`PrpListOp` 单 `pi` 字段；4 finalize→1；
  N>2 行为不变，nlb≤2 暂仍旧 dual（下阶段切）。architect 核范式单一性。
- **P3（B 核心：消灭 4096/8 硬编码 + 补全 Dual→List，全路径全 tier 零例外）**：
  - inline nlb=1 READ/WRITE：去页对齐强拒；Dual(O≤4088:(4096-O,8+O)) / **List(O∈(4088,4095])**。
  - separate nlb=1：Single(O=0)/Dual(O>0:(4096-O,O))。
  - separate nlb=2：Dual(O=0)/**List(O>0,3 页)**。
  - 每路径 O=0 回归 + O>0 新档差分 oracle 全绿。
- **P4（roadmap，需详化 gate）**：评估 plain Dual 是否并入统一 List dispatch。
  建议裁定：plain Dual 保留直发（热路径零拷贝）但用 `dispatch_segs` 算长度（P1 已做），
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
