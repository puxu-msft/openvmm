# SGL × PI 组合 —— PI NS 上的 SGL 数据路径（separate / inline，全 PRACT）

> 裁判轴：**长远正确 + 设计良好 + 有意义的完整，不认 ROI/成本/回归面/"少见就拒"。**
> spec-legal 的 SGL×PI 情形都要正确支持（可分基础/高级阶段，绝不为最小化砍）。Metadata-SGL
> （高级）作**可拆后置阶段**，是 spec 允许的能力裁剪（不 advertise 即合法不支持），**非 YAGNI 砍**。
> 本文件持久化 scope + POC 后的 v1 设计，待 architect review → v2 → subagent-driven 执行。

## 背景与现状

`nvme_firmware` 的 SGL 数据路径（PSDT=10，Phase R2）目前**仅 plain NS**：[io.rs:1493](../../src/controller/io.rs)（READ）/ [io.rs:2493](../../src/controller/io.rs)（WRITE）`if is_sgl { if !is_plain { return INVALID_PROTECTION_INFO } }`。PI NS（separate/inline metadata）上发 SGL → 直接拒。这是 SPEC_CONFORMANCE 标的 `SGL×PI ✗`。

PI 数据路径（PRP 侧）已在 **#4c-b** 收口：全 tier PRP1 偏移 + `PiFinalize{layout}` 统一 finalize（`prp_pi_write_finalize` 拼回连续流→按逻辑单元重切→verify-all-then-store-all）。本任务 = 让 SGL 数据路径接上同一 PI finalize。

## POC #1 结论（承重假设已解除，2026-06-14）

**SGL data + PI metadata 用 MPTR 连续缓冲，非 Metadata-SGL** —— 三独立来源一致：
1. **firmware 自身**（决定性，本地可验）：[cmd.rs:716](../../src/controller/cmd.rs) `id.sgls = 0x0001_0001`（bit0 data-SGL + bit16 Bit Bucket，条件 bit20 SAOS），**未设 bit19（MPTR-contains-SGL / metadata-SGL）** → host 看 bit19=0 **必走连续 MPTR 元数据**。**无需改 advertisement。**
2. **Linux nvme 驱动**：默认 `cmnd->rw.metadata = meta_dma`（连续 MPTR）；metadata-SGL 仅 `NVME_CTRL_SGLS_*`（MSDS/bit19 类）置位才启用；SGL-data + MPTR-metadata 是正常 mixed-mode，data 的 PRP/SGL 选择与 metadata 描述正交。
3. **spec**：openvmm `nvme_spec` 有 `mptr` 字段 + `METADATA_SGL_LENGTH_INVALID` SC（证 metadata-SGL 是独立可选高级形态）。

⟹ **separate 直接复用现有 MPTR 机件；inline 把 extended-block 放 SGL data 流；都不需 metadata-SGL parser**（后者 = P-E 可后置）。Linux `nvme_pci_use_sgls()` 按传输大小/分段选 SGL 与 NS 是否 PI 无关 → **PI NS 大/碎传输真会发 SGL+PI**，feature 非死代码。

## 设计：SglOp 当 PI finalize 的第二个生产者（architect v2 已纳入 C-1/H-1/H-2/H-3/M-1/M-2/M-3）

**核心洞察**：`SglOp.data`（[mod.rs:841](../../src/controller/mod.rs)）已是**重组后的连续流**（frag 按 `stream_offset` 填）；`prp_pi_write_finalize`（[completion.rs:331](../../src/controller/completion.rs)）的核心（从 `full` 流 + `sep_meta` 起：长度校验→verify-all→atomic store-all）**已是 gather-agnostic**，只耦合 `PrpListOp.{data_pages,pi,lba,nsid,ids}`。抽成共享 helper，SglOp 也喂同一三元组。

§32 的几何教训在 SGL 下**更尖锐**（fragment 边界完全任意、可 1 字节）：一个 LBA / extended block 可跨任意多 fragment → reassemble-then-resplit 必须；inline 的"fragment 切分 vs data/tuple(4096/8) 切分"是两条正交切轴，finalize 只在**拼回流**上按 block_bytes/pi_first 切，绝不在 fragment 上切。

### 两个共享 helper（H-1/H-2 修正后的精确签名，PRACT 内建 M-2）

**`op.data` / 逻辑平面恒 dense，与 SGL scatter 几何彻底解耦**——verify 永远按 LBA 在 dense 流上切（`i*unit..(i+1)*unit`），SGL 的 fragment / Bit Bucket 只影响**哪些字节往 host 发**，不影响 verify/store 的逻辑切分。

- **WRITE helper** `pi_write_finalize_stream(ctx, stream: Vec<u8>, sep_meta: Option<Vec<u8>>, geom: &PiGeom, lba, num_blocks, ids: PiCqeIds)`：
  - `PiGeom{pi_type, pi_first, data_bytes, block_bytes, prchk, **pract: bool**}`（标量几何，**PRACT 内建**——PRACT=1 时 controller 自算 tuple 落盘，PRACT=0 verify host tuple）；`sep_meta` **按值传**（保 `meta` move 的零拷贝，**不传 `&PiFinalize`** 避免与 `sep_meta` 双载，H-1）。
  - **职责切分（H-1）**：helper 只验 **post-gather stream 长度** `stream.len() == num_blocks*stream_unit`（+ meta_short）；**coverage 校验**（SGL frag 覆盖和 / PRP 页和 == expected）归各 caller，调 helper 前做。
  - 逻辑同 completion.rs:373-481（separate interleave 存 / inline 原样存），照搬。
- **READ helper** `pi_read_verify_split(interleaved: &[u8], geom: &PiGeom, lba, num_blocks) -> Result<(data_plane: Vec<u8>, tuple_concat: Vec<u8>), (PiCheck, u64)>`（H-2 新增，**补 plan v1 漏掉的 READ 侧复用**）：读 backing 的 N 个 interleaved block → verify-all stored PI（PRACT=1 strip）→ 返 dense data 平面 + N×8 tuple concat。PRP separate READ 与 SGL separate READ **都调它**，避免两份 verify+split 漂移（§32 同类风险）。scatter/MPTR-dispatch 几何（页段 vs frag 段）各路径自留。

### C-1 解决：Bit Bucket 语义（separate / inline PI READ）

Bit Bucket = controller→host **data 输出丢弃**指令（host 不要这段数据）。对 PI READ：
- **op.data / 逻辑平面恒 dense**（separate=`nlb×data_bytes`、inline=`nlb×block_bytes`）；`pi_read_verify_split` 在 dense 平面上 verify-all（与 SGL 几何无关，bucket 不影响 verify 的 LBA 切分）。
- Bit Bucket 只**抑制该字节范围的 data scatter**（READ 时 `walk_offset += length` 但不 push frag，[completion.rs:2727](../../src/controller/completion.rs)，与现 plain 行为一致）；后续 frag 的 `stream_offset` 自然跳过被 bucket 的 dense 区间。
- **separate**：metadata 经 MPTR 是**独立传输**（host 用 MPTR 显式要了 N 个 tuple），**与 data 平面 bucket 无关 → N 个 tuple 恒全发 MPTR**。
- **inline**：tuple 在 data 流内，bucket 丢 data 范围可能连带丢掉落在该范围的 inline tuple 字节（host 自己的选择）；controller 仍在 dense 流上 verify-all + （WRITE 时）正确落盘。
- **coverage 校验**：bucket 已 advance `walk_offset`，故 `walk_offset==expected_bytes` 仍成立（expected = 逻辑平面大小）。
- **P-B entry POC（known-answer，C-1 gate）**：构造一个 **bucket 跨 LBA 边界**（如 separate 丢 LBA0 尾 + LBA1 头共 1500B）的 READ，断 verify 仍按 LBA dense 切、scatter 跳对区间、N 个 tuple 全到 MPTR。不依赖真驱动。

## 分阶段（每阶段=语义单元=单 commit，含 reviewer gate + 差分 oracle 四件套 + revert-verify）

### P-A（execution-ready）— 抽两个 gather-agnostic PI helper（纯重构，行为不变）

抽 **WRITE helper** `pi_write_finalize_stream` + **READ helper** `pi_read_verify_split`（签名见「设计」，**`PiGeom` 含 `pract` 从 day-1**，M-2——PRACT=1 是 helper 内 compute/strip 分支，非后续阶段结构性追加）。
- `prp_pi_write_finalize` 改为：`op.data_pages.flatten()` 拼 `full` + 取 `op.pi` 的几何 → 调 WRITE helper（`sep_meta` move 进）。PRP separate READ 改调 READ helper做 verify+split。**行为逐字节不变**（b6b/b6c/n_gt_2 + #4c-b P3 测试守）。
- **H-1 职责**：helper 验 post-gather stream 长度；coverage 校验留 caller。**保 `meta` 零拷贝 move**（不传 `&PiFinalize`）。
- **revert-verify**：helper 切轴/PRACT 分支弄反 → 既有 PI 测试转红。
- 不动 SGL、不动 dispatch。**最先做，降后续风险。**

### P-B（execution-ready after P-A；含 C-1 entry POC）— SGL × separate-meta（PRACT 0/1，MPTR 连续元数据）

解禁 `is_plain` 门（[io.rs:1493/2493](../../src/controller/io.rs)）的 separate 分支；`SglOp` 加 `pi: Option<PiFinalize>`（复用 `PiLayout::Separate{mptr,meta,meta_pending}`，H-3）。
- **MPTR 双门控模型（H-3，承重，必须照此建）**：
  - MPTR 那条 DMA **不计入** `transfers_total`（[completion.rs:2868](../../src/controller/completion.rs) 仍 `=frags.len()`，否则 NvmSglData 的 `frags.get(frag_idx)` 越界/错位）。
  - MPTR 走**独立 PendingOp 变体** `NvmSglSepMeta{op_id}`（**非** `NvmSglData`），镜像 PRP 的 `Nvm{Read,Write}PrpListSepMeta`。
  - data 平面 `transfers_done==transfers_total` **且** meta gate（`PiLayout::Separate.meta_pending`/`meta`）满足才 finalize；**两个完成点（末 data frag 到 / MPTR 到）各查合取**，同 [completion.rs:2595-2599](../../src/controller/completion.rs)。
- **`finish_sgl_done` 加 PI 分支（M-1，防双计）**：PI op → 调 helper 后**立即 `return`**，**绝不**落到 plain 的 `ns.write_at(&op.data, op.lba*sector)`（[completion.rs:2920-2923](../../src/controller/completion.rs)）+ `advance_zns_wp`/`stat_*`（helper 内已做，[completion.rs:464-466](../../src/controller/completion.rs)）。`SglOp.sector_bytes` 的 backing-offset 双角色是陷阱——PI 路径只经 helper。
- **WRITE**：expected_bytes=`nlb×data_bytes`；frags gather 纯 data 流 + `NvmSglSepMeta` `guest_read(mptr, nlb*8)`；双门控满足 → WRITE helper（sep_meta=Some）。
- **READ**：READ helper 在 backing N 块上 verify+split → `op.data`=dense 纯 data、`tuple_concat` 入 `meta`；`start_sgl_transfer` scatter data per-frag + `NvmSglSepMeta` `guest_write(mptr, tuple_concat)`；双门控 → success。Bit Bucket 按 C-1 处理。
- **校验**：`sqe.mptr!=0`（L-2，在 `dispatch_sgl_write/read` **alloc op 前**同步查 INVALID_FIELD）；coverage `walk_offset==expected_bytes`（data 平面）。
- **C-1 entry POC + 差分 oracle**（known-answer，不靠真驱动）：一 LBA data 跨 frag（[3000,1096]）、**bucket 跨 LBA 边界**、多 LBA 不规则 frag、0-length DataBlock 跳过、MPTR(gpa,16)。断每条 frag/MPTR 的 DmaRead/Write (gpa,len) + backing interleaved 逐字节 + PiTuple::compute 独立 oracle + 断走 SglOp（accum map + PendingOp 双证）+ stored-PI 失败→Media SCT=2 不 scatter。

### P-C（execution-ready，依赖 P-A/P-B + 详化 gate）— SGL × inline-meta（PRACT 0/1）

`op.data`=dense extended-block 流（`nlb×block_bytes=4104`，meta 在流内）；无 MPTR（更简）。
- **WRITE**：expected=`nlb×block_bytes`；frags gather → WRITE helper（sep_meta=None，inline 切 data/tuple）→ store-as-is（经 `finish_sgl_done` PI 分支 + return）。
- **READ**：READ helper verify（inline 切轴）→ `op.data`=dense extended-block 流 → scatter per-frag。
- **正交切轴守卫**：extended block 跨 fragment 时 finalize 只在拼回流上按 block_bytes 重切、再按 pi_first 切 data/tuple——绝不在 fragment 上切。oracle 必含「extended block 跨 fragment」+「CMB-relative inline」（M-3）。
- **详化 gate**：进入前确认 P-B 的 helper 接口 + `finish_sgl_done` PI 分支稳定。

### ~~P-D~~（已并入 P-A/P-B/P-C，M-2）— PRACT=1 不再独立阶段

PRACT=1 是「谁产 tuple」的 per-block 决策（controller gen/strip vs host），非独立 transport。**`PiGeom.pract` 从 P-A 内建 → P-B/P-C 各自带 PRACT 0/1 两分支**，避免先建 PRACT=0-only 形状再回填。gen=`PiTuple::compute`（[completion.rs:1144](../../src/controller/completion.rs) Zone Append 同款）落盘；strip=verify 后不回 tuple。

### P-E（roadmap，高级可后置，裁判轴允许）— Metadata SGL Segment（advertise bit19/MSDS + parser）

advertise SGLS bit19 + metadata-SGL descriptor parser，让 metadata 本身也 SGL。**spec 允许不 advertise 即合法不支持**，纯增量零回归。详化 gate：仅当真需求才进；进入前查 spec § + Linux `nvme_pci_setup_meta_sgls` 精确布局。

## 测试矩阵（差分 oracle 四件套，§23/§32 具体化）

每 case：① backing/`PiTuple::compute` 期望；② **显式断每条 SGL fragment 的 DmaRead/DmaWrite (gpa,len)**（含 0-length 跳过、Bit Bucket 跳段、MPTR 那条）——CaptureTransport 不截断喂入，光断 backing 抓不到「frag 切错/漏 frag」；③ **断走 SglOp（非 PrpListOp/bespoke）**（accum map + PendingOp 变体双证）；④ revert-verify 两向（把 frag 切分/verify/MPTR 步骤退回错误形态 → 断言转红）。

**必测临界**：单 frag=整传输 / 一 LBA 跨 N frag（不规则 [3000,1096] 类）/ 多 LBA 不对齐 frag / extended block 跨 frag（inline）/ 0-length DataBlock / **Bit Bucket 跨 LBA 边界（C-1，READ data 平面丢 1500B 跨 LBA0/1）** / **CMB-relative SGL data × PI（inline + separate）** / PRACT=0 vs PRACT=1（host tuple vs controller gen/strip）/ MPTR 非 0 校验 / fragment 覆盖 ≠ expected（DATA_SGL_LENGTH_INVALID）/ stored-PI verify 失败（Media SCT=2 不 scatter）。

**回归**：plain SGL（现有 R2 测试）+ 所有 PRP PI 路径（#4c-b，因 P-A 改其 finalize/verify 接 helper）逐字节不漂移——P-A 后立即全跑。

## 共享树多会话纪律
本 crate 有并行会话同改 io.rs/mod.rs/completion.rs。开工 `neighbors_list` + `neighbors_status`，改前后 `neighbors_send` 对齐区域。**提交隔离**：不同文件 `git commit -m … -- pathspec`；同文件与别会话 hunk 共存 → filtered-patch（`git diff HEAD file` 看全 hunk → `git apply --cached` 自己的 → 裸 commit）；**committer 侧防御**：pathspec commit 前即便开工时干净也 `git diff HEAD -- <file>` 重核 hunk（并发会话会在 check 与 commit 间追加，见 `[[git-commit-shared-index-multisession]]`）。绝不 `git add -A`；fmt 走 `rustfmt --edition 2024 <自己文件>` 不在共享树跑 `cargo fmt`。

## 源码锚点
- **复用**：`prp_pi_write_finalize`（completion.rs:331，P-A 抽 WRITE+READ 两 helper 核心）；`PiFinalize`/`PiLayout`（mod.rs:703，`Separate.meta_pending` 给 SglOp 双门控复用）；MPTR 机件（io.rs PRP separate 路径 1283-1454/2324-2395）；`SglOp`/`SglPlanFrag`（mod.rs:825-864）；`start_sgl_transfer`/`finish_sgl_done`（completion.rs:2845/2914，PI 插入点 + M-1 PI 分支）；`dispatch_sgl_{read,write}`（io.rs:506/607）；PRACT=1 gen `PiTuple::compute`（completion.rs:1144）。
- **改**：io.rs SGL 分流 `is_plain` 门（1493/2493）；`SglOp` 加 `pi: Option<PiFinalize>`（mod.rs）；新 PendingOp `NvmSglSepMeta{op_id}`（mod.rs，**独立于** NvmSglData，H-3）；completion.rs SGL finalize 接 helper + `finish_sgl_done` 加 PI 分支（return 防双计 M-1）。

## 显式标注的边界（正交，不在本任务）
1. Metadata-SGL（P-E）：spec 允许不 advertise 即不支持，作可拆高级阶段。
2. 非标准 PI NS 布局（block≠4104）：#3 子系统级，与 PRP 侧同 defer。
3. SGL Keyed/Transport descriptor（NVMe-oF）：现已 reject，本任务不扩。
4. **Fused（Compare+Write）× SGL PI**（L-1）：若 fused-SGL 现已上游 reject 则保持，本任务不扩；进 P-B 时确认其拒点不被 PI 解禁误开。

## CMB-relative SGL × PI 注记（M-3）
- **MPTR 恒是 flat GPA**（spec：metadata pointer 不是 SGL descriptor），**无 CMB-relative rebase**；CMB-relative（sub_type=1）只作用于 **SGL data descriptor**，parse 时已 rebase 成 `cba+offset`（[io.rs:375](../../src/controller/io.rs)）→ 到 `guest_read/write` 已是普通 GPA，**data 平面 CMB-relative × PI 自然可用**。
- inline PI 的 4104-stride extended block 落 CMB 时，确认与 CMB 窗口边界检查不冲突（测试矩阵加 CMB-relative × PI inline+separate 行）。

## 承重假设需 POC（状态）
- ✅ **#1 metadata 形态**（MPTR vs metadata-SGL）：POC 完成，MPTR-only 可行（见 POC 结论）。
- ✅ **C-1 Bit Bucket 语义**：设计已解（op.data dense LBA-indexed + bucket 仅抑制 scatter + tuple 恒全发 MPTR，见「设计」C-1）；**P-B e2e known-answer 测试（bucket 跨 LBA）已落地并绿**，不依赖真驱动。
- ✅ **进 P-B 前**：C-1 known-answer + 「SGL frag 跨 LBA + MPTR tuple」helper 接管已由 P-B e2e 测试覆盖。

## 实施进度（live）
- ✅ **P-A 完成**（commit `0678946ca`）：抽 `pi_write_finalize_stream` + `pi_read_verify_split` 两个 gather-agnostic PI helper（`PiGeom` 含 `pract` day-1，M-2），PRP 既有 finalize/separate READ 改调，纯重构行为不变；3 known-answer 直测 + revert-verify；rust-reviewer APPROVE 0 C/H。
- ✅ **P-B 完成**（commit `05139ba6f`）：SGL × separate-meta（PRACT 0/1）。**关键前提发现**：PI NS 上 PRP PI 处理块（io.rs `if !pract && is_pi_path`）拦截所有 PI-NS 命令含 SGL（prp1=0 误处理），故在其**之前**加 `if is_sgl && is_pi_path` 早路由。`SglOp.pi`+`pract` 复用 `PiLayout::Separate` 双门控（抽 `pi_*` 自由函数单一真相源，PRP/SGL 共享，§32 anti-drift）；`NvmSglSepMeta` MPTR 子-DMA 不计入 transfers_total（H-3）；`try_finish_sgl_pi` 双门控合取；`finish_sgl_done` PI 分支（M-1 防双计）。5 个 e2e 差分 oracle（含 C-1 bucket 跨 LBA）+ revert-verify 双门控 load-bearing；rust-reviewer APPROVE 0 C/H（M1 DRY 收敛 + L1 clippy allow）。SPEC_CONFORMANCE 同步：SGL×PI separate-meta ✅ / inline ⏳。
- ⏳ **P-C（inline-meta，PRACT 0/1）**：io.rs 早路由对 `meta_inline_r` 暂拒 INVALID_PROTECTION_INFO，待 P-C 解禁。详化 gate 见 P-C 节。
- ⏳ **P-E（Metadata SGL，roadmap）**：未启动。

