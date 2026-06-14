# usnvmemu — 工程教训 (LESSONS.md, 项目级)

> 每条都是真踩过 + 修过的坑；带 commit / phase 引用。新踩坑 + 修后第一时间
> 加这里，下次别重蹈。
>
> **完成定义（蒸馏槽位）**：每条 LESSONS 末尾**强制三选一**标一行蒸馏判定——
> `→ 蒸馏：PRINCIPLES「<节名·条名>」`（**仅当**规则在 **≥2 个独立场景**复现，同
> §19/§27 的"两家才证成抽象"门槛）/ `→ 已是：PRINCIPLES「<...>」` /
> `→ 纯 incident，无可迁移规则`（**默认 / 最常见答案**）。问"是否含可迁移规则"——
> 答案常是否；默认产物是 0 个新原则，别硬塞。空着 = 像缺 anchor test 一样的可见
> 缺口。锚一律按**标题 / 命名**引，不按 §号（§号随插条重排会悬挂，见 §18）。

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

**修法**：cd 到 `usnvmemu/crates/nvme_of_tcp_target/` 再跑 cargo。
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
进 plan（[2026-06-06-phase-v-followup-tls-psk-survey.md](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v-followup-tls-psk-survey.md)
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

---

## 17. doc audit 必须配 `git log` 复核，光凭印象会夸大或漏记 (HIGH)

**坑** (2026-06-06 audit pass 1+2+3 实测)：

- **审 K-20 hotplug** 时光看 `K20_HOTPLUG_DESIGN.md` 自标 "v2 设计文档，
  未实施"，直接照搬"仍未实施 (2026-06-06)"。**实际** `git log --grep K-20`
  3 个 commit (`a99cdc63` 实施 + `64da8fb6` polish + `93c5fa5f` SESSION_LOG
  状态转 ✅ v9 完成) 早已 shipped。**audit pass 自己造了假信息**。
- **审 SESSION_LOG** 时只看顶 header 自标 "覆盖 2026-05-29 → 2026-05-31"，
  没核 git log 发现 Phase J / K / L / M / N / O / P / Q1-Q12 / R / S /
  T / U1-U5 + U-followup / V 整族都没写进日志。
- **审 example crates** 时没数有几个 crate；4 个 (`pcie_device_sdk` /
  `vfio_user_transport` / `rng_device_example` / `pcie_remote_test_harness`)
  完全没 README。
- **审 ROADMAP §6** "历史 phase plan 索引" 时**只列有 `.md` plan 的**，
  漏掉所有 "无独立 plan 但 shipped" 的工作 (NVMe userspace Q/R/S 等)。

**修法 (audit 流程)**：

1. `git log --oneline main..HEAD --no-merges | wc -l` 算总 commit 数 — 心
   里有个量级（本仓 256 commit），不只数自己最近做的。
2. `git log --oneline | awk -F':' '{print $1}' | sort | uniq -c | sort -rn`
   按 prefix 分类 — 谁的 commit 量最多 = 哪个模块最可能有漏记。
3. `git log --oneline | grep -iE "Phase [A-Z]"` 抽所有 Phase 标签 — 比对
   文档里写了几个。
4. **每个声称 "未实施" 的 design doc** 都跑 `git log --grep <K-编号>` 复核。
5. **`find docs/ -name README.md`** 数 README，不存在的 example crate 都
   是漏记候选。
6. **声明 "shipped" 时附 commit hash**；空口 "shipped" 等于没说，未来
   audit 自己都无法验。

**为什么重要**：上一轮 audit 自己引入虚假信息 → 下一轮 audit 又要重审 →
信任度下降；本条等于 "audit 流程的 anchor test"。

**来源**：2026-06-06 audit pass 3 (commit `5c2ea335` 后 immediate
follow-up)。

---

## 18. 大规模 move/rename 后用脚本 + 死链扫描器修 link, 别手算深度 (HIGH)

**坑** (2026-06-08 usnvmemu/ 重组实测): 把 6 crate + 几十个文档搬目录 + 改名,
markdown link 关系成网。手算相对路径深度 (`../../../../`) 错了好几次 (跟
[[lesson §1]] offset 同类病: 手算 = false confidence)。

**修法**:
1. 写权威 `old basename → new repo-root 路径` 映射表, Python 脚本对每个
   .md 的每个 `](...)` link 自动 `os.path.relpath` 重算。
2. 配死链扫描器 (解析所有 link target, `os.path.exists` 验), 迭代到 0 broken。
3. **跨目录链接优先用 repo-root-absolute `/usnvmemu/...`** (GitHub 渲染 `/`
   开头 = repo root, 官方文档确认), 消除深度脆弱。同目录裸名保留。
4. mdBook (Guide) 例外: 它的 `/` 指 book root 非 repo root, 必须相对路径。
5. multi-line 分割的 "链接" (markdown 不允许 () 内换行) 脚本会误判, 手动核。

**为什么 repo-root-absolute 更好**: `/usnvmemu/docs/ROADMAP.md` 不受文件深度
影响; 文件再搬目录, 链接不变。未来独立 repo 时全局 `/usnvmemu/` → `/` 一次转。

**来源**: usnvmemu/ 子目录化 + crate 改名 + 文档归属重组 (commit
94d2f94a / 文档重组 / 849a3f83), 3 轮 Python 脚本 + 死链扫描达 0 broken。

## 19. 对称性是伪需求 —— read/write/completion 三者语义本不齐 (MEDIUM)

**坑**: Phase W 设计时一度想给 transport 抽一个"对称全双工入站 trait"(把
MMIO read/write、cfg、reset、dma-completion 统一成一个 `Inbound` enum)。

**为什么错**:
- **PCI read 必须同步返回值, write 没有** —— 强塞进一个 enum 等于用 `Option`
  把"读一定有值/写一定没值"的编译期保证换成运行期 match。
- **dma-completion 不是所有 transport 的真入站事件** —— pcie_remote 是真异步
  wire 帧; vfio-user 的 DMA 是同步往返, adapter 合成一条完成事件。强求"对称"
  逼每个 adapter 假装有这个事件。
- **nvme-of 一个入站 variant 都不用** —— 它没有 MMIO/cfg/reset。两家不足以
  证成一个抽象 (YAGNI)。

**正确做法**: 保留 `PcieDevice` 现有**非对称**入站方法 (read 返值/write 不返);
入站派发是 **adapter 私事**, 不进 core trait。`on_dma_complete` 文档明确它是
"transport 内部 token 完成通知", 不是对称入站事件 (见 device.rs 该方法 doc)。

**来源**: ADR-010 / Phase W; architect subagent 复核否决了 generic 对称 trait
这个过度设计。教训: 抽象前先问"三个实现里有几个真用得上这个 variant"。

→ 蒸馏：PRINCIPLES「设计 review 追问清单 · 抽象前数实现 / 对称性常是伪需求」

## 20. self-consistent 假设当 wire 判据是反模式 —— 修自家两端也逃不过 (HIGH)

**坑**: 修 vfio-user DMA head-of-line 阻塞(等 reply 时 defer 插入帧)时,
`read_reply_deferring_inbound` 用 `frame.msg_id == expected` 单条件判定"这是
本次 reply"。注释还自辩"server-initiated id 顶位 0x8000,与 client id 不撞"。

**为什么错**: vfio-user spec 两方向 msg_id **独立**、**不保留**顶位。client 的
inbound command 完全可能 msg_id 撞上我们的 server-initiated id → 被误当 reply
吞掉 → command 丢失 + `validate_dma_reply` 因 cmd/flag mismatch 报错 → 连接挂。
**这正是在"修一个对称性/head-of-line bug"的 commit 里,又引入一个同类 bug** ——
用 self-consistent 约定(我自己两端的"顶位默契")当 wire 判据。

**修法**: wire 判据必须用**协议语义**(reply flag `is_reply()`),不用自家编号约定:
```rust
if frame_id == expected_msg_id && frame.header.flags().is_reply() { return Ok(frame); }
```

**两条根本教训**:
1. **[[lesson §2]] self-consistent ≠ spec-conformant 适用于"我自己写的两端"**。
   自写 server + 自写 client(或 Python harness),共享的错误假设两边都不报错、
   单测全绿。**catch 它的是独立第二实现**(libvfio-user 官方 client / kernel /
   libnvme)。任何"我两端都这么约定所以对"的推理都可疑。
2. **不跳 subagent review**。本 bug 是跳过 review 直接 commit、被用户质问"是否糊弄"
   后补 review 才抓到的。单测绿(测试数据 self-consistent 不撞)不等于 review 过。
   见 [[lesson §4]]。

**判据速记**: 写任何 wire 匹配/校验时问自己"这是协议字段(flag/type)还是我的
编号范围/顶位/两端默契?"。后者一律换前者。

**来源**: vfio-user head-of-line fix 的 review H-1(rust-reviewer),commit
`b1cb8574` 引入 → fixup 修。

→ 蒸馏：PRINCIPLES「设计 review 追问清单 · 判据来自独立 oracle」(保留"写码判据用
协议字段不用自家默契"这半)；"独立第二实现才能 catch"那半已升结构律 → ADR-013。

---

## 21. "这版本不带某功能" ≠ "上游不支持/需 fork" —— 先查上游合入历史 (MEDIUM)

**坑**: 验 vfio-user 第 3 条接入时，用环境里现成的 QEMU **8.2** 实测，发现
`-device vfio-user-pci` 不存在（只有 vhost-user），就下结论写进 ROADMAP：
"主线 QEMU 无 vfio-user 客户端、从未并入主线、真 QEMU e2e 需带补丁的 fork
源码编译"。据此把真 QEMU e2e **整个 defer 到 future**，退回到只用自家 Python
client 验证。

**为什么错**: vfio-user 客户端（`vfio-user-pci` 设备）由 Nutanix/John Levon
**已在 QEMU 10.1（2025-08）合入上游主线**。我手上的 8.2 只是**早于该版本**，
不是"上游不支持"。用 linuxbrew 装 **QEMU 11.0.1**，`-device
{"driver":"vfio-user-pci","socket":{...}}` 直接 realize 成功——真 QEMU e2e
不仅可行，还立刻抓出 2 个自家测试全绿的握手 bug（见 [[lesson §20]]）。

**根本教训**: 从"我手上这个二进制不带 X"**不能**推出"上游不支持 X / 需要
fork"。这是把**单个版本的快照**当成**项目能力的全集**。正确动作：
1. **查上游合入历史**（release notes / 邮件列表 / commit log / `git log --grep`），
   确认功能是"从未有"还是"某版本后才有"；
2. 若是后者，**装够新的版本**再下结论，别基于旧版本写"不支持/需 fork"；
3. 尤其当结论会**砍掉一整条验证路径**（本例：真 QEMU e2e → defer future）时，
   下结论前的版本核查不是可选项。

**与 [[lesson §20]] 咬合**: §20 说"独立第二实现才能 catch self-consistent bug"。
本例差点因为"误判上游不支持"而**永远拿不到那个独立实现（真 QEMU）**——错误的
版本认知会直接剥夺最有价值的 oracle。

**来源**: ROADMAP V-followup-vfio-user-cross-process-harness 段曾写的错误
"现实修正"，2026-06-09 用 QEMU 11.0.1 实测推翻并更正。

---

## 22. unsafe 的 SAFETY 判据必须用独立 oracle，不可用不可信输入自证（mmap SIGBUS）(CRITICAL)

**坑**: 实现 vfio-user mmap 零拷贝 DMA 时，`map_dma_fd` 把 client 在 DMA_MAP 里
声明的 `size` 直接当 mmap 长度，SAFETY 注释写"所有 gpa→偏移索引都在 mmap_read/
write 里按 size 边界校验"——**看似严密，实则致命**。

**为什么是 CRITICAL**: `mmap(2)` 只要求 offset 页对齐，**不校验** `offset+size ≤
文件真实大小`；映射超出真实页的区间会**成功**返回一个长度合法的 VMA，但 memcpy
触碰无 backing 的页 → 内核投 **SIGBUS 杀整个进程**。client 发"大 size + 小 memfd"
即可 DoS。而我的所有 Rust slice 边界检查（`off+len ≤ bytes.len()`）**完全无效**——
越界发生在内核页层，不是 slice 层；`bytes.len()` 本身就等于那个撒谎的 size。

**根本错误（与 [[lesson §20]] 同源，第 3 次复现）**: SAFETY 判据用的是
**client 声明的 size**（不可信输入），而它和"映射区间长度"是同一个值——**自己
印证自己**。真正的 oracle 是 `fstat(fd).st_size`（内核真相），代码从未查过。
"按 size 校验 size" 是 self-consistent 假设当判据的教科书案例，只是这次戴上了
unsafe 的帽子，代价从"连接挂"升级到"进程崩"。

**修法**: mmap 前 `fstat` fd 取真实大小，校验 `offset+size ≤ st_size`，否则退回
message 路径（绝不 mmap）。SAFETY 注释重写为**诚实列外部不变量**：fstat 保证
backing / TOCTOU shrink 残留依赖（生产须 `F_SEAL_SHRINK`）/ u8 并发无 UB——而非
假装"全在我方校验内"。

**两条根本教训**:
1. **unsafe 的 SAFETY 论证，每个不变量都要问"判据来自独立 oracle 还是不可信
   输入的自我印证？"**。映射一个外部 fd，长度 oracle 是 `fstat` 不是对方的声明；
   解析 wire，判据是协议 flag 不是自家编号约定（[[lesson §20]]）。同一把尺子。
2. **review 第 3 次抓到我会 ship 的严重 bug**（§20 DMA head-of-line HIGH → 本条
   SIGBUS CRITICAL）。模式稳定：我自信写完 unsafe + 测试全绿（5 个测试全用
   size==fd 真实大小，self-consistent 数据，碰不到 SIGBUS 路径），reviewer 用
   adversarial 视角（"size>fd 会怎样"）一问即破。**unsafe 必过 review，且测试
   必须含 adversarial 用例**（不可信输入的极端值），不能只测 happy path。

**来源**: vfio-user mmap DMA 的 rust-reviewer 2 轮（首轮 BLOCK CRITICAL C-1），
commit dfa9fefb；回归测试 `dma_map_fd_smaller_than_declared_size_falls_back_no_sigbus`。

→ 蒸馏：PRINCIPLES「设计 review 追问清单 · 判据来自独立 oracle」(SAFETY 不变量必用
独立 oracle，不可用不可信输入自证)。

## 23. 改对称代码要 sweep 所有 reachable sibling；自写两端的测试必 revert-verify (HIGH)

**坑**: 纯-4K 把 LBA↔byte 换算从硬编码 512 改 per-NS `1<<lbads`。我系统改了
READ/WRITE/COMPARE/VERIFY/WRITE_ZEROES/COPY 的 dispatch + completion 全 PRP 档，
自测 `pure_4k_io_round_trip_mixed_ns` 全绿。reviewer 却抓到 **Fused Compare-and-Write
的 dispatch 仍写死 512**——它的 completion 我改了（扇区感知），dispatch 没改，
4K 上 Compare 恒 fail（512-len data vs 4096-len backing）。fused 是 COMPARE 的
**reachable sibling**，sweep 漏了。

**根本**: ① 改一类语义（扇区换算）时，grep 出**所有**触发点不够——还要找**语义同族
但代码路径独立**的 sibling（fused 是 compare 的变体，走单独 dispatch helper）。
dispatch/completion 任一侧改了另一侧没改 = 长度不对称 = 数据 corruption 的经典形。
② 我的 e2e 测试一开始注入 4096B 数据，**无论 dispatch 请求 512 还是 4096 都过**——
测试碰不到 bug。reviewer 指出后改为断言 captured `DmaRead.len == 4096`（锁
dispatch 侧请求字节），并**revert-verify**：把 dispatch 改回 512 确认测试 FAIL，
才算真锁住。

**第二个 sibling 类坑（同 commit，fabric 侧）**: session 合成假 PRP 的 prp2 阈值 /
nlb cap / chunk 大小三处都按 512B。改对了**单 dispatch** 的扇区感知，reviewer 抓到
**多 conn Format/IO TOCTOU**：另一 conn 的 Format 在「session 读 lbads」与
「controller dispatch」之间改 lbads（锁释放窗口），合成的 prp2 与 controller 实际
byte-routing 不符 → 撕裂。修=dispatch 同锁内复读 lbads + 守 ≤2 页中止。

**三条根本教训**:
1. **改对称/同族语义，列 sibling 清单**：grep 触发点 + 问"还有哪条独立代码路径走
   同一语义？"（fused vs 普通 compare、单 dispatch vs chunked、dispatch vs completion）。
2. **自写两端的测试必须能 catch bug 才算数**：注入数据要让正确/错误实现产生**不同**
   wire 可观测量（DmaRead.len / SC / C2HData.len），并 revert-verify 注入 bug 后测试
   真 FAIL（[[lesson §20]] 同源：self-consistent 数据测不出 self-consistent bug）。
3. **跨锁读的值 = TOCTOU 隐患**：在锁 A 读、锁 B 用，两锁间状态可被改。判据值（这里
   lbads）必须在**最终用它的那把锁内复读**，否则共享状态的并发修改会让决策过期。

**来源**: 纯-4K firmware（commit `3668b36a`，2 轮 reviewer 抓 fused HIGH）+ NVMe-oF
TCP fabric（commit `73429e28`，2 轮 reviewer 抓多 conn TOCTOU HIGH）；e2e
`pure_4k_over_fabric_format_then_io_sector_aware` revert-verified；测试 harness
`setup_qid1_with_controller(_, sess_pumps)` 的 sess_pumps 必 = setup PDU(4) + IO 操作数，
多了 server 线程 join 挂死（独立调试坑）。

**后续实证（commit `aae0eb7f`）—— uniform pattern 掩盖真 corruption bug，独立
oracle 用不同代码路径才有牙**: 为验纯-4K fabric，写独立 Python wire 测试。最初
用 `(b+const)&0xFF` pattern——`period-256 + 4096%256==0 → 每 LBA 字节全同`，测过；
但**把 pattern 换成 per-LBA distinct marker 后立刻 FAIL**：捞出一个**pre-existing
多 chunk fabric IO 偏移 corruption**——session 拆 chunk 后每 chunk 的 R2T 偏移(写)/
C2HData DATAO(读) 重置为 0（chunk-relative）而非 host-buffer 累计，第 N>0 片数据被
host 当第 0 片发/落 → 多 chunk 读写数据错位。所有既有测试（`io_size_sweep` 1..256
LBA、`io_write_e2e`）都用 period-256 uniform pattern（每 LBA 同字节），**chunk 错位
被 byte-equal 静默掩盖了 V-followup-prp-list 以来若干 phase**。

两条加强教训：
4. **round-trip byte-equal 的判据强度 = pattern 的位置唯一性**。write/read 走同一
   代码路径时，symmetric 偏移 bug + period-N pattern（N | LBA_size）会 round-trip
   静默通过。pattern 必须 **per-LBA 位置唯一**（marker = LBA 序号），整-LBA 错位才
   可见。这是 [[lesson §20]]「self-consistent 数据测不出 self-consistent bug」在
   **测试数据生成**层的具体化。
5. **独立 oracle = 不同代码路径的交叉验证**。chunked write 后用 **单-LBA(非
   chunked) 读**逐个验落对绝对 LBA：单-LBA 走 controller 直接 `slba*sector` 偏移，
   是 chunked 偏移逻辑的独立判据；同路径 round-trip 再 distinct 也只证「不撕裂」，
   证不了「绝对位置对」。挑 oracle 时问：判据来自**另一条**代码路径，还是同一条
   自己印证自己？（同 [[lesson §22]] unsafe oracle、[[lesson §20]] wire flag 判据）。

## 24. review BLOCK 的对抗-case bug，常是"换更简单的设计"而非"补复杂设计" (HIGH)

**坑**: fused C&W over fabric 首版用 capture-based 设计——给 Compare/Write 数据各
设不同 PRP sentinel，靠 captured dma_read 的 gpa 反推该数据属于哪条命令（→R2T
cccid），session 与 controller **双状态**（session `pending_fused_compare: Option<u16>`
+ controller `pending_fused`）。happy path 全绿、e2e 过、revert-verify 也过。但
rust-reviewer 对抗审出 **HIGH-1**：连续两 FIRST 时双状态失步（controller abort A+
stash B，session 据"有无 CQE"启发式误清 pending）→ 后续 Write 的 Compare 数据
R2T 发**错 cccid** → 比错 buffer → **静默 CAS corruption**。还有 HIGH-2 锁在 CAS
中途因 wire I/O 释放（非原子）、HIGH-3 多 conn 共享 controller `pending_fused` 串对。

**修法不是补 capture-based 的洞，是换 hoisted 设计**：session **独占** fuse 状态
（存整条 Compare SQE），SECOND 到达**先经 R2T 把两 host buffer 各按自己 cccid 取齐**，
再单次调 controller 无状态 `nvme_fused_cas`（单 `&mut self` 内 read-compare-write）。
一举消三 HIGH 且**代码更短**（删 sentinel、删多轮捕获驱动、删双状态）。

**三条根本教训**:
1. **review 抓到复杂设计在对抗 case 下的洞，先问"是不是设计本身太绕"**，而非逐个补
   洞。capture-based 的 gpa→CID 推断 + 双状态是洞之源；hoisted 把"取数据"与"原子
   操作"分离，对抗 case 自然消失。简单设计的对抗面更小。
2. **跨 fabric/wire 的原子性 = 所有外部数据先取齐，再进单锁同步临界区（无 await
   跨锁）**。只要临界区中途因 wire I/O 释放锁，别的 conn 就能插队 → 非原子。
   parking_lot 锁 + `#![deny(clippy::await_holding_lock)]` 本就逼你这么分。
3. **buffer/路由隔离要靠 wire 级协议字段，不靠自家推断**（同 [[lesson §20]]）。
   hoisted 下 H2CData 落对 buffer 靠 reassembler 按 `(cccid,ttag)` 拒错配（协议
   语义），不是靠 gpa→CID 猜。这也是 HIGH-1 消失的 linchpin。
4. **happy-path 全绿 + 自家 revert-verify 都不够**：对抗输入（两 FIRST / SECOND-
   无-FIRST / 不匹配 / 并发）是 data-integrity 路径的必测面。mandatory review 第
   N 次抓到我会 ship 的 silent-corruption（[[lesson §22]] 同模式：我自信、reviewer
   adversarial 一问即破）。data-integrity 代码，对抗 case 测试 + review 不可省。

**来源**: fused C&W over fabric（commit `79daadc1`，2 轮 rust-reviewer：首版
capture-based BLOCK HIGH-1/2/3 → hoisted 重设计 APPROVE）；firmware 单元
`fused_cas_atomic_compare_and_write`（FAIL→backing 不变 原子性不变量）+ wire e2e
`fused_cw_e2e.py` 9 检查含对抗状态机。

→ 蒸馏：PRINCIPLES「设计 review 追问清单 · 结构修 vs 简化的公因子」(简化半：对抗
BLOCK 优先换更简单设计消掉对抗态)。

## 25. spec 常量值（SC/offset）必须 anchor 到 canonical 源, 手填的迟早错 (HIGH)

**症状**: SGL R2d 收尾时给 `sc::` 的 SGL status code 写 anchored 测试（每个常量 ==
`nvme_spec::Status::*.0`），一跑就红：R1 把 SGL SC **全部手填错**——
`SGL_DESCRIPTOR_TYPE_INVALID` 填 0x15（实为 OPERATION_DENIED）、`..NUMBER_OF_
DESCRIPTORS` 填 0x14（实为 ATOMIC_WRITE_UNIT_EXCEEDED）、CMB 0x16（实为
SGL_OFFSET_INVALID）、granularity 0x17（实为 RESERVED）。连带 reviewer 又揪出
`SANITIZE_IN_PROGRESS=0x12`（应 0x1d Generic）是 **live wire bug**：Sanitize-in-
progress 完成包以 SC 0x12 = Invalid Use of CMB 发出，真 driver 误解。

**为什么长期没暴露**: SGL「极少被真驱动跑到」（[[scope]]：OpenHCL+fabric 都走 PRP），
错的 SC 从没被对照过。doc comment 还信誓旦旦写「spec § 4.6.1.2.1 Generic SC 0x15」
——自己编的注释给自己背书（[[lesson §22]] 同模式：用不可信源自证）。

**根治**: `cmd.rs::sc` 既然 crate 已依赖仓库内 canonical `nvme_spec`，就把每个 SC 常量
**编译期 anchor** 到 `nvme_spec::Status::X.0`——drift 即测试红。Command-Specific 码
（spec 值在 0x1xx）用 `& 0xff` 取 SC byte，emit 时配 `SCT_COMMAND_SPECIFIC`。这与
[[lesson §1]] 的 `offset_of!` 锚定是**同一把尺子**：spec 数值（offset / SC / field
位）一律不手抄，绑到 canonical 源，让 build 替你查。

**三条**:
1. **任何 spec 数值（SC、offset、bit 位、长度）手填 = 迟早错且静默**。有 canonical
   源（nvme_spec / kernel header / libnvme）就 anchor；没有就 `offset_of!` / 真 wire
   字节捕获自锚。手算/手抄的 confidence 是假的（[[lesson §1]][[lesson §17]] 同源）。
2. **自己写的 doc comment 不是 spec**。「spec SC 0x15」这种注释是作者当时的理解，可能
   就是 bug 本身；校验得回 canonical 源，不是回自己的注释。
3. **anchored 测试是低成本高杠杆**：一个 `assert_eq!(ours, canonical)` 把一整类「手填
   错值」挡在编译期。新增 spec 常量就顺手 anchor，别等真驱动跑到才发现。

**来源**: SGL R2d（commit `2babf32a`）；anchored 测试 `sgl_status_codes_match_nvme_spec`；
reviewer 2 轮（HIGH 0x12 collision + live Sanitize bug → 校正 + 扩 anchor）。

## 26. in-process / 小内存 harness 看不见 >4 GiB 地址截断 —— 真 host e2e 不冗余 (HIGH)

**症状**: 真 Hyper-V OpenHCL guest（4 GiB RAM）起不来 NVMe 盘。controller log：guest
以单条 8-byte（qword）写 `ASQ=0x1_02eb_0000`，controller 却存成 `0x02eb_0000`（高 32
位丢），随后 SQE fetch 从截断后的错 GPA 读，首条 admin 命令读成全 0 → `opc=0x0` 被当
`Delete IO SQ` → guest 初始化挂。**间歇性**：admin 队列碰巧落在 4 GiB 以下时不触发。

**根因**: `controller/mmio.rs::mmio_write_impl` 对 8-byte 寄存器（ASQ 0x28 / ACQ 0x30 /
BPMBL 0x48）只按 `offset` 匹配、无视 `size`，一律 `value & 0xffff_ffff`——把单条 qword
写截断成 32 位。读侧本来就 size-aware（`(0x28,8)=>self.asq`），**只有写侧漏了 → 不对称**。

**为什么长期没暴露**: 三个对 firmware 的 harness——L1 跨进程 TCP（`tests/openhcl_
pcie_remote_e2e.rs` 用 flat 16 MiB guest-mem `Vec`）、vfio `qemu_interop`、`#[cfg(test)]`
单测——的 GPA **全部 < 4 GiB**。flat 小内存模型**结构上生不出 4 GiB 以上地址**，于是
「64-bit 地址高半被截断」这一整类 bug 对它们**不可见**。只有真 guest + 真实 RAM
（Windows nvme.sys 把 admin 队列放 4 GiB 以上）才触发。三 transport 单测全绿 ≠ 安全。

**根治**: 写侧改 size-aware（`size==8` 存全 64 位；否则低半合并）；ASQ/ACQ/BPMBL 三处
统一。单测 `asq_acq_qword_write_above_4gib_not_truncated` 锚 `0x1_02eb_0000` round-trip
（写 + `mmio_read_impl` 回读）+ revert-verify（重新引入截断 → 测试红 `0x02eb0000 ≠
0x102eb0000`）。真 host harness `scripts/hyperv_interop/` 双 oracle 复验。

**三条**:
1. **真 host e2e 不是 in-process e2e 的冗余**——它覆盖**结构性不可见的 bug 类**：地址
   宽度（>4 GiB）、对齐、真硬件/真 driver 的访问尺寸（nvme.sys 单条 qword 写 ASQ）、
   真时序。小内存 mock 的「全绿」是假安心（同 [[lesson §22]]：自洽 ≠ 正确）。
2. **任何 64-bit 寄存器 / GPA 的 MMIO 写必须 size-aware**（4-byte low/high vs 8-byte
   qword）；只按 offset 分支迟早截断。读写两侧要对称——本仓正是写侧漏了读侧对的。
3. **自写 harness 的 guest-memory 模型要敢上真实尺寸（≥4 GiB）或显式把队列/PRP 放高
   地址**，否则等于把高地址路径排除在测试外，制造结构性盲区。

**来源**: 真 Hyper-V guest e2e harness `scripts/hyperv_interop/` 首次跑当前 binary 即
抓到；fix `controller/mmio.rs`（ASQ/ACQ/BPMBL size-aware）+ 单测 + revert-verify；
rust-reviewer 确认无 sibling 截断点（PRP/IO 队列 base 走 SQE 的 u64，非 MMIO）。

## 27. API footgun（每个调用点手填同一参数）→ 结构性修，让数据自带 (HIGH)

**症状**: §25 校正 SGL SC 后，审计整个 `sc::` 模块发现错的不止字节——`Cqe::error(..,
sc: u8, sct: u8)` 把 **SCT（status code type）当独立参数在 224 个调用点手填**，多处填错：
`INVALID_PROTECTION_INFO` 当 Generic 发（driver 误读"Capacity Exceeded"）、
`NAMESPACE_IS_WRITE_PROTECTED` 当 Cmd-Specific 发、PI verify fail 当 Generic（应 Media）、
ZNS `INVALID_FIELD` fallback 被强制 Cmd-Specific、`RESERVATION_CONFLICT` 0x183、
`LOCKDOWN` 0x123、AER limit 用 0x05(power-loss)……全是"在每个调用点重复做同一个易错决策"。

**为什么 anchor 不够**: §25 的 byte-anchor 测试只锁**值**，锁不住"调用点把对的值配错
SCT 发出去"。footgun 在 API 形状里：只要 SCT 是独立的手填参数，第 225 个调用点还会填错。

**根治 = 改 API 让数据自带 SCT，不靠调用点决策**: `sc::` 常量改成**完整 u16 status**
（SC + SCT<<8，精确镜像 nvme_spec::Status）；`Cqe::error(.., status: u16)` 单参，
SC+SCT 由 `sf_of(status)` 自动派生。调用点再也不传 SCT → **整类 bug 从源头消失**，
且 byte-anchor 顺带变成 full-status anchor（连 SCT 一起锁）。224 个调用点脚本化迁移
（const 站点 drop SCT 参；运行期算 SC 的少数站点用 `sc::status(sc_byte, sct)` 显式拼）。

**三条**:
1. **同一个易错决策在 N 个调用点重复 = API footgun，不是 N 个独立 bug**。别逐点补
   （补完还会有第 N+1 个）；把决策**移进数据/类型**，让调用点无从填错。这里是"SCT 从
   status 高字节派生"，本质同 [[lesson §20]]"判据用协议字段不用自家约定"。
2. **结构性重构改 200+ 调用点不是"改动太大"的理由**（[[user-values-long-term-correctness]]）。
   脚本化迁移 + 编译器驱动（改签名→编译器标出每个站点）+ 全量测试 + anchor 锁，安全可控。
3. **审计要顺藤摸瓜到 API 形状**：reviewer 第一轮只抓了撞 byte 的 SANITIZE；真问题是
   SCT footgun。发现一个手填错值，要问"这个值/参数是不是每个调用点都在手填"，是就改结构。

**来源**: sc 模块结构性重构（commit `2a6d71ec`）；anchored 测试 `sc_constants_match_nvme_spec`
（全 sc:: == nvme_spec::Status）；rust-reviewer 2 轮（确认 4 处 intended SCT 校正 + 额外
挖出 RESERVATION_CONFLICT/LOCKDOWN 2 处错值，HIGH-1 一处 PI error-log 漏 Media SCT）。

→ 蒸馏：PRINCIPLES「设计 review 追问清单 · 结构修 vs 简化的公因子」(结构修半：同一易错
决策 N 处重复 = 一个结构缺口，决策移进数据/类型)。

## 28. realize-only（CPU 暂停）e2e 掩盖真 guest 才触发的整类 bug —— vfio-user 4 bug (HIGH)

**症状**: vfio-user realize-only harness（QEMU `-S` 暂停 CPU + 只读 config space + 验 PCI
总线看到设备）一直 PASS，但一引真 guest 驱动设备就 QEMU fatal：`unexpected reply` /
`bad header size` / `vfio: DMA mapping failed`，guest 串口空白（连内核都没起）。

**根因（4 个，全在"真 guest 驱动设备"路径上，realize 命不到）**:
1. **posted(NO_REPLY) write 回了 reply → wire 失步**。guest 写 CC.EN→0（BAR0 REGION_WRITE
   带 QEMU 的 NO_REPLY 标志）我们却回 reply；QEMU 没在等，把那 16 字节当**下一条消息
   header** 解析 → `unexpected reply` + `bad header size` → 整条流报废。realize 不写 BAR0。
2. **config space 没 MSI-X capability** → guest `nvme_probe -EINVAL`。realize 不真探中断。
3. **SET_IRQS 拒绝 masked vector**（QEMU 对 `-1` 向量发 0 个 fd，旧码 `fds.len()!=count`→EINVAL）。
4. **广告 DBBUF 但只存 shadow 不轮询** → Linux 信任我们轮询而**跳过真 MMIO doorbell** →
   那些命令永不被 fetch → IO 30s 挂。realize 不做 IO。

**为什么 realize 全命不到**: realize-only = 设备 realize 握手（VERSION/GET_REGION_INFO/config
读/DMA_MAP）+ `-S` 暂停 CPU。它只验"设备造出来了 + 握手对"，**从不让真 guest CPU 驱动设备**
——不写 BAR0、不探中断、不做 IO。4 类 bug 全在那条没被走的路径上。同 [[lesson §26]]（in-process
harness 看不见 >4 GiB）一个道理：便宜 harness 的 PASS 是结构性假安心。

**三条**:
1. **"设备 realize 成功" ≠ "真 guest 能用"**。realize 只验造设备 + 握手；真 guest 驱动（MMIO
   读写 / 中断 setup / DMA / IO）是另一整条路径，bug 都藏那。每条 transport 都要真 guest e2e。
2. **wire 的 posted/fire-and-forget 语义必须严守**：回一个对端不期待的 reply 就**失步**，整流
   报废。NO_REPLY 标志要 thread 到**每个** reply 点（成功 + 错误路径）。（"失步"=接收方对消息
   边界判断错位且一路传染——你多发一条它就把后续每条都从错误的地方切开。）
3. **中立 `describe()` 里别塞 transport 专属的 PCI-config 细节**（MSI-X cap）：它转发给所有
   transport，在自己用 MsixEmulator 合成 MSI-X 的 OpenHCL 上会撞车（双 cap / 4-align 握手拒）。
   config-space cap 是 per-transport 派生物，不是中立设备属性——本次在 OpenHCL SDK 转发时 strip。

**来源**: vfio-user guest-boot harness `scripts/qemu_interop/run_qemu_vfio_guest.py` 首跑即抓
4 bug；fix `vfio_user_transport/{session,dma,irq,framing}.rs` + `nvme_firmware/{cmd,controller/
mmio,controller/mod,regs}.rs` + `pcie_device_sdk/run.rs`(MSI-X strip)；rust-reviewer 2 轮
（核心 wire 逻辑 APPROVE + BLOCK 3 项 clippy/fmt/OpenHCL-MSI-X 回归全修）。

## 29. 自馈 async poll 循环：3 层 bug 全在 happy-path harness 盲区 + cap 必须界定对的量 (HIGH)

**背景**: DBBUF (shadow doorbell, spec § 5.7/§ 7.13) 从 stub（只存 shadow/event_idx GPA 不
轮询、广告 OACS.DBBUF=0 规避，见 [[lesson §28]] bug #4）补成真实现：controller DMA-poll driver
shadow buffer 拿真 SQ tail/CQ head + 写回 event_idx。因 `Transport::dma_read` 是 **token-异步**
（无同步读，完成经 `on_dma_complete`），轮询只能实现成**跨完成回调的异步状态机**。

**3 层 HIGH bug 全在 async 边角，连真-QEMU-guest happy-path harness 都看不见**（vfio 同步 drain
把整条链一次跑完 + 真 driver 行为良性，故这些 interleaving 永不自然发生），逐轮 reviewer 越挖越深：
1. **dropped-ring strand**：poll 链在 drain 时到的 doorbell ring 被 inflight guard 丢弃；若
   driver 的 submit 恰落在链最后一次 re-read 之后的 settle window，命令被漏 → 滞留到下次 tick
   （本 bin `read_timeout=60s`；早期误记为 SDK 默认 5s）。
   修：per-(qid,is_cq) one-shot `shadow_ring_pending` flag，settle 分支若置则再 re-read 一次。
2. **wrap-saturation false-CFS**：cap 用绝对环索引做 high-water-mark，max-depth 队列 wrap 后
   饱和于 size-1 → 健康 controller 的合法 wrap 全被判"无进展" → 撞 CFS（reviewer 实测 size=
   65536 在第 131070 步必触发）。
3. **vfio 同步 `dma_read` 让 poll 链自馈 `drain_dma_completions` 的 while 循环**：同步 drain 里
   每次 `issue_shadow_read` 立刻 push 完成回正在 drain 的同一队列 → in-range 振荡 shadow 让链
   永不 settle → **host 线程在一次 drain 内无限自旋**（guest-触发 DoS）。OpenHCL async 路径只
   是软浪费（帧交错），vfio 同步路径是硬挂。

**cap 度量的核心教训——"防御性 cap" 必须界定对的量**（三次试错）：
- 界定 **advance**（推进多少命令）→ 真高吞吐 driver 稳态流水合法推进无界、永不 settle → 任何
  有限 cap 都误伤真 driver。**错**（这正是 bug #2 原版）。
- 界定 **ring 距离**（累计前向距离）→ 看似 wrap-safe，但振荡的 wrap-back 也算前向距离 → cap
  单调不可触发=**dead**；且这是 bug #2 那类"环索引语义假设"的脆弱启发，reviewer 明确反对手搓
  振荡检测器。**错**（且 dead cap 没人发现，直到 reviewer 追问"它真能 fire 吗"）。
- 界定 **每链自续深度**（一条链总共 re-read 多少次，仅链起始重置）→ correct driver 链深仅 ~tens
  （代码实测 ~14）≪ 上限；自馈死循环无界 → 撞 `MAX_SHADOW_POLL_ITERS` 置 CFS + 收链
  （有限 ≤65536 往返终止）。这是 host-liveness 界，**对**。

**怎么验这些盲区**: 全靠 **deterministic 单测手搓 interleaving + revert-verify**——CaptureTransport
喂定值序列驱动状态机走过 reviewer 描述的精确交错（dropped-ring 落在 settle window / 振荡**有界**
喂 MAX+8 次以免测试自身死循环），断言命令被救 / CFS 恰在第 MAX 步触发；revert（抽掉修复）必让
该测试红。真 harness 只证 happy-path 通 + DBBUF 真被行使（shadow 领先 MMIO），证不了边角。

**三条**:
1. **token-异步 transport 上的轮询必是跨回调状态机**，不是同步 loop；其 race（submit-during-
   window）/ liveness（自馈死循环）边角 happy-path e2e 结构性看不见，只能 deterministic 单测
   reproduce + revert-verify。同 [[lesson §26]]/[[lesson §28]]：便宜/happy harness 的绿是假安心。
2. **"防御性 cap" 必须界定对的量**：先问"要挡的坏情况和正常负载在哪个量上分得开"——挡 advance
   误伤真负载、挡 ring 距离脆弱且 dead、挡自续深度才既不误伤又能终止。cap 写完要证它**真能
   触发**（dead cap = 装了个永不响的保险）。
3. **synchronous-drain transport 上设备自发的 DMA 会自馈 drain 循环**：任何"完成里再发请求"的
   逻辑都可能把 host 线程卡死在一次 pump 内。**封顶必须放 device 侧**（per-链自续深度 cap，量 =
   "自续深度"而非"advance/ring 距离"，见上 cap 三选）：device core 是 runtime-agnostic 的，一处
   正确则三条 transport 全受益。transport 的 `session.rs` drain 循环**不该**加迭代上限——其合法
   worst-case = 链数 × per-链 cap，扁平常量要么误伤真负载（=重演"挡 advance"）要么是 dead cap，
   且 transport 拿不到"哪条链在自馈"的语义。当前每条递归 DMA-in-completion 路径（SGL / PRP-list /
   shadow-poll / CMB-drain）均已各自封顶且各有 firing 测试；**完整不变式台账（每路径→终止机制→
   firing 测试 + I1/I2/I3 承重不变式 + PR 回归闸 + ChainDepth 收敛下一步）见
   `crates/nvme_firmware/docs/DMA_COMPLETION_INVARIANTS.md`**。

**来源**: DBBUF 真实现 commit `f35e5a71`；4 轮 rust-reviewer 逐层抓 HIGH-1(dropped-ring) →
HIGH-2 wrap-saturation → point-3 vfio 自馈死锁（每轮修完下一轮挖更深，全在 vfio 同步 harness
盲区）；单测 `dbbuf_tests`（`high1_dropped_ring_during_settle_window_is_recovered` /
`high2_realistic_maxdepth_wrap_settles_never_trips_cfs` / `high2_inrange_oscillating_chain_
trips_cfs_at_cap` 等，全 revert-verify）；真 QEMU 11 vfio 2-vCPU guest GREEN（19 次 shadow 领先
MMIO，burst 0 CFS）。

→ 蒸馏：PRINCIPLES「设计 review 追问清单 · 防御性界限：界定『对的量』+ 必证能触发」

## 30. 清理"死代码"前先分辨：过时残留 vs 前瞻 scaffolding (HIGH)

**用户铁律（2026-06-11）**：「清理旧代码必须确保真的是过时代码，而不是为了完整、长远
目标提前埋设的内容」。删除是不可逆动作；删错一个前瞻预置 = 撤掉长远正确路线上的台阶。

**背景**：测试覆盖率审计把 6 个"从不 emit 的 `sc::` 常量"标成 dead；本想 Wave 4 删其中
2 个（`BOOT_PARTITION_WRITE_PROHIBITED`、`SGL_INVALID_USE_OF_CMB`）。用户喊停——它们是
**有效 NVMe 状态码、为 spec 完整性预置**（boot-partition / CMB 特性落地即用）。删 = 撤
scaffolding。改判：**保留 + 锚到 nvme_spec（验值）+ `#[allow(dead_code)]` 注明"X 支持
落地时 wire"**；那 4 个真缺口（NAMESPACE_NOT_READY/ATOMIC_WRITE_UNIT_EXCEEDED）不是删，
是 **wire（补实现）**。

**判据（未用符号该删还是该留）**：
1. **对得上 spec / 已声明的长远目标 → 保留 + 锚定**（如本项目 [[teaching-means-rigor-not-toy]]：
   每层 spec-complete 是目标，spec 状态码即使暂未 emit 也是完整性的一部分）。
2. **对不上、纯历史残留 / 重复 / 已被取代 → 才清**（如 [[lesson §26]] 删的不是常量而是
   "手填 SCT 参数"那个 footgun 形状）。
3. **拿不准 → 留 + 标注意图**，别赌。删错的代价 >> 留一个带注释的未用符号。

**纪律**：
- 审计工具（cargo-mutants/llvm-cov/dead-code lint）报"未用"是**信号不是判决**——它不知道
  "前瞻预置"的意图。删除前回到 spec / 项目愿景核对。
- "未用 + 对得上 spec" 的正确动作是 **anchor 它**（值锚到 canonical 源）+ 注明落地条件，
  而非删。这样既不积灰（值有锚保护）又不撤 scaffolding。
- reflection：每轮清理后回看自己是否删过 scaffolding，及时挽救（本轮自查：sc 重构
  `comm -23` 证 0 常量被 drop，R2 的 flatten_data_blocks/SglFragment scaffolding 仍在，
  无违规——在 Wave 4 真删之前就被用户铁律拦下）。

**来源**：测试覆盖率 Wave 4 计划评审（2026-06-11，用户两次强调 + 要求长记）。

## 31. 把"重/真机 oracle"变成 standing CI gate：frozen-vector 回放，冻确定性前缀 (HIGH)

**背景**：[ADR-013](DECISIONS.md) 档2 frozen-vector —— 真 guest-boot harness（真 Linux nvme 驱动
驱动 vfio-user 设备）是最强独立 oracle，但太重（整 guest 引导 + QEMU + initramfs），进不了无人
值守 CI（档3）。要把它的权威信号搬进 standing gate，做法 = **抓一次真 oracle 的 byte-exact wire
transcript → 冻成黄金 → 对新起 server 回放真 oracle 的 client stimulus，断言响应逐字节等黄金**。

**三个非显然承重事实——全靠分析真 transcript 坐实，不是靠假设（[[poc-before-settling-design]]）**：
1. **回放可线性 ⇔ 对端不主动发消息**。本 server 走 **mmap-based DMA**（经 DMA_MAP mmap guest
   memfd 后直读内存），**从不**主动发 wire DMA_READ/WRITE → 整条流是纯 client-request→server-reply
   → 同步 send-one→read-one 可线性回放。若 server 会主动发起请求（要 client 反应），回放就得模拟
   对端状态机——量级翻倍。**先在真 transcript 里 grep 有无"对端主动消息"再定能否线性回放**。
2. **冻结边界 = 响应开始依赖隐藏状态的那一点**。enable（CC.EN=1）后首个 doorbell ring 触发 server
   读 **mmap'd guest 内存**取 SQ entry → 回放用零填充合成 memfd 会读到零、行为分叉。故黄金**只能冻
   到首 doorbell 之前**的纯寄存器/枚举前缀（响应与内存内容无关、确定）。**边界由"server 何时首次
   读外部不可复现状态"定**，机械判据（首个 BAR0 offset≥0x1000 的 REGION_WRITE）写进抽取脚本。
3. **fd-bearing 消息若响应 mode-无关，可不传 fd 回放**。前缀含 DMA_MAP（带 memfd），但其 reply 是
   mode-无关的 OK（server 对无-fd DMA_MAP 走 message-mode fallback），且前缀不访问内存 → **0-fd
   回放的响应与原 mmap-mode 黄金逐字节相同**（POC 实证）→ 省掉 SCM_RIGHTS 复杂度，纯阻塞 socket。

**牙 / 独立性**：stimulus 是**真 kernel 实际发的字节**（独立 oracle，catch 手搓 client 想不到的真实
用法）；golden 是 server 自身响应被冻在 guest-PASS 时刻 → 回归 gate。与手搓-client oracle（[[lesson §28]]
那条 `vfio_user_wire_e2e`）互补：手搓 = "我以为该发什么"，frozen = "真 kernel 实际发了什么"。

**纪律（防 frozen 退化成 §20 假安心）**：① 抓包代理对 fd 必须**透明转发**（自验=guest 经代理仍
PASS）；② golden 必带 provenance（kernel/QEMU 版本 + marker）+ **机械抽取脚本**（消除 JSON→txt 人工
断点）+ refresh owner；③ 回放配对**双向锚定**（reply.id==刚发请求 id + golden s2c id 唯一），把
"server 回错请求/失步"也纳入牙；④ 硬编码帧数锚定防 golden 被悄改截断；⑤ revert-verify（注入 server
前缀-面回归看测试红）。

**来源**：vfio-user 档2 frozen-vector 落地（2026-06-13）。抓包 `run_qemu_vfio_guest_capture.py`
（fd-aware MITM）+ 抽取 `extract_wire_prefix.py` + golden `vfio_guest_wire_prefix.txt` + 回放
`vfio_user_guest_replay_e2e.rs`；POC 3× 0-mismatch；rust-reviewer 2 层独立 revert-verify（revision
字节 / BAR size 皆 catch）。**连带发现**：建这个 gate 的前置 usnvmemu CI（[[usnvmemu-no-ci-gate-workspace-excluded]]）
首跑就会抓到 CMB 工作引入的 `NvmeController: !Send` HEAD 回归——standing gate 的价值即时兑现。

## 32. 物理页边界 ≠ 逻辑单元边界：拼回-重切 + 正交切轴不混淆 + tier 当唯一裁判 (HIGH)

**症状**：PI 数据路径（separate-meta / inline-meta）原按"每 LBA / 每段**固定**大小"切 host DMA
（separate 每段 4096；inline dual-PRP 硬编码 (4096,8)）。PRP1 可带**任意页内偏移 O**（spec NVMe
Base §4.1.1：**仅 PRP1** 可带页内偏移，PRP2/list-entry 须页对齐），使一个**逻辑单元**（separate 的
一个 LBA data / inline 的一个 4104 extended block）跨 host 页边界。固定切分在 O>0 **silent 错位**——
从 `PRP1@offset` 盲读整 4096 字节跨进下一页、且漏掉本该落 PRP2 的尾段 = 真**数据完整性洞**（非 panic，
happy-path O=0 看不见）。

**修复范式 = 拼回-重切（reassemble-then-resplit）**：host DMA 按**物理页段**收/发（`prp::dispatch_segs`：
首段 `page−O`、中段整页、末段 partial）→ 拼回连续逻辑流 → 按**逻辑单元大小**（`data_bytes` / `block_bytes`）
重切 → 逐单元 verify/store。**物理段数 ≠ 逻辑单元数**（O>0 时一个逻辑单元跨多段；List 档一段含多单元）。
统一了 dual / list 两条曾经 bespoke 的路径。

**正交切轴陷阱（inline extended-LBA 特有，最隐蔽）**：inline block 有**两条独立切轴**——
① "页划分" `(4096−O, 8+O)`（决定 host DMA 段长 / PRP1·PRP2 落点）；② "data/tuple 划分" `(4096, 8)`
（决定 PI 校验在哪切 guard）。**finalize 必须在拼回后的完整 4104 block 上切 data/tuple，绝不在 host
段上切**，否则 guard 算错 → PI mismatch。两轴在 O=0 **巧合对齐**（`4096−0 == 4096`），正是 latent bug
的藏身处——O=0 测试全绿、O>0 才暴露。

**tier 当唯一裁判，别耦合 nlb**：分流判据写 `nlb>2` / `nlb>=2` 是脆弱耦合（漏掉"nlb 小但偏移把传输顶进
更高档"）。改用 `prp::tier(O, total)` 单点裁定 Single/Dual/List：separate nlb=2 O>0 → 3 页 → List；
inline nlb=1 O>4088 → 3 页 → List。这些都是 **spec-legal corner，真做不拒**（known-answer 单测直构造
验 List 机件接管，不靠真 driver 触发；不拿"真 driver 不构造"砍，见 [[meaningful-complete-not-minimal]]）。

**验证（差分 oracle 四件套，§23 的具体化）**：① backing/`PiTuple::compute` 期望；② **显式断每条
DmaRead/DmaWrite 的 `(gpa,len)`**——`CaptureTransport` 不截断喂入，光断 backing/重组字节**抓不到**"固定
4096 盲读"盲点，必须断段长；③ **断走哪个 tier 分支**（Dual vs List，op 变体 + 累积器 map 双证）；
④ revert-verify 两向（把切分退回硬编码 → O>0 段长/段数断言转红）。临界必测 O=0/100/4088/4090/4095。

**来源**：#4c-b P3（2026-06-14，commits `31ea153`/`3e28ee8`/`823f3fe`/`19bd047`，各 ecc:rust-reviewer
APPROVE 0 C/H/M）。全 PI 路径全 tier PRP1 偏移零 spec-legal 例外，见 MILESTONES §1.8 + plan
`2026-06-13-4cb-unified-prp-segment-abstraction.md`。

## 33. 跨实现互通的盲区藏在「另一端默认走的窄路」之外 —— 真异构对端是唯一 oracle (HIGH)

**症状**：NVMe-oF TCP target 在纯 Linux（`nvme-cli` + `nvme-tcp.ko`）全栈 shipped、真机互通绿、306
tests 绿、已"冻结"。但真 WS2025 Windows inbox initiator 一连，IO 队列建不起来、不出盘。深挖出**三个**
纯 Linux 永远盖不到的缺口，全因 Linux nvme-cli 的**默认行为**把 spec 允许的岔路遮住了：

1. **static controller model**：Linux nvme-cli 默认 dynamic（Connect CNTLID=`0xFFFF`），Windows inbox
   默认 `connect -ci`（static 具体 CNTLID）。target 旧码无视 host 请求的 CNTLID 静默返 1——non-conformant，
   但 dynamic host 永不送具体值，故测不出。修=校验 + reject mismatch（C1）。
2. **transport-SGL Connect data**：Linux 总把 1024B Connect data 放 **in-capsule**；Windows IO Connect
   走 **Transport SGL（`sqe[39]=0x5A`）经 R2T/H2CData**（因 target 自己广告 `IOCCSZ=4`=IO 无 in-capsule，
   Windows 是对的）。target 旧码 parse 前 `data.len()!=1024` 拒、从不发 R2T。修=transport-SGL 路由 + R2T
   fetch（C3，ADR-008）。
3. **SCT 0x07→0x01**：fabrics SC(0x80-0x9F) 原用 SCT=0x07 Vendor Specific（既存 wire bug），self-consistent
   测试一起骗过——**self-consistent ≠ spec-conformant 第 N 次**（见 §17 同根、auto-memory
   `review-not-optional-self-consistent-trap`）。Windows 校验 SCT 才暴露。

**可迁移教训**：
- **这是 [[fuzz-the-contract-not-current-impl]] 在 interop 维度的重演**：契约（spec 允许 static + transport-SGL）
  比"当前唯一消费方（Linux nvme-cli）碰巧怎么调"宽，宽出来的那部分藏 latent 缺口。**真异构对端**（另一个
  独立实现的 host）是行使契约这些角落的唯一 oracle——自家两端（Rust target ↔ 自家 Python harness）共享
  同一套窄假设，永远互相盖不到。
- **定位手法**：relay 旁路抓 **byte-log**，解码 host 的**第一个决策分叉**（admin Connect vs IO Connect 的
  SGL type byte 差异）一眼定位根因，**别从最深症状（IO 不出盘）反推**——符合 [[debug-root-cause-at-driver-first-branch]]。
- **真机也可能 non-conformant**：Windows H2CData 把 CommonHdr PLEN 设为 HLEN（不计 data、违 TP-8000），
  健壮实现据 PSH DATAL 补读（越界仍拒）。对端违规不等于自己跟着违规，而是"宽容接收 + 自己发严格"。
- **方法论**：一条线"shipped + 真机绿 + 冻结"只代表**测过的那条 host 路径**对，不代表 spec 契约全覆盖。
  新接一个独立实现的对端 = 一次廉价但高价值的契约 fuzz。

**来源**：V-followup-static-controller-model C1（`e2c022575`）/ C2（`9f1ae4d87`）/ C3（`4df5c3f83`），
2026-06-14 真 WS2025 出盘 PROVEN。详 MILESTONES §4.14 + ADR-008 + 证据本位回应
`experiments/2026-06-13-openhcl-vpci-nvme/usnvmemu-response-to-ws-handbook.md`。

## 34. CMB 真机：两 self-consistent-trap 寄存器 bug + 「源码追踪异-config 消费方 ≠ 验证 primitive」(HIGH)

CMB L4（真 Linux nvme over QEMU）+ 零拷贝-on-OpenHCL POC 两段，各长一条教训：

**①（self-consistent ≠ spec，连 spec 误读+architect review 都骗过的第 N 次）**：CMB L4 真机暴露两个
firmware 寄存器 bug，in-process 测试 + 我读的 spec + architect review **三道防线全自洽通过**，唯真 Linux
驱动作独立 oracle 才捅破：
- **CMBSZ 位布局非 spec-aligned**——SQS 编 bit4 而非 §3.1.14 bit0、SZU bits3:0 而非 11:8；firmware 意图广告
  全能力却写出 `0x002001f0`，真 Linux 按 spec 解码成 **SQS=0** 拒用 CMB。测试 `cmbsz_field_offsets_match_spec`
  **反而 enshrine 了错布局**（断言 `SQS==1<<4`）→ 全绿。
- **CMBMSC 跨 Controller Reset 误清**——`disable()`(CC.EN→0) 清 cre/cmse/cba，但真 Linux `nvme_map_cmb` 仅
  编程一次（`if dev->cmb_size return`）依赖其持久；标注「spec §3.1.24 + architect 复核」却都错，QEMU
  `nvme_ctrl_reset` 不动 cmbmsc = interop oracle 钉死。
- 泛化（同 §17/§20/§22/§32/§33）：凡 firmware **自定义 wire 寄存器布局/生命周期 + 自测 + 自证 spec**，三者
  同源不算验证；必须真对端（真驱动 / 参考实现 QEMU 源码）独立核。commit `3a1779c1d`。

**②（源码追踪消费方是廉价 POC，但消费方在异 config 时会给假阳）**：feasibility 实验**做了**「源码追踪
i440bx 消费 create_ram_gpa_range」这个本仓推荐的最廉价 POC，据此**推断 host 支持**。但真机首调即对任意
GPA／真 VPCI 设备 BAR 窗口／backend-live **一律 FAILED**——因为 i440bx 是 **Gen1/PCAT、本 VM 是 Gen2 从不
走它**。「生产代码依赖 X」≠「X 在你的 config 下成功」；闭源对端（真 Hyper-V `IVmGuestMemoryAccess`，仓里
只有硬编码 FAILED 的 GED stub）+ 唯一消费方在异 config = **必须真机调一次**，别靠「它在别处被依赖」推断。
4 轮真机 + 2 轮独立 reviewer（GED-stub 假阴性经 INVALID_GPA 排除）。

**来源**：MILESTONES §3.7、auto-memory `review-not-optional-self-consistent-trap`(第6次) /
`poc-before-settling-design`(2026-06-14 反向) / `debug-root-cause-at-driver-first-branch`；
归档 `experiments/2026-06-13-cmb-l4-realmachine-qemu/` + `experiments/2026-06-14-cmb-zerocopy-openhcl-phase1/`。
