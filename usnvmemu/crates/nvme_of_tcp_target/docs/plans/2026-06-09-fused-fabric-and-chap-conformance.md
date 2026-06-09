# Plan: nvme_of fused fabric + Python CHAP wire conformance（纯代码，无 host-root）

> 2026-06-09。两个**不需要用户操作**、新会话满上下文可直接执行的项。各按
> what / why / 现状 / 步骤 / acceptance / 陷阱 给。

---

## A. V-followup-fused-cmd 的 nvme_of fabric 部分（MEDIUM）

### 现状（已核实，doc-audit）
- **nvme_firmware controller 的 Fused C&W 已完整 + 测试**（Phase O2/O3）：`dispatch_sqe`
  检测 fuse 01/10 → `pending_fused: HashMap<SQ→(first SQE, sq_head)>`（mod.rs:762）→
  completion 路径 "Compare PASS → dispatching Write" / "Compare FAIL → aborting Write
  (atomic)"（completion.rs:1424/1438）。测试 `o3_fused_cw_protocol_invariants` /
  `o3_fused_cw_dispatch_chain_smoke` / `identify_controller_advertises_fused_cw` /
  `fused_on_admin_sq_rejected_by_design`（controller/tests.rs）。
- nvme_of 经 doorbell（`mmio_write` BAR0，fabric.rs）驱动 controller；IO cmd 走
  `handle_io_cmd_async`（async_session.rs:1401）。

### 真正的 gap（需做）
1. **`handle_io_cmd_async` 对 Read/Write 做 NLB chunking**（`V5_NLB_MAX=16` 拆 sub-SQE，
   async_session.rs:1437 `handle_io_cmd_chunked_async`）。**Fused 命令不可拆**——一条
   fused Compare 或 Write 若被 chunk 成多条 sub-SQE，原子语义和 pending_fused 配对全破。
   → **先在 dispatch 决策里识别 fuse 字段（cdw0 bits 9:8 ≠ 00），fused 命令绕开
   chunking 路径，整条提交。**
2. **确认 fuse 字段透传**：`handle_io_cmd_async` 读 `Sqe::read_from_bytes(sqe_bytes)`
   后到 controller submit 的全链路不丢 cdw0 fuse bits。`Sqe` 有 fuse field（cmd.rs:333）；
   核 sub_sqe 构造（async_session.rs:1504 `let mut sub_sqe = original_sqe`）chunk 路径会
   复制 cdw0——但 chunk 本身对 fused 非法，见 gap 1。
3. **两条 fused capsule 落到同一 in-memory SQ**：controller 的 pending_fused 是 per-SQ
   的，要求 Compare(fuse=01) 紧接 Write(fuse=10) 进同一 SQ。确认 nvme_of 对同一 IO
   queue 的两条 capsule 顺序提交到同一 SQ（不并发重排）。
4. **双 CQE 回**（spec §6.2）：fused 两条都需 individual CQE。controller 已产两个 CQE，
   确认 nvme_of 的 CQE→C2HData/CapsuleResp 回传两条都送达 host。

### 步骤
1. 读 `dispatch_plan.rs::decide_io_nlb_check` + `async_session.rs:1401-1520`，画清 IO
   cmd → SQ submit → CQE 回传链路。
2. 在 NLB 决策前加 fuse gate：`if sqe.fuse() != 0 { /* 不 chunk，整条提交 */ }`。
3. 加 lib test：构造 fused Compare(01)+Write(10) 两 SQE（同 IO SQ），过 nvme_of dispatch
   → 断言 controller pending_fused 配对 + 双 CQE + Compare-PASS-then-Write 原子。
4. **adversarial**（§22 教训，必加）：fused Write 的 nlb > V5_NLB_MAX 时**不能**静默
   chunk——断言要么整条提交要么明确拒（不可拆 fused）。
5. rust-reviewer。

### Acceptance
- fused Compare+Write 不被 chunk、fuse 透传、controller 原子处理、双 CQE 回。
- lib test 覆盖正常 + adversarial（fused+大 nlb）。
- 真 nvme-cli fused IO 互通 = host-root（见 RUNBOOK §1 同款，留 e2e 确认）。

### 陷阱
- 别假设"chunk 路径复制了 cdw0 所以 fused 也 OK"——**fused 被 chunk 本身就是 bug**
  （self-consistent 测试数据碰不到）。测试必须用 nlb 触发 chunk 的 fused 命令。

---

## B. V-followup-py-harness-spec-wire-conformance（MEDIUM）

### What / Why
把 `scripts/interop_py/chap4_spec_wire_e2e.py` 扩到覆盖 reviewer M-4 的 5 个 CHAP wire
错误路径 case，对齐 lib test。lib test 覆盖了，但跨进程 Python harness 缺；跨进程实证
wire 错误路径才能保 Linux nvme-cli 拿到正确 FAILURE1 diagnostic。

### 5 个 case（spec §8.13.5）
1. **REPLY before challenge**：host 在收到 AUTH_Challenge 前发 AUTH_Reply → target 须
   拒（protocol violation）。
2. **REPLY truncation**：AUTH_Reply 长度短于 response_len 声明 → 拒。
3. **tid mismatch**：AUTH_Reply 的 t_id ≠ NEGOTIATE 协商的 t_id → 拒。
4. **SUCCESS2 before auth**：host 在 auth 完成前发 AUTH_Success2 → 拒。
5. **FAILURE2 from host**：host 发 AUTH_Failure2 → target 干净关闭 + 记原因。

### 步骤
1. 读 `chap4_spec_wire_e2e.py` 学它怎么构造/收发 DH-HMAC-CHAP capsule（AUTH_Send/Recv
   fabric cmd + payload struct）。**关键**：复用它已有的 wire helper，别另起炉灶。
2. 对每个 case，**先读对应 lib test**（dhchap.rs 的 negative test）确认 target 的期望
   行为（reject SC / 关闭），再用 Python 构造触发该路径的 capsule 序列。
3. 断言 target 回 FAILURE1（含正确 reason code）或干净关闭——**断言要 anchor 到
   target 真实回应字节**，不是 harness 自己的预期（§20/§22：别让测试因错误原因通过）。
4. 跑 `uv run python chap4_spec_wire_e2e.py`（venv 见 pyproject.toml）。

### Acceptance
- 5 个 case 各一个 Python scenario，跨进程真触发 + 断言 target 回应。
- 与 dhchap.rs lib test 行为一致。

### 陷阱（§20/§22）
- 构造 adversarial CHAP 消息时，**判据用协议字段（t_id / length / message type）**，
  不是 harness 自家约定。一个"tid mismatch"测试若 tid 恰好没真的 mismatch（构造错），
  会 self-consistent 地"通过"却没测到东西。每个 case 加一句注释证明它真触发了目标路径
  （如：先发正确序列确认 target 接受，再改单一字段确认 target 拒——差分证明）。

---

## 优先级
A（fused fabric）有真 spec 实质 + controller 已就绪，优先；B（CHAP harness）是测试
补强。两者都纯代码、可无人值守，新会话直接从本 plan §步骤起手。
