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

---

## 11. decision-then-IO 借用模式 (HIGH, 反复出现的 async 借用陷阱)

**坑**：async fn 拿了 `&mut self.field` 后跨 `.await` 又要调 `self.send_*().await`
→ E0499 "cannot borrow `*self` as mutable more than once at a time"。
最初 V-dhchap-4 把所有错误处理 inline 在 match arm 里，编译器 4 处错。

**修法**：把 *所有* state mutation 收进一个 scope，scope 内算出一个枚举
`Decision` 描述要做什么（不含 self ref），scope 出后再做 async I/O。模式：

```rust
async fn handle(&mut self) -> Result<()> {
    enum Decision {
        Ok,
        ErrCapsule(u8),
        WireFailThenCapsule(Vec<u8>, u8),
    }
    let decision = {
        let field = self.field.as_mut().unwrap();
        if cond_a {
            field.state = State::Failed;
            Decision::ErrCapsule(0x83)
        } else if cond_b {
            let wire = build_failure(field.tid, ...);
            field.state = State::Failed;
            Decision::WireFailThenCapsule(wire, 0x83)
        } else {
            Decision::Ok
        }
    }; // ← borrow ends here
    match decision {
        Decision::Ok => self.send_ok().await,
        Decision::ErrCapsule(sc) => self.send_err(sc).await,
        Decision::WireFailThenCapsule(w, sc) => {
            self.send_wire(&w).await?;
            self.send_err(sc).await
        }
    }
}
```

**为什么比"先 drop guard"通用**：当 self 既要 mutate 字段又要调多个 async
method，drop guard 模式得反复 re-acquire；decision-then-IO 一次性算出所有
副作用让 borrow checker 静默。

**来源**：commit `eff95619` (V-dhchap-4) borrow-fix refactor。

---

## 12. WebFetch → 源验证 → anchor test (HIGH, 工作流模板)

**坑**：早期实现 wire / crypto 全靠 spec PDF + 想象，第一次实测必错。

**修法 (三步法)**：
1. **WebFetch** raw Linux kernel mirror (`raw.githubusercontent.com/torvalds/linux/master/...`)
   或 nvme-cli `libnvme` 源；spec PDF 经常 403/可视化噪音多
2. **抽 layout / 算法**：找 `struct nvmf_*` / `nvme_auth_*` / format strings
   (kernel 用 `"NVMe%u%c%02u %s %s"` 这种)；记字段顺序 + size + 算法步骤
3. **同 commit 写 anchor test**：用 `offset_of!` 锁字段位置 / 用 known
   input → known output 锁 crypto；测试名带 `_anchor_` / `_kernel_ci_`
   前缀便于 grep

**何时跳过 WebFetch**：spec § 8.13.5 那种纯 wire layout 我们已经 fetch 过
的 — 直接 grep `2026-06-04-nvme-tcp-wire-reference.md` 查；不重复 fetch。

**何时必须 WebFetch**：写新 crypto / 新 wire 字段 / packed struct 偏移 *任何
不确定* 的瞬间 — 比手算更省时间，因为 false-positive 测试通过的 cost 远
高于一次网络请求。

**来源**：V-dhchap-4 NEGOTIATE/CHALLENGE 字段 + V-tls-psk HKDF-Expand-Label
labels 都靠这条工作流确认。

---

## 13. anchor test 是"spec drift 的金丝雀"，非单元测试 (MEDIUM)

**坑**：reviewer L-2 让我加 SHA-384 端到端 self-consistent test。当时
理解是"覆盖率 +1"。

**真正价值**：dispatch table 第二臂 (Sha384) 万一某天被人误删 / 改错 →
SHA-256 路径全绿，*只有这条 anchor 红*。覆盖率工具看不出"算法支持几种
hash"的语义，anchor test 看得出。

**判据**：当代码有 dispatch / lookup table / 多 hash / 多 algorithm 分支，
*每条分支* 至少配一个 anchor test，命名带 `_dispatch_arm_<N>` / `_branch_*`
便于 grep。

---

## 14. 手算 offset 失败 case 速查表 (CRITICAL, 教训具体化)

[[lesson §1]] 提了"不手算 packed struct"，下表是实际踩过的 9 个坑，下次
怀疑某个字段算错时先查表：

| spec struct | 我手算 offset | 真实 offset (offset_of! 验) | 修法 commit |
|------------|--------------|--------------------------|------------|
| IdentifyController.KAS | 320 (对了但凑出来的) | 320 | offset_of! 锁定 |
| IdentifyController.MNAN | 524 | 540 | `665ba1ec` 系列 |
| IdentifyController.CMIC ANA bit | 设 1 | 必须设 0 (Disc Ctlr NN=0) | 同 |
| DiscoveryEntry.eflags | 漏字段 | offset 10 (2 B) | V-interop-5 |
| DiscoveryEntry.rsvd0 | 22 B | 20 B (eflags 占了 2) | 同 |
| Get Log Page LPO | 忽略 | u64 cdw12/13 拼，做 offset slice | V-interop-6 |
| Discovery SUBNQN | 用 IO target NQN | 必须 `"nqn.2014-08.org.nvmexpress.discovery"` | 同 |
| Fabric Connect IO qid Connect Data | 用默认 | 必须 nlb cap 检查 | V5a |
| MDTS | 5 (= 128 KiB) | 5，但只对 V5_NLB_MAX=16 时是"对"的 | V-prp-list 修对应关系 |

**通用模式**：spec PDF 字段表看着对 ≠ packed struct 字节布局对。任何怀疑
先 `offset_of!` anchor 一遍。

---

## 15. "不做"也是一种工程决策 (HIGH)

**坑**：F 段 (rustls external-PSK survey) 一度纠结"要不要 fork rustls"。
半天后我意识到决策本身就是 "不接 fork，等上游"，并且要 **同等严肃** 写
进 plan（[2026-06-06-phase-v-followup-tls-psk-survey.md](2026-06-06-phase-v-followup-tls-psk-survey.md)
§4 "推荐：路径 A + C 并行"）。

**为什么重要**：不写下来的"不做"决策一周后没人记得为什么；下次有人冲动
fork 时又得重新走一遍 cost-benefit。

**模板** (写 survey 时用)：
- 候选路径 A / B / C / ...
- 每条 cost + risk + side-effect
- 推荐 + rationale
- 副产物：明列"不接 路径 X，因为 ..."

---

## 16. reviewer L-fix 即修 vs deferred 判定 (MEDIUM)

**坑**：L 级 finding 默认"可下个 phase 修"，但 V-tls-psk L-1/L-2/L-5
我都在同 commit 修了。判据没写过。

**判据**：
- ≤ 5 LOC 改 + 不引入新依赖 + 不动 public API → **即修，节省 reviewer
  下次再来一轮**
- 需要外部数据 (如 kernel CI vector) / 跨 module 修 / 改 public API
  → **deferred 加 TODO 标 reviewer 编号** (例: `TODO(reviewer L-3)`)
- 命名 / 文档 LOW → 看心情，但若多个 L 集中在同一文件 → 一次性扫

**例**: V-tls-psk reviewer 给 L-1..L-5，L-1/2/5 ≤ 5 LOC 改，L-3 是
"NQN 校验责任" 需修改函数 signature 才彻底 → 改成 doc 标注 + TODO，留
下个 phase。
