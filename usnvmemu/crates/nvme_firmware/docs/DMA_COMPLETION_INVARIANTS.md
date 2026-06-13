# DMA-in-completion 不变式台账（guest 触发的 host 存活性 DoS 防线）

> **这是 live 工件,不是叙事**。它枚举 controller 每一条"completion 回调里再发起后续 DMA"
> 的路径,记录各自的**终止机制**与**firing 测试**,作为新路径的回归闸。教训叙事见
> `usnvmemu/docs/LESSONS.md` §29(自馈 async poll 循环);本台账是 §29 立的纪律的可核查落地。

## 威胁模型(为什么需要这张表)

controller 的 IO 状态机是 **token-异步**模型:命令处理时发起 `dma_read`/`dma_write`/
`guest_read`/`guest_write`,完成经 `on_dma_complete`(→ `controller/completion.rs`
`on_dma_complete_impl`)按 `PendingOp` variant 派发。危险模式:**某些 completion 分支会再
发起后续 DMA**。

在**同步-drain transport**(如 `vfio_user_transport/src/session.rs:220`
`drain_dma_completions` 是无界 `while let Some(c) = pop_front()` 循环)上,若 guest 能构造
"完成里再发请求且永不收敛"的输入(SGL segment 自环、PRP-list chain pointer 自环……),
host 线程会在**一次 drain 内无限自旋** = guest 触发的存活性 DoS(§29 point-3 实证)。
OpenHCL async transport 上同样的链只是"软浪费"(帧交错),同步路径才硬挂——但封顶放
device 侧,两条 transport 都受益(见下 I3)。

## 承重不变式

**I1 — 封顶量必须是"hop/自续深度"而非"进展(advance/ring 距离)"。**
任何"下一跳地址由 guest 数据决定(可自环)"的递归 DMA-in-completion 路径,**必须**带一个
**计 hop 数 / 自续深度**的上限,在**发起下一跳之前**检查。§29 三次试错的裁定:计 advance
(推进命令数)会误伤真高吞吐 driver(稳态合法推进无界);计 ring 距离脆弱且 dead;只有
计"每链自续深度,仅链起始重置"既不误伤又能终止。**计进展型 cap = 漏洞**(会被振荡/自环绕过)。

**I2 —（✅ 已强制）CFS 置位后,`on_dma_complete_impl` 对任何 token O(1) 终止性消化、不再
派生 DMA。** 入口门控:`if self.csts & CFS != 0 { 消费 token + return }`(completion.rs 顶部,
debug_assert 之后),在任何路由（shadow-poll 续读 / PRP·SGL scatter / CMB cascade）之前短路。
残留 in-flight 表项由 mandatory reset 收（`disable` 清 `pending_ios`/`prp_list_ops`/`sgl_ops`/
`pi_reads`/… 全表,enable.rs）。**前提:`disable`(CC.EN→0)必须清 CSTS.CFS（spec § 3.1.4.2）**——
否则 CFS 跨 reset 粘滞,重新 enable 的 controller 虽 RDY=1 但门控继续拦截所有新 completion =
砖化（test `disable_clears_cfs_so_gate_does_not_brick_on_recovery` 守此前提）。firing 测试
`cfs_set_completion_derives_no_further_dma`（controller/tests.rs：置 CFS 后喂会 scatter 的
PRP-list 完成 → 断言 0 条新 DMA;TDD red 实测无门控时 scatter 出 3 条）。
- **澄清(纠正早期措辞)**:这一类的**存活性闭合靠 I1(每条链各自 hop cap)**,**不**靠 I2——
  即便某条链撞 cap 置 CFS 后别的 in-flight 完成仍正常处理,只要每条链各自有界,drain 循环
  仍终止。I2 关乎的是「CFS 后不再做无谓/不合规的工作」(NVMe spec:CFS = controller fatal,
  host 须 reset),属正确性/整洁 + 收紧 cap 撞顶后的收链语义,**非 DoS 存活性闭合条件**。
- transport 侧:**不该**强行 `clear()` 自己的队列(它不持有 token↔链 的语义,强清会让 device 的
  `pending_ios` 表与队列失步、泄漏 op)——这一条与 I2 恒成立。

**I3 — 封顶归属在 device 侧(`pcie_device_core` 的 controller),不在 transport(session.rs)。**
`pcie_device_core` 是 runtime-agnostic 的,device 不该假设 transport 同步性。挂死的根因量
(自续深度)是 device 的属性。把 cap 放 device → 一处正确、三条 transport(vfio / NVMe-oF
TCP / OpenHCL)全受益:同步 transport 得硬终止,async transport 自动得"软浪费也有界"。
这正是 `MAX_SHADOW_POLL_ITERS`/`MAX_CMB_DRAIN_ITERS` 放在 controller 而非 session.rs 的原因。
**`session.rs` 的 drain 循环不得加迭代上限**:其合法 worst-case = (in-flight 链数) ×
(per-链 cap),扁平常量要么取小了误伤真负载(= §29 "挡 advance"的重演),要么取大成 dead cap;
且 transport 拿不到"哪条链在自馈"的语义,无法区分"100 条健康链各推进一次"与"1 条链自馈
100 次"。(可选:session 侧可加一个**仅 debug** 的烟雾哨兵——单次 drain 迭代数超过"绝无
可能的合法上限"时 `debug_assert` panic,报"某条 device 链漏了 per-path cap"。release 零开销,
非运行时 DoS 防护。)

## 台账:会发起后续 DMA 的路径(分类 a/b/c;其余 d=终端见末尾)

锚点:enum `controller/mod.rs` `PendingOp`;分发 `controller/completion.rs`
`on_dma_complete_impl`。分类:**a**=递归 guest-descriptor 链(可自环,必须 hop cap)、
**b**=有界扇出(后续 DMA 数由传输长度/MDTS 派生,非 guest 任意链)、**c**=无 host DMA
(纯 controller-side backing)。

| 路径 | 类 | 终止机制(常量 @ 位置) | 计量(I1) | firing 测试 |
|---|---|---|---|---|
| SGL segment chain `NvmSglFetch` | **a** | `MAX_SGL_SEGMENTS=64` @ `mod.rs`;`walk_segments+=1` 跟 chain 前查 @ `completion.rs` | hop 数 ✓ | `sgl_segment_self_loop_is_bounded_by_hop_guard` @ `tests/fuzz_poc_sgl_chain.rs` |
| Read PRP-list chain `NvmReadPrpListFetch` | **a** | `MAX_PRP_LIST_PAGES=16` @ `mod.rs`;`list_pages_fetched=saturating_add(1)` 跟 chain 前查 @ `completion.rs` | list 页数 ✓ | `c1_chain_depth_cap_rejects_malformed_chain` @ `controller/tests.rs`(自环 + revert-verify) |
| DBBUF shadow poll `issue_shadow_read` | **a**(自馈轮询) | `MAX_SHADOW_POLL_ITERS=1<<16` @ `mod.rs`;`next_iters` 仅链起始重置、issue 前查 | 自续深度 ✓ | `high2_inrange_oscillating_chain_trips_cfs_at_cap` 等 `dbbuf_tests`(振荡 + revert-verify) |
| CMB drain cascade `drain_cmb_completions` | **a**(循环级自馈) | `MAX_CMB_DRAIN_ITERS=1<<20` @ `cmb.rs`;`iters` 每 pop +1、dispatch 前查 → CFS + clear | 循环迭代数 ✓ | `cmb_drain_iters_cap_fires_and_sets_cfs` @ `controller/tests.rs`(预填撞顶 + revert-verify) |
| Write/Compare PRP-list fetch `NvmWritePrpListFetch`/`NvmComparePrpListFetch` | **b** | `.take(total_pages-1)`;**不读末位为 chain pointer**——MDTS=5(≤32 页)< 511 entry/页 → 单 list 页天然够 | n/a(无链) | 结构性(无自环面) |
| PI-list fetch `NvmReadPiListFetch`/`NvmWritePiListFetch` | **b** | `for i in 0..pages_total-1` 有界扇出;单次 fetch list 页,**无 chain 重入** | n/a(无链) | 结构性 |
| Fused Compare→Write `NvmCompareSinglePrpFused` | **b** | 重发的是**捕获的固定 Write SQE**(经 dispatch_io / MDTS-checked),一对 fused 至多 +1 命令、非 guest 可构链 | n/a | 结构性 |
| Simple Copy ranges `NvmCopyFetchRanges` | **c** | per-range 纯 controller-side backing read/write,**零 host DMA**;`num_ranges` 受 Copy NR 字段(单 descriptor 页)界 + checked_add 溢出防护 | n/a(无 host DMA) | 结构性 |

**其余 27 个 PendingOp variant 均为 d=终端**(completion 里不再发起后续 DMA):
`NvmWriteDmaRead`、`NvmReadDmaWrite`、`NvmReadDualPrpSiblingHalf`、`NvmWritePrpListData`、
`NvmReadPrpListData`、`NvmSglData`、`NvmCompareSinglePrp`、`AdminFwDownloadChunk`(另有自身
64 MiB cap)、`NvmReservationCmd`、`AdminNsCreate`、`NvmCompareDualPrp`、`NvmComparePrpListData`、
`AdminSetHostIdentifier`、`AdminNsAttachmentList`、`NvmWritePi`、`SepMetaWriteData`、
`SepMetaWriteMeta`、`SepMetaReadDone`、`InlineMetaWriteSeg`、`InlineMetaReadDone`、
`NvmWritePrpListSepMeta`、`NvmReadPrpListSepMeta`、`NvmReadPiDmaWrite`、`NvmZoneAppend`、
`NvmWritePiMulti`、`NvmReadPiListData`、`NvmWriteDualPrp`。

## 第二层结构防御(加固上表)

`on_dma_complete_impl` 入口有**非重入铁律**(`completion.rs` 的
`debug_assert!(!self.cmb_in_access_guest, ...)`):`guest_read`/`guest_write` 对 CMB 命中
**只入队 `cmb_completions` 不直接递归调派发器**(`cmb.rs` 的 `cmb_in_access_guest` 哨兵)。
即 completion 中再发的 DMA **绝不同步递归重入派发器**,而是压回 drain 队列被迭代处理。
因此唯一 DoS 面就是"无界喂 drain 队列的链",正好被上表 hop cap 堵死——cap 的封顶量与威胁
模型精确对齐。

## PR 回归闸(改这块代码时必读)

1. **新增/修改任何在 `on_dma_complete_impl` 里再发起后续 DMA 的 `PendingOp` 路径**:必须在
   上表加一行,声明类(a/b/c)与终止机制。若是 a 类(下一跳地址由 guest 决定),**必须**带计
   hop/深度的 cap(I1)+ 一个对抗性 self-loop firing 测试(否则 = dead cap)。
2. **已知的未来陷阱**:`mod.rs` 的 `MAX_PRP_LIST_PAGES` doc 显式记录——admin **写方向** PRP-list
   chaining(FW Image Download、Set Features Save)目前**不**跟 chain(靠 MDTS 单页够),若未来
   接上 chaining,**须在对应 write fetch arm 同样维护 `list_pages_fetched` 计数**。这是 a 类
   会被静默重新引入的最可能入口。
3. 每个 cap 都要有 firing 测试("firing 测试"列不得为空),并附 revert-verify 说明:
   **自环输入式**(SGL/PRP-list/shadow-poll)把 cap 调到 `u32::MAX` → 路径发散/测试转红;
   **预填式**(CMB drain,预填条数绑定 cap)**不能**用 `u32::MAX`(会预填 ~4.3B 条 OOM),等价
   判别是把**预填数**调到 `cap-1`(或把 cap 抬过预填数)→ 不达上限 → CFS 不置 → 转红。

## 下一步结构性加固(高级,未做)

当前每条 a 类路径**各自**带 ad-hoc 深度计数(`ShadowPollCtx.iters`、
`PrpListOp.list_pages_fetched`、`SglOp.walk_segments`)。"新路径忘了加 cap"这一回归面目前**仅
靠 reviewer 警觉 + 本台账 PR 闸**兜。更强的 by-construction 封闭:把三处计数收敛成一个
device 侧 `ChainDepth` newtype,其 `fn advance(self) -> Result<Self, DepthExceeded>` 把"检查
上限"变成**再发 DMA 调用点无法绕过的一步**——新路径若再发 DMA 而不经 `ChainDepth::advance`,
代码拿不到 token 去发,漏加 cap 从"运行时才 CFS"变成"写不出来"。

- **触发条件**:上述#2 的 admin 写方向 chaining 真要落地时,优先做此收敛(避免再加一个
  ad-hoc 计数器)。
- **承重前提**:三处计数上限不同(16/64/65536),`ChainDepth` 须参数化上限
  (`BoundedDepth<const MAX>` 或携带 max 字段),不是单一常量。
- **风险**:触及 3 个 working 子系统(shadow/PRP-list/SGL),共享工作树下需 hunk 级隔离提交。
  当前威胁面**已被逐路径 cap 关闭**(本台账证),故此项是回归加固、非堵漏,可在#2 落地时一并做。
