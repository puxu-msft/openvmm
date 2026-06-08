# Phase V8e 详细实施计划 — tokio async refactor（KATO timer / AER wakeup / async controller share）

> **✅ SHIPPED (2026-06-06 audit)** — V8e-1..V8e-6 + V8e-7 全段落地 (commits `06a16176` → `eb04fcf4`)；192 active test + clippy 0；tokio runtime + AsyncSession + AER Notify (< 10ms) + KATO timer + 多 conn 并发 e2e；dispatch_plan 决策表 + bin spawn_blocking 退役。这是项目从 sync TCP 转 tokio 的关键 phase；async/await + lock 教训见 [LESSONS.md](LESSONS.md) §3。本文档保留为完整 design 记录。

> **Status:** design draft
> **Date:** 2026-06-06
> **Branch:** `feat/pcie-remote-experimental`
> **Prereq:** V8a/b/c/d/f 完成（最新 push commit `5a009d48`）；
> nvme_of_tcp_target 124 active lib+integration test + 1 bin smoke 全 green；
> tokio 1.47.1 已在 workspace `Cargo.lock`（rt-multi-thread / macros / net / sync /
> time / signal 全 feature 已被其它 crate 拉过，本 crate 仅需声 dep）
> **Spec 参考:** NVMe-oF 1.1a § 7.13 (Keep Alive)、§ 7.12 (AER lifecycle)、
> NVMe Base 2.0c § 5.2 (Asynchronous Event Request)、TP4126 (TLS/secure channel —
> 仍不在本 phase 范围)

---

## 0. V8e 起点事实

| 事实 | 位置 | V8e 含义 |
|---|---|---|
| bin 用 `std::thread::spawn` + non-blocking `TcpListener` + 100ms poll | `bin/nvme_of_tcp_target.rs:117-465` | 整 main 改 `#[tokio::main]` + `tokio::spawn` |
| session 阻塞 `std::net::TcpStream` + `read_pdu/write_pdu(&mut TcpStream)` | `session.rs:130` `framing.rs` | `read_pdu` 给一个 async 版（`async fn read_pdu_async`），保留同步版给 V8b/c/d/f 测试 |
| `parking_lot::Mutex<NvmeController>` 短锁（V8b `with_controller` helper） | `lib.rs:85` `session.rs:258` | 改 `tokio::sync::Mutex`；`with_controller` 改 `async fn` 或保留 sync 双轨 |
| `pump_one_with_events(tick=100ms)` 真 polling | `session.rs:311` | 改 `tokio::select!` arm = `read_pdu_async / aen_notify.notified() / kato_sleep / shutdown.cancelled()` |
| `aen_pending` 单源 + per-conn `conn_id` 标签（V8c） | `controller/mod.rs:~747` | 加 `tokio::sync::Notify`；fire_aen_for_conn / pop_pending_aer 时 `notify_waiters` |
| KATO 字段解到 `ConnectFabricFields.kato` 但无 timer | `fabric.rs:111` `session.rs:997` | session 持 `tokio::time::Sleep` per-conn；admin cmd 走完即 reset deadline |
| `ctrlc` crate + `AtomicBool running` | `bin/nvme_of_tcp_target.rs:323` | 改 `tokio::signal::ctrl_c` + `CancellationToken` (tokio_util)；ctrlc dep 删 |
| BC API `accept_and_handshake(TcpStream, NvmeController)` 同步签名 | `session.rs:182` | 保 sync wrapper（内部 spawn current_thread runtime 兜底）让现有 30+ test 不改签名 |
| `forbid(unsafe_code)` 全 crate | `lib.rs:37` `bin/nvme_of_tcp_target.rs:34` | tokio 自身 unsafe 不传递；forbid 只检本 crate 编译单元，OK |
| rust toolchain 1.95 | 仓库根 `rust-toolchain.toml` | tokio 1.47.1 在 1.75 即已稳定，1.95 全 API 可用 |

V6b plan 注释（`session.rs:307`）已留 hint："V8 + tokio refactor 后改真 select!"；V8b lib.rs:139 也写了"V8e tokio refactor 时改 controller field 为 `tokio::sync::Mutex<NvmeController>`"。两条线索都指向本 plan。

---

## 1. V8e vs V8a/b/c/d/f 差异

| 维度 | V8f 完成 | V8e-1 | V8e-2 | V8e-3 | V8e-4 | V8e-5 | V8e-6 |
|---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| `tokio` dep & runtime | 缺 | **加** | ok | ok | ok | ok | ok |
| bin `#[tokio::main]` | 缺 | 缺 | **改** | ok | ok | ok | ok |
| `CancellationToken` shutdown（替 ctrlc） | 缺 | 缺 | **改** | ok | ok | ok | ok |
| `tokio::net::TcpListener` accept | 缺 | 缺 | **改** | ok | ok | ok | ok |
| session async pump（`read_pdu_async`） | 缺 | 缺 | spawn_blocking 桥 | **改** | ok | ok | ok |
| controller `tokio::sync::Mutex` | parking_lot | parking_lot | parking_lot | **改** | ok | ok | ok |
| `with_controller` async 版 | sync | sync | sync | **加** | ok | ok | ok |
| AER wakeup `Notify` channel | 100ms poll | poll | poll | poll | **加** | ok | ok |
| KATO timer 真实施 | 缺 | scaffolding | scaffolding | scaffolding | scaffolding | **加** | ok |
| Multi-conn 真并发 IO e2e | 串行 V8b | 缺 | 缺 | 缺 | 缺 | 缺 | **加** |
| AER 延迟 | ~100ms | ~100ms | ~100ms | ~100ms | **< 10ms** | < 10ms | < 10ms |
| Linux nvme-cli `connect -i 4 --keep-alive-tmo 10` 真实施 | 仅 ICReq timeout | 同左 | 同左 | 同左 | 同左 | **加** | ok |

---

## 2. 风险表

| 标号 | 描述 | 触发条件 | 缓解 phase | 检测手段 |
|---|---|---|:---:|---|
| R-1 | tokio 与 sync `std::net` 桥接：现有 30+ 集成测试用 `std::net::TcpStream::connect` + `accept_and_handshake(stream, ctrl)`；改 API 后全部碎 | session API 全 async 化 | V8e-3 | 保留 `accept_and_handshake` sync wrapper（内部 `Builder::new_current_thread` 跑 async path）；每 phase 跑 `cargo test --all-targets` 验回归 |
| R-2 | `tokio::sync::Mutex<NvmeController>` await 跨锁死锁（V8b R-1 在 async 下的新形态）：`with_controller_async(|c| async { ... await ... })` 让 read_pdu 在锁内 await | dma_read_via_r2t 在 async 版重构走偏 | V8e-3 | `with_controller_async` closure 用 `FnOnce(&mut NvmeController) -> R`，不传 future；clippy lint `await_holding_lock` deny；R2T loop 走 lock-pop / unlock / await wire / lock-complete 三段 |
| R-3 | `tokio::sync::Mutex::lock()` 返 `MutexGuard` 持锁跨 await 编译过却 deadlock | 重构后某条 hot path 不小心持 guard await | V8e-3 | clippy `await_holding_lock` 全 crate `-D warnings`（升级 deny） |
| R-4 | BC wrapper `accept_and_handshake` 同步签名怎么保？同步路径需要 runtime | 现有测试不让改签名 | V8e-3 | 同步 wrapper 内 `Builder::new_current_thread().enable_all().build().block_on(...)`；加 doc 警告 |
| R-5 | AER `Notify` 通知后 controller `aen_pending` 共享读但 lock 顺序错→ session 拿到 stale 计数 | fire_aen_for_conn 与 notify 顺序：先 push 后 notify vs 反 | V8e-4 | "push 入 deque → drop guard → notify_waiters"；单测 `v8e4_notify_after_push_visible` |
| R-6 | KATO timer reset 单源：admin cmd 完成时刷 deadline，多线程 / spawn 任务都要看见 | KATO timer task 与 admin pump task 是否同 task | V8e-5 | 同 task：select! 内 admin path 直接 `Pin::as_mut(&mut sleep).reset(now + tmo)` |
| R-7 | per-conn AER `Notify` 还是 controller-wide `Notify`？per-conn 内存大 + 共享 controller 拿不到 conn-specific notifier | controller 内部不知 session 在哪一条 conn 上 wait | V8e-4 | controller 持 `Arc<Notify>` 单实例；session select! 都 wait 同一 Notify；唤醒后自检 `nvme_pending_aer_count_for_conn(self.conn_id)` |
| R-8 | shutdown：`CancellationToken` vs `Notify` vs `broadcast`？bin 主 + V8f 第二 accept loop + per-conn worker 都要听到 | 现 V8f 用 `AtomicBool` 共享 | V8e-2 | `tokio_util::sync::CancellationToken`；clone 给所有 task |
| R-9 | tokio `forbid(unsafe_code)` 兼容性 | tokio crate 内部 unsafe；担心传递 | V8e-1 | `forbid(unsafe_code)` 是 lint 本 crate 编译单元，不递归 dep（V5d/V8f 已用 parking_lot 等含 unsafe dep 证实） |
| R-10 | tokio 引入 cold build +12s | 全员开发体感差 | V8e-1 | 文档化；feature 已被仓库其它 crate 拉过，`Cargo.lock` 不新增 entry |
| R-11 | `tokio::time::Sleep` reset 后 select! 旧 future 仍 pending → 死循环 | KATO reset 实现走偏 | V8e-5 | `Pin::as_mut(...).reset(deadline)`；select! 用 `&mut *sleep`；单测 `v8e5_kato_reset_under_load_no_double_wake` |
| R-12 | `accept_and_handshake` 在 tokio runtime 内被调时 `block_on` panic | V8e-3 后 bin 测试同时还有 V8b 老 sync 测试 | V8e-3 | sync wrapper 加 `Handle::try_current().is_err()` guard：runtime 内调 → `bail!("use accept_and_handshake_async in tokio context")` |
| R-13 | `inject_aen` test API（V6b）改 async 后 V6c 现有 3 个 e2e 测试碎 | session API 改动 | V8e-3 | `inject_aen` 双轨：留 sync (内部 block_on) + 新 `inject_aen_async` |
| R-14 | `pump_one_with_events` 也是 pub API；V6c / V8c / V8d 都直接调 | API 改动 | V8e-3 | 加 `pump_one_async`；老 `pump_one_with_events(tick)` 保留作 sync wrapper |
| R-15 | KATO=0 = "Keep Alive disabled" (spec § 7.13)；不能默认起 timer | V6c 某些测试用 kato=0 | V8e-5 | `Option<Pin<Box<Sleep>>>` 用 match 而非 unwrap；None 时 arm = `std::future::pending()` |
| R-16 | tokio mt-runtime 任务 panic 不传播；V8b reviewer H-3 已注意 controller half-mutated 风险 | session task panic 后 controller lock guard 不 release | V8e-3 | `tokio::sync::Mutex` 无 poison；文档化"任一 conn task panic 应整 process 重启"；bin 主 task `JoinSet` 监听 panic |
| R-17 | 现有 `bin_smoke.rs` 用 `Command::new` 起子进程；改 `#[tokio::main]` 后 startup 路径变 | bin 入口语义 | V8e-2 | smoke 测试不调内部函数，验外部行为；只要 listen / accept / handshake 行为一致即 pass |
| R-18 | V8f 双 listener 共享 `running: AtomicBool`；改 `CancellationToken` 后需要 2 个 child token 还是 1 个 | shutdown 协同 | V8e-2 | 1 个 `CancellationToken`，两个 accept loop 都 select! 它；`child_token()` 留 V9 区分 sub-system shutdown |

---

## 3. 设计 Q&A

### Q1: `tokio::sync::Mutex` vs `parking_lot::Mutex`？

**答**: 换 `tokio::sync::Mutex`。
- `with_controller` closure 看似 sync，但上层被 `read_pdu_async.await` 包夹；parking_lot 持锁卡 worker thread。
- `tokio::sync::Mutex::lock().await` 让 runtime 可调度别的 task。
- 代价：MutexGuard 跨 await 编译过却语义死锁 → clippy `await_holding_lock = deny` + closure-only API（拒 future）。

### Q2: KATO timer 用 `Interval` 还是 `Sleep`？

**答**: `Option<Pin<Box<tokio::time::Sleep>>>`。KATO 本质是 deadline（每次 admin cmd 后 reset），`Interval` 是周期事件语义不符。`Pin::as_mut(...).reset(deadline)` 原地刷新；`Option` 处理 spec § 7.13 KATO=0 disabled。

**reset 触发点**：(1) handle_admin_cmd 入口、(2) handle_io_cmd 入口、(3) opc 0x18 Keep Alive 走 (1)、(4) AER fire **不算**（server-side）。

### Q3: AER wakeup — `Notify` vs `mpsc` vs `broadcast`？per-conn vs controller-wide？

**答**: controller-wide 单 `Arc<tokio::sync::Notify>` + per-conn select! 自检 `nvme_pending_aer_count_for_conn(self.conn_id)`。

| 候选 | per-conn | controller-wide |
|---|---|---|
| Notify | N 个 + controller 端 HashMap 管理 | 1 个广播；自检成本 = 1 atomic load + 1 short lock |
| mpsc | per-conn channel；controller 持 HashMap<conn_id, Sender> | 需要 broadcast |
| broadcast | N/A | 有 lag 风险，背压复杂 |

实现 ~15 LOC vs mpsc ~50 LOC；spurious wakeup 无 perf 影响。

### Q4: BC wrapper 同步签名是否保留?

**答**: 保留（V8b R-10 一脉相承）。
- `accept_and_handshake(TcpStream, NvmeController)` sync 签名不变；内部 `Builder::new_current_thread().enable_all().build().block_on(...)`
- 加 runtime-in-runtime guard：`Handle::try_current().is_err()` → 否则 `bail!`
- 新增 `accept_and_handshake_async(TokioTcpStream, SharedController)` 给 bin 用
- `pump_one_with_events` 同样双轨

30+ V8b/c/d/f 集成测试用 `std::thread::spawn(|| accept_and_handshake(...))` + `std::net::TcpStream::connect` 全继续工作；V8e 新测试用 `#[tokio::test]` + `tokio::net::TcpStream`。

### Q5: dma_read_via_r2t 的 await 跨锁怎么避？

**答**: 三段式严格不变（V8b plan §9 Q5，V8e 强化）：
```
let req = {                                     // 第 1 段：短锁
    let mut c = self.controller.lock().await;
    c.pop_pending_read()
};                                              // guard drop
let bytes = dma_read_via_r2t_async(&mut self.stream, req).await?;  // 第 2 段：锁外 wire
{                                               // 第 3 段：短锁
    let mut c = self.controller.lock().await;
    c.nvme_admin_complete_dma(req.token, &bytes);
}
```
clippy `await_holding_lock = deny` 做编译期防御。

### Q6: shutdown 用 `CancellationToken` vs `Notify` vs `broadcast`?

**答**: `tokio_util::sync::CancellationToken`。Notify 只通知一次（新 spawn 子 task 会错过）；broadcast lag 语义复杂；CancellationToken 持久状态 + clone cheap + `child_token()` 留 V9 扩展。

**dep 新增**：`tokio-util` 0.7（仅 `sync` feature）；workspace `Cargo.lock` 已含。

---

## 4. 分阶段拆分

每 phase ≤ 300 LOC impl + ≤ 200 LOC test；每 phase 必须独立 reviewer-clean commit；**保持现有 124+1 测全 green**（regression gate）。

### V8e-1 — tokio dep & async scaffolding（热身）

**目标**：Cargo.toml 加 tokio/tokio-util dep；lib.rs 加 async surface 占位；不动 bin / session 行为。

**文件**：
- `Cargo.toml`：加 `tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros", "net", "sync", "time", "signal"] }` + `tokio-util = { version = "0.7", features = ["sync"] }`
- `src/lib.rs`：`pub mod async_runtime;` 新空 mod
- `src/framing.rs`：`pub async fn read_pdu_async<S: AsyncRead + Unpin>` stub（unimplemented!）+ `write_pdu_async`
- `tests/v8e1_tokio_dep_smoke.rs`：5 测

**impl LOC**：~50；**test LOC**：~80

**测试**：
1. `v8e1_tokio_runtime_basic` — `Builder::new_current_thread().build()` 不 panic
2. `v8e1_cancellation_token_smoke` — clone + cancel + cancelled().await
3. `v8e1_tokio_time_feature_enabled` — `tokio::time::sleep(1ms).await` 通过验 time feature
4. `v8e1_forbid_unsafe_still_holds` — `cargo check` 编译过
5. `v8e1_no_existing_test_regression` — meta：`cargo test --workspace --no-fail-fast` 记 baseline

**Entry hint to V8e-2**：tokio runtime 就位后 bin 入口 `#[tokio::main]` 是最小 surface 改动；session 仍同步走 `spawn_blocking`。

### V8e-2 — bin `#[tokio::main]` + CancellationToken + tokio TcpListener

**目标**：bin 主入口改 tokio runtime；ctrlc → tokio::signal::ctrl_c；AtomicBool running → CancellationToken；TcpListener → tokio::net::TcpListener；每条 conn `tokio::task::spawn_blocking` 包当前 sync handle_conn。

**文件**：
- `src/bin/nvme_of_tcp_target.rs`：
  - `fn main()` → `#[tokio::main(flavor = "multi_thread")] async fn main()`
  - 删 ctrlc + `Arc<AtomicBool> running` → `CancellationToken shutdown`
  - `TcpListener::bind` → `tokio::net::TcpListener::bind`
  - non-blocking poll → `tokio::select! { _ = listener.accept() => ... , _ = shutdown.cancelled() => break }`
  - `std::thread::spawn(handle_conn)` → `tokio::task::spawn_blocking(move || handle_conn(stream.into_std()?, shared))`
  - V8f 第二 accept loop 同改；shutdown token clone 给两 loop
- `Cargo.toml`：删 `ctrlc = "3"`
- `tests/v8e2_bin_shutdown.rs`：6 测；`bin_smoke.rs` 不变

**impl LOC**：~180；**test LOC**：~180

**测试**：
1. `v8e2_bin_starts_tokio_runtime_and_listens` — spawn 子进程；TCP 连一次；SIGTERM 关
2. `v8e2_cancellation_token_stops_accept_loop` — SIGTERM → 2s 内 exit 0
3. `v8e2_dual_listener_both_stop_on_shutdown` — V8f 路径 + shutdown 协同
4. `v8e2_spawn_blocking_path_handles_existing_sync_handshake` — sync handle_conn 在 spawn_blocking 内工作
5. `v8e2_max_connections_still_enforced` — N+1 conn 被 drop（V5d C-1 不回归）
6. `v8e2_no_ctrlc_crate_in_deps` — `cargo tree -p nvme_of_tcp_target` 不含 `ctrlc`

**Entry hint to V8e-3**：bin 已纯 async；下一步把 handle_conn 内 sync session pump 改 async；最敏感面是 `with_controller` 改 `tokio::sync::Mutex`。

### V8e-3 — session async pump + `tokio::sync::Mutex<NvmeController>`（大手术）

**目标**：framing 加 `read_pdu_async / write_pdu_async`；session 加 `accept_and_handshake_async / pump_one_async`；SharedControllerInner controller 字段换 `tokio::sync::Mutex`；sync API 保留 BC wrapper。

**文件**：
- `src/framing.rs`：`read_pdu_async` / `write_pdu_async` 真实施（移植 sync 逻辑）
- `src/lib.rs`：`SharedControllerInner.controller: parking_lot::Mutex → tokio::sync::Mutex`；`allocate_token_slab / allocate_conn_id` 不变（Atomic）
- `src/session.rs`：
  - 加 `pub async fn accept_and_handshake_async(tokio_stream, shared)`
  - 加 `pub async fn pump_one_async(&mut self, shutdown: CancellationToken)`（select 暂 2 arm：read_pdu_async / shutdown.cancelled；AER/KATO V8e-4/5 加）
  - `with_controller(|c| ...)` → `with_controller_async(|c| ...).await`（closure `FnOnce(&mut NvmeController) -> R`，禁 future）
  - `dma_read_via_r2t` 三段式重构
  - 保留 sync `accept_and_handshake / pump_one / pump_one_with_events / inject_aen`：内部 `Handle::try_current().is_err()` 后 `block_on(...)`；in-runtime 时 `bail!`
- `Cargo.toml`：保留 `parking_lot`（如仍其它处用）
- `tests/v8e3_session_async.rs`：8 测

**impl LOC**：~280；**test LOC**：~200
**clippy**：`-D await_holding_lock`

**测试**：
1. `v8e3_read_pdu_async_basic`
2. `v8e3_write_pdu_async_basic`
3. `v8e3_accept_and_handshake_async_full_iccr`
4. `v8e3_with_controller_async_no_deadlock_under_concurrent_conn` — `#[tokio::test(flavor="multi_thread")]` 2 task 共享 controller 并发 dispatch
5. `v8e3_sync_wrapper_in_runtime_bails_safely`
6. `v8e3_sync_wrapper_outside_runtime_still_works` — `std::thread::spawn(|| accept_and_handshake(...))` 老路径
7. `v8e3_dma_read_via_r2t_three_stage_no_lock_across_await`
8. `v8e3_no_existing_v8b_test_regression` — meta：`cargo test --test v8b_multi_conn`

**Entry hint to V8e-4**：select 只 2 arm；下一步加 AER wakeup `Notify` 取代 100ms poll。

### V8e-4 — AER wakeup `Notify` channel + select! arm

**目标**：controller 加 `Arc<tokio::sync::Notify> aen_notify`；fire_aen_for_conn / fire_aen 后 `notify_waiters`；session `pump_one_async` 加第 3 arm；唤醒后自检 conn-id 计数。

**文件**：
- `crates/pcie_remote_nvme_userspace/src/controller/aer.rs`（或 mod.rs）：加 `pub fn aen_notify_handle(&self) -> Arc<Notify>`；fire 末尾 `notify_waiters()`
- `src/session.rs`：
  - V2Session 加 `aen_notify: Arc<Notify>`（accept_and_handshake_async 内 clone）
  - `pump_one_async` 第 3 arm：`_ = self.aen_notify.notified() => { /* self-check + drain */ }`
  - `inject_aen` → `inject_aen_async`（sync wrapper 保留）
  - 删 `pump_one_with_events` 内 100ms poll；保 API 兼容（内部 `block_on(pump_one_async)`）
- `tests/v8e4_aer_wakeup.rs`：6 测

**impl LOC**：~150；**test LOC**：~180

**测试**：
1. `v8e4_notify_after_fire_aen_visible` — fire 后立即 `notified()` 完成
2. `v8e4_aer_emit_latency_under_10ms` — `#[tokio::test]` 测 fire→wire < 10ms
3. `v8e4_two_conns_aer_only_target_conn_drains` — V8c per-conn 路由 + Notify 自检
4. `v8e4_notify_does_not_pre_signal`
5. `v8e4_v6c_regression` — `cargo test --test aer_e2e`
6. `v8e4_v8c_regression` — `cargo test --test v8c_per_conn_aer`

**Entry hint to V8e-5**：select! 已 3 arm（read+shutdown+AER）；下一步加 KATO timer 第 4 arm。

### V8e-5 — KATO timer 真实施 + admin cmd 触发 reset

**目标**：session 持 `Option<Pin<Box<Sleep>>> kato_deadline`；Connect 成功后读 tmo；select! 第 4 arm；超时 → `Err(KatoExpired)` 关 conn；handle_admin_cmd / handle_io_cmd 入口 reset。

**文件**：
- `src/session.rs`：
  - V2Session 加 `kato_tmo: Duration` + `kato_deadline: Option<Pin<Box<Sleep>>>`
  - `handle_connect` 解析 kato 后初始化（kato=0 → None）
  - `pump_one_async` 加第 4 arm（None 时 `std::future::pending::<()>()`）
  - 加私有 `fn reset_kato_deadline(&mut self)` — admin/IO cmd 入口调
- `src/fabric.rs`：无改（kato 已解出）
- `tests/v8e5_kato.rs`：7 测

**impl LOC**：~120；**test LOC**：~200

**测试**（spec § 7.13）：
1. `v8e5_kato_tmo_zero_disables_timer` — kato=0 → 5s 不返
2. `v8e5_kato_tmo_expired_closes_conn` — kato=1s + 不发 cmd → 1.5s 内 `Err(KatoExpired)`
3. `v8e5_kato_reset_by_admin_cmd_keeps_conn_alive` — 每 500ms 发 Identify；3 轮后仍活
4. `v8e5_kato_reset_by_keep_alive_opc_0x18`
5. `v8e5_kato_reset_by_io_cmd_extends_deadline`
6. `v8e5_kato_reset_under_load_no_double_wake` — R-11
7. `v8e5_aer_in_flight_does_not_count_as_kato_reset` — Q2 决策

**Entry hint to V8e-6**：所有 V8e 子能力齐了；下一步 e2e 真并发 IO 兑现 V8c reviewer TODO。

### V8e-6 — e2e 真并发 IO + Linux nvme-cli manual

**目标**：tokio 起多 conn 真并发 IO；README 加 V8e 章节 + nvme-cli `connect -i 4 --keep-alive-tmo 10` manual。

**文件**：
- `tests/v8e6_concurrent_io.rs`：5 测
- `tests/v8e6_kato_e2e.rs`：3 测
- `README.md`：V8e 章节（KATO 启用 / nvme-cli manual / latency 数据）

**impl LOC**：~30（doc）；**test LOC**：~280

**测试**：
1. `v8e6_4_conns_concurrent_io_read_different_namespaces` — 4 task 真并发 read
2. `v8e6_4_conns_concurrent_io_write_no_data_race`
3. `v8e6_aer_storm_does_not_block_io` — 1 conn 收 100 AER 不阻塞另一 conn IO
4. `v8e6_kato_timeout_in_one_conn_does_not_affect_others`
5. `v8e6_shutdown_drains_inflight_io` — CancellationToken 后等 in-flight
6. `v8e6_nvme_cli_manual_recipe_documented` — meta：README grep `nvme connect -t tcp -i 4 --keep-alive-tmo 10`
7. `v8e6_full_v8_capability_matrix_summary_doc` — meta：README 含完整矩阵
8. `v8e6_bin_smoke_with_4_conns_runs_under_5s` — perf gate

---

## 5. 测试矩阵汇总

| Phase | unit | integration | e2e | regression gate |
|---|:---:|:---:|:---:|:---|
| V8e-1 | 5 | 0 | 0 | 124+1 baseline 全 green |
| V8e-2 | 0 | 6 | 0 | bin_smoke + v8f 不破 |
| V8e-3 | 8 | 0 | 0 | v8b_multi_conn 全 green |
| V8e-4 | 6 | 0 | 0 | aer_e2e + v8c_per_conn_aer 全 green |
| V8e-5 | 7 | 0 | 0 | v8d_disconnect 全 green |
| V8e-6 | 0 | 0 | 8 | 156+ 测全 green |

**总计新增：40+ 测**

---

## 6. 推荐执行顺序

```
V8e-1 (scaffolding, 50+80 LOC, 1h, 低风险)
  ↓
V8e-2 (bin async, 180+180 LOC, 3h, 中风险)
  ↓
V8e-3 (session async, 280+200 LOC, 6-8h, 高风险) ★ 最大手术
  ↓
V8e-4 (AER wakeup, 150+180 LOC, 3h, 中风险)
  ↓
V8e-5 (KATO timer, 120+200 LOC, 3h, 低中风险)
  ↓
V8e-6 (e2e + manual, 30+280 LOC, 4h, 低风险)
```

**总 impl LOC**：~810；**总 test LOC**：~1120；**估时**：20-24h。

**关键 gate**：每 phase commit 前必跑 `cargo test --workspace --no-fail-fast` + `cargo clippy --workspace --all-targets -- -D warnings -D clippy::await_holding_lock` + `cargo fmt --check`；reviewer-clean 才推 commit。**不绕过 git hooks**。

---

## 7. 与 V8b/c/d/f reviewer 已记 LOW item 联动

| Item | 来源 | V8e 处理 phase | 处理方式 |
|---|---|:---:|---|
| M-1 mirror 一致性 | V8c reviewer | V8e-4 | Notify wakeup 后强制重新 sync mirror；删 truncate hack |
| M-4 max_connections 默认 16 偏低 | V8b reviewer | V8e-2 | bin CLI doc 改进；默认值不动 |
| L-1 ctrlc sync handler 与 tokio runtime 双线 signal 风险 | V8f reviewer | V8e-2 | ctrlc dep 删除，改 `tokio::signal::ctrl_c` |
| L-2 V8f dual listener `inflight` 两 AtomicUsize 重复 | V8f reviewer | V8e-2 | 复用 `JoinSet` 或保留 + 文档化 |
| L-3 V8b `with_controller` 名字易混 | V8b reviewer | V8e-3 | sync 改名 `with_controller_sync_blocking` + deprecated |
| L-4 V6b `pump_one_with_events` tick=100ms 偏长 | V6b reviewer | V8e-4 | 变 sync wrapper；async 路径用 Notify |
| L-5 V8c 测试 fixtures 重复 boilerplate | V8c reviewer | V8e-3 | common module 抽 `async fn fresh_session_pair()` |
| L-6 V5d SIGPIPE 注释假设 std startup 默认 | V5d reviewer | V8e-2 | 注释从 bin 顶移到 lib.rs doc |
| L-7 V6c inject_aen 命名 `_for_test` 但实际是 prod path | V6b reviewer | V8e-4 | 拆 `inject_aen_async` (prod) + `inject_aen` (sync test wrapper) |

---

## 8. 下次执行 hint

```
1. 切回 feat/pcie-remote-experimental 分支 + 确认 git status clean
2. cargo test -p nvme_of_tcp_target --no-fail-fast 2>&1 | tee /tmp/v8e_baseline.txt
   baseline 应为 124 active + 1 bin smoke + controller_core 73 全 green
3. 起 V8e-1：
   a. 改 usnvmemu/crates/nvme_of_tcp_target/Cargo.toml 加 tokio + tokio-util
   b. 写 src/async_runtime.rs 空 mod + 顶层 doc
   c. 写 src/framing.rs read_pdu_async / write_pdu_async stub
   d. 写 tests/v8e1_tokio_dep_smoke.rs 5 测
   e. cargo test -p nvme_of_tcp_target --test v8e1_tokio_dep_smoke
   f. cargo clippy -p nvme_of_tcp_target --all-targets -- -D warnings
   g. cargo fmt --check
   h. 调 rust-reviewer subagent (run_in_background: false)
   i. reviewer-clean 后 commit "feat(nvme-of-tcp): Phase V8e-1 — tokio 基础设施 + scaffolding"
4. 进 V8e-2 前先看本 plan §3 Q4 + §4 V8e-2 / §2 R-1 R-4 R-12 R-17 R-18
```

---

## 9. Definition of Done

- [ ] V8e-1..V8e-6 各独立 commit；reviewer 0 critical / 0 high
- [ ] 198 baseline 全 green，无回归
- [ ] V8e 新增 40+ 测全 green
- [ ] `cargo clippy --workspace --all-targets -- -D warnings -D clippy::await_holding_lock` clean
- [ ] `cargo fmt --check` clean
- [ ] `forbid(unsafe_code)` 全 crate 保持
- [ ] README V8e 章节 + nvme-cli `connect -i 4 --keep-alive-tmo 10` manual recipe
- [ ] AER 端到端延迟 < 10ms（v8e4_aer_emit_latency_under_10ms 验）
- [ ] KATO timer 真生效（v8e5_kato_tmo_expired_closes_conn 验）
- [ ] 4 conn 真并发 IO 无 corruption（v8e6_4_conns_concurrent_io_* 验）
