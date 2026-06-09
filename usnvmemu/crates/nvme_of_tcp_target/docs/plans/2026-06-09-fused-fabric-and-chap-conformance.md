# Plan: nvme_of fused fabric + Python CHAP wire conformance（纯代码，无 host-root）

> 2026-06-09。两个**不需要用户操作**、新会话满上下文可直接执行的项。各按
> what / why / 现状 / 步骤 / acceptance / 陷阱 给。

---

## A. V-followup-fused-cmd 的 nvme_of fabric 部分（MEDIUM）

### 现状（已核实 + **architect review 二次纠错**）
- **nvme_firmware controller 有 Fused C&W 实现，但只在 `dispatch_sqe` 路径**（mod.rs:1723，
  由 doorbell / `on_fetched_sqes` 驱动）：fuse 01/10 检测 → `pending_fused`（mod.rs:762）→
  completion "Compare PASS → Write" / "Compare FAIL → abort (atomic)"（completion.rs:1424/1438）；
  fused C+W 硬上限 8 LBA（1 page，mod.rs:1864）。测试 `o3_fused_cw_*`（controller/tests.rs）。
- **⚠️ 关键纠错**：nvme_of **不走** doorbell→`dispatch_sqe`。它走
  `handle_io_cmd_async`（async_session.rs:1401）→ `nvme_io_dispatch`（mod.rs:1113）→
  **`dispatch_io`（controller/io.rs:410）**——而 **`dispatch_io` 完全没有 fuse / pending_fused
  处理**：`match sqe.opcode()` 把 COMPARE（io.rs:1600）和 Write 各当**独立**命令，fuse
  bits 9:8 被无视。

### 真正的 gap（architect 纠错后）
**经 nvme_of fabric 提交的 fused Compare+Write，被 `dispatch_io` 当两条无关命令各自执行
——Compare 返结果、Write 无条件写盘，零 atomic CAS 语义。** 这与 chunking 无关（见下）。
修复方向：让 IO fabric 路径具备 fuse 配对——要么 `dispatch_io` 自身加 fuse 处理（复用
controller 已有的 pending_fused 机制），要么 session 层维护 fused 状态。两者择一需先定
（建议：在 controller 加一个 fabric-path 也走的 fuse 配对入口，避免 dispatch_sqe/dispatch_io
两套 fuse 逻辑分叉——参考 dispatch_sqe 的 pending_fused 实现移到共享层）。

**~~chunking 不是 gap（已证伪）~~**：原以为"fused 被 NLB chunking 拆碎"。实则
chunking 阈值 `V5_NLB_MAX=16`（session.rs:105），而 controller fused 上限 8 LBA
（mod.rs:1864，超即 INVALID_FIELD）——**任何合法 fused 命令 nlb ≤ 8 < 16，永不触及
chunking**。且 Compare opc=0x05 本就不在 `is_rw=(0x01|0x02)` 内，连进 chunk 判断的资格
都没有。删除原 chunking gap 分析 + 对应 adversarial 测试（场景逻辑不可达）。

### 步骤
1. 读 `controller/io.rs:410 dispatch_io` + `mod.rs:1723 dispatch_sqe`（fuse 逻辑所在），
   对比两条路径，确定 fuse 配对应抽到哪个共享层（避免分叉）。
2. 让 fabric IO 路径（dispatch_io / nvme_io_dispatch）走 fuse 配对：Compare(01) 暂存、
   Write(10) 到达时原子 CAS（Compare PASS→Write / FAIL→abort），双 CQE。
3. 确认两条 fused capsule 顺序进同一 SQ 上下文、不并发重排。
4. 加 lib test：fused Compare(01)+Write(10) 过 **nvme_of fabric 路径**（非 dispatch_sqe）
   → 断言原子 CAS + 双 CQE。**关键**：测试要打 `dispatch_io` 路径，否则测的是已 work 的
   dispatch_sqe（self-consistent 假阳，§20/§22）。
5. rust-reviewer。

### Acceptance
- fused Compare+Write 经 **nvme_of fabric 路径** 被 controller 原子处理（CAS）、双 CQE 回。
- lib test 明确走 dispatch_io 路径（不是 dispatch_sqe）。
- 真 nvme-cli fused IO 互通 = host-root（见 RUNBOOK §1 同款，留 e2e 确认）。

### 陷阱（§20/§22）
- **别在 dispatch_sqe 路径测**——那条已 work，测它是 self-consistent 假阳。fabric 的 bug
  在 dispatch_io，测试必须打这条路径才证明修对了（差分：修前 fabric fused 无原子性、
  修后有）。
- 别再押"chunking 破坏 fused"——已证伪（阈值 16 vs 上限 8 互斥）。

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
