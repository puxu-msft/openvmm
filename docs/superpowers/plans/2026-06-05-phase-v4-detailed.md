# Phase V4 详细实施计划 — R2T / H2CData 循环 + SGL 完整化

> **✅ SHIPPED (2026-06-06 audit)** — V4a/V4b/V4c 全段落地 (commits `f8ef8e48` / `ad594893` / `665ba1ec`)；64 tests + reviewer H-1/H-2 修。本文档保留为 design 记录。§9 V5 entrypoint hint 已被 V5a-d 系列吸收。

> **Status:** design draft
> **Date:** 2026-06-05
> **Branch:** `feat/pcie-remote-experimental`
> **Prereq:** Phase V3（admin in-capsule C2HData ≤ 4 KiB 已 work）+ V3-polish (commit 61ed326d)
> **Roadmap 节锚:** [`2026-06-04-phase-v-nvme-of-tcp.md`](./2026-06-04-phase-v-nvme-of-tcp.md) § 7 第 V4 行
> **Wire 参考:** [`2026-06-04-nvme-tcp-wire-reference.md`](../specs/2026-06-04-nvme-tcp-wire-reference.md) §1、§3、§4 (R2T / H2CData / C2HData / flags)
> **Goal:** 把 controller-initiated `ctx.dma_read(...)` 翻译成 NVMe-oF TCP R2T → H2CData 闭环；
> SGL 走 in-capsule data + transport-specific (0x5) 两条路径；MAXH2CDATA 分片；为 V5 IO Write 铺好底

---

## 1. 总览

V3 之前 controller 只产生 *write-out* DMA（dispatch_admin 内部 `ctx.dma_write(prp1, identify_buf)`），
session 把 captured `dma_write` 翻成 C2HData PDU + CapsuleResp。该模型在 admin Identify/Get Log 这种
**纯下行** 的语义下成立。

V4 引入 **controller-initiated `dma_read`** —— controller 处理 NVM **Write** 或 admin **NS Attachment
controller list fetch** 这类命令时，会在 dispatch 阶段调 `ctx.dma_read(host_addr, len) -> token`，
等 caller 投 `nvme_admin_complete_dma(token, ok=true, bytes)` 才继续 post_cqe。

PCIe 下 dma_read 是真 DMA；NVMe-oF TCP 下 host 内存不可达，必须 round-trip：

```
controller.dma_read(addr, len)      ← capture
    ↓
session emit R2T (cccid, ttag, r2t_offset, r2t_length)
    ↓
host send H2CData (cccid, ttag=match, data_offset, data_length, [DATA_LAST])
    ↓
session 攒齐 len bytes → on_dma_complete(token, ok=true, bytes)
    ↓
controller post_cqe → session 转 CapsuleResp
```

同步路径下 `read_pdu` 阻塞会导致单连接只能一条 R2T 在飞行；V4 范围内接受这个简化
（multi-R2T pipelining 留 V8 + tokio refactor）。

---

## 2. V4 与 V3 的关键差异

| 维度 | V3 | V4 |
|---|---|---|
| Captured 流 | `dma_write` only（data writes + CQE write） | `dma_write` + `dma_read`（新加：待 host 上传的 read 请求） |
| 状态机 | per-cmd 一次性 drain | per-cmd 可能多轮 R2T/H2CData 交错（Phase 2-1 → 2-N → CQE） |
| PRP1 sentinel | 单值 `PRP1_SENTINEL = 0x1000_0000` | 多 sentinel：每个 in-flight `dma_read` 一个独立 ttag-加权地址，以便 collision-free 区分多段 |
| token 编码 | 自增计数器（per-`Default::default()` 重置） | 同左，但 **必须跨 cmd 单调**（修 V3-polish review M-1），由 session 拥有 |
| `dma_read` 行为 | TcpAdminTransport warn + 返 token 不 capture（review M-2 已知缺陷） | 入 `pending_reads: VecDeque<PendingHostRead>`，session 转 R2T |
| SGL 类型 | 仅 inline DataBlock (Type 0x0) | + Segment / Last Segment (0x2 / 0x3，**chain in-capsule**) + Transport-specific (0x5，**驱动 R2T 循环**) |
| 单 PDU 上限 | C2HData 限 4 KiB（教学单 PDU） | 宣告 MAXH2CDATA = 64 KiB，> 64 KiB 自动分片 |

---

## 3. 风险 / 已知陷阱（V3 review 遗留必须先修）

| 标号 | 来源 | 描述 | V4 处理 |
|---|---|---|---|
| **R-1** | V3-polish review **M-1** (`tcp_transport.rs:53`) | `TcpAdminTransport::next_token` 每次 `Default::default()` 重置 `1<<48`；若 bail 中途打断异步路径会让残留 pending_ios entry 与下一条 cmd 第一个 token 撞。 | **V4a/V4b** 把 token 计数器上提到 `V2Session` 字段 `next_token: u64`，每次 dispatch 用 `TcpAdminTransport::new_with_token_base(self.next_token)`，结束后 `self.next_token = t.token_high_water()`；跨 cmd 单调。 |
| **R-2** | V3-polish review **M-2** (`tcp_transport.rs:60`) | `dma_read` 不投 `on_dma_complete(ok=false)` → session Phase 3 找不到 CQE bytes → bail "did not produce CQE" → 客户端拿不到 INVALID_OPCODE 而是 TCP 断。 | **V4b** 重写 `TcpAdminTransport::dma_read`：捕获到 `pending_reads` 队列；session 拿到队列后 **emit R2T**，等 H2CData 收齐再调 `nvme_admin_complete_dma(tok, true, bytes)`；不可恢复路径走 `ok=false`。彻底消除 silent path。 |
| **R-3** | 新 — 单线程 read_pdu | `pump_one` 同步阻塞读，dispatch 阶段产 R2T 后必须立刻进 read loop 收 H2CData；不能再回 `pump_one` 外层 dispatch。 | **V4b** 在 `handle_admin_cmd` 内部添加 *内嵌* `await_host_data(cid, ttag, len)` 子函数，循环 `read_pdu` 只接 `H2C_DATA`/`H2C_TERM`，其余 PDU 视 PDU_SEQ_ERR 发 TermReq 后 bail。 |
| **R-4** | 新 — sentinel 池 | V3 单 sentinel 在 V4 不够：同一 cmd 可能多个 dma_read（如 NVM Write 双 PRP）。 | **V4b** sentinel = `0x1000_0000 + ((ttag as u64) << 16)`（ttag u16，最多 64K 段；区间 0x1000_0000..0x1000_FFFF_0000 << `CQ_BASE_GPA = 0xC0DE_0000_0000_0000`，0 冲突）。 |
| **R-5** | 新 — flags bit | Linux nvme-tcp host 收到 R2T 后 *可能* 一次性发完所有 H2CData（连续多 PDU 直到 DATA_LAST），也 *可能* 分多次；测试必须双路径覆盖。 | **V4a** unit test 双 case；**V4b** 接收侧用 `data_offset + data_length` 拼装 `Vec<u8>`，校验 `sum == r2t_length` 且最后 PDU 必带 `DATA_LAST`。 |
| **R-6** | 新 — DDGST + flags | H2CData 若带 DDGST，framing.rs 已处理；但 R2T encode 不能加 DDGST（无 data）。 | **V4a** encode helper assert `data.is_empty() || pdu_type != R2T`。 |

---

## 4. 子阶段切分

每 sub-phase = **1 commit + 1 subagent reviewer 轮**（rust-reviewer 优先；改到 NVMe spec 边界顺手叫 code-reviewer）。

### 4.1 V4a — wire layer only（R2T encode + H2CData decode/reassembler + TTAG 分配器）

#### Motivation

把"协议字节级 / 内存里完成"和"controller 异步路径耦合"两件事拆开。
V4a 完全不动 `session.rs::handle_admin_cmd`，也不接 `TcpAdminTransport`；
新 module + 一组纯函数 + 一组 unit test（in-process TcpStream pair 模拟）。
若 V4a fail，定位仅在 wire encoding；不会被 controller 异步路径混淆。

#### 测试隔离方式

- V4a 测试用 `tcp_pair()`（已有 helper）+ 直接 `write_pdu` / `read_pdu`；
  controller 完全不出场。
- 现有 V3 测试不受影响（V4a 不改 session.rs 任何方法签名）。

#### 文件清单

| 操作 | 文件 | 说明 |
|---|---|---|
| 新建 | `nvme_of_tcp_target/src/r2t.rs` | R2T encode 助手：`encode_r2t(cid, ttag, offset, len) -> (CommonHdr, R2tPsh)` |
| 新建 | `nvme_of_tcp_target/src/h2c_reassembler.rs` | `H2cReassembler { expected_ttag, expected_total, buf: Vec<u8>, received: u32 }`；`accept_pdu(&Pdu) -> AcceptOutcome { Continue, Done(Vec<u8>), Error(fes) }` |
| 新建 | `nvme_of_tcp_target/src/ttag.rs` | `TtagAllocator { next: u16 }`，`alloc() -> u16`，wrap 跳过 0 |
| 修改 | `nvme_of_tcp_target/src/lib.rs` | 追加 `pub mod r2t; pub mod h2c_reassembler; pub mod ttag;` |
| 修改 | `nvme_of_tcp_target/src/pdu.rs` | 确认 `R2tPsh` / `DataPsh` 已完整（无需扩字段），仅追加 doc |

#### PSH 结构现状核对（参考 `wire-reference.md` §4）

| PSH | 当前定义（`pdu.rs`） | spec 要求 | 状态 |
|---|---|---|---|
| `R2tPsh` | cccid u16 + ttag u16 + r2t_offset u32 + r2t_length u32 + rsvd [u8;4]（16 B） | 同 | ✓ |
| `DataPsh` | cccid u16 + ttag_or_rsvd u16 + data_offset u32 + data_length u32 + rsvd [u8;4]（16 B） | 同 | ✓ |

→ V4a **不需要** 新增 PSH 结构，仅追加 helper。

#### controller wrapper 改动

无。V4a 全部在 target crate 内。

#### 测试覆盖（≥3，实际 6）

1. `r2t_encode_byte_exact_per_spec`
2. `h2c_reassembler_single_pdu_complete`
3. `h2c_reassembler_multi_pdu_with_offsets`
4. `h2c_reassembler_rejects_ttag_mismatch`
5. `ttag_allocator_skips_zero_and_wraps`
6. `r2t_no_ddgst_invariant`

#### Commit message

```
feat(nvme-of-tcp): Phase V4a — R2T encoder + H2CData reassembler + TTAG allocator (wire only)
```

---

### 4.2 V4b — 单 R2T / 单 DataBlock 0x5 接入 controller（无切片、无 SGL chain）

#### Motivation

让"controller dma_read 一段 host bytes → 走 R2T 拉回来"这条最短路径先 work；
对应 NVMe spec 里最常见的小 Write（≤ 64 KiB 单段）。这个 sub-phase 不动 MAXH2CDATA
分片，也不动 SGL Segment chain；只引入 transport-specific (0x5) 单段 SGL 的解析。
完成后 admin path **NS Attachment 0x15 controller list fetch + 任何 ≤ 64 KiB 单 PRP 异步 dma_read** 已可用。

#### 测试隔离方式

- V4a 的 wire 测试继续 green。
- 新加 integration test：admin queue 上跑一条 controller 仅 dma_read 的命令——优先沿用
  NS Attachment 0x15（`controller/admin.rs:1261`）。
- 不接 IO Read/Write（IO 留 V5）。

#### 文件清单

| 操作 | 文件 | 说明 |
|---|---|---|
| 修改 | `nvme_of_tcp_target/src/tcp_transport.rs` | 删 V3 dma_read warn-only；新加 `pending_reads: VecDeque<PendingHostRead { gpa, len, token }>`；`dma_read` 入队；新增 `pop_read()`、`new_with_token_base(u64) -> Self`、`token_high_water() -> u64`。**修 R-1 + R-2**。 |
| 修改 | `nvme_of_tcp_target/src/session.rs` | `V2Session` 字段加 `next_token: u64`、`ttag_alloc: TtagAllocator`；`handle_admin_cmd` 内：dispatch 后若 `tcp_t.pending_reads` 非空，**为每条 read** alloc ttag → emit R2T → 调内嵌 `await_host_data` 收齐 → `nvme_admin_complete_dma(tok, true, bytes)` → loop 直到 pending_reads 空 → Phase 3 drain captured writes 不变。 |
| 修改 | `nvme_of_tcp_target/src/session.rs` | sentinel scheme 改：删 `PRP1_SENTINEL` 单值；改用 `prp1_sentinel_for_ttag(ttag: u16) -> u64`。 |
| 修改 | `pcie_remote_nvme_userspace/src/sgl.rs` | `SglType::TransportSpecific` 在 target 侧可接受（PCIe path 仍 reject）；放开 `parse_transport_specific` 入口；保持 67 测试不回归。 |

#### controller wrapper 改动

| API | 现状 | V4b 是否新加 | 说明 |
|---|---|---|---|
| `nvme_admin_dispatch` | 已有 | 否 | 复用 |
| `nvme_admin_complete_dma(token, ok, data)` | 已有 | 否 | V4b 把 `data: Vec<u8>` 填上 H2CData 重组结果 |
| `nvme_post_cqe` | 已有 | 否 | 复用 |
| `nvme_install_admin_cq` | 已有 | 否 | 复用 |
| `nvme_in_flight_read_count() -> usize` | 无 | **是**（新增） | debug-only：`await_host_data` 超时打 actionable warn |

#### 测试覆盖（≥3，实际 5）

1. `v4b_admin_dma_read_single_segment_round_trip` — NS Attachment 0x15 + 单 R2T 回 4 KiB
2. `v4b_token_monotonic_across_cmds` — 防 R-1 退化
3. `v4b_dma_read_failure_propagates_to_cqe` — host 发 H2CTermReq → controller 走 DATA_TRANSFER_ERROR CQE（覆盖 R-2）
4. `v4b_h2c_data_wrong_ttag_yields_term` — ttag 错配 → C2HTermReq
5. `v4b_sgl_transport_specific_type_accepted_in_target` — SGL 0x5 解析

#### Commit message

```
feat(nvme-of-tcp): Phase V4b — controller-initiated dma_read → R2T → H2CData 闭环（单段 ≤ 64 KiB）
```

---

### 4.3 V4c — MAXH2CDATA 分片 + 多 R2T 循环 + SGL Segment chain (0x2/0x3) 走 in-capsule

#### Motivation

- **大 Write 切片**：`dma_read.len > MAXH2CDATA (64 KiB)` 时需多 R2T；本步走串行多 R2T。
- **SGL chain 解析**：NVMe-oF TCP 下 Type 0x2/0x3 segment 必须 inline；与 PCIe 不同。

#### 测试隔离方式

- V4a + V4b 测试不动。
- 新 fixture：mock host 发 Type 0x2 Segment chain（3 段 DataBlock，每段 2 KiB），断 walker 走 V4b 闭环。
- 新 fixture：触发 controller dma_read 192 KiB → 3 条独立 R2T。

#### 文件清单

| 操作 | 文件 | 说明 |
|---|---|---|
| 修改 | `nvme_of_tcp_target/src/session.rs` `await_host_data` | 升级 multi-R2T 循环 |
| 新建 | `nvme_of_tcp_target/src/in_capsule_sgl.rs` | `walk_in_capsule_chain(...)`；递归（深度 ≤ 8） |
| 修改 | `nvme_of_tcp_target/src/session.rs` `dispatch_capsule_cmd` | 解 SQE.sgl1，按 type 分发 |
| 修改 | `nvme_of_tcp_target/src/session.rs` | const `MAXH2CDATA_BYTES: u32 = 64 * 1024;` 与 ic_handshake 宣告值绑一份 |

#### 测试覆盖（≥3，实际 6）

1. `v4c_dma_read_192kib_emits_three_r2t`
2. `v4c_dma_read_64kib_exact_one_r2t`
3. `v4c_dma_read_64kib_plus_one_emits_two_r2t`
4. `v4c_in_capsule_sgl_segment_chain_three_blocks`
5. `v4c_sgl_chain_depth_exceeded_rejected`
6. `v4c_maxh2cdata_const_matches_handshake`

#### Commit message

```
feat(nvme-of-tcp): Phase V4c — MAXH2CDATA 分片 + 多 R2T 循环 + in-capsule SGL Segment chain
```

---

## 5. V4 完成后的 capability matrix

| 能力 | V3 | V4a | V4b | V4c |
|---|:---:|:---:|:---:|:---:|
| Admin CapsuleCmd 解 + dispatch | ✓ | ✓ | ✓ | ✓ |
| Admin Read-out (C2HData ≤ 4 KiB / Identify / Get Log) | ✓ | ✓ | ✓ | ✓ |
| Admin dma_read 单段 ≤ 64 KiB（如 NS Attachment 0x15 ctrl list） | ✗ (review M-2) | ✗ | **✓** | ✓ |
| Admin dma_read 单段 > 64 KiB（分片） | ✗ | ✗ | ✗ | **✓** |
| IO Read 走 C2HData | ✗（V5） | ✗ | ✗ | ✗ |
| IO Write 走 R2T/H2CData（单 PRP，≤ 64 KiB） | ✗ | ✗ | ⚠ wire-ready | ⚠ wire-ready |
| IO Write > 64 KiB（多片） | ✗ | ✗ | ✗ | ⚠ wire-ready |
| SGL DataBlock (Type 0x0) inline | ✓ | ✓ | ✓ | ✓ |
| SGL Bit Bucket (Type 0x1) | ✓ | ✓ | ✓ | ✓ |
| SGL Segment / Last Segment (0x2 / 0x3) in-capsule chain | ✗ | ✗ | ✗ | **✓** |
| SGL Transport-specific (0x5) | ✗ | ✗ | **✓** | ✓ |
| R2T encode | ✗ | **✓** | ✓ | ✓ |
| H2CData decode + 重组 | ✗ | **✓** | ✓ | ✓ |
| MAXH2CDATA negotiated = 64 KiB | ✓（仅 ICResp 宣告） | ✓ | ✓ | ✓ + 真分片 |
| Token 跨 cmd 单调（修 R-1） | ✗ | ✗ | **✓** | ✓ |
| dma_read 失败可投 ok=false CQE（修 R-2） | ✗ | ✗ | **✓** | ✓ |

> ⚠ **wire-ready** = wire 层闭环完成；IO Read/Write 真正接入是 V5 的事。

---

## 6. 跨 sub-phase 顺序与 reviewer 安排

```
V4a (commit) → rust-reviewer → 若 0 critical/high 直接 V4b
   ↓
V4b (commit) → rust-reviewer + security-reviewer（TCP 输入边界 + sentinel scheme）
   ↓
V4c (commit) → rust-reviewer + code-reviewer（chain depth、off-by-one、ttag wrap）
   ↓
最后 doc-updater：把 capability matrix 同步进 phase-v-nvme-of-tcp.md § 7 V4 行
```

---

## 7. 不在 V4 范围内（明确 punt）

| 项 | 原因 | 接收方 |
|---|---|---|
| 多 R2T 并发流水（同 cmd 内多 ttag 同时 in-flight） | 单线程 read_pdu 阻塞模型不支持 | V8 |
| ICDOFF（Write inline data，省 1 RTT） | SGL parser 需 + offset 字段 | V4.5 / V5.5 |
| H2CData DDGST 验证 | framing.rs 已统一处理 | 已完成 (V1) |
| Multi-queue per session（IO SQ ≥ 2） | session 需 `Arc<Mutex<…>>` | V8 |
| AER 在 R2T 进行中送达 | 单 CQ 顺序约束 | V6 |
| Discovery subsystem 走 V4 路径 | Discovery 全 admin C2HData，不触发 dma_read | V7 |

---

## 8. Definition of Done（V4 整体）

- [ ] V4a / V4b / V4c 各自 commit + reviewer 0 critical / 0 high
- [ ] 现有 V3 测试 + pcie_remote 67 测试全部 green
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] R-1 / R-2 由 V4b 测试可触发可修复
- [ ] capability matrix V4c 列所有 ✓ 项有对应 passing test
- [ ] `phase-v-nvme-of-tcp.md` § 7 V4 行回填实际 LOC
- [ ] 末尾追加 V5 entrypoint hint

---

## 9. V5 entrypoint hint

V5 接入 IO Read/Write 时：
- IO SQ/CQ 建立走 admin Create IO SQ/CQ (opcode 0x01/0x05)，dispatch 已在 controller。
- IO 命令分发：`dispatch_capsule_cmd` 内 `opc != FABRIC && nsid != 0` → 调
  `controller.nvme_io_dispatch(ctx, sqe, cid, sq_id)`（V5 新增 wrapper）。
- IO Write 复用 V4b/V4c R2T 闭环。
- IO Read 复用 V3 captured `dma_write` → C2HData 路径。
