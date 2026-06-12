# Kani 有界形式化验证 POC — 结论与落地（含价值证伪）

> **状态**：POC 完成、双重 subagent 复核共识（architect 评估 + rust-reviewer POC 复核，含 unwind 反证）。
> **结论**：工具链可行、属性可证，但**价值边际**——目标与已有 proptest+cargo-mutants 覆盖面重叠，
> in-crate 又被 workspace MSRV 硬阻断。**当前判定 LOW 优先级**，非高价值投资。日期：2026-06-13。归属：`nvme_firmware`。
> **来源**：reviewer 提议（"Kani 证有界 walker 终止性 + CC/CSTS 状态机"）+ architect 评估 + 本 POC（`/tmp/kani_poc/`）+ rust-reviewer 复核。

## 0. 一句话

reviewer 想用 Kani 证"有界 SGL walker 终止性 + CC/CSTS 状态机无非法跃迁"。**两个目标都基于对架构的误读**：
walker 不是纯函数（是跨异步 DMA 往返的 continuation）、状态机不是纯转移函数（effectful，改 HashMap+flush I/O）。
Kani 真正能证的只有 **self-free 的 SGL 字节解析谓词组**对 host 可控输入"全输入无 panic"——而**这组函数已被
proptest 不变量 + cargo-mutants 覆盖透**（见姊妹 fuzz plan 把它列为"Tier 1 边际收益低"）。故 Kani 在此是
**对已充分测试代码的 proof-grade 升级**，不是补未覆盖的洞。

## 1. 评估结论（architect 复核共识，file:line 锚定）

reviewer 原提案的 5 条论断核验：

| reviewer 设想 | 实际 | 锚点 |
|---|---|---|
| "纯函数 walker，Kani 证终止性" | **非纯**：segment chain 是跨异步 DMA 往返的 continuation；终止由跨回调计数器 `walk_segments` + 运行时 `hops>MAX_SGL_SEGMENTS` 检查保证，无单段有界循环纯代码 | [io.rs:557/642](../../src/controller/io.rs)（发 DMA+塞 PendingOp）、[completion.rs:2667/2677](../../src/controller/completion.rs)（计数+界） |
| "CC/CSTS 状态机，Kani 证无非法跃迁" | **非纯转移函数**：`CtrlState` 仅 `Disabled\|Ready` 两变体；真状态是 `csts` 位标志；`write_cc` 决策后调 `enable/disable/process_shutdown`，改十几个 HashMap + 触发 `ns.flush()` I/O | [regs.rs:247](../../src/regs.rs)、[enable.rs:20/91/155/66](../../src/controller/enable.rs) |
| "范围可控，不碰 async" | 当前形态下两目标都**碰 async / effectful**；要 Kani 须**先重构抽纯内核** | —— |
| "grep kani=0 → 从测过了升到证明了" | **虚荣指标**。引入 Kani 的唯一工程理由 = host 可控字节全输入空间无法穷举、采样测试在整数/长度边界最弱 | —— |
| 真正可证目标 | `parse_sgl_list` + `resolve_sgl_address`（`saturating_add`）+ `validate_segment_pointer` + `subtype_to_sc`，均 self-free | [sgl.rs:205/175/144](../../src/sgl.rs)、[io.rs:403](../../src/controller/io.rs) |

## 2. POC 结果（rust-reviewer 复核，无 blocking）

`/tmp/kani_poc/`（独立零依赖 crate，**逐字拷贝**生产函数 + 5 harness）：

- ✅ **5 harness 全 VERIFICATION SUCCESSFUL，0/440 checks failed**，~2s。
- ✅ **拷贝逐字保真**（控制流/边界/算术零漂移；关键常量 `NVME_PAGE_SIZE=4096` 对齐 [regs.rs:42](../../src/regs.rs)）。
- ✅ **unwind 界经反证确认充分（非假阳性）**：故意把 `unwind` 降到 1/2 重跑 → Kani 报 `unwinding assertion FAILURE`，
  证明 Kani 0.67 默认开 unwinding assertion、界不够会 FAIL；`[u8;32]=2 chunk` 恰需 `unwind(3)`（迭代 2 + 终止判定 1）。
- ✅ 断言非恒真废断言（subtype 等价类 / validate 三联不变式都断到点上）。

### 证明范围（**勿夸大**，rust-reviewer 强调）
覆盖 = **无 panic + 成功路径不变式 + subtype_to_sc 等价类**。**不**覆盖"错误路径返回的 SC 值是否选对"
（除 subtype_to_sc）。即 `parse_sgl_list` 在 `from_byte=None` 时返 0x11 而非别的——这类"SC 选错"逻辑 bug 本 POC 不证。
落地文档**禁止**表述成"SGL 解析全正确"。

## 3. 两个承重 finding（决定落地形态）

- **Finding ①**：Kani 0.67（最新）强制用自带 `nightly-2025-11-21 (rustc 1.93)`，**无视 `rust-toolchain.toml` 的 1.95**。
  这是 Kani 固定绑定，非配置项。
- **Finding ②（硬阻断）**：in-crate `cargo kani` 被 workspace MSRV 卡死——`nvme_firmware` 经 path-dep
  `nvme_spec`（`rust-version.workspace=true` ← [根 Cargo.toml:84](../../../../../Cargo.toml) `rust-version="1.95"`）撞墙，
  Kani 的 1.93-nightly < 1.95 → cargo resolver 拒。`--ignore-rust-version` 非 Kani CLI flag，**绕不过**。
  实测：`error: rustc 1.93.0-nightly is not supported ... nvme_spec requires rustc 1.95`。

→ **不可能把 `#[cfg(kani)]` harness 直接挂进现 crate**（只要它传递依赖任何 1.95-pinned crate）。

## 4. 落地路径（若决定做）

唯一可行 + 避免 self-consistent-oracle 漂移的形态：**把 self-free 谓词组抽进零/轻依赖叶子 crate**
（如 `nvme_sgl_parse`，不继承 1.95 MSRV、不依赖 nvme_spec/memmap2/transport），Kani harness 挂那里，
生产 `nvme_firmware` 反过来依赖它 → 生产代码与 harness **共用同一函数，无拷贝漂移**。

- rust-reviewer 核了 6 函数依赖闭包 = **仅裸 `const` + 本地 POD 类型**（`SglType`/`SglDescriptor` 字段只有
  u64/u32/u8/enum，不碰 nvme_spec/memmap2/transport）→ 确认可独立成 crate，解掉 Finding②、无新 MSRV 继承。
- 成本：动 [sgl.rs](../../src/sgl.rs)/[io.rs](../../src/controller/io.rs) 模块边界 + `pub(crate)→pub` 可见性扩大（预期，无安全影响）。

## 5. 价值定位（**诚实**，与姊妹验证 track 对照后下调）

本仓验证生态已很厚，Kani 的目标**与既有覆盖高度重叠**：

| Kani 目标 | 既有覆盖 | Kani 边际价值 |
|---|---|---|
| `parse_sgl_list`/`SglDescriptor::parse` 等纯 parser | proptest 不变量 + cargo-mutants（[sgl.rs](../../src/sgl.rs) 满 mutant-kill 测试）；姊妹 fuzz plan 列为 **Tier 1"边际收益低"** | **低**：proof 升级采样，但对象已充分测试 |
| 异步 segment-chain 终止 / `on_dma_complete` 状态机 | 姊妹 [coverage-guided fuzz plan](./2026-06-13-coverage-guided-fuzz-prp-sgl-admin.md) 的 **Tier 2（金矿）**——fuzzer 可驱动多步 DMA completion 序列 | **Kani 做不了**（self-consistent 陷阱），fuzz 覆盖 |
| 命令语义正确性 | 姊妹 [QEMU 差分测试 track](../../../../../docs/superpowers/plans/2026-06-13-nvme-differential-testing.md) | Kani 不涉及 |

**结论**：Kani 唯一不重叠的增量 = 把"纯 parser 无 panic / `saturating_add` 不溢出"从 proptest **采样**升级为 CBMC
**全输入证明**。这是真增量但边际——proptest+mutants 已给高置信。**故判 LOW**：除非叶子 crate 抽取因
架构/可测试性理由独立值得做，否则不为这点 proof-grade 升级单独投入重构。

## 6. 决策

### 已自决
1. **reviewer 原提案（证异步 walker 终止性 + CC/CSTS 状态机）拒绝**：基于架构误读；异步路径上 Kani 须手写模型
   → 踩 self-consistent-oracle 陷阱（只证模型不证生产）。该路径由姊妹 fuzz plan Tier 2 正确覆盖。
2. **Kani 范围（若做）= 仅 self-free 字节解析谓词组，且必须先抽叶子 crate**。
3. **优先级 = LOW**：目标与已有 proptest+mutants 重叠，边际增量不足以单独驱动重构。

### 待用户拍板 / 待窗口
- 是否值得为 proof-grade 升级 + 顺带的可测试性收益，做 `nvme_sgl_parse` 叶子 crate 抽取（独立于 Kani 也有架构价值）。
- **文档同步待并行会话释放文件**：ROADMAP（当前被并行会话 ` M`，加一行 LOW 项）、`DECISIONS.md`（若决定做，写 ADR：
  "Kani 仅限 leaf-crate 纯 parser，异步归 fuzz"）、`LESSONS.md`（Kani-MSRV-toolchain 教训，见 memory）。
  这些一行项**未写**，等对应文件空出再补——本 doc 是当前唯一落点。

## 7. 复现
```bash
cargo install --locked kani-verifier && cargo kani setup   # 装 0.67.0 + nightly-1.93 + CBMC
cd /tmp/kani_poc && cargo kani                              # 5 harness 全绿
# 反证 unwind 充分性：改 unwind(3)→1 重跑 → unwinding assertion FAILURE
```

## 8. 关键先例 / 交叉引用
- 姊妹 track：[coverage-guided fuzz](./2026-06-13-coverage-guided-fuzz-prp-sgl-admin.md)（Tier 2 async 状态机）、
  [QEMU 差分测试](../../../../../docs/superpowers/plans/2026-06-13-nvme-differential-testing.md)（语义层）。
- 被证函数：[sgl.rs](../../src/sgl.rs)、[controller/io.rs:403](../../src/controller/io.rs)。
- POC 工件：`/tmp/kani_poc/`（throwaway，未入库；落地时迁入叶子 crate `fuzz`-同级的 Kani harness）。
