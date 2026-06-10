# 测试质量：持续提升覆盖率的连续机制

> 建立于 2026-06-11（test-coverage-hardening，Wave 1-5）。回答"如何**不断**提高覆盖率"。

## 核心判据

**覆盖率% 是 proxy，不是目的**。真正要保证的是三轴同时成立：

| 轴 | 含义 | 谁守 | 失守的教训 |
|----|------|------|-----------|
| **执行到** | 代码被某测试跑到 | `llvm-cov`（找冷错误路径） | SC bug 活在 18% 覆盖的 completion.rs 错误臂 |
| **独立 oracle 验对** | 断言用**独立于被测代码**的真相源，非自洽 | anchor 测试 + proptest 独立 oracle | LESSONS §20：self-consistent 测不出 self-consistent bug |
| **改坏会红** | 被测行为错了测试会 FAIL | `cargo-mutants` + 手动 revert-verify | LESSONS §23/§26：happy-path 全绿 ≠ 有牙 |

只满足"执行到"= 这次 SC/SCT bug 的现状；缺"独立 oracle"= §20 陷阱；缺"有牙"= §23 陷阱。

## 六条连续机制（M1-M6）

- **M1 锚定护栏** — `sc_constants_match_nvme_spec` / `opcode_feature_register_constants_match_nvme_spec`：
  每个**复制 nvme_spec 的常量**（sc / admin_opc / nvm_opc / fid / regs::Reg / pi SC byte）编译/测试期锚到
  canonical `nvme_spec`，drift 即红。**一加上就抓出真 bug**（`GET_LBA_STATUS=0x1e` 应 0x86）。
  *新增任何 spec 常量 → 顺手 anchor。*
- **M2 布局锚定** *(SMART/error-log/ZNS-Id 已锚（3/4）；resv-report 待)* — log/report 字节布局用 `#[repr(C)]` struct + `offset_of!`
  锚到 spec figure（替代"test 照抄 builder"的自洽假锚）。
- **M3 proptest** — `pt_*`：纯/近纯函数随机输入 + **独立 oracle** 不变量（copy-conflict 的 u128 brute /
  CRC 的 crc crate / SGL parser no-panic / ZNS 矩阵 / parse_prp_list 恒等 / PI 往返）。
  *新增数据路径 → 加 proptest + 独立 oracle。*
- **M4 driven 错误码矩阵** — `error_status_driven_matrix`：真驱 dispatch 进错误路径，断言**完整 16-bit
  status（含 SCT）**。职责分工：矩阵锚"条件→哪个码"，M1 锚"码→值"。*新增 error emit → 加 driven 断言。*
- **M5 e2e 去盲** — e2e `CqeResult.status: u16`（完整 16-bit），不再只取 8-bit SC 丢 SCT。
- **M6 经验门** — `scripts/test-quality.sh {mutants|coverage}`：cargo-mutants（自动 revert-verify，
  数据完整性热点保持 0 missed）+ llvm-cov ratchet。*CI 跑 `check`+`anchors`；定期跑 `mutants`。*

## 命令

```bash
scripts/test-quality.sh check     # 快门：lib + e2e + clippy + fmt（CI 必跑）
scripts/test-quality.sh anchors   # M1 锚定护栏
scripts/test-quality.sh mutants   # M6 变异测试（sgl/pi 应 0 missed）
scripts/test-quality.sh coverage  # M6 lib 覆盖率
scripts/test-quality.sh all       # 全部
```

**注（llvm-cov 盲区）**：`--lib` 覆盖率**严重低估**——e2e 跨进程子进程里 llvm-cov 测不到。所以
completion.rs/io.rs 的错误路径**优先下沉到 unit 层断言**（M4 矩阵）而非只靠 e2e 子进程。

## 工具（一次性安装）

```bash
cargo install cargo-mutants cargo-llvm-cov
rustup component add llvm-tools-preview   # llvm-cov 需要
# proptest + crc 已是 dev-dependency
```

## 经验成果（Wave 1-4）

- `sgl.rs` + `pi.rs` cargo-mutants：**8 missed → 0**。
- 抓出真 bug：`GET_LBA_STATUS=0x1e→0x86`（M1 anchor 当场暴露）。
- 132 lib test（Wave 前 102）+ SCT-aware e2e。

## Follow-up（按 LESSONS §30：清理 ≠ 删 scaffolding；不 speculative wire）

- **M2 reservation report 布局**（SMART/error-log/ZNS-Id 已锚）：reservation report 写成 repr(C) + offset_of! 锚 NVM CS figure；audit 发现 builder 漏 ptpls，补 ptpls 属特性修（按 §30 留真做时）。+（有真设备时）golden wire 字节替代自洽测试。
- **剩余无牙测试上牙**：`o3_fused_cw_dispatch_chain_smoke`（断言 events 空=啥也没测）、
  `aen_queue_fifo_order`（测 stdlib VecDeque）、`fw_download_cap`（断言 64MiB≥8MiB 重言）、
  `k4c_list_accum`（构造 struct 再断言自己）—— 给牙或并入 M4 矩阵。
- **真特性落地时 wire（不 speculative）**：AWUN 强制（ATOMIC_WRITE_UNIT_EXCEEDED）、NS-not-ready
  门（NAMESPACE_NOT_READY）、boot-partition（BOOT_PARTITION_WRITE_PROHIBITED）、CMB
  （SGL_INVALID_USE_OF_CMB）—— 这 4+2 个 sc 常量是 spec-complete scaffolding，**已锚定**，对应特性
  做时再 emit。
- **错误码普查扩展**：M4 矩阵当前覆盖 6 个同步码（含 SANITIZE）；reservation 分支 / NS-attach 族 / zone 限额 /
  PI media SCT=2 的 driven 断言可继续加（async completion 路径需 DeviceCtx 驱动）。
