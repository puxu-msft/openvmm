# Coverage-guided fuzzing：PRP/SGL/admin 指针追逐路径

> 状态：**方案已定，execution-ready**（POC 验承重假设 + architect 复核 v2：B-1 crate 布局已修、
> W-1/W-2/W-3/W-4 已纳入）。日期：2026-06-13。归属：`nvme_firmware`。
> 来源：reviewer 提议 + architect 两轮复核 + POC（`tests/fuzz_poc_sgl_chain.rs`，已绿）。

## 0. 一句话

把 controller 的 **SGL segment chain / PRP-list chain / admin dispatch** 这三段
host-controlled 指针追逐代码，用仓库原生 `xtask fuzz`（libfuzzer + arbitrary）做
**结构感知、coverage-guided** fuzz；oracle = **不 panic + 迭代有界 + 守卫不被绕过**。
Tier 1 纯 parser 已被 proptest+cargo-mutants 覆盖透，仅作轻量补充，不是主菜。

## 1. 背景与判断（已核验，file:line 锚定）

- 现状：`usnvmemu/` **零** coverage-guided fuzz；已有 proptest（[pi.rs](../../src/pi.rs)、
  [controller/tests.rs](../../src/controller/tests.rs)）+ cargo-mutants（[sgl.rs](../../src/sgl.rs) 满是 mutant-kill 测试）。
- 指针追逐代码确实存在：SGL 递归段 fetch + 自环上限 [completion.rs:2677](../../src/controller/completion.rs)；
  PRP-list chain 上限 [mod.rs:763](../../src/controller/mod.rs)；admin dispatch [admin.rs:101](../../src/controller/admin.rs)。
- 仓库**有原生 fuzz 框架** `xtask fuzz`（`xtask_fuzz` = workspace 依赖 [根 Cargo.toml:88]）；
  模板先例 `vm/devices/storage/disk_nvme/nvme_driver/fuzz/`。**不引裸 cargo-fuzz**。

### 价值分层（architect 复核确认）
- **Tier 2（金矿，主菜）**：`on_dma_complete_impl` 状态机——递归重入、hop guard off-by-one、
  跟随攻击者控制的 DMA 返回字节走链、fragment 累积算术、`op_id` 表生命周期。**有跨调用可变状态**
  （`pending_ios`/`sgl_ops`/`prp_list_ops`/`walk_offset`/`walk_segments`），fuzzer 可构造
  **多步 DMA completion 序列**驱动状态机进入 proptest 单步模型到不了的组合态。
- **Tier 1（轻量补充）**：`sgl::parse_sgl_list`/`SglDescriptor::parse`/`prp::*` 纯函数，
  已被 proptest 不变量 + mutant-kill 覆盖，coverage-guided 边际收益低。

### "fit" 纠偏
- **§22（unsafe SAFETY 独立 oracle）fit 错位**：咬人的 mmap SIGBUS 在 `vfio_user_transport`，
  parser 是**纯 safe Rust**（sgl/prp 无 unsafe）。fuzz parser **不**强化 §22。要兑现 §22 须
  **另开**打 transport mmap 的 target（fstat oracle）——成本模型不同（真 fd / 真 SIGBUS /
  需 process isolation），列为**独立可选 target #5**，不与 Tier 2 混在一个 target。
- **§14（手算 offset 表）fit 弱**：`Sqe` 走 `zerocopy::FromBytes`（[cmd.rs:340](../../src/cmd.rs)），
  layout 由 derive + `offset_of!` anchor 保证；手算 offset 坑已消除。PRP 分段算术（prp.rs）仍
  弱相关，但已被不变量 proptest 守。**§14 不作为本方案理由**。

## 2. POC 结论（承重假设已验，`tests/fuzz_poc_sgl_chain.rs` 已绿）

承重假设：「外部 crate 能否经**公共 API** + `CaptureTransport` 同步驱动 SGL chain walker
并喂任意字节」。POC 用外部 integration test（可见性 == fuzz crate）证实：

1. ✅ **公共 API 可达，无需 fuzz-only hook**：`NvmeController::open` →
   `nvme_io_dispatch`（PSDT=10 SGL READ）→ 读 `CaptureTransport.events()` 的
   `DmaRead{token,gpa,len}` → `PcieDevice::on_dma_complete(token, true, <fuzzer 字节>)`
   → continuation 触发下一 `DmaRead` → 循环。**纯 pub 面即可驱动整条链。**
2. ✅ **同步、无 async runtime**：plain `cargo test` 跑通（stable 1.95），fuzz_target body
   内可同步跑完 dispatch→complete（不需 `block_on`，与 architect 当场证伪"未验证"一致）。
3. ✅ **hop guard 精确生效**：Segment 自环 → **恰好 65 次**（`MAX_SGL_SEGMENTS`+1）fetch 后
   `finish_sgl_error` 截断（`assert_eq!(hops, 65)`，revert-verify 强度的精确 oracle，
   排除"其它早错路径"假阳性）。
4. ✅ **任意字节不 panic + 有界**：6 个对抗种子（全 0/全 F/像链/sub_type=1/随机）全过。
5. ✅ **blocker #3（tmpfile 复用）**：每 case 新建 tmpfile + best-effort 删，无句柄泄漏致命问题。

### POC 暴露的真 finding（影响设计）
- **内部不变量外部不可断言**：`MAX_SGL_SEGMENTS`/`sgl_ops` 等是 `pub(super)`，外部 fuzz crate
  看不到。POC 靠"数 DmaRead 事件数"间接推 hop 数。**决策点**：若要更强的内部不变量 oracle
  （如"每个 op 收尾后 `sgl_ops` 表为空"），需加一个 `#[doc(hidden)] pub fn __fuzz_invariants()`
  访问器。倾向：先用 observable-only oracle（无 panic + 事件有界 + 最终 CQE），按需再加访问器。
- **工具链现实**：仓库钉 **stable 1.95**，**cargo-fuzz 未装**，libfuzzer 需 **nightly**。
  fuzz **crate 本身**在 stable 可 `cargo build`（`fuzz_target!` 非 `cfg(fuzzing)` 时退化为普通
  main）；**实际 fuzzing run** 需 nightly + `cargo install cargo-fuzz` + linux-gnu。是 env 步骤。

## 3. 方案

### 3.1 crate 布局（解决 workspace-exclude）
`nvme_firmware` 被**排除**出根 workspace（[根 Cargo.toml:74](../../../../Cargo.toml) exclude），自身
`Cargo.toml` 只有 `[package]`、无 `[workspace]`，靠 standalone（excluded 包即单包 workspace 根）构建。

**关键纠正（architect B-1）**：`nvme_driver/fuzz/Cargo.toml` **没有** `[workspace]` 表——它能编是因为
是**根 workspace member**（[根 Cargo.toml:39] 显式列入），靠根继承 `edition.workspace`/`xtask_fuzz.workspace`/
`arbitrary.workspace`/`libfuzzer-sys.workspace`。所以**不能"抄它的骨架"**：照抄那些 `.workspace = true`
到一个脱离根 workspace 的 nested crate 会因无继承源而**解析失败，第 1 步 cargo build 即报错**。

正确做法：fuzz crate 自带 `[workspace]`（空表，脱离根）+ **显式写死**所有原本继承的键。最终
`usnvmemu/crates/nvme_firmware/fuzz/Cargo.toml` 形如：

```toml
[workspace]            # 空表：使本 crate 自成 workspace 根，脱离被 exclude 的 nvme_firmware
[package]
name = "nvme_firmware_fuzz"
publish = false
edition = "2024"       # 字面量（对齐 nvme_firmware/pcie_device_core，二者均非继承）
rust-version = "1.95"

[dependencies]
nvme_firmware = { path = ".." }
pcie_device_core = { path = "../../pcie_device_core" }
arbitrary = { version = "1.3", features = ["derive"] }
xtask_fuzz = { path = "../../../../xtask/xtask_fuzz" }   # 相对深度已核

[target.'cfg(all(target_os = "linux", target_env = "gnu"))'.dependencies]
libfuzzer-sys = "0.4"

[package.metadata]
cargo-fuzz = true
# 去掉 [lints] workspace = true —— nested 无此继承源

[[bin]]
name = "fuzz_sgl_chain"
path = "fuzz_targets/fuzz_sgl_chain.rs"
test = false
doc = false
```

只抄模板的 **`fuzz_target!` 接线套路 + `arbitrary` 取字节模式**（`fuzz_main.rs`），
**harness 内核抄 `controller/tests.rs::drive()` 同步 pump，绝不抄 `fuzz_nvme_driver.rs` 的
async chipset 模型**（那是 NvmeDriver+pal_async，与本同步 controller 错配）。

`xtask fuzz` 的 target 发现走 `ignore::Walk` **文件系统遍历**（[parse_fuzz_crate_toml.rs:233]），
**不依赖 workspace membership**——故 fuzz crate 自成 nested workspace **不影响被发现**；
`.gitignore` 只忽略 `fuzz/{target,corpus,artifacts,coverage}`、不忽略 `fuzz/Cargo.toml`。

### 3.2 fuzz targets（按价值排序）

| # | target | 打什么 | 优先级 | 状态 |
|---|--------|--------|--------|------|
| 1 | `fuzz_sgl_chain` | PSDT=10 SGL segment 链 walker（`NvmSglFetch` 递归 + hop guard） | **HIGH** | POC 已证驱动可行 |
| 2 | `fuzz_prp_list_chain` | read 方向 `NvmReadPrpListFetch` 链（`MAX_PRP_LIST_PAGES` 守卫） | **HIGH** | 同机制（已核：经 `nvme_io_dispatch` 可达、同 `on_dma_complete` 入口） |
| 3 | `fuzz_admin_dispatch` | `dispatch_admin`（Identify/Get Log/Create-Delete Q 等 opcode 分派） | MEDIUM | 经 pub `nvme_admin_dispatch` + `nvme_admin_complete_dma`（后者内部直接 `self.on_dma_complete`，已核） |
| 4 | `fuzz_parsers` | Tier 1 纯函数（`parse_sgl_list` + prp 算术），`arbitrary` 直喂 | LOW | 轻量补充 |

> **target #5（§22 vfio-user mmap，fstat oracle）不在本 plan 范围**：它打的是
> `vfio_user_transport` 的 unsafe mmap SIGBUS，成本模型不同（真 fd / 真 SIGBUS / process
> isolation），归属应是 `vfio_user_transport/fuzz/` 独立 crate + 独立 ADR。本 plan 只在
> §4 step8 留指针，**不**混入 `nvme_firmware/fuzz`。

### 3.3 结构感知输入（arbitrary）
reviewer 最有价值的洞见。`#[derive(Arbitrary)]` 一个 fuzz-input 结构，让 fuzzer 直接生成
**有结构的多步序列**而非裸字节，命中深层路径：

```rust
#[derive(Arbitrary)]
struct SglChainInput {
    slba: u16,            // 限幅到 NS 容量内/外都试
    nlb: u8,
    sgl1_len: u16,        // SGL1 segment 指针 length
    sgl1_id: u8,          // type+sub_type nibble
    segments: Vec<SegPage>,   // 每次 fetch 喂回的 segment 页
}
#[derive(Arbitrary)]
struct SegPage { descs: Vec<RawDesc16> }   // 每页若干 16B 描述符
#[derive(Arbitrary)]
struct RawDesc16 { address: u64, length: u32, id_byte: u8 } // fuzzer 控 type/sub/len/addr/对齐
```

驱动循环（同 POC）：dispatch → 对每个未服务 `DmaRead` 按序喂 `segments[i]` 序列化字节 →
继续到排空/守卫截断。fuzzer 通过控 `segments` 的拓扑（自环 continuation、超长链、空段、
非 16 倍数、未知 type、sub_type≥2、length 越界）压测整条 walk。

> **W-2 输入限幅**：`Vec<SegPage>`/`Vec<RawDesc16>` 的默认 `Arbitrary` 会生成任意长向量，
> 单 case 可能炸内存/时间。harness 必须对 `segments.len()` 和每页 `descs.len()` 限幅
> （如各 ≤ 128），用 `u.arbitrary_len()` 或显式截断；POC 的 `ITER_SAFETY_CAP` 只兜底 fetch
> 次数、不兜底输入 Vec 长度。`Sqe` 本身**不** derive `Arbitrary`（[cmd.rs:340] 仅 zerocopy
> 派生），由 harness 用 pub 字段手工组装（同 POC）。

### 3.4 oracle（observable-only，无 golden）
- **不 panic**（safe Rust 越界应返 SC 而非 panic——任何 panic 即 bug）。
- **迭代有界**：每轮喂回的 `DmaRead` 数 ≤ `MAX_SGL_SEGMENTS`/`MAX_PRP_LIST_PAGES` + 小常数
  （守卫必须截断自环/超长链；POC 已证 SGL=65 精确）。
- **不无界内存**：累积 `frags`/gather buffer 有界（受 nlb×sector + entry 上限约束）。
- **最终收尾**：链结束必发一条 CQE（成功或精确 SC），不丢命令。
- （内部不变量，经 `fuzzing` feature 访问器）断言每 op 收尾后 `sgl_ops`/`prp_list_ops` 表清空
  （无 op_id 泄漏）、hop 计数可直读 `MAX_SGL_SEGMENTS`/`MAX_PRP_LIST_PAGES` 常量精确断言。

### 3.5 corpus / CI
- seed corpus：把 POC 的 6 个对抗种子 + 既有 e2e（`openhcl_sgl_segment_chain_read/write`）的
  合法链拓扑序列化为种子。**W-1：`**/fuzz/corpus` 被 `.gitignore` 忽略**（[.gitignore:21]），
  故种子放在**不被忽略**的 `fuzz/seed_corpus/`（或 `include_bytes!` 内联），CI 启动时 copy 进
  `fuzz/corpus/` 再跑——直接提交 `corpus/` 会丢失。
- CI：stable 上 `cargo build` 各 target（防 bit-rot）；nightly job 跑定时短 fuzz（如每 target
  60s）。**linux-gnu only**（libfuzzer 限制，[Guide fuzzing.md] 已述）。

## 4. 执行步骤（execution-ready）

1. **脚手架**：建 `fuzz/Cargo.toml`（**自带 `[workspace]` 空表 + 显式 pin 所有键**，见 §3.1 完整
   样例——**不照抄** nvme_driver/fuzz 的 `.workspace=true`）+ `fuzz_main.rs`（抄模板接线）。
   验收：**在新建的真 nested fuzz crate 上** `cargo build` 在 stable 通过（W-3：POC 是 `tests/`
   下、走 nvme_firmware 自身 workspace，**不能**沿用其"stable 可 build"结论；须在 nested crate
   现验，尤其 `xtask_fuzz` 相对 path 深度 + `fuzz_target!` 非 `cfg(fuzzing)` 退化为普通 main 是否
   仍需 libfuzzer-sys 符号）。坐实只依赖 firmware+core，无 async/vfio/mmap。
2. **target #1 `fuzz_sgl_chain`**：harness 内核 = POC 的 `drive_sgl_chain` 升级为 `arbitrary` 驱动
   （§3.3 结构）。复用 POC 已证的公共 API 路径。**比 POC 多覆盖两点**（rust-reviewer 标）：
   ① 也驱动 `on_dma_complete(token, false, _)` DMA-失败路径（POC 只跑 `ok=true`）；
   ② 让 `feed` 按 `gpa` 返不同字节（address-dependent 拓扑，POC 对所有 gpa 返同一段）。
3. **target #2 `fuzz_prp_list_chain`**：镜像 #1，打 read PRP-list 链（dispatch READ PSDT=00 +
   `NvmReadPrpListFetch` 完成喂 list 页字节）。
4. **target #3 `fuzz_admin_dispatch`**：经 `nvme_admin_dispatch` + `nvme_admin_complete_dma`
   公共面，fuzzer 控 admin SQE opcode/cdw + DMA 返回字节。
5. **target #4 `fuzz_parsers`**（LOW）：`arbitrary` 直喂 `parse_sgl_list` + prp 算术。
6. **不变量访问器**（已定采纳）：`nvme_firmware/Cargo.toml` 加 `fuzzing = []` feature；用
   `#[cfg(feature = "fuzzing")] pub` 暴露 `sgl_ops`/`prp_list_ops` 表长度 + 守卫常量；fuzz crate
   依赖开 `features = ["fuzzing"]`（先例 `firmware_uefi`）。
7. **corpus + CI wiring**；按需 nightly job。
8. **§22 target #5**：独立 ADR + 任务（不阻塞 #1-#4）。
9. **文档同步**：ROADMAP（标 fuzz 落地）、本 crate README/codemap（新增 fuzz 目录说明）、
   若发现真 bug → LESSONS。

每个语义单元结尾经 **rust-reviewer review** 再 commit（项目纪律）；commit 粗粒度按 target。

## 5. 决策

### 已自决（按项目既定价值观 + architect 复核，不塞给 user）
1. **范围 = #1+#2+#3 全做**（Tier 2 三条指针追逐全覆盖）。三条已核为同机制、同 `on_dma_complete`
   公共入口，非 either-or 破坏性分支；"有意义的完整、不为小而小"取向直接指向全做。#4 作 LOW 补充。
2. **§22 transport mmap target = 后续独立 ADR，不纳入本轮**。跨 crate（vfio_user_transport）、
   成本模型不同、不阻塞 #1-#4（其论据已自锁结论）。
3. **POC 测试 `tests/fuzz_poc_sgl_chain.rs` 保留为回归测试**（首个外部直驱 harness、已绿、
   `assert_eq!(hops,65)` 精确 oracle）；后续 #1 的 harness 内核以它为基础升级。

### 待用户拍板（真设计决策）
- ~~是否允许加不变量访问器~~ **已定（2026-06-13 用户批准）：加，但用仓库原生 `fuzzing` cargo
  feature 形态，非 `#[doc(hidden)] pub`**。先例 `firmware_uefi`（[lib.rs:44-47]：`#[cfg(feature =
  "fuzzing")] pub` / `#[cfg(not)] pub(crate)` 仅在 feature 下放宽可见性）。落地：
  - `nvme_firmware/Cargo.toml` 加 `[features]` 段 `fuzzing = []`；
  - 需要被 fuzz 断言的内部（如 `sgl_ops`/`prp_list_ops` 表长度、`MAX_SGL_SEGMENTS` 常量）用
    `#[cfg(feature = "fuzzing")] pub fn __fuzz_invariants(&self) -> FuzzInvariants` 暴露；
  - fuzz crate `Cargo.toml` 依赖写 `nvme_firmware = { path = "..", features = ["fuzzing"] }`。
  - **生产构建零额外公共面**（不开 feature 时仍 `pub(crate)`）。这优于 `#[doc(hidden)] pub`
    （后者永久进公共 API、semver 负担）。
  - oracle 因此升级为 observable-only **+ 内部不变量**：每 op 收尾后 `sgl_ops`/`prp_list_ops`
    表清空（无 op_id 泄漏）、hop 计数 ≤ 守卫上限（可直读常量精确断言，不再靠数事件）。

## 6. 关键先例文件
- harness 蓝本（同步）：[controller/tests.rs `drive()`](../../src/controller/tests.rs)
- POC（已绿）：`tests/fuzz_poc_sgl_chain.rs`
- Cargo/fuzz 骨架（**仅抄骨架，勿抄 async 内核**）：`vm/devices/storage/disk_nvme/nvme_driver/fuzz/`
- 公共驱动面：`nvme_io_dispatch`/`nvme_admin_dispatch`/`nvme_admin_complete_dma`（[mod.rs](../../src/controller/mod.rs)）、
  `PcieDevice::on_dma_complete`（[device.rs:71](../../../pcie_device_core/src/device.rs)）、
  `CaptureTransport`（[transport_capture.rs](../../../pcie_device_core/src/transport_capture.rs)）
