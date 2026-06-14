# §22 vfio-user mmap DMA fuzz target（唯一 unsafe/内存安全面）

> 状态：**✅ 已落地（2026-06-14）**——两 entry point 全覆盖：
> **§22 A**（`fuzz_map_dma_fd`，commit ef2c97ec0）直驱 `map_dma_fd`，双 oracle（no-SIGBUS + fstat 决策），
> 真 coverage-guided 4,081,078 runs 全绿；承重假设 `_proof_sigbus_catchable` 确定性证 ASan 能 catch SIGBUS（CI exclude）。
> **§22 B**（`fuzz_handle_dma_map`，commit 1dd5531b1）经真 socketpair+SCM_RIGHTS+wire Message 驱 pub
> `handle_dma_map` + 新 `__fuzz_mmap_access` 直驱 mmap_read/write，补 A 漏的 wire 解析 + checked_add 溢出 +
> mmap region-relative 边界（SAFETY #3）3 缺口；真 coverage-guided+ASan 1,234,139 runs 0 crash（边界 robust）。
> 日期：2026-06-14。归属：`vfio_user_transport`。**架构/约定见 [FUZZING.md](/usnvmemu/docs/FUZZING.md)**。
> 来源：fuzz 子项目优先级②（B 类内存安全，区别于 nvme_firmware 的 A 类存活性 DoS）。

## 0. 一句话

给 `vfio_user_transport::dma::map_dma_fd`（§22 的 SIGBUS 面，整个系统**唯一 unsafe 代码**）建
coverage-guided fuzz：fuzzer 控 `(memfd 真实大小, offset, size, writeable)` → map → 触碰每页 →
**oracle = 不 SIGBUS（ASan 兜）+ accept/reject 决策匹配独立 fstat 判据**。

## 1. 背景：§22 是什么、为什么最高安全价值

LESSON §22（CRITICAL）：`map_dma_fd(fd, offset, size, writeable)` 把 client 经 SCM_RIGHTS 传来的
memfd 按声明 `size` 做 mmap。`mmap(2)` 只要求 offset 页对齐、**不校验 `offset+size ≤ fd 真实
大小`**；映射超出真实页的区间**成功**返回合法 VMA，但 memcpy 触碰无 backing 的页 → 内核投
**SIGBUS 杀整个进程**。client 发"大 size + 小 memfd"即 DoS。所有 Rust slice 边界检查无效（越界
在内核页层，非 slice 层）。

**修（review C-1，dma.rs:463-482）**：mmap 前 `fstat(fd)` 取真实 `st_size`，校验 `offset+size ≤
st_size`（仅普通文件；字符设备如 mshv_vtl_low st_size=0 跳过，由驱动 mmap handler enforce，W5a）。

**为什么最高安全价值**：这是全系统**唯一 unsafe 面**（lib `#![deny(unsafe_code)]`，仅
`map_dma_fd` 的 mmap + `framing` 的 fd 两处例外）。A 类（nvme_firmware 链自环）是**存活性**
（线程挂死）；§22 是**内存安全**（进程被杀 + 潜在越界）——后果重一档。目前 **0 fuzz 覆盖**。

## 2. POC 结论（承重假设已验，`fuzz/fuzz_sigbus_poc.rs`）

§22-fuzz 用"SIGBUS 不发生"当 oracle 的**最关键承重假设 = libfuzzer 能否 catch SIGBUS**。POC
确定性触发 §22（memfd ftruncate 1 页 → mmap 2 页 → 写第 2 页无 backing）→ cargo-fuzz 跑：

```
==ERROR: AddressSanitizer: BUS on unknown address ... in trigger_sigbus fuzz_sigbus_poc.rs:48:5
SUMMARY: AddressSanitizer: BUS ... in trigger_sigbus
Test unit written to .../crash-...
```

✅ **ASan 捕获 SIGBUS（BUS）+ 精确源码行 + crash artifact + 非零退出** → SIGBUS-as-oracle **可行**。
✅ fuzz crate（依赖 vfio_user_transport，nested workspace）stable 可 build。
✅ harness 能 `memfd_create` + `ftruncate` + `mmap`（真 target 的基础动作）。

## 3. 可达性 finding（决定 target 设计）

§22 unsafe 核 **不可外部直驱**：`map_dma_fd` **私有**、`mmap_read`/`mmap_write` **`pub(crate)`**；
唯一 pub wire 入口是 `handle_dma_map`（需 `UnixStream` + SCM_RIGHTS fd + wire `Message`）。故两条路：

- **A（聚焦，首期）**：`fuzzing` cargo-feature 暴露**一个合并 helper**直驱 §22 核（详见 §4）。
  隔离 unsafe 逻辑、快、信噪比高。**A 的已知覆盖缺口（architect 复核）**：① 不覆盖 `mmap_read`/
  `mmap_write` 的 **region-relative 边界（SAFETY #3，dma.rs:266/288 用 `bytes.len()`）**——A 只验
  `map_dma_fd` 的 backing 完整性；② 不覆盖 `handle_dma_map` 的 wire 预处理（dma.rs:376 `size==0`/
  dma.rs:386 `addr+size 溢出` 校验在 wire 层、A 绕过）；③ `offset_pages` 受限 → 难触 `checked_add`
  溢出 reject 分支。
- **B（wire-level，planned 必要 follow-up——非 optional）**：驱 pub `handle_dma_map`（socketpair +
  SCM_RIGHTS + wire Message）+ 经 session `dma_read` 触发 `mmap_read`。**专补 A 的上述 3 缺口**
  （region-relative 边界 / wire 解析 / 溢出分支）。重、慢，二期做。

## 4. 方案（A：fuzzing-feature 聚焦 target）

### 4.1 fuzzing-feature 暴露（vfio_user_transport crate）—— 单一合并 helper（architect 建议）
- `Cargo.toml` 加 `[features] fuzzing = []`（空 feature）。
- `dma.rs`：暴露**一个自包含 helper**（**不**单独暴露 `map_dma_fd`/`mmap_read`/`DmaMmap`，最小化
  unsafe 暴露面）：`#[cfg(feature="fuzzing")] pub fn __fuzz_map_and_touch(fd: BorrowedFd, offset: u64,
  size: u64, writeable: bool) -> io::Result<()>` —— 内部 `map_dma_fd(...)?` 后**逐页 volatile-touch**
  返 `Ok(())`。**关键（architect MEDIUM-1）**：touch 必须 `core::ptr::read_volatile` + `black_box`——
  无副作用的普通"读 1 字节"会被优化器 DCE 消除 → SIGBUS 永不触发 → oracle 静默失效（POC 用"写"
  有副作用才侥幸触发；本 helper 用读须防 DCE）。touch 每页首字节 `[0, PAGE, 2*PAGE, ...]` + 末字节
  （防 size 非页整除漏末页）。生产不开 feature → 零增量公共面。hunk 隔离提交。

### 4.2 fuzz target `fuzz_map_dma_fd`
结构感知输入（`arbitrary`），**采样压到 accept/reject 边界 + 可达溢出分支**（architect MEDIUM-2）：
```rust
struct MapInput {
    backing_pages: u8,   // memfd ftruncate 到 N 页（≤64，避免 OOM）
    offset_pages: u16,   // mmap offset 页数（× PAGE 保证页对齐）
    size_delta: i32,     // 实际 size = max(0, (backing×PAGE - offset×PAGE) + size_delta)：压到边界
    writeable: bool,
    overflow_probe: bool, // true → offset 取近 u64::MAX，触 checked_add 溢出 reject 分支
}
```
驱动：① `memfd_create`+`ftruncate(backing_pages × PAGE)`；② 算 offset/size（overflow_probe 时
offset=u64::MAX-x）；③ `__fuzz_map_and_touch(fd, offset, size, writeable)`。

### 4.3 oracle（双判据，按 architect HIGH-1/HIGH-2 修正）
1. **no-SIGBUS（ASan 兜，主 oracle）**：helper 返 Ok 即已 volatile-touch 完每页、**绝不 SIGBUS**；
   越界输入被 accept 却 touch SIGBUS → ASan BUS crash。**覆盖边界（收紧，HIGH-2）**：本 oracle 只守
   **`map_dma_fd` 的 backing 完整性**（§22 SIGBUS 本体）；**不**守 `mmap_read/write` 的 region-relative
   边界（SAFETY #3，dma.rs:266/288 用 `bytes.len()`）——那条留 B。
2. **决策 oracle（独立判据，按 HIGH-1 与被测逐分支对齐）**：
   - **前提（显式）**：harness 恒造 memfd ⟹ `is_regular==true`（字符设备分支物理不可达，见 §4.4）。
   - **判据**：reject ⟺ `offset.checked_add(size).is_none()`（溢出）**∨**（`is_regular` 恒真下）
     `offset+size > backing_bytes`。harness 用**已知 ftruncate 值** `backing_bytes` 自算（独立于被测的
     `fstat`——两个数据来源）。断言 `result.is_ok() == !reject`。
   - **`size==0` 单独短路（HIGH-1）**：`map_dma_fd` 不挡 size=0（挡它的是 wire 层 dma.rs:376），但
     `memmap2 opts.len(0)` 可能 Err。故 `size==0` **跳过 is_ok 断言**，仅断言"不 SIGBUS / 不 panic"。

### 4.4 明确不在范围（已接受暴露 / A 物理不可达，避免误报洪水）
- **TOCTOU shrink（SAFETY #4，dma.rs:499-502）**：map 后 `ftruncate` 缩小 → 事后 touch SIGBUS，是 §22
  SAFETY **显式记录的已接受教学边界**（生产须 `F_SEAL_SHRINK`）。harness **不 shrink after map**，不当
  finding。（未来加 F_SEAL_SHRINK 硬化时再补"shrink 被 seal 拒"正向回归。）
- **字符设备路径**（st_size=0 跳过上界，dma.rs:471-474）：fuzz 进程**无法构造**真 guest-RAM 字符设备
  fd；memfd 恒 `S_IFREG`，该分支**物理不可达**（非选择）——故 §4.3 决策 oracle 依赖 is_regular 恒真
  是合法前提。
- **`mmap_read/write` region-relative 边界（SAFETY #3）+ wire 解析（dma.rs:376/386 的 size=0/溢出
  校验）**：A 覆盖不到，归 B。

### 4.5 CI / corpus
- fuzz crate Cargo.toml 加 `[[bin]] fuzz_map_dma_fd`;接 `usnvmemu.yml` 的 `fuzz build + smoke` 步
  （新 crate 需在 CI 加一段，或扩展现有 fuzz 步覆盖 vfio_user_transport/fuzz）。
- 真 coverage-guided run 走 `cargo +nightly fuzz run`（nightly+ASan 已装，不动 1.95 pin）。

## 5. 执行步骤
1. `fuzzing` feature + **单一** `__fuzz_map_and_touch` helper 暴露（dma.rs，volatile-touch 防 DCE，
   hunk 隔离）。
2. fuzz crate Cargo.toml：`nix = { version="0.30", features=["mman","fs"] }`（memfd+ftruncate 必需，
   architect 补）+ memmap2 + arbitrary + libfuzzer-sys + xtask_fuzz；`[[bin]] fuzz_map_dma_fd`。
3. `fuzz_map_dma_fd` target（结构化输入压边界 + overflow_probe 维度，§4.2）。POC `fuzz_sigbus_poc`
   **改名 `_proof_sigbus_catchable` + 注释"故意崩、勿入 CI"**，CI 配置显式 exclude（它故意崩）。
4. build（stable + feature）+ 真 coverage-guided run（nightly+ASan）验 no-SIGBUS + 决策 oracle。
5. rust-reviewer（重点：feature-gate 隔离、单 helper unsafe 暴露面、volatile 防 DCE、oracle 逐分支对齐）。
6. CI 接入（usnvmemu.yml 加 vfio_user_transport/fuzz 段，exclude proof target）+ commit（hunk 隔离）。
7. 文档同步：本 plan 标完成；B（wire-level）作 planned follow-up 入 ROADMAP/台账类文档。

## 6. architect review 已完成（2026-06-14）—— 裁定 WARN，已纳入修订
- **HIGH-1（oracle 公式）** → §4.3 决策 oracle 改为与被测逐分支对齐（溢出 ∨ is_regular&&越界）+
  size=0 单独短路 + is_regular 恒真显式前提。
- **HIGH-2（A 覆盖声称夸大）** → §4.3 oracle① 收紧为"只守 map_dma_fd backing 完整性、不守 SAFETY #3"；
  **B 从 optional 上调为 planned 必要 follow-up**（补 region 边界/wire 解析/溢出分支 3 缺口，§3）。
- **MEDIUM-1（DCE 致 oracle 静默失效）** → §4.1 helper 用 `read_volatile`+`black_box`。**最致命的一条**
  （POC 用写侥幸过，target 用读会被消除）。
- **MEDIUM-2（输入采样）** → §4.2 size 压到 backing±δ 边界 + overflow_probe 维度可达溢出分支。
- **暴露面** → §4.1 改为**单一** `__fuzz_map_and_touch` helper（不暴露 map_dma_fd/mmap_read/DmaMmap）。
- **nix feature** → §5 步骤 2 补 `["mman","fs"]`。
- **POC 去留** → 留作"SIGBUS-catchable 证明"，改名 `_proof_*` + CI exclude（§5 步骤 3）。
