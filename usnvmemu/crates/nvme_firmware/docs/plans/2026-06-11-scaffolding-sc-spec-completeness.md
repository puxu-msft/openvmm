# scaffolding-SC spec-completeness 收尾 — 执行说明 (handoff)

> 写于 2026-06-11。**目的**：把 `nvme_firmware` 里"SC 常量已锚定 nvme_spec、但对应特性/场景尚未
> emit 该 SC"的 scaffolding-SC 缺口逐个补全。本文是给**新 context 会话**的自包含执行说明——
> 不依赖原对话历史，照本文 + 引用的工程文档即可独立执行。
>
> **已完成（本系列首项，勿重做）**：CMB-SGL 精确 SC（commit `c0cfb419`）——`sgl::parse_sgl_list`
> 现对 SGL Data Block descriptor `sub_type==1`(Offset/CMB-relative) 返 `SGL_INVALID_USE_OF_CMB
> (0x12)`（PSDT=10 segment 路径）。**但留了一致性 follow-up，见下方 §item-0。**

---

## 0. 背景：什么是 scaffolding-SC

`src/cmd.rs` 的 `pub mod sc` 把每个 NVMe status code 锚定到 canonical `nvme_spec::Status`
（`vm/devices/storage/nvme_spec/src/lib.rs`，有 `sc_constants_match_nvme_spec` M1 anchor 守）。
其中 4 个是 **spec-complete scaffolding**：常量在、但对应特性/场景未实现 ⇒ 该 SC 永不被 emit。
`docs/TEST_QUALITY.md` Follow-up 明确"对应特性做时再 emit"。本文逐个落地它们。

| SC（cmd.rs / nvme_spec 值） | 当前状态 | 本文项 |
|---|---|---|
| `SGL_INVALID_USE_OF_CMB` 0x12 | ✅ PSDT=10 segment 路径已 emit (`c0cfb419`)；**io.rs 两路径仍 0x11** | **item-0**（收尾，小） |
| `NAMESPACE_NOT_READY` 0x82 | 无 not-ready 状态（NSTAT.NRDY 硬编码 ready） | **item-1**（需设计，中） |
| `ATOMIC_WRITE_UNIT_EXCEEDED` 0x14 | awun=255 已广告，但无 emit 场景 | **item-2**（需 spec 分析，可能止于"记为 scaffolding"） |
| `BOOT_PARTITION_WRITE_PROHIBITED` 0x11e | 刻意不支持 boot partition（BPSZ=0） | **item-3**（大特性，独立 phase） |

---

## 1. 不可违背的工程纪律（执行者必读）

这些是本仓 + 用户反复强调的硬约束，违背会被 reviewer / 用户打回：

1. **每个用到/新增的 spec 常量必须锚定 `nvme_spec`**（inline `#[cfg(test)]` 断言 `sc::X ==
   nvme_spec::Status::X.0`，或并入 `tests.rs` 的 M1 `sc_constants_match_nvme_spec`）。手填 SC 值
   迟早错——CMB 那次 inline anchor 当场抓到 `0x108`(实为 INVALID_INTERRUPT_VECTOR) 应是 `0x10c`
   的错。见 LESSONS §1/§25。
2. **测试三轴**（`docs/TEST_QUALITY.md`）：① 执行到 ② **独立 oracle**（断言用独立于被测代码的真相源，
   绝不 self-consistent）③ **改坏会红**（每条数据路径测试必 revert-verify：注入 bug 确认测试 FAIL，
   再还原）。self-consistent 当判据 = 反模式（LESSONS §20/§22）。
3. **新内容用中文**（注释/文档/commit message/PR）；原仓库英文不动。
4. **commit 前必过 subagent review**（`ecc:rust-reviewer`，sync 等结果），修 CRITICAL/HIGH/MEDIUM
   再 commit。不可"我验过了"就跳。
5. **不在乎代价，只要长远正确**——别用"改动大/省事"劝阻正确方向；该重构就重构（如 item-0 的共享
   classifier）。
6. **共享工作树纪律**：本 crate 同时有**另一会话**在写 firmware 特性（Abort/MDTS/B-C-phase/PRP-
   chaining/类型重构），活跃域 = `controller/{io,completion,mod,admin}.rs` + `cmd.rs` + `tests.rs`。
   - **commit 时只 `git add` 你自己的文件**（显式列路径，绝不 `git add -A`），避免卷入它的未提交 WIP。
   - 碰它的活跃文件（尤其 io.rs/completion.rs）前先 `git status` 确认 clean，改动最小化 + 快提交；
     若它正在改同文件，**sleep 等其落地**（commit）再动。
   - 别动 `tests.rs`（它的 M1/M4 矩阵）、别动它正在写的特性 handler。
7. **行宽 100**（rustfmt 默认；工作区根 `rustfmt.toml` 无 max_width override）。
8. **构建/测试**：`cd usnvmemu/crates/nvme_firmware`（本 crate 在 root Cargo.toml `exclude`，不能
   `-p`）；`--features openhcl`（e2e 走 openhcl transport 的 `--tcp-addr` 路径；默认 `vfio-user`
   feature 是 Unix-only）；门禁 `scripts/test-quality.sh {check|anchors|mutants}`。

---

## 2. 测试基础设施：OpenHCL 跨进程 e2e harness

`tests/openhcl_pcie_remote_e2e.rs` 是本系列测试的主场（**Linux 可跑，无需 Windows**）：扮 OpenHCL/
VTL2 侧，spawn 真 `nvme_firmware --tcp-addr` bin，经 pcie_remote 协议（pcie_remote_protocol +
wire）驱动。要点：

- **独立 oracle**：断言读 firmware 经 wire 发回的 `CqeResult.status`（**完整 16-bit** SCT|SC，见 M5），
  期望值用**硬编码 spec 字面值**（不 import `crate::cmd::sc`，避免 self-consistent）。
- **结构性限制（重要）**：pump 用 `biased` device-first `select!` + 单线程串行 SDK loop。后果：它
  **每次都把 firmware 在飞 DMA / poll 链 drain 到 settle 才投递下一条 test 命令**。所以某些**异步
  交错**经此 harness **结构性不可达**（如 ring 落在 poll 链 settle 窗口、命令在 DMA 在飞时被 abort/
  delete）——这些精确交错由 **controller 单测（`CaptureTransport` 可任意编排帧）**拥有，e2e 只能打
  "更广、可达"的路径。**别试图在 e2e 里硬造不可达交错**（会写出假测试或 flaky）。
- **DMA-hold 机制**（`Cmd::HoldDma{gpa}`/`ReleaseDma{gpa}`、`driver.hold_dma()/release_dma()`，
  默认惰性零影响）：要造"命令在 DMA 在飞中"的状态（如 abort-in-flight 测试用），hold 该命令的 data
  DMA → 命令滞留 `pending_ios` → 可被命中。**item-1 NS-not-ready 若需"IO 卡在某状态"也可借鉴。**
- SGL 测试 helper：`sgl_desc(addr,len,id_byte)` / `sgl1_last_segment` / `sgl1_segment`；
  `id_byte` 高 nibble=type、低 nibble=sub_type（`0x01` = Data Block + CMB-relative Offset）。
- 现有同类测试可仿照：`openhcl_sgl_*`、`openhcl_delete_io_queue_lifecycle`（backing-file 负 oracle +
  admin fence 去时序）、`openhcl_abort_inflight_io_command`（DMA-hold + 双 oracle）。

> 注：error-SC 的 **unit 覆盖**优先进另一会话的 M4 `error_status_driven_matrix`（`tests.rs`，
> llvm-cov 测不到 e2e 子进程）；但 e2e 验"SC 经真 wire 正确返回"是其独有价值。两层互补。

---

## 3. 逐项执行说明

### item-0 — CMB-SGL 一致性收尾（小，跨域协调）

**缺口**：`c0cfb419` 只让 PSDT=10 segment 路径（`sgl::parse_sgl_list`）对 sub_type=1 返 0x12；
另两条 SGL 路径仍返粗粒度 `SGL_DESCRIPTOR_TYPE_INVALID (0x11)`：
- `src/controller/io.rs:325` `resolve_data_pointers`（inline PSDT=01 single Data Block）。
- `src/controller/io.rs:387` 区 `validate_segment_pointer`（SGL1 指针 / chain continuation）。

同一非法输入（sub_type=1）跨路径返不同 SC = 不一致（功能上都 reject，仅 SC 码不同，且真 driver 永不
触此防御分支——故非紧急、非回归，但 spec-completeness 要求一致）。

**做法（理想形态，防漂移）**：在 `src/sgl.rs` 抽一个共享 classifier，如
`pub(crate) fn subtype_to_sc(sub_type: u8) -> Option<u16>`（0→None=合法；1→`Some(SGL_INVALID_USE_OF_CMB)`；
≥2→`Some(SGL_DESCRIPTOR_TYPE_INVALID)`），让 `parse_sgl_list` + io.rs 两处**都调它**，三处共用一个判
据不再漂移。
- **io.rs 是另一会话活跃域** → 先 `git status` 确认 io.rs clean，改最小（两处 `if sub_type != 0`
  换成调 classifier），快提交；它在改 io.rs 就等。
- **测试**：扩 e2e——加 inline-PSDT=01 CMB-relative + SGL1-pointer CMB-relative 两 case，各断言 wire
  status==0x0012；revert-verify（还原 classifier 调用 → 0x11 → FAIL）。`sgl.rs` 给 classifier 单测。

### item-1 — NS-not-ready (`NAMESPACE_NOT_READY` 0x82)

**缺口**：无 not-ready 状态。`src/controller/admin.rs:88` 把 Identify NS 的 NSTAT.NRDY 硬编码 0
(ready)；Format 期间用的是 `FORMAT_IN_PROGRESS (0x84)`，不是本 SC。

**先做 spec 分析（别硬造场景）**：NVMe spec — NAMESPACE_NOT_READY 用于"NS 已 attach 但当前不可用"
（NSTAT.NRDY=0）。教学 controller 里合理触发点候选（需挑一个 spec-自洽的）：
- 一个新建/刚 attach、尚未 ready 的 NS（NSTAT.NRDY=0），对它的 IO 返 NAMESPACE_NOT_READY，直到某事件
  置 ready；
- 或某异步准备过程（如 sanitize/format 之外的）进行中。
**判据**：选定后必须 NSTAT.NRDY 广告与门逻辑一致（Identify NS 的 NSTAT 反映真状态），别让 ready 广告
与 IO-reject 矛盾。

**做法**：引入 per-NS not-ready 状态 + IO 路径门（`controller/io.rs`/`completion.rs` dispatch 早期，
not-ready → 返 NAMESPACE_NOT_READY）+ Identify NS 的 NSTAT.NRDY 跟随状态。**io.rs/completion.rs 是另一
会话域，协调。**
- **测试**：e2e——把某 NS 置 not-ready（经选定的触发），对它 IO → 断言 wire status==0x0082；ready 后
  IO 正常。revert-verify（去门 → IO 不返 0x82 → FAIL）。unit 进 M4 矩阵更佳（协调另一会话）。

### item-2 — AWUN (`ATOMIC_WRITE_UNIT_EXCEEDED` 0x14)

**先 spec 分析，可能结论是"记为 scaffolding 不 emit"**：`src/cmd.rs:697-698` 广告 `awun=255`/
`awupf=255`（NS 级 `nawun`/`nawupf` 同）。**关键**：AWUN 是**原子性保证**（≤AWUN 的写保证原子），
**不是写大小上限**——标准 NVMe **不**因写 > AWUN 而 reject。所以"size > AWUN → 0x14"是**错的**，别这么写。

`ATOMIC_WRITE_UNIT_EXCEEDED` 的真实 emit 场景较窄（NVMe § 6.4 atomicity / Compare-and-Write 等），
需查 spec + libnvme/kernel 源确认本 controller 是否有任何命令路径**应当**返它。
- 若找到 spec-自洽的场景（如某 fused/atomic 命令超 AWUPF 边界）→ 实现 + emit + 测。
- **若本教学 controller 无干净的 emit 场景** → 诚实结论：保留为 anchored-only scaffolding，在
  `docs/TEST_QUALITY.md` / 代码注释写明"AWUN 是保证非上限，本 controller 无 reject 场景，SC 仅锚定备
  spec-complete"。**别为凑 emit 而 fabricate 一个非 spec 的 reject。**（这本身是合规的"长远正确"结论。）

### item-3 — boot-partition (`BOOT_PARTITION_WRITE_PROHIBITED` 0x11e)

**大特性，建议独立 phase**：`src/controller/mmio.rs:41-48,122-127` 显示 controller **刻意**不支持
boot partition——BPINFO BPSZ=0（不广告）、BPRSEL/BPMBL 是 RW no-op（写入即返，避免 driver poll BRS 挂）。

emit 此 SC ⇒ 须实现整个 **Boot Partition 特性**（NVMe § 8.13 + § 3.1.x 寄存器）：广告 BPSZ>0、支持
Boot Partition Read（driver 写 BPRSEL→controller DMA boot image 到 BPMBL 指向的 buffer、置 BPINFO.BRS）、
以及写保护（对 write-prohibited BP 的写 → BOOT_PARTITION_WRITE_PROHIBITED）。这是实打实的新特性
（当前 stub 化正是为避免 driver 挂），**工作量与一个 Phase 相当**，应拆成自己的 plan + 多 commit。
- **测试**：e2e boot-partition read 往返 + write-prohibited→0x11e；revert-verify。

---

## 4. 推荐顺序 + 验收

**顺序**：item-0（收尾 CMB，小）→ item-1（NS-not-ready，中，设计先行）→ item-2（AWUN，**先分析**，
可能止于记为 scaffolding）→ item-3（boot-partition，大特性，独立 phase）。逐项：分析/设计 → 实现 →
独立-oracle 测试 + revert-verify → `ecc:rust-reviewer` → 只 stage 自己文件 commit。

**每项验收门**（全绿才 commit）：
```bash
cd usnvmemu/crates/nvme_firmware
cargo test --lib --features openhcl            # 含你加的 SC anchor / 单测
cargo test --test openhcl_pcie_remote_e2e --features openhcl
cargo fmt --check && cargo clippy --tests --features openhcl -- -D warnings
# 锚定 + 变异（在干净点）：
scripts/test-quality.sh anchors
```

## 5. 参考（新会话起手先读）

- `docs/TEST_QUALITY.md` —— 三轴 + M1-M6 机制（**最重要**，定义"测试有牙"的判据）。
- `usnvmemu/docs/LESSONS.md`（repo-root-absolute；**注意它在 usnvmemu/docs/ 而非本 crate 的 docs/**）
  —— §1/§25（不手算 SC/offset，锚 canonical）、§20/§22（self-consistent ≠ 正确）、§26（真-host e2e
  不冗余）、§29（async-poll 盲区 + cap 度量）。同理 ROADMAP/PRINCIPLES/DECISIONS 在 `usnvmemu/docs/`。
- `vm/devices/storage/nvme_spec/src/lib.rs:280-341` —— 4 个 SC 的 canonical 值（anchor 源）。
- `src/cmd.rs` `pub mod sc`（260/264/275/288）+ Identify 广告（awun 697 / nawun 858）。
- `src/sgl.rs`（CMB classifier 落点）、`src/controller/io.rs:312-390`（item-0 两 site + NS 门候选）、
  `src/controller/mmio.rs:38-127`（boot-partition 寄存器现状）、`src/controller/admin.rs:88`（NSTAT）。
- 本系列已提交参考实现：`c0cfb419`(CMB-SGL)、`a928f7ba`(队列管理错误处理：同套"加 SC + inline anchor
  + e2e + revert-verify"模式)、`b7eed0ec`(abort-in-flight：DMA-hold + 双 oracle)。
- e2e harness：`tests/openhcl_pcie_remote_e2e.rs`（§2 的限制 + helper + 现有同类测试）。
