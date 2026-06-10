# 测试覆盖率加固 — 进度 & 待办（可审阅总账）

> 单一事实源。回答"如何**不断**提高覆盖率 + 现在做到哪、还剩什么"。
> 机制原理详见 `TEST_QUALITY.md`；这里是**进度账 + TODO**。
> 建于 2026-06-11（test-coverage-hardening）。

---

## 0. 一句话现状

把"覆盖率%"换成可持续的**三轴判据**（执行到 × 独立 oracle 验对 × 改坏会红），
建立 6 条连续机制（M1–M6），按多 subagent 流水线（探测→独立审计→设计→review→
实施→review→测试→review）落地 Wave 1–7。**135 lib test**（Wave 前 102）、
sgl/pi 变异 **8→0 missed**、当场抓出 **1 个真 bug**（`GET_LBA_STATUS`）。

---

## 1. 方法论（用户指定的流水线）

```
不同 subagent 并行探测覆盖盲区
        ↓
subagent 分别独立审计证实（互不看对方结论）
        ↓
以最佳方案设计 → review → 实施 → review → 测试 → review
```

**三轴判据**（覆盖率% 只是 proxy）：

| 轴 | 含义 | 谁守 | 失守教训 |
|----|------|------|----------|
| 执行到 | 代码被某测试跑到 | `llvm-cov` | SC bug 活在 18% 覆盖的 completion.rs 错误臂 |
| 独立 oracle 验对 | 断言用**独立于被测代码**的真相源，非自洽 | anchor + proptest 独立 oracle | LESSONS §20：self-consistent 测不出 self-consistent bug |
| 改坏会红 | 行为错了测试 FAIL | `cargo-mutants` + 手动 revert-verify | LESSONS §23/§26：happy-path 全绿 ≠ 有牙 |

---

## 2. 已完成（Wave 1–7，按 commit 可追溯）

| Wave | commit | 内容 | revert-verify 证据 |
|------|--------|------|---------------------|
| sc 重构 | `2a6d71ec` (+`5de65170` 补 stage) | sc 状态码全 u16（SC\|SCT<<8）锚 nvme_spec，消手填 SCT footgun | — |
| **W1 / M1** | `672501c8` | opcode/feature/register/Log-LID/PI-SC 全锚 `nvme_spec` | **当场抓真 bug**：`GET_LBA_STATUS 0x1e→0x86`（0x1e 实为 NVMe-MI Receive，且已被 admin 派发 + OACS 广告） |
| **W2 / M4+M5** | `03ecd79b` | driven 错误码矩阵（驱 dispatch_io 进 6 错误臂验完整 16-bit status）+ e2e `CqeResult.status:u16` 去盲 | 派发侧注错码 → 2 vs 385 FAIL（非改常量自洽） |
| §30 原则 | `290367df` | LESSONS §30：清理"死代码"前分辨过时残留 vs 前瞻 scaffolding | — |
| **W3 / M3** | `bfd975a4` | proptest harness（copy-conflict u128 brute / CRC vs crc crate / SGL no-panic / ZNS 矩阵 / PRP 恒等 / PI 往返）+ pi 已知答案 | 杀光 sgl/pi 存活变异 8→0；known-answer 杀 `pi.rs:79 &→^` |
| **W4 牙** | `d446de94` | 给 `ns_detached`/`copy-on-PI` 两个无牙测试上牙（真驱 dispatch）+ 守住 spec scaffolding | 驱动后断言完整 status |
| **W5 / M6** | `d2997ec8` | `scripts/test-quality.sh {check\|anchors\|mutants\|coverage\|all}` + `TEST_QUALITY.md` | — |
| **W6 census** | `0f6ab619` | M4 矩阵扩 `SANITIZE_IN_PROGRESS`（0x001d） | — |
| **W7 / M2** | `8204042e`/`d9760394`/`3ac3e3fc` | 布局锚：SMART(Fig 207) / error-log(Fig 205) / ZNS-Id(ZSZE@2816)，`#[repr(C,packed)]`+`offset_of!` 锚 spec figure，替代"test 照抄 builder" | builder offset 32→30 → 锚 FAIL |

---

## 3. 连续机制 M1–M6（落地状态）

| 机制 | 是什么 | 状态 | 维护动作 |
|------|--------|------|----------|
| **M1 锚定护栏** | 复制 nvme_spec 的常量编译/测试期锚 canonical | ✅ 已立 | 新增 spec 常量 → 顺手 anchor |
| **M2 布局锚定** | log/report 字节布局 `offset_of!` 锚 spec figure | **3/4**：SMART/error-log/ZNS ✅；resv-report ⏳ | 新增 log builder → 加 repr(C) 锚 |
| **M3 proptest** | 纯/近纯函数随机输入 + 独立 oracle 不变量 | ✅ 已立 | 新增数据路径 → 加 proptest |
| **M4 driven 矩阵** | 真驱 dispatch 进错误臂验完整 SCT | ✅ 6 同步码 | 新增 error emit → 加 driven 断言 |
| **M5 e2e 去盲** | e2e 验完整 16-bit status | ✅ 已立 | — |
| **M6 经验门** | `test-quality.sh` + cargo-mutants + llvm-cov | ✅ 已立 | CI 跑 check+anchors；定期 mutants |

---

## 4. 待办 TODO（按性质分类 + rationale）

### 类 A — 纯增量，有价值（可直接做）

- [ ] **M4 错误码普查扩到 async-completion 路径**：reservation 冲突 7/8 分支、NS-attach 族
  （ALREADY_ATTACHED / NOT_ATTACHED / ID_UNAVAILABLE）、zone 限额（TOO_MANY_OPEN/ACTIVE）、
  PI media SCT=2。**需 DeviceCtx 驱动 async 完成路径**，每码 ~30 行。
- [ ] **proptest 扩到更多数据路径**：随被测面增长持续加独立 oracle 不变量。

### 类 B — 特性耦合，按 §30 "留真做时"（**勿 speculative wire**）

- [ ] **M2 resv-report 布局锚**：审计发现 builder 漏写 `ptpls@19`。补 ptpls 是**特性修**
  （不是测试改），落地该特性时一并锚 NVM CS figure。当前可先锚 builder 已写字段的 header offset。
- [ ] **AWUN 强制**（ATOMIC_WRITE_UNIT_EXCEEDED）、**NS-not-ready 门**（NAMESPACE_NOT_READY）、
  **boot-partition**（BOOT_PARTITION_WRITE_PROHIBITED）、**CMB**（SGL_INVALID_USE_OF_CMB）：
  这 4+2 个 sc 常量是 spec-complete scaffolding，**已锚定值**，对应特性真做时才 emit。
  > §30 铁律：清理/wire 前先分辨过时残留 vs 前瞻预置；prefer keep+anchor 而非 speculative emit/删。

### 类 C — 低价值（给牙投入 >> 收益，诚实留注释优先于 fake-strengthen）

- [ ] `o3_fused_cw_dispatch_chain_smoke`（断言 events 空=没测行为）
- [ ] `aen_queue_fifo_order`（测的是 stdlib VecDeque）
- [ ] `fw_download_cap`（断言 64MiB≥8MiB 重言；真 cap 是 completion.rs 函数内 const 不可 import，给牙须驱 async FW-download 完成路径）
- [ ] `k4c_list_accum`（构造 struct 再断言自己的字段）
  > 这 4 个给真牙要驱对应 async 路径，性价比低；按 §30 **不删**（prefer 上牙或诚实标注弱）。

### 已交付的"持续"动作（非一次性 TODO）

- 新增 spec 常量 → 顺手 M1 anchor；新增 log builder → M2 锚；新增数据路径 → M3 proptest；
  新增 error emit → M4 driven 断言。固化在 `TEST_QUALITY.md` 各机制的"维护动作"。

---

## 5. 已知问题（faithful report）

- **~~`openhcl_delete_io_queue_lifecycle` 并行偶发 FAIL~~ —— 已由并发会话 commit `462faf87` 解决**。
  - 原症：e2e 每 case spawn 真 `nvme_firmware` OS 进程；旧版用 `sleep(200ms)` 等"删后试写未落盘"，
    并行 21 进程 CPU 争用下偶发 read-before-settle（读回全 0）。**非逻辑回归**。
  - 修复：`462faf87` 用 **admin Identify fence**（非 timing-fragile sleep）——biased device-first
    pump 必先排干 broken 路径 IO 的两段 DMA 往返 + CQE 才把 fence doorbell 上 wire，fence 返回即
    保证"若 SQ 仍在则写必已落盘"。并行 e2e 现 21/21 稳过。故**无需** `--test-threads=1` 串行 gate。


---

## 6. 命令 & 工具

```bash
scripts/test-quality.sh check     # 快门：lib + e2e(串行) + clippy + fmt（CI 必跑）
scripts/test-quality.sh anchors   # M1 锚定护栏
scripts/test-quality.sh mutants   # M6 变异（sgl/pi 应 0 missed）
scripts/test-quality.sh coverage  # M6 lib 覆盖率
scripts/test-quality.sh all
```

```bash
cargo install cargo-mutants cargo-llvm-cov      # 一次性
rustup component add llvm-tools-preview          # llvm-cov 需要
# proptest + crc 已是 dev-dependency
```

**llvm-cov 盲区**：`--lib` 覆盖率严重低估——e2e 跨进程子进程测不到。故 completion.rs/io.rs
错误臂**优先下沉到 unit 层断言（M4 矩阵）**而非只靠 e2e 子进程。

---

## 7. 引用

- 机制细节：`docs/TEST_QUALITY.md`
- 教训：`../../docs/LESSONS.md` §20（self-consistent 陷阱）/§23·§26（无牙陷阱）/§30（清理 vs scaffolding）
- 记忆：`cleanup-scaffolding-vs-obsolete` / `spec-aligned-field-order`
