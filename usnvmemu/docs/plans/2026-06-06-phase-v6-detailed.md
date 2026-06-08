# Phase V6 详细实施计划 — AER (Async Event Request) 投递

> **✅ SHIPPED (2026-06-06 audit)** — V6a/b/c 全段落地 (commits `c6e03aa6` → `d16f6ca0`)；97 tests + reviewer H-1/M-1/M-3/M-4 修；inject_aen API + select-style pump_one_with_events。V8c per-conn AER routing 进一步升级。本文档保留为 AER 初版设计记录。

> **Status:** design draft
> **Date:** 2026-06-06
> **Branch:** `feat/pcie-remote-experimental`
> **Prereq:** V5 全段完成 (commits ffe4e92a → f77c5876)；84 lib tests + 1 bin smoke + 67 controller
> **Roadmap 锚:** [`2026-06-04-phase-v-nvme-of-tcp.md`](./2026-06-04-phase-v-nvme-of-tcp.md) § 7 第 V6 行
> **Spec 参考:**
> - NVMe Base 2.0c § 5.2 Async Event Request command (Figure 174)
> - NVMe Base 2.0c § 5.21.1.11 Set Features FID `0x0B`
> - NVMe TCP Transport spec 1.0a § 3.6（CapsuleResp 不区分 AER vs 普通 cmd）

---

## 0. 调研结论（影响后续决策）

| 事实 | 位置 | V6 含义 |
|---|---|---|
| `aen_pending: VecDeque<(cid, sq_id, cq_id)>` 已在 controller | `controller/mod.rs:~747` | session 不需自建队列；只需镜像 + cap |
| `pub(super) fn fire_aen(ctx, type, info, log_id) -> bool` | `controller/mod.rs:~1724` | **需新 `pub fn nvme_fire_aen` wrapper** 才能 session 调 |
| `fire_aen` 内部 `post_cqe` → `ctx.dma_write(cq_sentinel, 16B)` | `controller/mod.rs:~1753` | session **必须**用 `TcpAdminTransport` capture 才拿到 CQE bytes |
| controller `tick()` 三处自然 fire_aen：err count 涨 / Sanitize 完成 / Self-Test 完成 | `mod.rs:~2043/2056/2071` | V5 session 现在 silent drop |
| admin AER (opc=0x0C) push 后 dispatch 返 `None` | `controller/admin.rs:~649-668` | session `run_post_dispatch` 走 Phase 4 `cqe_bytes.ok_or` ⇒ `bail!` ⇒ **致命 regression** |
| Set Features FID 0x0B (`ASYNC_EVENT_CONFIG`) 走 `features.insert(fid, cdw11)` 默认分支 | `controller/admin.rs:~570` | V6a 只需 wire-test，不改 controller |
| session main loop `while sess.pump_one()? {}` | `bin/nvme_of_tcp_target.rs:~304` | V6b 单行改成 `pump_one_with_events(tick)` |
| `accept_and_handshake` 握手后 `set_read_timeout(None)` | `session.rs:~159` | V6b 重 set 不冲突；保留旧 pump_one |
| `TcpAdminTransport.writes` FIFO `VecDeque<DmaWriteRecord>` | `tcp_transport.rs:~76` | drain 可反复 `pop_write` |

**最致命**：host 一发 AER，当前 session `bail!` 断 TCP 连接。V6a 必先消。

---

## 2. V6 vs V5 关键差异

| 维度 | V5 | V6 |
|---|---|---|
| `run_post_dispatch` 无 reads + 无 writes | `bail "did not produce CQE"` | + AER fast-path：peek opc=0x0C → push pending + Ok |
| controller wrapper | 5 件 (`nvme_admin_dispatch` 等) | + `nvme_fire_aen`、`nvme_drain_aer_completions`、`nvme_pending_aer_count`、`nvme_has_pending_aen_event` |
| 主循环 | `while pump_one()? {}` 单点阻塞 | + `pump_one_with_events(tick)` select 风格 |
| AER 追踪 | controller `aen_pending` 单源 | + session 镜像 `pending_aers: Vec<PendingAer>` (cap = AERL+1 = 4) |
| 事件来源到 wire | 全 silent | controller tick 内 `fire_aen` 真到达 host |
| Set Features FID 0x0B | controller 接受 / wire 无 test | wire roundtrip test |
| 阻塞模型 | 纯同步 | 半同步：`set_read_timeout(Some(100ms))` 轮询 |

---

## 3. 风险 / 已知陷阱

| 标号 | 描述 | V6 处理 |
|---|---|---|
| **R-1** | `run_post_dispatch` 把 AER 误判为 controller bug bail | V6a peek opc=0x0C fast-path |
| **R-2** | `fire_aen` 内 `dma_write` 必须被 transport capture | V6b drain wrapper 强制传 `TcpAdminTransport` |
| **R-3** | 单线程 `read_pdu` 无 timeout ⇒ 事件无人 emit | V6b `set_read_timeout(Some(100ms))` + WouldBlock continue |
| **R-4** | spec § 5.2 多事件超 pending AER 按 spec drop | controller `fire_aen` 返 false 已实现；V6c test 覆盖 |
| **R-5** | session drop 时 controller `aen_pending` cid 残留可能跨 reconnect 误投 | V6 session lifetime 1:1 controller；多 conn punt V8 |
| **R-6** | poll < 100 ms 拖 CPU；≥ 100 ms 与 KATO 默认 5 s 不冲突 | 100 ms 与 V5d keepalive tick 对齐 |
| **R-7** | AER 只走 admin (spec § 5.2)；controller 强制 sq_id=0 | session 不变 |
| **R-8** | drain 后 caller 不知具体 fire 的 type/info | V6c test 用 `nvme_fire_aen` 显式触发已知 type |
| **R-9** | select 不 fair：cmd 持续来时 drain 永不 poll | 每条 cmd 后 + 每次 timeout 都跑 drain |
| **R-10** | 恶意 host 发洪量 AER → `pending_aers` 无界增长 | session cap = AERL+1 = 4；超出返 SC=0x05 `ASYNC_LIMIT_EXCEEDED` |
| **R-11** | `set_read_timeout` 后 partial 读 ⇒ 帧破 | `read_exact` + OS TCP buffer 天然保留；新增 `FramingError::ReadTimeout` 让 caller continue |
| **R-12** | drain 风暴占 CPU | drain 内 cap `MAX_DRAIN_PER_TICK = 4` |

---

## 4. 子阶段切分

### 4.1 V6a — AER cmd 不 bail + Set Features FID 0x0B wire test

**Motivation**：消最致命 regression。host 发 ≤ AERL+1 条 AER 不断连；controller `aen_pending` 累积。

**文件**：
- 改 `session.rs`：V2Session + `pending_aers: Vec<PendingAer>`；`handle_admin_cmd` 入口 peek opc=0x0C → AER fast-path（调 dispatch 让 controller 接，跳过 Phase 2..5 不 bail，push pending_aers，超 MAX 返 SC=0x05）
- 新 `aer.rs`：`PendingAer { cid, sq_id, registered_at }`、`ADMIN_OPC_AER: u8 = 0x0C`、`MAX_PENDING_AERS: usize = 4`、`peek_admin_opc(sqe) -> u8`
- 改 `lib.rs`：`pub mod aer`
- 改 `nvme_firmware/src/lib.rs`：新 `nvme_pending_aer_count() -> usize`

**controller wrapper**：`nvme_pending_aer_count` (read-only)

**测试 (5)**：
1. `v6a_aer_cmd_does_not_bail_session`
2. `v6a_multiple_aers_accumulate_to_aerl_plus_one`
3. `v6a_aer_then_identify_ns_still_works`
4. `v6a_set_features_async_event_config_roundtrip` (FID=0x0B cdw11=0x00FF → Get 同值)
5. `v6a_aer_on_io_queue_path_returns_invalid_opcode`

### 4.2 V6b — controller-initiated event emit (select-style pump_one)

**Motivation**：V6a 后 controller 攒 AER 但永不弹。V6b 接通 `fire_aen` 到 wire。

**文件**：
- 改 `session.rs`：保留旧 `pump_one`；新 `pump_one_with_events(tick: Duration) -> Result<bool>` — 入口 `set_read_timeout(Some(tick))` → 循环 drain + try read_pdu
- 新 helper `drain_aer_completions() -> Result<usize>` — 创 fresh TcpAdminTransport → controller drain wrapper → 每 16B CQE 走 `write_capsule_resp_bytes` → cap `MAX_DRAIN_PER_TICK=4` → 刷 `pending_aers` 镜像
- 改 `framing.rs`：新增 `FramingError::ReadTimeout`（`io::ErrorKind::WouldBlock` / `TimedOut` 映射）
- 改 `bin/nvme_of_tcp_target.rs`：`pump_one()` → `pump_one_with_events(Duration::from_millis(100))`
- 改 `nvme_firmware/src/lib.rs`：`nvme_fire_aen`、`nvme_drain_aer_completions`、`nvme_has_pending_aen_event`

**Wire 路径**：

```text
pump_one_with_events(tick):
  loop {
    if has_pending_aen && pending_aer_count > 0:
        for cqe in drain_aer_completions()?:
            write_capsule_resp_bytes(&cqe)?
    set_read_timeout(Some(tick))
    match read_pdu {
      Ok => dispatch + return Ok(true)
      ReadTimeout => continue
      PeerClosed => return Ok(false)
      other => return Err
    }
  }
```

**测试 (4)**：
1. `v6b_fire_aen_after_pending_emits_capsule_resp`
2. `v6b_fire_aen_without_pending_drops_silently`
3. `v6b_pump_one_with_events_passes_through_normal_cmds`
4. `v6b_drain_caps_at_max_per_tick`

### 4.3 V6c — 端到端 + README + doc 回填

**Motivation**：V6a/b 是单测；V6c 拼端到端 + 用户文档。

**文件**：
- 新 `tests/aer_e2e.rs`：ICReq → Connect → Identify → AER × 4 → trigger × 2 → 2 条 CapsuleResp + 剩 2 仍 pending
- 改 `README.md`：新 `### V6 AER` 章节
- 改 `phase-v-nvme-of-tcp.md` § 7 V6 行

**测试 (4)**：
1. `v6c_e2e_aer_then_smart_threshold_completes_first`
2. `v6c_e2e_aer_then_self_test_done_completes_next`
3. `v6c_e2e_fire_more_than_pending_drops_extras`
4. `v6c_e2e_aer_interleaved_with_io_read`

---

## 5. V6 完成后 capability matrix

| 能力 | V5d | V6a | V6b | V6c |
|---|:---:|:---:|:---:|:---:|
| Admin AER (0x0C) 不 bail session | ✗ | **✓** | ✓ | ✓ |
| 多 AER 累积 (≤ AERL+1) | ✗ | **✓** | ✓ | ✓ |
| `pending_aers` 越界保护 | ✗ | **✓** | ✓ | ✓ |
| Set Features FID 0x0B wire test | ✗ | **✓** | ✓ | ✓ |
| controller-initiated AEN emit on wire | ✗ | ✗ | **✓** | ✓ |
| 主循环 select read_pdu / drain_aer | ✗ | ✗ | **✓** 100 ms | ✓ |
| `nvme_fire_aen` external trigger | ✗ | ✗ | **✓** | ✓ |
| `nvme_drain_aer_completions` wrapper | ✗ | ✗ | **✓** | ✓ |
| `nvme_pending_aer_count` wrapper | ✗ | **✓** | ✓ | ✓ |
| `nvme_has_pending_aen_event` wrapper | ✗ | ✗ | **✓** | ✓ |
| 多事件超 pending → drop 语义 | ✗ | ✗ | ⚠ 无 test | **✓** |
| AER 与 IO Read/Write 并发顺序正确 | ✗ | ✗ | ⚠ 无 test | **✓** |
| Linux nvme-cli `nvme aer` manual interop | ✗ | ✗ | ⚠ wire-ready | **✓** README |

---

## 6. Reviewer 安排

```
V6a → rust-reviewer + code-reviewer（封装 + MAX 上限）
V6b → rust-reviewer + security-reviewer（read_pdu timeout + DDoS + partial-PDU + 公平性）
V6c → rust-reviewer + doc-updater（同步 § 7 V6 行）
```

---

## 7. 不在 V6 范围

| 项 | 接收方 |
|---|---|
| 真 timer-driven 温度阈值触发 | V-followup |
| Persistent Event Log (spec § 5.16.1.14) | V7+ |
| AER 与 Discovery subsystem 联动 | V7 |
| 多 conn 共享 controller 的 AER 路由 | V8 |
| 真 tokio 解耦 read_pdu / fire_aen | V8 / V-followup |
| KATO timer-based 死连接检测 | V8 |
| AER 重试 / 持久化跨 reconnect | V-followup |
| TLS / DH-HMAC-CHAP 下 AER | V-followup |

---

## 8. Definition of Done

- V6a/V6b/V6c 各 commit + reviewer 0 critical/high
- V5 + pcie_remote 全 84+ 测 green
- V6 新增 **13 测** (5 + 4 + 4) 全 green
- clippy + fmt clean
- capability matrix V6c ✓ 项均覆盖 test
- README 含 V6 章节
- `phase-v-nvme-of-tcp.md` § 7 V6 行回填实际 LOC

---

## 9. V7 / V8 / V-followup entrypoint hint

**V7 (Discovery)**：spec § 5.16.1.20 Get Log Page 0x70；纯 admin C2HData 路径无 AER 依赖；只需 `logs.rs::build_discovery_log(nqn, portal)` + admin LID=0x70 分支；bin 可加 `--mode=discovery|target`。

**V8 (Multi-queue / Disconnect / Arc-Mutex / 多 conn 共享 controller)**：
`Arc<tokio::sync::Mutex<NvmeController>>`；Disconnect (fctype 0x08) 真清理；AER 路由扩 `aen_pending` 带 conn_id；V6b 的 read_timeout-style 替换为真 tokio `select!`；移除 V5d R-8 单 conn-per-backing。同时合 V5-P4 (per-controller share)。

**V-followup**：TLS 1.3 + PSK / DH-HMAC-CHAP / Centralized Discovery (TP4126) / 持久化 reservation 跨 reconnect / 多 PDU C2HData telemetry / Combined-transport binary。

---

## 10. 设计 Q&A

**Q: 为什么不直接上 tokio？** V6 ~150 LOC；tokio refactor 影响 V5 全部 sync test + bin ~400 LOC，ROI 不划算。100 ms tick 对 AER 语义完全够（spec 不规定延迟）；V8 引 Arc-Mutex 时一并上 tokio。

**Q: `set_read_timeout(100ms)` 对 V5 阻塞测试有影响吗？** 无 — V6b `pump_one_with_events` 是独立 API；老 `pump_one` 保留 `set_read_timeout(None)` 行为。

**Q: drain_aer 内部要不要锁 controller？** V6 内 session 持 controller by value，单线程 ⇒ 无锁。V8 多 conn 时 `Arc<Mutex<...>>` 每次 fire/dispatch 一次 acquire。

**Q: 为什么 V6a 不顺手做 V6b？** V6a 是"消 regression"（host AER 不能断连），即使 V6b 永不做也是合理 milestone。V6b 是"加 capability"。分两 commit 让每段缺陷曝露面小。

**Q: drain_aer 后 pending_aers 镜像如何同步？** controller `aen_pending` 是 source of truth；session 镜像仅用于 cap + debug；drain 后按 `nvme_pending_aer_count` truncate 重算。

**Q: partial PDU + timeout 怎么办？** `read_exact` + OS TCP buffer 天然保留 partial bytes；新增 `FramingError::ReadTimeout` 类别让 caller continue；mock TcpStream test 覆盖 partial→resume。
