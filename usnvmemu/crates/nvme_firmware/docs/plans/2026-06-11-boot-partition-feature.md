# Boot Partition 特性 — 独立 phase plan（scaffolding-SC item-3）

> **✅ 已实现（2026-06-11，commit `4c40d065`）**。最终采用比本 plan 更简单的**只读出厂镜像**
> 模型：`--boot-partition-file` 装载只读 BP → 广告 BPSZ>0 + 服务 Boot Partition Read（BPRSEL
> 触发→DMA 到 BPMBL，BRS 跨回调状态机）+ FW Commit BPID→`BOOT_PARTITION_WRITE_PROHIBITED`
> (0x11e)。**BP-2（经 FW Commit 真写 BP）未做**——BP 只读、write-protected，写尝试即拒，足以
> coherent emit 0x11e。本 plan 下文的 BP-0/1/2/3 分阶是当初设计，保留作设计记录；真实现见
> `SPEC_CONFORMANCE.md` Boot Partition 行 + commit `4c40d065`。若未来要支持 BP 可写（真 BP-2），
> 再按下文 BP-2 做。

---

> 写于 2026-06-11。**目的**：把 `nvme_firmware` 里刻意 stub 掉的 **Boot Partition**（NVMe
> Base spec § 8.13）做成真特性，从而**真 emit** 目前 anchored-only 的 scaffolding-SC
> `BOOT_PARTITION_WRITE_PROHIBITED` (0x011e)。本文是给**新 context 会话**的自包含执行
> 说明——不依赖原对话历史。
>
> **来源**：从 `docs/plans/2026-06-11-scaffolding-sc-spec-completeness.md` §item-3 拆出
> （那条说"boot-partition 是大特性，建议独立 phase"）。其余 3 个 scaffolding-SC（CMB-SGL
> 一致性 item-0、NS-not-ready item-1、AWUN item-2）见原文。

---

## 0. 为什么现在是 stub（先读懂"刻意不做"的理由）

`src/controller/mmio.rs` + `src/regs.rs` 已含 Boot Partition 的**寄存器骨架**，但**刻意不
advertise、不服务**：

- `mmio.rs` `(0x40,_) => 0`：**BPINFO=0**，即 **BPSZ=0**（不广告任何 boot partition）。
- `mmio.rs` `0x44`：**BPRSEL** 是 RW no-op（写入即存 `self.bprsel`，不触发任何读）。
- `mmio.rs` `0x48`：**BPMBL** 是 RW no-op（size-aware 存 `self.bpmbl`，不 DMA）。

**为什么刻意 stub（关键，别重蹈）**：注释里的 **"12 轮 H-Q5"** 教训——曾经 BPSZ=1 让
driver 看到 1 个 partition，但 controller **没有 boot image 服务**，于是 driver 写 BPRSEL
后**poll BPINFO.BRS 永远是 0 = 挂死**。改回 BPSZ=0 后 driver enumerate 阶段直接跳过 boot
partition，避免挂。BPRSEL/BPMBL 仍保留 RW 让 BAR0 register layout 完整（教学读寄存器时
能看见）。

**对本 plan 的硬约束**：**一旦 BPSZ>0（BP-1 广告），就必须真服务 Boot Partition Read 并
把 BPINFO.BRS 推进到 complete(2)/error(3)**，否则立刻退回那个 driver-hang 坑。这是本特性
"要么不做、要做就做全"的根本原因（[[teaching-means-rigor-not-toy]]）。

---

## 1. 不可违背的工程纪律（执行者必读）

与 `docs/plans/2026-06-11-scaffolding-sc-spec-completeness.md` §1 完全一致，照搬要点：

1. **每个 spec 数值（SC / offset / bit 位）必须锚定 canonical 源**（`nvme_spec::Status` /
   `Register` / `offset_of!`），手填迟早错且静默（LESSONS §1/§14/§25）。
   `BOOT_PARTITION_WRITE_PROHIBITED` 已锚 `nvme_spec::Status::BOOT_PARTITION_WRITE_PROHIBITED
   = 0x11e`（cmd.rs M1 `sc_constants_match_nvme_spec`）；BPINFO/BPRSEL/BPMBL 偏移已锚
   `Register::*`（tests.rs M1 `opcode_feature_register_constants_match_nvme_spec`）。
2. **测试三轴**（`docs/TEST_QUALITY.md`）：执行到 + **独立 oracle**（非 self-consistent）+
   **改坏会红**（revert-verify）。
3. **新内容用中文**；原仓库英文不动。**行宽 100**。
4. **commit 前必过 `ecc:rust-reviewer`**（sync 等结果），修 CRITICAL/HIGH/MEDIUM 再 commit。
5. **不在乎代价，只要长远正确**（[[user-values-long-term-correctness-not-roi]]）。
6. **共享工作树纪律**（重要！）：本 crate 同时有**另一会话**在写 firmware 特性（B6b /
   separate-metadata 等），活跃域 = `controller/{io,completion,mod,admin}.rs` + `cmd.rs` +
   `tests.rs`。Boot Partition 特性**也要碰这些文件**（见 §3 各 phase 的落点），所以：
   - commit 时**只 `git add` 你自己的文件**（显式列路径，绝不 `git add -A`）。
   - 碰活跃文件前先 `git status` 确认 clean；它在改同文件就 **sleep 等其 commit 落地**再动。
   - 别动 `tests.rs` 的 M1/M4 矩阵、别动它正在写的特性 handler。
7. **构建/测试**：`cd usnvmemu/crates/nvme_firmware`；`--features openhcl`；门禁
   `scripts/test-quality.sh {check|anchors|mutants}`。

---

## 2. spec 速查（Boot Partitions，NVMe Base § 8.13 + § 3.1.x 寄存器）

Boot Partition = 一块**不需初始化 admin queue / CC.EN** 即可被 host 读出的区域，供 BIOS /
bootloader 在 NVMe 栈起来前读 firmware / boot image。**不是 namespace**（独立寻址、独立存
储）。两个 boot partition（BPID 0 / 1），其一为 active。

### 2.1 寄存器（已在 `regs.rs` / `mmio.rs` 有骨架）

| 寄存器 | 偏移 | 大小 | 关键字段（spec § 3.1.x） |
|---|---|---|---|
| **BPINFO** | 0x40 | 4B RO | bits 14:0 = **BPSZ**（boot partition size，**128 KiB 单位**）；bits 25:24 = **BRS**（Boot Read Status：0=no read / 1=read in progress / 2=read complete / 3=error）；bit 31 = **ABPID**（active boot partition id） |
| **BPRSEL** | 0x44 | 4B RW | bits 9:0 = **BPRSZ**（read size，**4 KiB 单位**）；bits 29:10 = **BPROF**（read offset，4 KiB 单位）；bit 31 = **BPID**（要读哪个 partition）。**写它触发一次 Boot Partition Read** |
| **BPMBL** | 0x48 | 8B RW | **Boot Partition Memory Buffer Location** — host 提供的 guest memory 地址；controller 把 boot partition 内容 DMA 到这里 |

### 2.2 Boot Partition Read 流程（spec § 8.13.1）

1. host 写 **BPMBL** = 目标 guest buffer 地址。
2. host 写 **BPRSEL**（BPRSZ + BPROF + BPID）→ **触发读**。controller 置 BPINFO.BRS=1（in
   progress）。
3. controller 从 boot partition 存储里把 `[BPROF*4KiB, +BPRSZ*4KiB)` 区间 **DMA-write** 到
   BPMBL 指向的 guest buffer。
4. 完成 → BPINFO.BRS=**2**（complete）；失败（越界 / DMA 错）→ BRS=**3**（error）。
5. host **poll BPINFO.BRS** 直到 2/3。

### 2.3 Boot Partition Write 流程（spec § 8.13.2 + Firmware Commit § 5.16）

boot partition **不经普通 IO 写**，而是：
1. **Firmware Image Download**（admin opc 0x11）把 image 字节 stage 进 download buffer。
2. **Firmware Commit**（admin opc 0x10）带 **BPID**（cdw10 bit 31）+ 对应 Commit Action，把
   staged image 写进指定 boot partition 存储。
3. boot partition 可被**写保护 / lock**；对 write-protected boot partition 的 Firmware
   Commit → **BOOT_PARTITION_WRITE_PROHIBITED (0x011e)**。**这就是本 scaffolding-SC 的 emit
   点**（SCT=Command Specific 0x1，SC byte 0x1e）。

> 注：现有 Firmware Commit handler 在 `admin.rs` `FW_COMMIT`（约 1169 行起），cdw10 bit 31 =
> BPID 当前被忽略（无 BP）。BP-2 在此接入。

---

## 3. 逐 phase 执行说明（建议 BP-1 → BP-2 → BP-3）

> SC `BOOT_PARTITION_WRITE_PROHIBITED` 的真 emit 在 **BP-3**，但它依赖 BP-1（广告 + 存储）
> 与 BP-2（写路径）。所以三段顺序做，emit 收尾在 BP-3。

### BP-0 — boot partition 存储模型（设计先行，无独立 commit）

boot partition 不是 namespace，需要独立存储。**候选**（选定后写进本节，[[lesson §15]]"不做
也是决策"同理要把选择写下来）：
- **(A) 每 BP 一个 backing 文件**（如 bin 新增 `--boot-partition-file <path>`，类比
  `--backing-file`）。优点：与 namespace backing 同构、可直读做独立 oracle。**推荐**。
- (B) 内存 buffer（`Vec<u8>`）。简单，但无持久化、无文件级独立 oracle。
- (C) 复用某 namespace 的尾部区域。耦合 namespace，违反"BP 不是 NS"，**否**。

存储大小 = BPSZ × 128 KiB（教学可短化为 1×128 KiB 或更小，但 BPSZ 字段单位是 128 KiB，
广告值要与真实存储一致——**广告⟺implement 一致**，同 [[lesson §25]] SGL advertise 纪律）。

**控制器状态**（`controller/mod.rs` 加字段，注意共享树纪律）：
- boot partition 存储句柄（File / mmap，按 BP-0 选型）。
- `boot_read_status: u8`（BRS：0/1/2/3）——驱动 BPINFO bits 25:24。
- BP 写保护标志 `boot_partition_locked: bool`（per-BPID 若做两 BP）——驱动 BP-3 的 emit。
- 可能的 pending Boot Partition Read DMA accumulator（见 BP-1 异步说明）。

### BP-1 — 广告 + Boot Partition Read happy path

**目标**：BPSZ>0（广告）；BPRSEL 写触发真 DMA 到 BPMBL；BRS 推进到 2。

落点：
- `mmio.rs` `(0x40,_)`：BPINFO 返 `(BPSZ) | (brs<<24) | (abpid<<31)`，**BPSZ 从存储大小派生**
  （非硬编码）。**read 侧 size-aware**（BPINFO 是 4B）。
- `mmio.rs` `0x44`（BPRSEL 写）：从 no-op 改成**触发 Boot Partition Read**——解析 BPRSZ/
  BPROF/BPID，置 BRS=1，发起从 BP 存储 `[BPROF*4KiB, +BPRSZ*4KiB)` 到 `self.bpmbl` 的
  **DMA-write**（经 `DeviceCtx`，**token-异步**，完成在 `on_dma_complete` → 置 BRS=2；
  越界 / 0 长度 → BRS=3，不发 DMA）。**异步状态机**参考既有 PRP/SGL accumulator 模式
  （`pending_*` map + `on_dma_complete` 续完）。⚠️ 别在同步路径里假设 DMA 立即完成
  （[[lesson §29]] token-异步轮询/完成必是跨回调状态机）。
- BRS error 边界：BPROF+BPRSZ 超出 BPSZ → BRS=3（error），不 DMA。

**关键回归防护**：BPSZ>0 后**必须**保证任何合法 BPRSEL 写都能让 BRS 最终到 2/3
（[[本文 §0]] 的 driver-hang 坑）。写个 e2e 真让 driver-风格的 poll 收敛。

**测试（BP-1）**：
- e2e `openhcl_boot_partition_read_roundtrip`：seed BP 存储已知 pattern（独立写 backing /
  buffer）→ 驱动写 BPMBL + BPRSEL 触发读 → poll BPINFO.BRS 到 2 → 读 BPMBL 指向的 guest
  buffer，**独立 oracle = 直读 BP backing 的同区间**，断言 byte-equal（pattern 须 **per-4KiB
  位置唯一**，避免 [[lesson §23]] uniform-pattern 掩盖偏移 bug）。
- revert-verify：把 BPRSEL handler 改回 no-op（不发 DMA / 不置 BRS）→ poll BRS 永 0 →
  测试超时 / FAIL。
- 单测：BPINFO 字段打包（BPSZ/BRS/ABPID 位段）+ BRS error 边界（超界→3）。

### BP-2 — Boot Partition Write（经 Firmware Commit + BPID）

**目标**：FW Image Download stage image → FW Commit 带 BPID 把 image 写进 BP 存储。

落点：`admin.rs` `FW_COMMIT`（约 1169）——cdw10 bit 31 = BPID 当前忽略。BP-2 让 BPID=1（或对
应 Commit Action）把 `self.fw_download_buf` 内容写进选定 boot partition 存储。Commit Action
与 BPID 的合法组合见 spec § 5.16 表（BP 写用特定 CA）。

**测试（BP-2）**：
- e2e：FW Download 一段 image → FW Commit BPID → 之后 Boot Partition Read 同区间回读 ==
  下载的 image（独立 oracle = 直读 BP backing）。revert-verify：commit 不写存储 → 回读不符。

### BP-3 — 写保护 + emit `BOOT_PARTITION_WRITE_PROHIBITED` (0x011e)【SC 收尾】

**目标**：boot partition 可被 lock（写保护）；对 locked BP 的 Firmware Commit →
`BOOT_PARTITION_WRITE_PROHIBITED`。**这是本 scaffolding-SC 的真 emit 点**。

落点：
- 写保护来源（选一，spec-自洽）：BP lock 经 Set Features / 某 BPINFO 写位 / 配置（bin flag
  `--boot-partition-locked`）。spec § 8.13.2 的 boot partition write protection 语义为准——
  **先查 spec 确认 lock 如何被置位**（[[lesson §12]] WebFetch → 源验证 → anchor），别臆造。
- `admin.rs` `FW_COMMIT` 的 BP-2 写路径前加门：目标 BP 被 lock → 返
  `sc::BOOT_PARTITION_WRITE_PROHIBITED`（已锚 0x11e，SCT=Command Specific 经 `sf_of`/full
  status 自动派生，**别手填 SCT**，[[lesson §27]]）。

**测试（BP-3）**：
- e2e `openhcl_boot_partition_write_prohibited`：lock BP → FW Commit BPID 写它 → 断言 firmware
  经真 wire 回的**完整 16-bit CQE status == 0x011e**（独立 oracle = 硬编码 spec 字面值，
  非 import `sc::`）。revert-verify：去掉 lock 门 → status 变 success/别的 → FAIL。
- unit：进 `tests.rs` M4 `error_status_driven_matrix`（driven 断言完整 status；**协调另一会话**，
  别擅改 M4 矩阵——按原 handoff doc §2 注，error-SC unit 覆盖优先进 M4，但 tests.rs 是其
  活跃域，需协调或留 e2e 为主）。

---

## 4. 验收门（每 phase 全绿才 commit）

```bash
cd usnvmemu/crates/nvme_firmware
cargo test --lib --features openhcl
cargo test --test openhcl_pcie_remote_e2e --features openhcl
cargo fmt --check && cargo clippy --tests --features openhcl -- -D warnings
scripts/test-quality.sh anchors        # M1：含 BP 寄存器偏移 + SC 值锚定
```

逐 phase：分析/设计 → 实现 → 独立-oracle 测试 + revert-verify → `ecc:rust-reviewer`（sync 等
结果，修 C/H/M）→ **只 stage 自己文件** commit。

## 5. 参考（新会话起手先读）

- `docs/plans/2026-06-11-scaffolding-sc-spec-completeness.md` —— 母 plan（item-0/1/2/3 全景 +
  §1 工程纪律 + §2 e2e harness 结构性限制）。
- `docs/TEST_QUALITY.md` —— 三轴 + M1-M6（测试有牙判据）。
- `usnvmemu/docs/LESSONS.md` —— §1/§14/§25（不手填 spec 值）、§12（WebFetch→源验证→anchor）、
  §27（SCT footgun：别手填 SCT）、§29（token-异步轮询=跨回调状态机）、§15（"不做"也写下来）。
- `vm/devices/storage/nvme_spec/src/lib.rs:341` —— `BOOT_PARTITION_WRITE_PROHIBITED = 0x11e`
  （anchor 源）。
- `src/controller/mmio.rs:38-159` —— BP 寄存器现状（BPINFO=0 stub + BPRSEL/BPMBL no-op）。
- `src/regs.rs:89-93` —— BPINFO/BPRSEL/BPMBL 偏移 + 字段注释（已 M1 anchor）。
- `src/controller/admin.rs` `FW_COMMIT`（约 1169）—— BP-2/BP-3 接入点（cdw10 bit31=BPID）。
- `src/cmd.rs:288` —— `BOOT_PARTITION_WRITE_PROHIBITED: u16 = 0x011e`（已锚）。
- **spec**：NVMe Base § 8.13（Boot Partitions）+ § 3.1.x（BPINFO/BPRSEL/BPMBL）+ § 5.16
  （Firmware Commit BPID）。写新 wire / lock 语义前先 WebFetch kernel/libnvme 源核对
  （[[lesson §12]]），别只凭 spec PDF 臆测。
