# Phase V8e-7 详细实施计划 — AsyncSession 完整 admin/IO dispatch + bin 切纯 async

> **Status:** design draft
> **Date:** 2026-06-06
> **Branch:** `feat/pcie-remote-experimental`
> **Prereq:** V8e-1..V8e-6 完成（HEAD `c6c11148`）；nvme_of_tcp_target 157
> active lib+integration test + 1 bin smoke 全 green；
> `#![forbid(unsafe_code)]` + `#![deny(clippy::await_holding_lock)]` 全 crate；
> `AsyncSession::pump_one_async` 已 select! 4 arm（Shutdown/PeerClosed/
> KatoExpired/AenReady/Pdu），但 `PumpEvent::Pdu(_)` 未真 dispatch；bin 每条
> conn 仍 `tokio::task::spawn_blocking(handle_conn)` sync 桥。
> **Spec 参考:** NVMe-oF 1.1a § 3.5 / § 5.2 / § 7.13；NVMe Base 2.0c § 5.x
> admin cmd 集。V8e-7 不引入新 spec 行为，仅把 sync V2Session dispatch 表面
> 平移到 async path。

---

## 1. 总览 + V8e-7 vs V8e-1..6 关系

V8e-7 是 V8e tokio refactor 系列的**收尾段**：把 `AsyncSession` 从 "select!
骨架" 补成 "真 dispatch 引擎"，让 bin 完全摆脱 spawn_blocking 桥。

| 段 | 落地能力 | 当前 dispatch 路径 |
|---|---|---|
| V8e-1 | tokio dep + `read_pdu_async / write_pdu_async` | sync only |
| V8e-2 | bin `#[tokio::main]` + watch shutdown + tokio TcpListener | **`spawn_blocking(handle_conn)` 桥** |
| V8e-3 | `AsyncSession::accept_and_handshake_async` + `pump_one_async` 骨架 | async path 只回 `PumpEvent::Pdu`，不分发 |
| V8e-4 | `Arc<Notify> aen_notify` + per-conn 自检 | sync only |
| V8e-5 | `kato_deadline: Option<Pin<Box<Sleep>>>` + reset | sync only |
| V8e-6 | e2e 真并发 sync IO + nvme-cli manual | sync only |
| **V8e-7** | **AsyncSession 完整 dispatch + bin 切纯 async** | **全 async** |

---

## 2. 风险表

| 标号 | 描述 | 触发条件 | 缓解 phase | 检测手段 |
|---|---|---|:---:|---|
| R-1 | **sync / async dispatch 漂移**：sync `handle_admin_cmd` 600 LOC，新加 async 版后两套独立演化 | 后续小改 | V8e-7-1 | 抽 sans-IO 决策表共用；wire emit 各自；review checklist 写入两边都改 |
| R-2 | **KATO reset 漏点**：spec § 7.13 任一 cmd 应 reset；handler 入口忘调致 idle timer 误触发 | dispatch 多入口 | V8e-7-2 | 入口集中 `dispatch_pdu_async` 顶部统一调；单测 `v8e7_dispatch_resets_kato_per_pdu` 验 3 类 PDU 都触发 |
| R-3 | **Disconnect 跨 task 状态泄漏**：sync `handle_disconnect` 走 `bail!`，async 改 PumpEvent 让 caller 接管，conn_id / io_queues 状态在 AsyncSession Drop 没 sweep IO queue | Disconnect path | V8e-7-2 | AsyncSession Drop 补 IO queue sweep；引入 PumpEvent::Disconnected；单测验 controller pending_aer / list_io_sqs 归零 |
| R-4 | **持锁跨 await**：dma_read_via_r2t_async 写错 → 编译过却 clippy `await_holding_lock` 触发；closure 内 await 死锁 | R2T 三段式 | V8e-7-2 | lock-pop / unlock-await wire / lock-complete 严格三段；与 V8b §9 Q5 / V8e §3 Q5 一致 |
| R-5 | **AER 顺序 invariant 破坏**：async drain captured CQE 严格 data 先 / CQE 后，async task 穿插可能改 push 顺序 | drain_aers_async + 并发 IO | V8e-7-3 | drain_aers_async 单短锁内 fire + pop captured；不让任何 await 穿插；单测 `v8e7_drain_aer_no_capture_split` |
| R-6 | **spawn_blocking 删除后线程模型变**：多 task 共享 worker thread，任何 sync IO 残留阻塞 worker | bin 切 handle_conn_async | V8e-7-4 | 编译期 handle_conn_async 仅接 tokio::net::TcpStream；review grep std::net；运行时 perf gate |
| R-7 | **AsyncSession Drop 在 runtime drop 时序问题**：runtime shutdown → AsyncSession Drop 在 worker thread 跑 controller.lock() | runtime shutdown | V8e-7-2 | controller 保 parking_lot::Mutex（V8e §3 Q1 决策）；Drop 内只 sync lock；V8e-3 已沿用此设计 |
| R-8 | **回归测试覆盖盲点**：dispatch_pdu_async 新写 ~500 LOC 单测难穷举；现有 V8b/c/d/f 走 sync 不 cover async | dispatch_pdu_async 行为漂移 | V8e-7-3 | (a) 新 integration test ≥12 测覆盖每 fctype + admin opc + IO Read/Write；(b) sync vs async byte-identical gate 2 项 |
| R-9 | **handle_disconnect bail 语义改 PumpEvent**：与 sync 同测试断言会碎 | v8d_disconnect 测试形式 | V8e-7-2 | v8d 走 sync API 不动；async 新加 `v8e7_disconnect_emits_pump_event`；分轨独立 |
| R-10 | **Pin 移动 / select! 借用穿插**：dispatch_pdu_async 内若再调 pump_one_async 形成嵌套 select! 借用冲突 | R2T 子路径 | V8e-7-2 | R2T 子路径**不**走 pump_one_async；直接 read_pdu_async；嵌套期间 AER/shutdown 暂不响应（acceptable） |
| R-11 | **代码膨胀**：复制 sync handlers → async 镜像 = 600+ LOC；单 phase 超 250 LOC | 一次写完 | V8e-7-1..4 | 分 4 段：决策表抽取 / fabric+admin / IO+AER / bin 切换 |
| R-12 | **inject_aen sync API 在 v6b/v6c 测试用**：删 / 改签名破老测试 | inject_aen path | V8e-7-3 | sync `inject_aen` 保留；新加 `inject_aen_async` 独立 |
| R-13 | **`with_controller_async` API 设计**：V8e §3 Q1 决策 closure-only；async path 直接复用 sync `with_controller`（parking_lot 短锁，no await） | controller lock 策略 | V8e-7-1 | 复用现 `with_controller`；不引入 async 版本直到真需要 tokio::sync::Mutex |
| R-14 | **`PumpEvent::Pdu` emit 后 dispatch 借走 `&mut self.stream`，嵌套 R2T 再借** | borrow 时序 | V8e-7-2 | pump_one_async 返 owned Pdu（已是设计）；handle_conn_async 调 `self.dispatch_pdu_async(pdu).await`；select! 已结束 |

---

## 3. 设计 Q&A

### Q1: V8e-7 是否需要新 `PumpEvent::Disconnected`？

**答**：**新增**，与 `Shutdown` / `PeerClosed` 语义区分。

- `Shutdown`：外部 (watch::Sender) 通知停；可能 mid-cmd 强退
- `Disconnected`：对端 fabric Disconnect cmd 完成后正常关；spec 行为，session
  已 ACK，IO queue / AER 应清，conn_id 立 release
- `PeerClosed`：peer 直接 close TCP 没发 Disconnect

handle_conn_async 主 loop 对三者都 break，但 log metadata 区分。R-9 担心
v8d_disconnect 测试碎：v8d 走 sync path 不经过 PumpEvent；async 分轨独立单测。

### Q2: KATO reset 在 `dispatch_pdu_async` 入口 vs 每 handler 入口？

**答**：**dispatch_pdu_async 函数体顶部统一调一次**。

spec § 7.13 "any command other than AER" 都算 keep-alive。集中调避免漏点
（R-2），handler 数 ≥ 8 每个加调用 = 8 处易错。AER 例外性：spec 文字模糊
（Linux nvmet 也按 "any cmd reset" 实现）；严格 "AER 不 reset" 版本留
V8e-followup。

### Q3 (核心)：sync/async 600 LOC dispatch 代码重复怎么办？

**答**：**抽 sans-IO 决策表（选 b） + 渐进迁移精神（c）**。

理由：决策核（admin opc 0x80 reject / AER 超 cap / discovery mode 白名单 /
V5e-2 PRP2 sentinel 规则 / sqe.prp1 sentinel 改写 / Create IO CQ/SQ peek）
都是纯函数。wire emit / R2T / await_host_data sync 跑阻塞 IO，async 跑 await
— 必须分两套。

抽 `DispatchPlan` enum：

```
enum DispatchPlan {
    SendCapsuleErr { cid, sc },
    SendCapsuleOk { cid, dw0, dw1 },
    SendC2hTerm { fes },
    FastPathAer { cid },
    DispatchControllerAdmin { sqe, cid, create_io_cq_qid, create_io_sq_pair },
    DispatchControllerIo { sqe, cid, sq_id, cq_id },
    Disconnect { cid },
}
```

sync / async dispatch 都先调 `decide_capsule_plan(pdu, snapshot)`，再各自
`execute_plan`。后续 V8f / V9 加新 fctype / opc 只需改 decide 函数 + 给 enum
加 variant。单测 decide 函数独立（纯函数好测）；execute 单测专注 wire byte。

副作用：sync handlers 也被改（V8e-7-1）；regression gate 必须严格 — 任何
byte-stream 改动会被 v8e1 byte-identical gate + v8b/c/d 既有测试拦截。

### Q4: drain_aers_async caller 调 vs 内部自动？

**答**：**AsyncSession 方法 `drain_aers_async(&mut self) -> Result<usize>`，
caller 收到 `PumpEvent::AenReady { pending }` 后显式调一次**。

理由：AsyncSession 持 stream + controller + next_token，drain 涉及短锁 fire
+ 改 next_token + write CapsuleResp 三件事，全在 AsyncSession 内最自然。
caller 显式调让 PumpEvent flow 保 "事件驱动 + caller 决策"，便于将来 V9 加
"AER throttle"（caller 决定 drain 多少条）。一次 drain N 条 PDU emit
(N = `min(pending, MAX_DRAIN_PER_PUMP)`)；与 sync V6b fairness 一致。

### Q5: handle_conn_async 主 loop 与 AsyncSession Drop 时序

```
async fn handle_conn_async(stream, shared, mut shutdown) -> Result<()> {
    let mut sess = accept_and_handshake_async(stream, shared).await?;
    loop {
        match sess.pump_one_async(&mut shutdown).await? {
            PumpEvent::Pdu(p) => {
                let out = sess.dispatch_pdu_async(p).await?;
                if out.disconnected { break; }
            }
            PumpEvent::AenReady { .. } => { sess.drain_aers_async().await?; }
            PumpEvent::KatoExpired
            | PumpEvent::PeerClosed
            | PumpEvent::Shutdown => break,
        }
    }
    Ok(())
}   // sess drop here → Drop impl 跑 AER cleanup + IO queue sweep
```

Drop 在 owned AsyncSession 离开作用域时跑，不依赖 tokio runtime drop 时机；
parking_lot 锁同步调，无 await，无 panic 风险（V8e-3 已 catch_unwind 防护）。

---

## 4. 分阶段拆分

每 phase ≤ 250 LOC impl + ≤ 200 LOC test；独立 reviewer-clean commit；保持
157+1 测全 green（regression gate）。

### V8e-7-1 — sans-IO 决策表抽取（refactor 段，不动行为）

**目标**：把 sync `dispatch_capsule_cmd / handle_admin_cmd / handle_io_cmd`
中"纯决策"部分抽成 `dispatch_plan.rs` 模块；sync handlers 改走 "decide →
execute" 两段。V8e-7-2/3 的 async dispatcher 复用同决策核。

**文件**：
- `src/dispatch_plan.rs`（新）：`DispatchPlan` enum + `decide_*` 纯函数 +
  `ConnStateSnapshot` struct
- `src/session.rs`：sync handlers 改 "snapshot → decide → match plan execute"
- `src/lib.rs`：`pub mod dispatch_plan;`

**impl LOC**：~180；**test LOC**：~180

**测试**（12 测，纯函数好测）：
1. `decide_fabric_connect_admin_returns_dispatch_controller_admin`
2. `decide_fabric_disconnect_returns_disconnect_plan`
3. `decide_admin_aer_over_max_returns_async_limit_exceeded`
4. `decide_admin_aer_under_max_returns_fast_path_aer`
5. `decide_admin_format_nvm_returns_invalid_opcode`
6. `decide_admin_discovery_mode_white_list_rejects_non_listed`
7. `decide_io_nlb_over_max_returns_sgl_data_length_invalid`
8. `decide_io_psdt_bits_cleared_in_plan`
9. `decide_io_dual_prp_sentinel_set_when_nlb_over_8`
10. `create_io_cq_plan_carries_qid_for_sentinel_rewrite`
11. `pdu_too_short_returns_send_c2h_term`
12. `unexpected_pdu_type_returns_send_c2h_term`

**regression gate**：v8b/c/d/f 全 sync 集成测试 + 既有 lib 单测全 green。

**Entry hint to V8e-7-2**：决策表就位 + sync 改走它后，async dispatcher 只需
调同一 decide_* + 自己写 async execute。

### V8e-7-2 — AsyncSession fabric + admin dispatch + Disconnect / KATO 接通

**目标**：补 AsyncSession 完整 fabric (Connect/Property/Disconnect) + admin
(Identify/Get Log/Keep Alive/etc) 路径；KATO reset 在 dispatch_pdu_async
入口接通；新加 PumpEvent::Disconnected；Drop 补 IO queue sweep。

**AsyncSession 新字段**：cntlid / admin_connected / current_qid / io_queues /
ttag_alloc / pending_aers

**文件**：
- `src/async_session.rs`：扩字段 + 新 `dispatch_pdu_async` + 各 async handler
  + `run_post_dispatch_async`（含 R2T 三段式）+ wire emit 系列 + Drop 加 IO
  queue sweep
- 新 `PumpEvent::Disconnected`

**impl LOC**：~230；**test LOC**：~200

**测试** (10)：
1. `connect_admin_async_returns_cntlid`
2. `property_get_cap_8byte_uses_dw1_async`
3. `property_set_cc_enables_csts_rdy_async`
4. `disconnect_async_emits_outcome_disconnected`
5. `disconnect_async_sweeps_io_queues_on_drop`
6. `admin_identify_controller_async_emits_c2hdata_and_resp`
7. `dispatch_resets_kato_per_pdu`
8. `admin_keep_alive_resets_kato_explicit`
9. `v4b_admin_dma_read_async_single_segment_round_trip`
10. `async_format_nvm_rejected_via_decide`

**Entry hint to V8e-7-3**：fabric/admin 已通；缺 IO Read/Write + AER drain。
IO path 与 admin path 共享 `run_post_dispatch_async`。

### V8e-7-3 — IO Read/Write async + drain_aers_async + inject_aen_async

**目标**：补 IO cmd async path；V8e-4 AER Notify 唤醒后真 wire emit；新加
inject_aen_async 给测试用。

**文件**：`src/async_session.rs` 加 `handle_io_cmd_async` / `drain_aers_async`
/ `inject_aen_async`；`dispatch_pdu_async` IO opc 路径接 handler。

**impl LOC**：~220；**test LOC**：~200

**测试** (10)：
1. `io_read_nlb1_async_emits_c2hdata_and_resp`
2. `io_write_nlb1_async_round_trip`
3. `io_read_nlb16_8kib_async_dual_prp`
4. `io_write_then_read_async_data_integrity`
5. `io_nlb_over_max_async_rejected_with_sc_18`
6. `drain_aers_async_after_inject_emits_capsule_resp`
7. `drain_aers_async_cap_4_per_pump`
8. `aen_notify_then_drain_async_round_trip`
9. **`sync_vs_async_byte_identical_admin_identify`** (R-1 / R-8 关键 gate)
10. **`sync_vs_async_byte_identical_io_read`** (R-1 / R-8 关键 gate)

### V8e-7-4 — bin 切 handle_conn_async + 删 spawn_blocking 桥

**目标**：bin 主 + V8f 第二 listener 全改 `tokio::spawn(handle_conn_async)`；
删 `spawn_blocking` 与 `stream.into_std()`；sync `handle_conn` 函数保留供
V8b/c/d/f 集成测试用。

**文件**：
- `src/bin/nvme_of_tcp_target.rs`：新 `handle_conn_async`；主/disc accept loop
  改 `tokio::spawn`；删 `stream.into_std()`
- `README.md`：V8e-7 章节 "bin 全 async；spawn_blocking 桥退役"
- `tests/v8e7_4_bin_async_smoke.rs`

**impl LOC**：~120；**test LOC**：~180

**测试** (4)：
1. `bin_handles_single_conn_async_io_read`
2. `bin_4_conns_concurrent_async_no_spawn_blocking`（用 tokio Handle metrics
  `num_blocking_threads()` < initial+1）
3. `bin_kato_async_path_closes_idle_conn`
4. `bin_disconnect_async_releases_conn_id`

---

## 5. 测试矩阵

| Phase | unit | integration | regression gate |
|---|:---:|:---:|:---|
| V8e-7-1 | 12 | 0 | 157+1 baseline 全 green |
| V8e-7-2 | 0 | 10 | v8b/c/d/f 全 green |
| V8e-7-3 | 0 | 10（含 2 个 byte-identical） | v5b/c/e/v6c/v8c 全 green |
| V8e-7-4 | 0 | 4 | bin_smoke + v8e2 + v8f 全 green |

**总计 ≈ 36 测**；预期 V8e-7 完成后 ≈ 193+1。

---

## 6. 推荐执行顺序

```
V8e-7-1  决策表抽取                180+180 LOC   ~3h   中风险
   ↓
V8e-7-2  AsyncSession admin+fabric  230+200 LOC   ~5h   高风险 ★最大手术
   ↓
V8e-7-3  IO + AER drain             220+200 LOC   ~4h   中风险
   ↓
V8e-7-4  bin 切 async + 删桥         120+180 LOC   ~3h   低风险
```

**总 impl LOC**：~750；**总 test LOC**：~760；**估时**：~15h。

每 phase commit 前必跑：`cargo test`（scoped） + `cargo clippy --all-targets
-- -D warnings -D clippy::await_holding_lock` + `cargo fmt --check` +
rust-reviewer + V8e-7-2/4 security-reviewer。**不绕过 git hooks**。

---

## 7. 与 V8b/c/d reviewer LOW item 联动

| Item | 来源 | V8e-7 phase | 处理方式 |
|---|---|:---:|---|
| L-3 V8b with_controller 名字易混 | V8b | V8e-7-1 | rename 留 V8e-followup（避免本 phase 改面爆炸） |
| L-4 V6b pump_one_with_events tick 100ms 偏长 | V6b | V8e-7-3 | async path 走 Notify；sync 标 `#[deprecated]` |
| L-5 V8c 测试 fixtures 重复 | V8c | V8e-7-2 | 抽 `async fn fresh_async_session_pair()` common helper |
| L-7 V6b inject_aen 命名 | V6b | V8e-7-3 | `inject_aen_async` prod path；sync inject_aen 留测试用 |
| spawn_blocking 桥退役 | V8e-2 | V8e-7-4 | README 章节 + bin 注释 |

---

## 8. Definition of Done

- [ ] V8e-7-1..7-4 各独立 commit；rust-reviewer + security-reviewer (V8e-7-2/4) 0 critical / 0 high
- [ ] 157+1 baseline 全 green，无回归
- [ ] V8e-7 新增 ≈ 36 测全 green
- [ ] clippy `-D warnings -D clippy::await_holding_lock` clean
- [ ] cargo fmt --check clean
- [ ] forbid(unsafe_code) 全 crate 保持
- [ ] bin 端 spawn_blocking(handle_conn) 全部删除；handle_conn sync 函数保留
- [ ] sync vs async dispatch byte-identical gate ≥ 2 项（admin Identify + IO Read）
- [ ] AsyncSession::dispatch_pdu_async doc 注明 fctype + admin opc + IO opc 矩阵
- [ ] README "V8e-7 — bin 全 async；spawn_blocking 桥退役" 章节
- [ ] AsyncSession Drop 在 disconnect / peer close / KATO 三场景下都 release conn_id / cleanup AER / sweep IO queue

---

## 9. 下次执行 hint

```
1. git status clean；HEAD 在 V8e-6 完成或之后
2. cargo test -p nvme_of_tcp_target --no-fail-fast 2>&1 | tee /tmp/v8e7_baseline.txt
   baseline 157 active + 1 bin smoke 全 green
3. 起 V8e-7-1（决策表抽取）：
   a. 读本 plan §3 Q3 + §4 V8e-7-1
   b. 新 src/dispatch_plan.rs：
      - DispatchPlan enum (§3 Q3 列出的 variant)
      - ConnStateSnapshot struct (immutable 字段)
      - decide_fabric_plan / decide_admin_plan / decide_io_plan 纯函数
   c. session.rs sync handlers 改 "snapshot → decide → match execute"
   d. tests/v8e7_1_dispatch_plan.rs 12 单测
   e. cargo test 全 green / clippy / fmt
   f. rust-reviewer subagent (run_in_background: false)
   g. reviewer-clean 后 commit "feat(nvme-of-tcp): Phase V8e-7-1 — dispatch_plan sans-IO 决策表"
4. 进 V8e-7-2 前再读 §2 R-3/R-4/R-10 + §3 Q1/Q5 + §4 V8e-7-2
5. V8e-7-2/3/4 类推
6. V8e-7-4 完成 + README 更新 + reviewer-clean 后 push；memory/MEMORY.md 加
   "NVMe-oF TCP V8e-7 完成"条目
```
