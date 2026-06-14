# Phase V9 详细实施计划 — NVMe-oF RDMA Transport Emulator

> **Status:** design draft（execution-ready 到 R2；R3+ 为 roadmap 详度，带详化 gate）
> **Date:** 2026-06-14
> **Branch:** `feat/usnvmemu`
> **量级:** 6+ 月 HUGE track（ROADMAP §3 / multi-month-spec §V9）
> **Prereq:** NVMe-oF TCP track 全栈 shipped（V-interop-8，306 tests）；firmware-core `NvmeController` runtime-agnostic；`trait Transport` 已抽出
> **Spec 参考:** NVMe-oF Transport Specification — RDMA Transport binding；一致性参考实现 = Linux `drivers/nvme/{host,target}/rdma.c` + `include/linux/nvme-rdma.h` + `drivers/nvme/target/auth.c`
> **本 plan 的特殊性:** 经 **3 路独立 subagent 审计**（architect 架构 / 代码 grounding / 协议独立 oracle）核验后定稿。协议事实 8/8 经 Linux 真驱动源码 CONFIRM；初版分析的 3 处设计错误已在本文纠正（见 §3 三个 ⚠️ 框）。裁判轴 = **长远正确 + 设计良好 + 有意义的完整**，非 ROI。

---

## 0. 一句话与立项理由

把 usnvmemu 的用户态 `NvmeController` 暴露成标准 **NVMe-oF RDMA target**：Linux `nvme connect -t rdma`（经 RoCEv2/iWARP/IB）可挂载它跑真 IO。它是 firmware-as-core 的**第 5 条接入**、第 2 条 fabric（TCP 已 done）。

**Why（不可拿 ROI 砍）**：spec 三大 fabric（TCP/RDMA/FC），RDMA 是**性能 ceiling**（零拷贝单边 DMA）。做它一是兑现「教学=spec-complete 非玩具」，二是把「一份 transport-agnostic controller 核同时支撑 PCIe + TCP-fabric + RDMA-fabric 三类截然不同 transport」推到最强证据。

---

## 1. 协议机制（8 条经 Linux 真驱动 CONFIRM，含 oracle 补充的承重细节）

| # | 机制 | 与 TCP 的差异 | 关键细节（来自 nvme-rdma 驱动） |
|---|---|---|---|
| 1 | **连接建立** = RDMA-CM | 无 ICReq/ICResp | host `rdma_resolve_addr→route→connect`；target `rdma_listen`+`CONNECT_REQUEST`；协商走 **CM Private Data** `struct nvme_rdma_cm_req`（32B：`recfmt`/`qid`/`hrqsize`/`hsqsize`/`cntlid`+rsvd[22]）；回 `nvme_rdma_cm_rep`（`recfmt`/`crqsize`）；reject 码 `nvme_rdma_cm_status` 0x01–0x09（含 `INVALID_CNTLID=0x09`、`INVALID_IRD/ORD`）。**cntlid 仅 IO queue 填**（admin connect 用于分配 cntlid） |
| 2 | **命令/响应胶囊** = SEND/RECV | 无 PDU、无 framing | command capsule 经 `IB_WR_SEND`；response 经 `IB_WR_SEND` 或 `IB_WR_SEND_WITH_INV`（远端失效 rkey）；host 预投 `ib_post_recv` 接 |
| 3 | **数据传输 = target 驱动单边**（最关键，初版分析此条正确） | 无 R2T/H2CData/C2HData 流控 | host **从不**显式发 RDMA R/W；只 SEND cmd + RECV rsp + `LOCAL_INV`。**写**（data-in）：target 发 **RDMA READ** 拉取；**读**（data-out）：target 发 **RDMA WRITE** 推送，链在 SEND 响应前。例外见 #5 |
| 4 | **寻址 = Keyed SGL Data Block** | 无假-PRP 合成 | host 在 capsule 内放 keyed SGL：`addr`(le64) + `key`(le32 rkey) + `length`(**le24，单描述符 ≤16 MiB**)；target `nvmet_rdma_map_sgl_keyed` 取后喂 `rdma_rw_ctx_init`。host 默认走 fast-reg MR + `NVME_SGL_FMT_INVALIDATE`（触发 target `SEND_WITH_INV`） |
| 5 | **in-capsule inline data**（RDMA 独有，初版遗漏） | 无对应 | 写命令小数据可内联在 capsule，**绕过 RDMA READ**：host `NVME_SGL_FMT_OFFSET` 描述符 `addr=icdoff`；target 置 `NVMET_RDMA_REQ_INLINE_DATA`，inline 页预分配在 recv WR `sge[1..N]`；容量 = `inline_data_size` 协商（默认 ~16 KiB） |
| 6 | **认证** | CHAP 可复用；TLS 不可 | **DH-HMAC-CHAP transport-无关**（`nvmet_setup_auth` 等无 transport 分支，跑胶囊里）→ 与 TCP 共享 core。**TLS = TCP-only**（内核全 `#ifdef CONFIG_NVME_TARGET_TCP_TLS`）；RDMA 机密性靠 **IPsec**（RoCE/iWARP 之下），**不在 NVMe 层做 TLS** |
| 7 | **QP/CQ 模型** | 每 queue 独立连接（与 TCP 同构） | 一 association = admin 1 对 + 每 IO queue 各 1 对 **RC QP**（`IB_QPT_RC`）；target 每 queue send+recv 共用 1 CQ，深度 `recv_queue_size + 2×send_queue_size`；可选 SRQ |
| 8 | **无 digest** | 删 HDGST/DDGST | RDMA 无 header/data digest（硬件/RoCE 自带 ICRC）；**别移植 TCP 的 `digest.rs`** |

---

## 2. 复用地图（经代码 grounding 审计 file:line 坐实）

**✅ 几乎 100% 复用（transport-无关）：**
- `nvme_firmware::NvmeController` — DMA 出口统一归结 `ctx.dma_read/dma_write(addr,…)`（[cmb.rs:220-262](/usnvmemu/crates/nvme_firmware/src/controller/cmb.rs)）；SGL 路径也 address-based、无线性/连续假设（[completion.rs:3057-3063](/usnvmemu/crates/nvme_firmware/src/controller/completion.rs)、[sgl.rs:179-201](/usnvmemu/crates/nvme_firmware/src/sgl.rs)）。**但见 §3 ⚠️③：controller 当前主动 reject Keyed Data Block。**
- `fabric.rs` Connect/Property 解码 — 纯 `&[u8]` 切片解码、零 stream 耦合（[fabric.rs:223-238](/usnvmemu/crates/nvme_of_tcp_target/src/fabric.rs)）；RDMA 只需把 **CM Private Data** 字节喂同一 `decode_connect_fields`。
- `dhchap.rs` in-band CHAP — transport-无关，复用。
- `Transport` trait — RDMA 仍 impl（[device.rs:41-54](/usnvmemu/crates/pcie_device_core/src/device.rs)）；`fire_interrupt` 桩 no-op（完成走 CQE 胶囊，非 MSI-X；TCP 已如此 [tcp_transport.rs:121-127](/usnvmemu/crates/nvme_of_tcp_target/src/tcp_transport.rs)）。

**⚠️ 部分复用 `dispatch_plan.rs`（4/7）：**
- ✅ 复用：`decide_capsule_kind` / `decide_admin_aer_path` / `decide_admin_discovery_whitelist` / `decide_admin_blocked_opc` / `decide_connect_cntlid`（纯 spec 语义）。
- ❌ RDMA 用不上（TCP 假-PRP 桥衍生物）：`prp2_sentinel_for_nlb` / `dual_prp_max_lbas` / `host_io_max_lbas`（keyed-SGL 不合成 PRP、不按 2-页拆）。`decide_io_nlb_check` 意图通用但上限来源需替换。

**🔴 重写（TCP-specific）：**
- `framing.rs` / `digest.rs`（RDMA 无 PDU/CRC，#8）→ 删。
- `r2t.rs` / `h2c_reassembler.rs` / `ttag.rs`（R2T 流控整套，#3）→ 删。
- `async_session.rs` 的 TCP byte-stream pump → 换 **CQ 轮询事件循环**。
- session chunking（[async_session.rs:1700-1802](/usnvmemu/crates/nvme_of_tcp_target/src/async_session.rs) 把 IO 拆成 ≤2 页块）→ RDMA keyed-SGL 单描述符寻址整段，**整块丢弃非适配**。

---

## 3. 核心架构（三处审计纠正已内化）

### 3.1 复用并泛化「假-GPA DMA-capture 桥」

controller 用 sentinel 地址发 `post_cqe/dma_read/dma_write`，session 拦截翻成 fabric 数据移动——RDMA 版同一拦截点，翻译目标换成 verbs：
```
dma_read(sentinel, len)   → RDMA READ  from host keyed-SGL（写命令取数据）
dma_write(sentinel, data) → RDMA WRITE to   host keyed-SGL（读命令送数据）
post_cqe(cq_sentinel, cqe)→ SEND response capsule（含 16B CQE）
```

> #### ⚠️ 纠正① —「sentinel→remote 映射表」是误读，改用 **token-FIFO + per-command keyed-SGL 旁路**
> **错在哪**：初版说「每命令一张 sentinel→(remote_addr,rkey) 映射」。但实测 TCP 桥**不靠 sentinel 值反查**——sentinel 是**写死常量**（`PRP1_SENTINEL=0x1000_0000`/`PRP2_SENTINEL=0x2000_0000`/`CQ_BASE_GPA`，[session.rs:54/69/116](/usnvmemu/crates/nvme_of_tcp_target/src/session.rs)）。区分同命令多段 DMA 靠：(a) gpa 阈值粗二分（`≥CQ_BASE_GPA`=CQE vs 数据，[session.rs:813/842](/usnvmemu/crates/nvme_of_tcp_target/src/session.rs)）；(b) **token-FIFO 顺序 + cumulative offset**（[session.rs:775/794/834-862](/usnvmemu/crates/nvme_of_tcp_target/src/session.rs)；completion 端逐 fragment 各得唯一 token、用 token 关联回 frag_idx [completion.rs:3065](/usnvmemu/crates/nvme_firmware/src/controller/completion.rs)）。
> **照初版实现的后果**：「每命令映射」+ **固定 sentinel** 自相矛盾，多命令并发 in-flight 时固定 sentinel 必撞车。
> **正解**：保留 token 模型 + FIFO + cumulative offset 机件；**新增 per-command keyed-SGL 旁路上下文**——在 SQE 改写前从入站 capsule 抽 `(remote_addr, rkey, len)` 挂命令上下文，token 完成时按 issue 顺序 + cumulative offset 算出本 fragment 的 `remote_addr + offset`。
> **POC gate（R1'）**：`poll_cq` work-completion 顺序 ↔ keyed-SGL fragment 发射顺序的对应关系——代码无法推断，须真 verbs/nvmet-rdma 源码追踪确认。

### 3.2 后端两层 trait 泛化

> #### ⚠️ 纠正② — `FabricBackend` 不可抽成 `pull_write_data(sgl,len)->Vec<u8>`（违反零拷贝）
> **错在哪**：返 owned `Vec` 强制一次拷贝；RDMA 全部价值是 controller 直读 MR 零拷贝。`->Vec<u8>` 把 RDMA 降级成「带额外拷贝的慢 TCP」，踩 [[hot-path-perf-first]]。
> **正解**：抽象粒度 = **「data placement intent」**（把 len 字节在 host 内存 ↔ 本地 buffer 间移动），且**允许借出 MR 引用**（`&mut [u8]` / `Bytes`，让零拷贝可表达），而非返 owned 字节。TCP impl 内部实现为 R2T 多步（含三段式 lock-pop/unlock-await-wire/lock-complete，满足 `#![deny(clippy::await_holding_lock)]`）；RDMA impl 实现为单 verb 零拷贝。**必须在 R2 定 trait 前修，否则 R5 改 trait = TCP impl 全改 + regression gate 全重录。**

抽象草图（R0 用 nvmet-rdma 源码定语义后再 finalize）：
```rust
// 第 1 层：fabric 数据移动后端（TCP / RDMA 各 impl）
trait FabricBackend {
    async fn recv_capsule(&mut self) -> CapsuleIn;                 // TCP: read PDU；RDMA: poll RECV CQE
    async fn send_response(&mut self, cqe: &[u8], inline: Option<&[u8]>);
    // placement intent，借 MR 不返 owned：
    async fn move_host_to_local(&mut self, host: HostBufRef, dst: &mut [u8]); // 写：TCP R2T 多步 / RDMA READ
    async fn move_local_to_host(&mut self, host: HostBufRef, src: &[u8]);     // 读：TCP C2HData / RDMA WRITE
}
// 第 2 层：verbs 薄抽象（MockRdma + IbverbsRdma）
trait RdmaVerbs {
    fn reg_mr(&mut self, buf: &mut [u8], access: Access) -> MrHandle;
    fn post_recv(&mut self, slot: RecvSlot);
    fn post_send(&mut self, capsule: &[u8], invalidate: Option<Rkey>);
    fn post_rdma_read(&mut self, local: MrSlice, remote_addr: u64, rkey: Rkey, len: u32) -> WrId;
    fn post_rdma_write(&mut self, local: MrSlice, remote_addr: u64, rkey: Rkey, data: &[u8]) -> WrId;
    fn poll_cq(&mut self) -> Vec<Completion>;  // 含 IBV_WC_*_ERR / QP-error 语义，见 §3.4
}
```

`dispatch_plan.rs` 决策核坐在 trait 之上不动。

### 3.3 ⚠️ 纠正③ —「controller 一行不改」是 milestone-1 脚手架，不是终态

- controller 当前**主动 reject** Keyed Data Block（[io.rs:396-399](/usnvmemu/crates/nvme_firmware/src/controller/io.rs)、[sgl.rs:293-296](/usnvmemu/crates/nvme_firmware/src/sgl.rs)），且 inline-SGL 有 ≤1 page 假设（[io.rs:368-374](/usnvmemu/crates/nvme_firmware/src/controller/io.rs)）。
- **milestone-1（M1，正确性/e2e parity）**：桥从 capsule 抽 keyed-SGL，把 SQE 伪装成 controller 能吞的形态喂进去。**精修（代码审计）**：RDMA 不该像 TCP 那样清 PSDT bits 把 controller 退化到 PRP 路径——controller 的 SGL 路径已 address-based 可用，RDMA 应**保留 SGL**（把 keyed-SGL 翻成带 sentinel 地址的 address-SGL），让 controller 走 `dispatch_sgl_read/write`。代价：性能被 controller dispatch 上限锁。
- **milestone-2（M2，真性能 = 立项理由「性能 ceiling」）**：必然要给 controller 加**多页 SGL-list dispatch**（ADR-006 已列起点）。
- **纪律**：M1 须**显式标注脚手架**，绝不拿它冒充终态（[[meaningful-complete-not-minimal]]：可拆基础/高级，但不砍让事情真正能用的部分）。

### 3.4 mock-first 精修（防 mock 自洽地错）

`MockRdma` 最易在 **completion/错误/QP-state 语义**上自洽地错（真 ibverbs 有乱序完成、RNR retry、`IBV_WC_*_ERR` 后 QP 进 error-state 需重建）。若 trait 迁就 mock 的乐观语义，R5 真 verbs 一接全改——[[review-not-optional-self-consistent-trap]] 重演。
**修法（R0）**：定 trait 前先用 **Linux `nvmet-rdma` 源码追踪当设计期最廉价 oracle**（[[poc-before-settling-design]]）把 completion/错误/QP-state 语义定对，**再让 MockRdma 模拟这些语义**，而非反过来。

---

## 4. 承重遗漏清单（初版没提，全部 long-term 相关，须纳入对应阶段）

| 遗漏 | 纳入阶段 | 要点 |
|---|---|---|
| **in-capsule inline data**（RDMA 独有） | R4 | 小写命令 data 在 capsule 内、绕过 RDMA READ；`dma_read(sentinel)` 时桥从已收 capsule buffer 取，非发 verb。漏了会逼 FabricBackend 加分支 |
| **RNR / recv-buffer 池管理** | R3 | target 须始终有 posted recv buffer 接 host SEND，否则 RNR retry/QP error。需 recv 池 + repost 逻辑（QP 活命） |
| **MR 注册生命周期 / pinning** | R4 | `reg_mr` 重操作，不能每 DMA 注册；需预注册 MR 池。直接放大纠正②的拷贝问题，是「真能用」的核心 |
| **rkey 远端失效**（`SEND_WITH_INV`） | R4 | 默认 fast-reg 路径 host 期望 target 送响应时失效 rkey；与 Linux host 互通必需 |
| **IRD/ORD 协商** | R3 | RDMA READ 是 target 发起，IRD/ORD 约束 READ 并发深度；reject 码已定义 |
| **CQ 深度公式** `recv+2×send` | R3 | 每命令最多 RDMA_RW + SEND 两个 send-side completion；sizing 与 TCP 全不同 |
| **fire_interrupt no-op** | R2 | 两 fabric 都 no-op（CQE 走数据通道），文档化防误以为要映射真 event |
| **多 QP ↔ session 聚合模型**（architect review，最重） | R0 决策 + R3 落实 | 一 association = admin 1 + 每 IO queue 各 1 对 RC QP，同一 RDMA-CM listen 下多个 `CONNECT_REQUEST`。现有 session 是 **per-conn 单 qid**（[session.rs:155-161](/usnvmemu/crates/nvme_of_tcp_target/src/session.rs)，"一 qid=一 TCP conn" 靠 OS 给每 accept 独立 stream）。RDMA 谁把 N 个 QP 聚合成"一 session 跨多 qid" vs "一 QP 一 session 实体"——决定 `FabricBackend` 是 per-QP 还是 per-association。**不在 R0/R3 定死 → 日后做 association-level 资源（shared CQ/SRQ/auth 跨 admin+IO queue）必重构**。优先级 = 三处纠正 |
| **association teardown / QP drain**（architect review） | R3/R4 | QP 进 error-state 或 disconnect 时，in-flight RDMA R/W 须 drain（等 flush completion）才能安全释放 MR；与"MR 注册时序"相邻但不同（这是拆除时序）。漏则 R5b disconnect 路径 use-after-free（尤其 R5a unsafe） |
| **Discovery over RDMA / KATO 区分**（architect review，LOW） | R3 | Discovery log 记录须填 `NVME_TRTYPE_RDMA` + RDMA TRADDR（非 TCP）；NVMe-KATO（capsule 层，R3 admin 已含）与 QP-liveness（RNR/timeout 传输层）是**两套** liveness，别混 |

---

## 5. 分阶段（R0-R5 + M2）

> **详度标注**：R0/R2 = **execution-ready**（可直接起手）；R1'/R3/R4/R5/M2 = **roadmap 详度 + 详化 gate**（依赖前序 POC 结论，起手前须把该阶段详化到 execution-ready，不伪详化，[[handoff-needs-full-prompt-plus-plan]]）。每阶段独立 commit + 过对应 reviewer。

### R0 — 设计期 oracle + binding 选型（execution-ready，零阻塞）

> **✅ 2026-06-14 DONE** → 交付 [nvme-rdma-wire-and-verbs-contract](../specs/2026-06-14-nvme-rdma-wire-and-verbs-contract.md)
> （字段级 wire A/B/C + verbs 语义契约 D + 设计决策 E `HostBuf`/F per-QP session + binding ADR 草案 G +
> 陷阱 H + smoke 清单 I）。2 路 oracle 抽 Linux 真驱动 + architect review（§F 源码核实 production-ready；
> §E 补 multi-SGL/separate-meta segments 模型 + len 不变式 + 两层 cntlid + completion 派发 open-Q + 3 smoke）。
> **binding 选定**：`sideway`（主，唯一完整 rdma_cm + 活跃）+ `rdma-sys`（兜底裸 FFI），藏 `trait RdmaVerbs` 后。
- **What**：①追踪 Linux `nvmet-rdma`/`nvme-rdma` 源码，把 `RdmaVerbs` 的 completion/错误/QP-state/RNR 语义 + `nvme_rdma_cm_req` 字节布局定成文档化契约。②`ibverbs` Rust binding 选型 survey（**GitHub search first**，development-workflow）：候选 `rdma`/`rust-rdma`（高层含 rdma-cm）、`ibverbs` crate、`rdma-sys`/fork `ibverbs-sys`（裸 FFI 兜底）；评估维护度/安全/是否覆盖 RDMA-CM；**结论藏在 `RdmaVerbs` trait 后，可换**。
- **Acceptance（architect review 拆成三个明确产出物，分别喂下游不同阶段）**：
  - **(a) 喂 R1'**：`RdmaVerbs` 语义契约（completion 乱序 / RNR / `IBV_WC_*_ERR`→QP-error-state / poll 模型）+ binding 选型 ADR 草案。
  - **(b) 喂 R2（前置 gate）**：capsule 内 **keyed-SGL / in-capsule-inline（`NVME_SGL_FMT_OFFSET`）的字节级布局** + `HostBufRef`/`CapsuleIn` 字段级定义——**必须同时承载 TCP-无远端 / RDMA-keyed-SGL(remote_addr+rkey+le24-len) / inline 三态**。这是 R2 trait 形状真 execution-ready 的前提（见 §9 gate ⑤）。
  - **(c) 喂 R3**：**association ↔ QP ↔ session 实体映射决策**（一 session 跨多 QP/qid，还是一 QP 一 session 实体）——决定 `FabricBackend` 是 per-QP 还是 per-association。优先级 = 三处纠正（见 §4 遗漏表）。
- **Reviewer gate**：architect（语义契约是否对齐真驱动 + 三态 `HostBufRef` 是否覆盖全 + QP 聚合决策是否长远正确）。
- **POC gate 输出**：(a)→R1' trait；(b)→R2 trait 形状；(c)→R3 session 模型。

### R2 — `FabricBackend` trait 泛化（execution-ready，零阻塞，建议优先）

> **✅ 2026-06-14 DONE** → 新 `src/fabric_backend.rs`（`HostBuf` 三态/segments + `FabricBackend` trait
> placement-intent 三方法 + `TcpFabricBackend<S>`，R2T/C2HData/CapsuleResp/await_host_data 逻辑从
> `AsyncSession` **迁入**非复制）。AsyncSession `stream`+`ttag_alloc` 两字段 → 单 `backend` 字段，6 叶方法
> 委托、删 `dma_read_one_chunk`+`await_host_data_async`。**~322 tests 全绿 + byte-identical + clippy clean +
> `#![forbid(unsafe_code)]` 维持**。rust-reviewer：byte-identical PASS + 修 HIGH-1（cap-before-allocate）；
> architect：**commit-ready 地基**，两笔分阶段债（R3 抽 `recv_next` / R4 调 `move_*` 签名 + 桥层 CQE 分流）
> 已补前向锚点注释（HostBuf 类型已 RDMA-complete 不需改）。
- **前置 gate（architect review）**：R0 产出物 (b) 必须先到位——`HostBufRef`/`CapsuleIn` 须按三态（TCP-无远端 / RDMA-keyed-SGL / inline）字段级定死，否则 R2 仅 TCP 视角盲定的类型在 R4 接 keyed-SGL 时要加变体 → 牵动 trait + byte-identical regression gate 重录（正是纠正②警告的失败模式挂在类型形状上）。
- **What**：把现有 TCP 写/读路径抽到 `FabricBackend` trait 后面（**纠正②的 placement-intent + 借 MR 形状**），TCP 迁过去。
- **Acceptance**：306 tests 全绿 + byte-identical regression gate（仿 `v8e1`）守护;clippy 0;`#![forbid(unsafe_code)]` 维持。**trait 签名不出现 `->Vec<u8>` 强制拷贝;`HostBufRef` 三态可表达。**
- **Reviewer gate**：rust-reviewer（trait 边界 + 零拷贝可表达性）+ architect（抽象粒度）。
- **价值**：纯结构收益、独立有价值（让 session 真 transport-agnostic），是 RDMA 地基。

### R1' — `RdmaVerbs` + `MockRdma` + hello-QP mock smoke（roadmap 详度，详化 gate=R0 结论）

> **✅ 2026-06-14 DONE** → 新 crate [`rdma_transport`](../../../rdma_transport/)（`src/verbs.rs` trait +
> `src/mock.rs` MockRdma + 12 tests，全绿/clippy/fmt clean）。MockRdma **忠实模拟 §D 语义**（completion
> 保序 / QP-error→flush→重建 / RNR / ORD 限深，**非乐观 FIFO**）。rust-reviewer APPROVE（ORD 守恒 4 站点
> 核实平衡）+ 修 M1（QP→ERR flush posted recv WR，防教错 recv-reposting 模型）+ 4 回归守卫。crate 不
> `forbid(unsafe_code)`（RDMA FFI 边界，IbverbsRdma 留 R5a）；根 Cargo.toml exclude 加 1 行。
- **What**：按 R0 语义契约实现 trait + `MockRdma`（**模拟真 completion/RNR/QP-error 语义**，非乐观 FIFO）+ 进程内 hello-QP smoke。
- **Acceptance**：mock smoke 跑通 QP 建/RDMA R/W/SEND 模拟 + 错误注入测试（QP-error 转移）。
- **Reviewer gate**：rust-reviewer + architect（mock 语义是否忠实 R0 契约，非反向迁就）。

### R3 — RDMA-CM Connect + admin over SEND + recv 池 + 多 QP 聚合（roadmap 详度）

> **进度（2026-06-15）**：拆 R3a/b/c/d。
> - **✅ R3a DONE**（commit `86f1e0be7`）：`FabricBackend::recv_next` 抽象（还 R2 的 select! 直借 stream 债）+
>   `RecvFrame{Frame(Pdu),PeerClosed}`；pump 经 trait recv；`stream` 转私有；324 tests 绿 + byte-identical +
>   rust-reviewer 无 C/H。**RecvFrame::Frame(Pdu) 已验对 R3b 充分**（RDMA 从 RECV completion 合成 CapsuleCmd
>   PDU 复用 dispatch）。
> - **✅ R3b DONE**（commits `9eb5aac63` R3b-1 + `338de0211` R3b-2）：
>   - **R3b-1** `src/rdma_cm.rs`：CM Private Data wire 结构（`NvmeRdmaCmReq/Rep/Rej` + cm_status 全 9 码 +
>     parse/off-by-one `recv_queue_size=hsqsize+1`），独立 oracle 对 Linux `nvme-rdma.h` 逐字 CONFIRM。
>   - **R3b-2** `src/rdma_backend.rs`：`RdmaFabricBackend<V:RdmaVerbs>`（recv_next=poll_cq RECV→byte_len 截断
>     →合成 CapsuleCmd PDU→repost；send_response=post_send；move_local/host=RDMA WRITE/READ；CQ demux 分流
>     RECV vs send-side；recv 池恒定 RNR 避免）。R3b 边界=单段 keyed-SGL，multi-seg/inline/separate-meta/
>     SEND_WITH_INV/MR 池 bail 显式化（R4）。11 rdma_* tests + rust-reviewer 无 C/H（CQ demux + PDU 合成两承重轴正确）。
>   - **未做（挪 R3c/d）**：CM Connect 接真 connect 流（用 parsed CM req 设 qid/cntlid 驱 fabric Connect）+
>     RdmaFabricBackend 接入 AsyncSession（须 AsyncSession 泛化于 FabricBackend）。
> - **⏳ R3c**：**AsyncSession 泛化于 `FabricBackend`**（现持具体 `TcpFabricBackend<S>`）+ 多 QP ↔ session 聚合
>   （per-QP backend，共享 `SharedControllerInner` keyed by cntlid，R0 §F）+ association teardown / QP drain。
>   - **✅ R3c-1 DONE**（commit `05323fe9b`）：AsyncSession `<S:AsyncSessionStream>`→`<B:FabricBackend=TcpFabricBackend
>     <TokioStream>>`；`FabricBackend` 加 `terminate(fes)`（TCP=C2HTerm/RDMA=QP teardown）；335 tests 绿 byte-identical
>     clippy clean；rust-reviewer 无 C/H，**确认 dispatch/pump/Drop 已 backend-agnostic、R3d 不翻车**。
>   - **⏳ R3c-2**：RDMA 构造器（类比 `accept_and_handshake_async`，built `RdmaFabricBackend` + 填 `negotiated` 默认）
>     + 多 QP 聚合 + teardown（terminate fes→neutral reason 顺手换）。
> - **⏳ R3d**：admin 全路径过 RDMA mock + Discovery-over-RDMA（`NVME_TRTYPE_RDMA`）+ IRD/ORD/CQ-depth sizing。
>   **⚠️ 发现（R3c-1 review）**：admin 返数据命令（Identify 4KB）的 e2e 需**桥层把命令 capsule 的 keyed-SGL 抽出
>   作 `HostBuf::Keyed` 喂 move_local_to_host**（现桥硬编 `FlowControlled`）——这是 R4 的桥层 HostBuf 来源债，故
>   R3d 真 admin-data e2e 与 R4 keyed-SGL 桥**交织**；CQE-only 命令（Keep-Alive/Set-Features）可先在 R3d 走通。

- **What**：CM Private Data Connect（喂 `fabric.rs` 复用解码）+ admin capsule over SEND（against Mock）+ **recv-buffer 池 + repost（遗漏 RNR）** + IRD/ORD/CQ-depth sizing + **多 QP ↔ session 聚合（落实 R0(c) 决策：admin+IO queue 各 QP 如何挂进 session 模型）** + **association teardown / QP drain 时序（in-flight R/W flush 后才释放）** + Discovery-over-RDMA 填 `NVME_TRTYPE_RDMA`/RDMA TRADDR。
- **Acceptance**：controller admin 全路径过 RDMA mock（Identify/Set-Features/AER/Keep-Alive）；recv 池耗尽/repost 测试;多 QP 建立/拆除 + drain 时序测试。
- **Reviewer gate**：rust-reviewer + architect。

### R4 — keyed-SGL → RDMA R/W 数据路径 + 全遗漏（roadmap 详度，最大阶段）
- **What**：桥泛化落地（**纠正① token-FIFO + keyed-SGL 旁路**）+ keyed-SGL→RDMA READ/WRITE + **in-capsule inline data 路径** + **MR 预注册池** + **rkey 远端失效** + SGL×PI；controller 保留 SGL 路径（**纠正③ M1 脚手架**）。
- **Acceptance**：IO Read/Write/Compare 数据正确性（against Mock，distinct-per-LBA pattern + 独立 oracle）；inline 小写 vs keyed-SGL 大写两路径；多命令并发不撞车（验纠正①）。
- **Reviewer gate**：rust-reviewer + architect。

### R5a — `IbverbsRdma` 真 FFI（roadmap 详度，**卡 soft-RoCE**，详化 gate）
- **What**：真 ibverbs FFI impl（大片 unsafe）。每 `// SAFETY` 用真 verbs completion 作**独立 oracle**（§22 标准，非 self-consistent）。MR 生命周期 vs DMA region 所有权按 vfio-user mmap 经验更严处理。
- **Reviewer gate**：rust-reviewer（**必过**，unsafe）+ security-reviewer。

### R5b — 真 `nvme connect -t rdma` 互通（roadmap 详度，**卡 soft-RoCE + host-root**）
- **What**：Linux kernel host initiator over soft-RoCE 驱我们的 target；第三方独立 oracle，Tier-1「真 host e2e」。
- **内核阻塞**：见 §8（用户决策**推迟到此**）。

### M2 — controller 多页 SGL-list dispatch（真性能终态，roadmap 详度）
- **What**：兑现「RDMA=性能 ceiling」——给 controller 加真多页 SGL-list dispatch（ADR-006 起点），解除 M1 脚手架的 ≤2-页锁。**显式标终态、非脚手架。**

---

## 6. Per-transport 独立-oracle 覆盖矩阵（并入 ROADMAP Tier-1 元项 / ADR-013）

RDMA 行须满足结构律「数据/wire 路径含 control plane，缺 ≥1『另一条代码路径』独立 oracle 作 standing gate 时不算完成」：

| 档 | oracle | 阶段 |
|---|---|---|
| 档1 独立-ish（standing CI gate） | 手搓 RDMA wire driver（**非** MockRdma 自家两端 loopback——那是 §20 反模式）：独立解码 CM Private Data + keyed-SGL + capsule | R4 后补，接 usnvmemu CI workflow |
| 档2 frozen-vector | 真 kernel `nvme-rdma` 抓包 → 机械抽 wire 前缀 → golden replay（仿 vfio `_guest_replay`） | R5b 可跑后抓 |
| 档3 live（不计 gate） | 真 `nvme connect -t rdma` over soft-RoCE | R5b |
| 横切 ≥4 GiB 子条款 | 手搓 harness 的 host 内存须能生成 ≥4 GiB/高地址（§26，根因在 firmware-core） | R4 harness |

---

## 7. 风险表

| 标号 | 描述 | 阶段 | 处理 |
|---|---|---|---|
| R-1 | token 完成顺序 ↔ keyed-SGL fragment 顺序对应 | R1'/R4 | **POC**（§3.1 gate），真 verbs/nvmet-rdma 源码确认 |
| R-2 | mock 自洽地错（completion/QP-state/RNR） | R0/R1' | R0 用真驱动定语义、mock 模拟之（§3.4） |
| R-3 | ibverbs FFI 大片 unsafe | R5a | §22 真 oracle 每 SAFETY；security-reviewer |
| R-4 | MR 注册生命周期 vs DMA region 所有权（**含 teardown-time**：QP drain / in-flight R/W flush 后才释放 MR，防 use-after-free） | R4/R5a | 预注册池；比 vfio-user mmap 更复杂的 fd/内存所有权;association teardown 时序 |
| R-5 | trait 形状偏离真 verbs 致 R5 返工 | R2/R1' | R0 先定语义；R2 trait 避 `->Vec<u8>` |
| R-6 | soft-RoCE 内核阻塞 | R5 | §8，用户决策推迟到 R5（mock-first 让 R0-R4 不卡） |

---

## 8. 内核阻塞（已坐实，用户决策：推迟到 R5）

oracle 确认（rdma-core README + 内核源）：无 RDMA 硬件时跑真 e2e（target + kernel host `nvme connect -t rdma`）**必须**有 soft-RDMA 内核模块（`rdma_rxe` Soft-RoCE 或 `rdma_siw` Soft-iWARP），**无纯用户态出路**（rdma-core 每 provider 1:1 配一个 `.ko`，userspace provider 无法在无内核 RDMA 设备时凭空提供 ibverbs 设备）。

**当前 WSL2 内核 `6.18.33.1` 实测**：`CONFIG_RDMA_RXE is not set` + `CONFIG_RDMA_SIW is not set`，无 `sw/` provider 目录；但 `ib_core/rdma_cm/ib_uverbs/rdma_ucm` 内核基建 + `libibverbs/librdmacm`/`rdma` 用户态都在。

**出路（同当年 `CONFIG_NVME_AUTH` 阻塞模式）**：重编 WSL2 kernel 开 `CONFIG_RDMA_RXE=m` / distro kernel VM / 现成带 rxe 镜像。属特权/外向环境改动 = 用户决策。

**本 plan 决策（2026-06-14 用户拍板）**：**推迟到 R5**——mock-first 让 R0-R4 全程不卡内核；到 R5 真互通时再交接用户重编内核（届时产出精确 runbook）。

---

## 9. 显式不做 / 未决 POC gate

- **TLS over RDMA**：不做（spec/内核确认 TLS=TCP-only，RDMA 靠 IPsec，§1 #6）。
- **FC transport**：本 plan 不含（spec 第三 fabric，另立）。
- **未决 POC gate**（须真 verbs 或 nvmet-rdma 源码才能定，**别假装 R2 能定死**）：①token↔fragment 顺序（R-1）；②`RdmaVerbs` completion/QP-state/错误语义形状（R-2）；③reg_mr 开销是否逼预注册池（R-4）；④inline_data_size/ICDOFF 与 sentinel 改写协同（遗漏 C）；**⑤（architect review）`HostBufRef`/`CapsuleIn` 三态形状（TCP-无远端 / RDMA-keyed-SGL / inline）须 R0(b) 字节级定死、设为 R2 起手前置**；**⑥（architect review）association↔QP↔session 聚合模型（per-QP vs per-association）须 R0(c) 决策、R3 落实**。

---

## 10. 起手提示（新会话）

1. 读本 plan + ROADMAP §3 V9 + multi-month-spec §V9 + auto-memory `nvme-of-tcp-current-state`。
2. **建议首动 = R2**（零阻塞、纯地基），或 **R0**（源码追踪定语义，最稳）。
3. 每阶段 fresh context + spec✓quality✓ 两段 review + 语义单元结尾 commit。
4. unsafe（R5a）必过 rust-reviewer + security-reviewer，SAFETY 用真 oracle。
