# FUZZING —— usnvmemu coverage-guided fuzz 体系（架构 + how-to）

> 本文是 usnvmemu fuzz 子项目的**单一入口**：心智模型、约定、CI、以及"如何加一个 fuzz target"。
> 对标 `HOW_TO_ADD_TRANSPORT.md`。覆盖现状/已挖 finding 见各 crate 的 `docs/plans/2026-06-*-*fuzz*.md`
> 与 `nvme_firmware/docs/DMA_COMPLETION_INVARIANTS.md`（A 类台账）；本文是**约定与方法**，不是进度。

## 0. 心智模型：三类攻击面 × 各自 oracle

fuzz 打的是**host/peer 控制的不可信输入触达的解析/状态机**。usnvmemu 有三类：

| 类 | 是什么 | 风险 | crate | oracle 策略 |
|---|---|---|---|---|
| **A. 存活性 DoS** | controller DMA-completion 链（SGL/PRP-list segment 自环、shadow-poll 自馈、CMB-drain 级联）| guest 构造自环 → host 线程无限自旋挂死 | `nvme_firmware` | observable + `__fuzz_invariants` op-表-drain（无 op_id 泄漏，CFS 例外）+ hop 守卫不被绕过 |
| **B. 内存安全** | vfio-user mmap 零拷贝（唯一 `unsafe` 面）| client 谎报 size → 触碰无 backing 页 SIGBUS 杀进程 | `vfio_user_transport` | no-SIGBUS（ASan 兜）+ 决策 oracle（map accept/reject == 独立 fstat 判据）|
| **C. wire parser** | NVMe-oF TCP PDU framing / H2CData reassembly / DH-HMAC-CHAP auth | attacker 控 wire 字节 → 长度算术下溢/OOB/未认证 DoS | `nvme_of_tcp_target` | observable-only（不 panic/abort/OOB/hang）|

**已挖的真 finding**（证明这套有牙）：truncated-read DoS（completion.rs，A 类）、hlen<8 整数下溢 DoS
（framing.rs，C 类）、§22 SIGBUS 在 4M runs 下被 fstat 修守住（B 类）。

## 1. 承重约定（加 target 前必读）

### 1.1 nested-workspace 布局（**不能用 `.workspace = true`**）
每个 fuzz crate 在 `<crate>/fuzz/`，**自带 `[workspace]` 空表**脱离根 + **显式 pin** 所有键
（edition/rust-version/`xtask_fuzz`/`arbitrary`/`libfuzzer-sys` 的相对 path）。
**根因**：usnvmemu 全 crate 在根 `Cargo.toml` 的 `[workspace.exclude]`、是 standalone，所以 fuzz crate
没有 workspace 继承源——照抄 upstream `nvme_driver/fuzz` 的 `.workspace = true` 会**第一步 cargo build
即解析失败**。这是 Cargo 硬约束，不是设计选择；自包含可独立 `cargo build` 是优点，别为消重发明 symlink。

### 1.2 纯公共 API 驱动（无 fuzz-only 驱动 hook）
harness 经**公共面**驱动 SUT，不加 fuzz-only 的*驱动*钩子：
- A 类：`NvmeController::open` → `nvme_io_dispatch`/`nvme_admin_dispatch`/`mmio_write`(doorbell) →
  `CaptureTransport` 观察 `DmaRead{token}` → `PcieDevice::on_dma_complete(token,ok,<fuzzer字节>)`。
- C 类：`read_pdu`(泛型 `<R:Read>` + `Cursor`) / `read_pdu_async`(`Cursor`+`block_on`) / `accept_pdu` /
  `parse_*`，全 `pub fn` 直喂字节。

### 1.3 `fuzzing` cargo-feature 访问器（暴露**内部 oracle**，非驱动）
要断言**内部不变量**（生产不可见的私有态）时，用 `fuzzing` 空 feature gate 一个 `#[cfg(feature="fuzzing")]
pub fn __fuzz_*` 访问器（先例 upstream `firmware_uefi`）：
- `nvme_firmware`：`__fuzz_invariants() -> FuzzInvariants`（op-表长度 + 守卫常量 + CFS），`impl NvmeController` 末尾（mod.rs 尾部）。
- `vfio_user_transport`：`__fuzz_map_and_touch(fd,offset,size,writeable)`（map_dma_fd + 逐页 volatile-touch）。
**生产构建不开 feature → 零额外公共面**。命名统一 `__fuzz_` 前缀。fuzz crate 依赖写 `features=["fuzzing"]`。
**只在需要内部断言时加**——纯公共枚举可观测的（如 reassembler 的 `AcceptOutcome`）不需要访问器。

### 1.4 observable-only oracle + **拒绝语义自洽断言**
基线 oracle = **不 panic / abort(OOM) / hang**（libfuzzer+ASan 兜，结果丢弃）。
**绝不**加"解出的字段彼此自洽"之类语义断言——那用被测**同一套假设**自证，是 self-consistent 陷阱
（见 `LESSONS`/memory `review-not-optional-self-consistent-trap`），且把 fuzz 从"找崩溃"偏成"验语义"。
内部不变量断言（1.3 的访问器）是允许的，因为判据来自**独立**来源（如 harness 已知的 ftruncate 值 vs
被测的 fstat）。

### 1.5 ITER_SAFETY_CAP 校准（backstop ≠ 正常 oracle）
harness 里的迭代安全上限**设在 SUT 真实守卫上限之上**，只为 catch"守卫失效→无限 fetch"。
**必须 ≥ SUT 合法最坏**：admin Get Log Page 不受 MDTS 限、合法最坏 16×511≈8176 DMA → cap=20000；
shadow-poll cap=65536 → backstop 200000。**backstop 触发 ≠ SUT bug**：先 instrument+抬 cap 诊断
（看 total 是否自然收尾、表是否 drain），再判 real-vs-artifact（实例：admin 4137 DMA 误报，见
`pdu-framing-fuzz` 反面 / memory `fuzz-the-contract-not-current-impl`）。

### 1.6 驱动循环用 **O(N) cursor+worklist**（不用 O(N²) find_map）
drain 循环服务所有未服务 DMA 时，用 O(N) cursor 推进（记已服务 token），**不用** `cap.events().find_map`
每轮重扫（O(N²)）。**判据**：`CaptureTransport` 事件 append-only + token 单调唯一不回收 → O(N) 恒正确；
O(N²) 只在 cap 小时"碰巧够快"，cap 一大（如 shadow 65536）合法撞-cap 会看起来像 libfuzzer hang（假阳）。
firmware fuzz crate 提供共享 `common::drive_dma_drain`，新 A 类 target 复用它。

### 1.7 `_proof_*` target 必须 CI exclude
`vfio_user_transport/fuzz/_proof_sigbus_catchable` **故意 SIGBUS**（证"libfuzzer+ASan 能 catch SIGBUS"
这一承重假设）。CI 两层都**机制化排除**它、不靠人记得别列：stable smoke 解析 Cargo.toml 中 `fuzz_*`
开头的 `name =` 行（`[[bin]].name` 约定；`_proof_*` 不带前缀 → 天然漏掉），nightly 用
`cargo fuzz list | grep -v '^_proof'`。
加任何"故意崩的承重假设证明"都用 `_proof_` 前缀（既给人读、又给两层过滤器吃）。

## 2. 双层 CI

- **`usnvmemu.yml` 的 `fuzz build + smoke` 步**（stable 1.95，per-PR）：`cargo build` + 定 seed **blind**
  smoke（无 coverage instrument），只防 harness bit-rot / SUT 公共 API 漂移即编译炸。target 列表
  **自动发现**（`grep -oP '^name = "\Kfuzz_[^"]+' Cargo.toml`——轻量、不给 per-PR gate 装 cargo-fuzz；
  `fuzz_*` 约定使 `_proof_*` 天然排除）。加 target 无需改 CI。
- **`usnvmemu-fuzz-nightly.yml`**（nightly + cargo-fuzz，定时）：真 coverage-guided + ASan，`cargo fuzz list
  | grep -v '^_proof'` 自动发现全 target、corpus 经 `actions/cache` 跨夜累积 + `cmin` 界增长、crash 上传 artifact。
- 真覆盖跑只在 `cargo +nightly fuzz run`；**不动 `rust-toolchain.toml` 1.95 pin**（restore-packages 用
  plain `cargo xflowey` 走 1.95，flowey 在 nightly 撞 `Path::absolute`）。本地真跑需 `rustup toolchain
  install nightly` + `cargo install cargo-fuzz`。

## 3. 如何加一个 fuzz target（分步）

1. **判要不要加**：这条路径是否 host/peer-控不可信输入触达的解析/状态机？已被确定性测试守住的不算 fuzz
   金矿（fuzz 是叠加，见 `DMA_COMPLETION_INVARIANTS` 的"为什么不加 SGL-in-CMB seed"决策范本）。
2. **承重假设先验**：能否纯公共 API 驱动？需要内部断言→加 `fuzzing` feature 访问器（1.3）；SUT 硬绑具体
   类型（如 `read_pdu(&mut TcpStream)`）→ 评估泛型化重构（behavior-preserving）vs socketpair。不确定就先
   POC（如 `_proof_sigbus_catchable` 证 SIGBUS 可 catch）。
3. **大改动**（生产重构 / 新攻击面）：plan → architect review → 实现；写进 `<crate>/docs/plans/`。
4. 在 `<crate>/fuzz/fuzz_<name>.rs` 写 target：首行 `xtask_fuzz::init_tracing_if_repro()`；模块注释写清
   "打什么 / 触发形状 / oracle / 为什么这样"（自包含微设计文档，刻意偏离约定时显式注明理由）。
5. `<crate>/fuzz/Cargo.toml` 加 `[[bin]]`（`test/doc/doctest=false`）。
6. **build（stable）+ 真 coverage-guided run（`cargo +nightly fuzz run`）**——跑出 crash 先诊断 real-vs-artifact
   （1.5），真 finding 报对应 owner（A 类→silver-heron 域 / C 类 framing→本 crate）。
7. rust-reviewer 复核 harness（无 false-positive、oracle 正确、限幅）。
8. CI 自动覆盖（无需改 CI——stable smoke 解析 Cargo.toml `fuzz_*`、nightly `cargo fuzz list` 都自动发现；
   `_proof_*` 两层都自动排除）。
9. commit：**共享文件 filtered-patch 隔离**（`git apply --cached` 自己 hunk + 裸 commit；提交前
   `git diff HEAD -- <file>` 重核 hunk 归属；`-F` 写 message 避反引号被 bash 当命令替换；见 memory
   `git-commit-shared-index-multisession`）。

## 4. 可迁移教训（这套体系沉淀的）
- **fuzz the contract not the current impl**：输入空间按 trait/协议契约定，契约比实现宽的那部分藏 latent
  缺口（hlen<8 / truncated-read 都是合法 peer 不会发、但契约允许的）。
- **backstop ≠ finding**：harness safety cap 触发先诊断（1.5）。
- **shared-tree commit hygiene**：pathspec / `apply --cached` / `--amend` 都吞整个 index 或工作树版；
  filtered-patch + 裸 commit + 提交前重核归属。
