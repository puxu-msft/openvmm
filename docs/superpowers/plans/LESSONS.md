# NVMe-oF TCP Target — 教训 (LESSONS.md)

> 每条都是真踩过 + 修过的坑；带 commit / phase 引用。新踩坑 + 修后第一时间
> 加这里，下次别重蹈。

## 1. 不要手算 packed struct 偏移 (CRITICAL)

**坑**：V-interop 阶段 9 个 wire blocker 几乎全是手算 offset 错：
- KAS=320 (实际通过 `offset_of!` 才知道)
- MNAN=524 错 vs 540
- DiscoveryEntry 缺 eflags → 后续所有 ASCII 字段 1B 偏移
- ID controller 多种 packed 字段

**为什么会错**：spec 里画字段顺序看着对，但 packed struct 内 `[u8; N]` 数组
+ alignment + 字节顺序，手算 = 100% 概率 false-positive 单元测试通过 +
production wire fail。

**修法**：每个 wire struct *必须* 配 `core::mem::offset_of!` anchor test：
```rust
#[test]
fn anchor_my_struct_field_offsets() {
    assert_eq!(offset_of!(MyStruct, kas), 320);
    assert_eq!(offset_of!(MyStruct, mnan), 540);
}
```
任何字段顺序变 → anchor 红 → 强制 review。

**来源**：[[nvme-of-tcp-real-linux-interop-milestone]]，commit
`665ba1ec..81dbed8e` 一系列 V-interop fix。

---

## 2. 测试是 "self-consistent" ≠ "spec-conformant" (HIGH)

**坑**：V-followup-dhchap-3 + tls-psk 都先写 self-consistent test (compute_response
两边都用本仓实现互验)，之前以为算法对了。Linux nvme-cli 实测才发现：
- 简化 wire 跟 spec 4-msg 完全不同 → 自家通 ≠ 真互通
- TLS PSK HKDF-Expand-Label 字段顺序错半字节 → 自家通但 kernel 拒

**修法**：spec wire / crypto 必须有 *third-party reference vector*：
- wire: 拿 Linux nvme-cli pcap dump 或 nvmet logs
- crypto: 拿 kernel `EXPORT_SYMBOL_GPL` 函数 probe dump 五元组

**当前差距**：tls_psk.rs 还在 self-consistent 状态（[ROADMAP](ROADMAP.md)
V-followup-tls-psk-kernel-vector HIGH 优先就为这个）。

---

## 3. async + locks = await-holding-lock 灾难 (HIGH)

**坑**：V8e tokio refactor 之前用 `std::sync::Mutex` 包 controller，async fn
`pump_one().await` 跨 .await 拿锁 → tokio runtime 全 stall + deadlock。

**修法**：
- crate-level `#![deny(clippy::await_holding_lock)]`
- 改 `tokio::sync::Mutex` 或 *先 scope drop sync lock 再 .await*
- 教训 pattern：
  ```rust
  let decision = {
      let guard = state.lock();
      compute_decision(&guard)  // sync only
  }; // drop guard
  do_async_work(decision).await
  ```
- AUTH_SEND/RECV handler V-dhchap-4 重构就是用这 pattern 跳过 borrow checker
  E0499（[V-dhchap-4 commit `eff95619`]）。

---

## 4. Reviewer subagent 必跑 (HIGH)

**坑**：早期某些 phase commit 前没跑 reviewer，事后 reviewer 抓到 H/M 必须
back-fix → commit history 脏。

**修法**：phase 完成 → `cargo test` 绿 → `ecc:rust-reviewer` → 修
CRITICAL/HIGH/MEDIUM (LOW 可下 phase) → commit。

**例外**：trivial 1-line typo / doc-only update 可省 reviewer，commit
message 标 "trivial"。

---

## 5. PreCompact / context-window 中断后恢复 (MEDIUM)

**坑**：context 接近极限时 system 自动 summarize；恢复后 todo state 可能
stale；任何手算 / 临时 buffer 数据会丢。

**修法**：
- 用 TodoWrite 跟踪 phase 进度，让 summarize 知道 "current task"
- 关键中间变量（如 wire struct 偏移、计算出的 secret）写到 memory file，
  不放心算就用 anchor test 锁
- 用户 prompt "继续" 时第一步 read MEMORY.md + 当前 todo，不靠"前面这样
  这样"的记忆

---

## 6. 用户偏好 / coding policy 提醒 (MEDIUM)

- **新代码用中文**：[[language-chinese-for-changes]]。原仓库英文别改。
- **方向明确时别问**：[[dont-stop-to-ask-when-direction-is-clear]]，用户给
  "ABC 都做" / "继续" 后直接做下一个 todo，只有破坏性操作 / 真 either-or
  才停。
- **Rust toolchain 1.95 钉死**：[[rust-toolchain-1-95]]，不要 `+nightly`。
- **subagent review required**：[[subagent-review-required]]。
- **不在乎代价做对**：用户 explicit 多次 "用最好的方式做长远的行动" — 不省
  reviewer pass，不省 anchor test，不假装 prod ready。

---

## 7. 跨进程 e2e 才能 catch wire bug (MEDIUM)

**坑**：lib test 全绿，Python harness 1-line python script 一跑就发现 wire
错。原因：lib test 都跑同一进程内，PDU framing / TCP buffer 边界 / partial
read 等问题不暴露。

**修法**：每个 spec wire phase 必配一个 `scripts/interop_py/*.py` 跨进程实证
脚本；不依赖 sudo / nvme-cli (那些再上一层 [ROADMAP] V-followup-tls-psk-kernel-vector
之类专门做)。

---

## 8. unwrap 在 trait infallible 实现内 (LOW, 但 reviewer 一定抓)

**坑**：`Hmac::new_from_slice(salt).unwrap()` 看着没事 (HMAC 收任意 key
长度)，reviewer L-1 标 "不如改 expect 给原因"。

**修法**：infallible 路径用 `.expect("人话解释为啥 infallible")`，永远别裸
unwrap (除 #[test])。

---

## 9. 文档 + 代码一致性 (LOW)

**坑**：V-dhchap-4 NEGOTIATE doc 之前写 "只看 [0]"，代码实际只看 [0]，
reviewer H-2 警告 "interop 边缘 case 会错拒"。V-dhchap-4d 修了代码，回头
更新 doc + 测试。

**修法**：改完代码立刻更 doc + 测试 + commit message；不留 doc / code drift。

---

## 10. cargo 命令 cwd (LOW, 但浪费时间)

**坑**：`cd /home/xp/refs/openvmm && cargo test` 会 build 整个 openvmm
workspace (含 openssl-sys dep 缺) → fail。

**修法**：cd 到 `docs/superpowers/examples/nvme_of_tcp_target/` 再跑 cargo。
本仓库 nvme_of_tcp_target 是独立 crate，不属于 openvmm workspace member。
