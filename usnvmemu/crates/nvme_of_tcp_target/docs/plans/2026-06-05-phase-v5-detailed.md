# Phase V5 详细实施计划 — IO Read/Write + bin 入口 + nvme-cli interop

> **✅ SHIPPED (2026-06-06 audit)** — V5a/b/c/d 全段落地 (commits `81dbed8e` / `0d0f6559` / `fc0d43b0`)；79 tests + bin smoke 全过；V5e-1/V5e-2 把 IO nlb cap 提到 16 LBA (`2d1c1282` / `1ba2cd5e`)；后续被 V-followup-prp-list 进一步提到 256 LBA (透明 chunking)。本文档保留为初版 IO 路径设计记录。

> **Status:** design draft
> **Date:** 2026-06-05
> **Branch:** `feat/pcie-remote-experimental`
> **Prereq:** Phase V4 (V4a + V4b + V4c) 全部完成（commit 665ba1ec）
> **Roadmap 节锚:** [`2026-06-04-phase-v-nvme-of-tcp.md`](2026-06-04-phase-v-nvme-of-tcp.md) § 7 第 V5 行
> **Wire 参考:** [`2026-06-04-nvme-tcp-wire-reference.md`](/usnvmemu/crates/nvme_of_tcp_target/docs/specs/2026-06-04-nvme-tcp-wire-reference.md) §1、§3、§4
> **Spec 参考:**
> - NVMe Base spec 2.0c § 5.4 (Create IO CQ)、§ 5.5 (Create IO SQ)、§ 6.8 (Read)、§ 6.9 (Write)
> - NVMe-oF spec 1.1a § 3.3 (Fabric Connect IO queue)、§ 3.5.1 (Fabrics 强制 SGL)
> - NVMe TCP Transport spec 1.0a § 3.6 (Capsule Cmd for IO)
> **Goal:**
> 1. IO Read 走 V3 C2HData 路径（controller `dma_write` capture）；
> 2. IO Write 走 V4 R2T/H2CData 闭环（controller `dma_read` capture）；
> 3. session 接入 admin Create IO CQ/SQ 流程 + Fabric Connect qid≥1；
> 4. 加 bin `main.rs`：TcpListener:4420 + per-connection thread + V2Session；
> 5. README + nvme-cli interop 步骤（Linux 真机 `nvme connect/list/dd/disconnect`）

---

## 1. 总览

V4 已让 admin queue 上的 controller-initiated `dma_read` / `dma_write` 走完
wire 闭环。V5 把同一套 dispatch/capture/闭环机制扩展到 **IO queue**。
V5d 把 lib 跑成对外 4420 服务，README 写 Linux nvme-cli 一步步 cmd。

---

## 2. V5 与 V4 的关键差异

| 维度 | V4 | V5 |
|---|---|---|
| 入口 cmd | 仅 admin (qid=0) | admin + IO (qid≥1) |
| controller dispatch wrapper | `nvme_admin_dispatch` | + `nvme_io_dispatch(ctx, sq_id, sqe, cid, cq_id)` |
| Fabric Connect qid≥1 | 仅 ack | 真校验 IO SQ/CQ 已建 + 记账 |
| session state | `admin_connected: bool` | + `io_queues: HashMap<u16, IoQueueState>`、`current_qid: u16` |
| 派发逻辑 | 仅 admin 分支 | + IO 分支（按 `self.current_qid != 0`） |
| sentinel | `PRP1_SENTINEL = 0x1000_0000` | + `cq_sentinel(qid) = CQ_BASE_GPA + qid*CQ_SENTINEL_STRIDE` |
| PSDT 处理 | session 改 prp1=sentinel | + 清 PSDT bits 让 controller 走 PRP path |
| bin 入口 | 无 | `main.rs` + TcpListener 4420 |

---

## 3. 风险 / 已知陷阱

| 标号 | 描述 | V5 处理 |
|---|---|---|
| **R-1** | controller 已能处理 Create IO CQ/SQ，但 session 不记账；后续 IO CapsuleCmd 找不到 cq_id | V5a session 加 `io_queues`；admin 路径 peek SQE → captured CQE 成功后 mirror queue |
| **R-2** | Fabric Connect qid≥1 现 stub-ack；V5 必须校验 IO SQ/CQ 已建 | V5a `handle_connect` qid≥1 分支查 `io_queues` 命中 → `connected=true` + `current_qid=qid` |
| **R-3** | session 区分 admin vs IO cmd 不能靠 opcode（IO Write 0x01 = admin Create IO SQ 0x01） | V5a 加 `current_qid`，单 conn 单 qid（per Linux nvme-tcp 真实行为） |
| **R-4** | IO Read/Write > 4 KiB 时 controller 走 PRP2 / PRP list；session sentinel scheme 不支持多 page | V5b/V5c 简化：要求 nlb=1（≤ 4 KiB）；多 page 留 V5e |
| **R-5** | NVMe-oF spec § 3.5.1 强制 SGL；controller `resolve_data_pointers` 在 PSDT=01 见 0x5 reject | V5b session 清 PSDT bits（`cdw0 &= !(0b11 << 14)`）让 controller 走 PRP；CQE 正确即合规 |
| **R-6** | IO CQ base_gpa 与 admin CQ sentinel 不能撞 | V5a `cq_sentinel(qid) = CQ_BASE_GPA + qid * 256`；admin Create IO CQ 时改写 sqe.prp1 |
| **R-7** | Linux nvme-cli `connect-all` 走 Discovery；V5 目标 IO subsystem | V5d README 用 `nvme connect -n nqn...`，不走 connect-all（Discovery 留 V7） |
| **R-8** | per-connection 多 conn 共享 backing file → controller 状态不一致 | V5d README 限制"单 backing 仅一 active conn"，per-file Mutex 拒新连接 |
| **R-9** | nvme-cli IO 前的 admin enum 可能漏 opcode | V5d README 推荐 `dmesg -w` + `nvme-cli --trace` 抓 log 回填 |

---

## 4. 子阶段切分

### 4.1 V5a — IO queue 安装 + Connect qid≥1 + dispatch 二分

**Motivation**：把"IO queue 在 controller 已经能建"告诉 session，并让 Fabric
Connect qid≥1 真校验。V5a **不接 IO Read/Write 数据**，仅打通"建 queue
→ connect queue → IO CapsuleCmd 进 dispatch 入口（V5a stub INVALID_OPCODE）"。

**文件**：
- 改 `session.rs`：V2Session 加 `io_queues: HashMap<u16, IoQueueState>`、`current_qid`、`cq_sentinel(qid)` helper
- 改 `session.rs`：`dispatch_capsule_cmd` 二分；`handle_connect` qid≥1 校验；`handle_admin_cmd` peek Create IO CQ/SQ 记账
- 新 `io_queue.rs`：`IoQueueState { kind: Cq{sentinel}/Sq{cq_id}, connected: bool }`
- 改 `lib.rs`：`pub mod io_queue`

**controller wrapper**：新 `nvme_io_dispatch(ctx, sq_id, sqe, cid, cq_id) -> Option<Cqe>` 薄 wrapper 调 `dispatch_io`。

**测试 (5)**：
1. `v5a_create_io_cq_sq_then_connect_io_qid1_succeeds`
2. `v5a_connect_io_qid_without_create_io_sq_rejected`
3. `v5a_create_io_cq_sentinel_distinct_from_admin`
4. `v5a_io_cmd_after_connect_returns_invalid_opcode`
5. `v5a_admin_path_still_green_after_io_queue_tracking`

### 4.2 V5b — IO Read（C2HData 闭环 ≤ 4 KiB / nlb=1）

**Motivation**：IO Read = controller `dma_write` capture → C2HData，与 V3 admin
Identify 完全同形。

**文件**：
- 改 `session.rs`：抽 `run_dispatched_cmd(...)` helper 共享 admin/IO；新 `handle_io_cmd(cid, sqe_bytes)` 查 sq→cq → `nvme_io_dispatch` → `run_dispatched_cmd`
- 改 `session.rs`：dispatch 前清 PSDT bits + nlb=1 guard（nlb>1 → SC=0x18 `SGL_DATA_LENGTH_INVALID`）

**controller wrapper**：V5a 已暴 `nvme_io_dispatch`，V5b 真用；`nvme_admin_complete_dma` 复用（admin/io 不区分）。

**测试 (4)**：
1. `v5b_io_read_nlb1_emits_c2hdata_and_resp` (tempfile pre-fill pattern + 内容校验)
2. `v5b_io_read_psdt01_transparent_to_host`
3. `v5b_io_read_nlb2_rejected_with_sgl_data_length_invalid`
4. `v5b_io_read_invalid_nsid_returns_invalid_namespace`

### 4.3 V5c — IO Write（R2T/H2CData 闭环 ≤ 4 KiB / nlb=1）

**Motivation**：IO Write = controller `dma_read` capture → R2T → H2CData →
on_dma_complete → post_cqe，完全复用 V4b/V4c `dma_read_via_r2t` 闭环。

**文件**：
- 改 `session.rs`：`handle_io_cmd` 增 WRITE 分支（`run_dispatched_cmd` helper 已 share Phase 2 闭环；仅保证 sqe.prp1=sentinel 同 V5b）
- 加 trace log 区分 admin/io 来源

**controller wrapper**：无新增；controller `dispatch_io::WRITE` 已被 V4b 闭环 cover。

**测试 (4)**：
1. `v5c_io_write_nlb1_round_trip`（后置 reopen 验 backing file 内容）
2. `v5c_io_write_nlb2_rejected`
3. `v5c_io_write_wrong_ttag_yields_term`
4. `v5c_io_write_then_read_roundtrip_data_integrity`

### 4.4 V5d — bin `main.rs` + README + Linux nvme-cli interop

**Motivation**：V5a/V5b/V5c 全是 in-process `tcp_pair()` 测试；V5d 跑成对外
4420 服务，作为真机 acceptance milestone。

**文件**：
- 新 `src/bin/nvme_of_tcp_target.rs`：clap CLI `--listen` / `--backing-file` (≥1) / `--vid` / `--ssvid` / `--zns-nsid` / `--max-conn` + TcpListener accept loop + per-conn thread + per-backing `Arc<Mutex<()>>` 限并发
- 改 `Cargo.toml`：加 `[[bin]]`
- 新 `README.md`：build/run/Linux interop step
- 新 `tests/bin_smoke.rs`：spawn bin → tcp 连接 → ICReq/Connect → close

**README 大纲**：
```
# nvme_of_tcp_target
## Build & Run
## Linux interop（modprobe nvme_tcp + nvme connect/list/id-ctrl/dd/disconnect）
## V5 已知限制（nlb=1, single conn per backing, 无 Discovery, 无 TLS）
## 测试
```

**测试 (1 + manual)**：
1. `bin_smoke_handshake_then_disconnect`（spawn bin + tcp 客户端）
+ manual: nvme connect / list / id-ctrl / dd 4k / disconnect

---

## 5. V5 完成后的 capability matrix

| 能力 | V4c | V5a | V5b | V5c | V5d |
|---|:---:|:---:|:---:|:---:|:---:|
| Admin CapsuleCmd dispatch | ✓ | ✓ | ✓ | ✓ | ✓ |
| Admin Create IO CQ/SQ → session 记账 | ✗ | **✓** | ✓ | ✓ | ✓ |
| Fabric Connect qid≥1 校验 | ⚠ stub | **✓** | ✓ | ✓ | ✓ |
| dispatch_capsule_cmd 二分 admin/IO | ✗ | **✓** | ✓ | ✓ | ✓ |
| IO Read (nlb=1, ≤ 4 KiB) | ✗ | ✗ | **✓** | ✓ | ✓ |
| IO Read (nlb>1) | ✗ | ✗ | ✗ (SC=0x18) | ✗ (SC=0x18) | ✗ |
| IO Write (nlb=1, ≤ 4 KiB) | ⚠ wire-ready | ⚠ | ⚠ | **✓** | ✓ |
| IO Write (nlb>1) | ✗ | ✗ | ✗ | ✗ (SC=0x18) | ✗ |
| IO Write→Read 持久化一致 | ✗ | ✗ | ✗ | **✓** | ✓ |
| PSDT=01 host wire 透明 | ⚠ | ⚠ | **✓** | ✓ | ✓ |
| `main.rs` TcpListener 4420 | ✗ | ✗ | ✗ | ✗ | **✓** |
| Linux nvme-cli connect / list / dd / disconnect | ✗ | ✗ | ✗ | ✗ | **✓** (manual) |
| 多 namespace (多 `--backing-file`) | controller-ready | ✓ | ✓ | ✓ | **✓** |
| Multi-queue per session | ✗ | ⚠ table ready | ⚠ | ⚠ | ⚠ → V8 |

---

## 6. 跨 sub-phase reviewer 安排

```
V5a → rust-reviewer + code-reviewer
V5b → rust-reviewer
V5c → rust-reviewer
V5d → rust-reviewer + security-reviewer + doc-updater
```

最后 doc-updater 同步 V5 行进 `phase-v-nvme-of-tcp.md` § 7。

---

## 7. 不在 V5 范围内

| 项 | 接收方 |
|---|---|
| IO nlb > 1 / IO > 4 KiB | V5e |
| Multi-queue per session | V8 |
| Discovery subsystem | V7 |
| AER | V6 |
| TLS / DH-HMAC-CHAP | V-followup |
| Disconnect 完整释放 | V8 |
| 共享 backing file 跨 conn | V8.5 |
| ICDOFF / inline Write data | V5.5 |
| 真 CI 跑 Linux nvme-cli | manual / future self-hosted runner |

---

## 8. Definition of Done

- V5a/V5b/V5c/V5d 各 commit + reviewer 0 critical/high
- V3/V4 + pcie_remote 67 测全 green
- V5 新增 14 测全 green
- clippy + fmt clean
- Linux 真机 manual 跑通 connect/list/id-ctrl/dd 4k/disconnect
- capability matrix V5d ✓ 项均覆盖 test
- README 覆盖 R-4/R-7/R-8 约束

---

## 9. V6 / V8 entrypoint hint

V6 (AER + SMART log)：spec 允许 controller 任意时刻 emit AER CQE；V5 单线程
同步 pump_one + R2T 占线 ⇒ AER 需要 controller-initiated CapsuleResp 路径
与 read_pdu 解耦；V6 + tokio refactor 最干净。

V8 (Multi-queue / Disconnect / Arc-Mutex controller core)：
- session 改 `Arc<Mutex<NvmeController>>` 让多 conn 共享
- 移除 V5d R-8 单 conn-per-backing 限制
- Disconnect (fctype 0x08) 真清理 io_queues + controller `delete_io_sq/cq`
