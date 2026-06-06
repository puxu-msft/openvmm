# Phase V8 详细实施计划 — 多 conn 共享 controller / Disconnect 真清 / Builder + multi-portal / 单 bin 双 listener

> **✅ SHIPPED (2026-06-06 audit)** — V8a/b/c/d/f 五段全段落地 (commits `d801306c` → `5a009d48`)；124 tests + clippy 0；8 轮 reviewer C-1/C-2/H-1..H-4/M-1..M-3 全修。原文档预期的 V8e 部分被新文档 [2026-06-06-phase-v8e-tokio-detailed.md](2026-06-06-phase-v8e-tokio-detailed.md) 接走（tokio runtime 重构）。本文档保留为 V8 主体设计记录。

> **Status:** design draft
> **Date:** 2026-06-06
> **Branch:** `feat/pcie-remote-experimental`
> **Prereq:** V7c 完成 (最新 `dcfec8ed`)；105 nvme_of_tcp lib + 73 controller_core + 1 bin smoke 全 green
> **Spec 参考:** NVMe-oF 1.1a § 3.5 (Disconnect), § 3.3 (Connect), NVMe Base 2.0c § 7.6.1 (Delete IO SQ/CQ), § 5.2 (AER)

## 0. 调研结论

| 事实 | 位置 | V8 含义 |
|---|---|---|
| `V2Session { controller: NvmeController }` 持值 | `session.rs:130` | 改 `Arc<Mutex<NvmeController>>`；~12 处调用点 |
| `aen_pending: VecDeque<(cid, sq_id, cq_id)>` 单源 | `mod.rs:~747` | 多 conn 共享时需 conn_id 标签 |
| 单 bin per-backing Mutex (V5d R-8) | `bin/...rs:204-212` | V8b 拆除 |
| Disconnect fctype 0x08 stub | `session.rs:384-388` | V8d 真实现 |
| Discovery + NVM 需 2 process | `bin/...rs` 单 listener | V8f 双 listener |
| `IdentifyController::build_v2_bytes` 不接 cntrltype | `cmd.rs:500` | V8a builder pattern |
| tokio 不在 deps | `Cargo.toml` | V8e 引入；建议推 V-followup |

## 1. V8 vs V7 差异

| 维度 | V7c | V8a | V8b | V8c | V8d | V8e | V8f |
|---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| Session controller | by value | by value | **Arc<Mutex>** | ✓ | ✓ | ✓ | ✓ |
| Per-backing Mutex (R-8) | ✓ | ✓ | **拆** | ✗ | ✗ | ✗ | ✗ |
| Multi-conn share controller | ✗ | ✗ | **✓** | ✓ | ✓ | ✓ | ✓ |
| Disconnect 真清 | stub | stub | stub | stub | **✓** | ✓ | ✓ |
| AER per-conn routing | broadcast | broadcast | broadcast | **✓** | ✓ | ✓ | ✓ |
| Async (tokio select!) | sync 100ms | sync | sync | sync | sync | **✓** | ✓ |
| Identify byte-111 patch | post-hoc | **builder** | builder | builder | builder | builder | builder |
| CLI 多 portal | 单 portal | **Vec zip** | ✓ | ✓ | ✓ | ✓ | ✓ |
| 单 bin 双 listener | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | **✓** |

## 2. 风险表

| 标号 | 描述 | sub-phase | 处理 |
|---|---|---|---|
| R-1 | Arc<Mutex> 后 dispatch 内嵌 dma_read_via_r2t 跨 read_pdu 死锁 | V8b | 短锁原则：pop_read → drop guard → read_pdu → re-acquire complete_dma |
| R-2 | 多 conn 并发 install_admin_cq 撞 | V8b | idempotent guard + `nvme_has_admin_cq()` |
| R-3 | qid 跨 conn 唯一性 | V8b | controller 端 cqs.contains_key 拒重复 |
| R-4 | AER 跨 conn 投错 | V8c | aen_pending 加 conn_id 字段 |
| R-5 | Disconnect 错删别 conn queue | V8d | 按 self.io_queues 表 + conn_id |
| R-6 | per-backing 拆除后并发 open 同 backing | V8b | bin startup 一次 open + Arc::clone |
| R-7 | next_token / ttag_alloc 跨 conn | V8b | per-session 独立保留 |
| R-8 | tokio sync Mutex 不能 hold across await | V8e | tokio::sync::Mutex |
| R-9 | 双 listener SIGINT 协同 | V8f | 共享 running: AtomicBool |
| R-10 | builder 改 API 破 73 controller test | V8a | thin wrapper 保 BC |
| R-11 | --discovery-target-* Vec 改后 BC | V8a | 默认 empty + validate len |
| R-12 | conn drop 后 controller 内 AER 残留 | V8c | Drop impl 调 nvme_cleanup_conn_aers |
| R-13 | tokio refactor 影响所有 sync test | V8e | 拆 V8e1 lib + V8e2 bin |
| R-14 | 双 listener 共享 controller AER 串扰 | V8f | 两 controller 实例 |

## 3. 子阶段

### V8a — IdentifyController builder + multi-portal CLI（**热身**）

**Motivation**：V7c-fix CNTRLTYPE 是 post-hoc byte 111 patch；V7 单 portal 限制让 nvme-cli `discover` 看不到真实多 subsystem。V8a 触面小（cmd.rs + admin.rs + bin Cli 各 1 处），用作 V8b 大手术前架的扩好。

**文件**：
- `cmd.rs`：加 `build_v2_bytes_with_cntrltype(vid, ssvid, nn, cntrltype) -> Vec<u8>`；`build_v2_bytes` 改 thin wrapper
- `controller/admin.rs`：CNS=0x01 删 post-hoc patch，改调 builder
- `bin/...rs`：`--discovery-target-nqn/-addr: Vec<String>`；zip + validate
- `README.md`：multi-portal 示例

**估计 LOC**：impl ~60 / test ~80

**测试 (5)**：
1. `v8a_builder_cntrltype_nvm_default` — byte 111 == 0x01
2. `v8a_builder_cntrltype_discovery_explicit` — byte 111 == 0x02
3. `v8a_admin_identify_ctrl_in_discovery_mode_uses_builder` — captured 4 KiB byte 111 == 0x02
4. `v8a_cli_two_portals_zip_correctly` — bin smoke 2 portals
5. `v8a_cli_mismatched_portal_args_fails_fast` — Vec len 不等 → bin exit != 0

### V8b — Arc<Mutex<NvmeController>>（**大手术**）

**Motivation**：V5d R-8 让 Linux nvme-cli 默认 4 IO queue（= 4 conn）失败。必须让多 conn 共享同一 controller 实例。

**文件**：
- `session.rs`：V2Session.controller 改 `Arc<Mutex<>>`；所有 controller 调用改"短锁"helper `with_controller`；R-1 R2T loop 重构
- `controller/mod.rs`：新 `nvme_has_admin_cq()`；`nvme_install_admin_cq` idempotent
- `bin/...rs`：删 per-backing Mutex；startup 一次 open + Arc::clone
- `lib.rs`：`pub type SharedController = Arc<std::sync::Mutex<NvmeController>>;`

**估计 LOC**：impl ~150 / test ~150

**锁粒度 Q1**：整 controller 一把 `std::sync::Mutex`；拒分锁（ordering 复杂）+ 拒 parking_lot（V8e tokio 才换 tokio::sync::Mutex）+ 拒 RwLock（几乎全 mut）

**R-1 死锁规避**：
```text
Phase 2 R2T loop:
  let req = with_controller(|c| c.pop_pending_read());  // 短锁
  let bytes = dma_read_via_r2t(...)?;                    // 锁外 wire
  with_controller(|c| c.nvme_admin_complete_dma(...));   // 短锁
```

**测试 (5)**：
1. `v8b_two_conns_share_controller_admin_then_io` — 两 conn 都完成 Connect+Identify+IO Read
2. `v8b_two_conns_concurrent_io_read_no_corruption` — pre-fill 不同 pattern 验数据对
3. `v8b_two_conns_concurrent_io_write_then_read_persistence` — 跨 conn 写读一致
4. `v8b_admin_cq_install_idempotent` — 重 install no-op
5. `v8b_per_backing_mutex_removed` — bin smoke 两 conn 都过 ICReq

### V8c — per-conn AER routing

**Motivation**：V8b 后 AER 跨 conn 投错；spec § 5.2 必须投回原 conn。

**文件**：
- `controller/mod.rs`：aen_pending struct 加 `conn_id: u32`；`nvme_fire_aen(..., conn_id)`；新 `nvme_admin_dispatch_with_conn`、`nvme_pending_aer_count_for_conn`、`nvme_cleanup_conn_aers`
- `session.rs`：V2Session 加 `conn_id: u32`；Drop impl 调 cleanup_conn_aers
- `aer.rs`：PendingAer 加 conn_id
- `bin/...rs`：handle_conn 分配 conn_id

**估计 LOC**：impl ~80 / test ~120

**Q2 AER routing**：选 conn_id label，拒 channel（V8c 仍 sync）；conn_id 用 AtomicU32 全局递增

**测试 (5)**：
1. `v8c_aer_routing_conn_a_does_not_steal_conn_b`
2. `v8c_aer_pending_count_per_conn`
3. `v8c_conn_drop_cleans_aen_pending`
4. `v8c_legacy_nvme_admin_dispatch_conn_id_zero_still_works`
5. `v8c_e2e_two_conns_aer_interleaved`

### V8d — Disconnect (fctype 0x08) 真清

**Motivation**：V2 stub 不清 queue → V8b 多 conn 后 qid 漏 / 重用撞。spec § 3.5 要求拆 controller-side cqs/sqs。

**文件**：
- `controller/mod.rs`：新 `nvme_delete_io_sq/cq(qid) -> Result<>`、`nvme_list_io_sqs() -> Vec<u16>`
- `fabric.rs`：`DisconnectFabricFields` + `decode_disconnect_fields`
- `session.rs`：`handle_disconnect` 真实现；V2Session Drop sweep
- `dispatch_capsule_cmd`：Disconnect 分支调 handle_disconnect

**估计 LOC**：impl ~100 / test ~100

**Spec § 7.6.1 ordering**：先 delete SQ（引用 CQ 的），再 delete CQ

**测试 (6)**：
1. `v8d_disconnect_clears_session_io_queues`
2. `v8d_disconnect_deletes_controller_side_qids`
3. `v8d_disconnect_invalid_recfmt_rejected` (SC=0x80)
4. `v8d_session_drop_without_disconnect_still_cleans` (peer close)
5. `v8d_disconnect_then_reconnect_qid_reusable`
6. `v8d_disconnect_sq_before_cq_ordering`

### V8e — tokio refactor（**可选，建议推 V-followup**）

**Motivation**：V6b 100ms tick 是 polling hack；AER 延迟 < 10ms 需要真 select!；KATO timer 需要 task；性能。

**代价**：framing.rs async + session.rs 全 async + 30 个 test 改 `#[tokio::test]` + bin `#[tokio::main]`；cold build +12s

**Q3 决策**：**推 V-followup**；V8a/b/c/d 已满足业务需求；tokio 主要收益是优化而非功能阻塞

若坚持进 V8，分 V8e1 (lib) + V8e2 (bin)

### V8f — 单 bin 双 listener (target + discovery)

**Motivation**：V7 部署需 2 process；V7 plan §5 punt 到 V8。

**文件**：
- `bin/...rs`：`--discovery-listen: Option<String>`；2 个独立 NvmeController；spawn 第二 accept loop；共享 running
- `tests/dual_listener_e2e.rs`
- `tests/v8_multi_conn_io_e2e.rs`：4 conn 并发 IO 模拟 nvme-cli 默认行为
- `README.md`：V8f 章节

**估计 LOC**：impl ~100 / test ~300

**Q6**：两 controller 实例（不共享）；防 AER 串扰 + namespace 状态污染

**测试 (6)**：
1. `v8f_dual_listener_starts_both_ports`
2. `v8f_dual_listener_discovery_finds_target_via_log_page`
3. `v8f_dual_listener_sigint_shuts_both`
4. `v8f_dual_listener_independent_max_conn`
5. `v8f_e2e_multi_conn_4_qid_concurrent_io` (V8b+c+d+f e2e)
6. `v8f_discovery_listener_aen_does_not_leak_to_target`

## 4. Capability Matrix

| 能力 | V7c | V8a | V8b | V8c | V8d | V8e | V8f |
|---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| Identify Ctrl CNTRLTYPE via builder | ✗ | **✓** | ✓ | ✓ | ✓ | ✓ | ✓ |
| CLI 多 portal discovery | ✗ | **✓** | ✓ | ✓ | ✓ | ✓ | ✓ |
| 多 conn 共享 controller | ✗ | ✗ | **✓** | ✓ | ✓ | ✓ | ✓ |
| V5d R-8 拆 | ✗ | ✗ | **✓** | ✓ | ✓ | ✓ | ✓ |
| 并发 IO 无 corruption | ✗ | ✗ | **✓** | ✓ | ✓ | ✓ | ✓ |
| AER per-conn routing | ✗ | ✗ | ⚠ | **✓** | ✓ | ✓ | ✓ |
| Disconnect 真拆 | ⚠ stub | ⚠ | ⚠ | ⚠ | **✓** | ✓ | ✓ |
| AER 延迟 < 10ms | ✗ | ⚠ | ⚠ | ⚠ | ⚠ | **✓** | ✓ |
| 单 bin 双 listener | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | **✓** |
| Linux nvme-cli 默认 4 IO queue | ⚠ 1 conn | ⚠ | **✓** | ✓ | ✓ | ✓ | ✓ |
| 完整 discover → connect → IO → disconnect | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | **✓** |

## 5. Reviewer 安排

```
V8a → rust-reviewer + code-reviewer
V8b → rust-reviewer + code-reviewer + security-reviewer (poison/race/reentry)
V8c → rust-reviewer + code-reviewer + security-reviewer (conn_id 伪造/AER 窃取)
V8d → rust-reviewer + code-reviewer (Drop ordering)
V8e → rust-reviewer + security-reviewer + harness-optimizer [可选]
V8f → rust-reviewer + doc-updater + security-reviewer (dual-port surface)
```

## 6. 不在 V8 范围（→ V-followup）

TLS 1.3 / DH-HMAC-CHAP / TP4126 / Discovery Log Change AEN / 持久化 reservation / 多 PDU C2HData batching / Combined-transport binary / KATO timer / AER cross-reconnect persist / Sanitize 真清 / CNTRLTYPE=0x03 Admin Ctrl / IO > MDTS PRP list (与 V8 正交)

## 7. Definition of Done

- V8a/b/c/d/f 各 commit + reviewer 0 critical/high (V8e 可选)
- V7c 现有 105 + 73 + 1 全 green，无回归
- V8 新增 31+ tests 全 green
- clippy + fmt clean
- README V8 章节 + nvme-cli `connect -i 4` 真机 manual
- phase-v-nvme-of-tcp.md § 7 V8 行回填 LOC

## 8. 推荐执行顺序

```
V8a (热身, 60+80 LOC, 1-2h)
  ↓
V8b (大手术, 150+150 LOC, 4-8h) ★ 最高风险
  ↓
V8c (per-conn AER, 80+120 LOC, 2-4h)
  ↓
V8d (Disconnect, 100+100 LOC, 2-3h)
  ↓
V8f (双 listener + e2e, 100+300 LOC, 3-5h)
  ↓
V8e (tokio, 600+200 LOC, 8-16h) — 推 V-followup
```

**建议落到 V8f 即收**：Linux nvme-cli 默认 4 IO queue 全互操，业务需求满足。

## 9. 关键设计 Q&A

**Q1 锁粒度**：整 controller 一把 std::sync::Mutex；短锁纪律 (with_controller helper)
**Q2 AER routing**：conn_id: u32 label（拒 channel）
**Q3 tokio**：推 V-followup（功能优化非阻塞）
**Q4 builder BC**：thin wrapper 保 BC
**Q5 死锁规避**：dma_read_via_r2t 锁外跑（lock-pop-unlock-wire-lock-complete）
**Q6 双 listener**：2 个独立 controller 实例（防 AER 串扰）
