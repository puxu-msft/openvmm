# 外部化评估：把 NVMe / PCIe 教学项目搬出 openvmm 仓库

> 调研 2026-06-06，basis: 实际 `cargo metadata` + `git log --oneline main..HEAD` + 代码 grep。

## TL;DR

**可行，且比想象方便**。已搬过半（example crates 早就是 workspace `exclude`）。剩余工作分两类：

- **"完全我们的"代码** (`docs/superpowers/examples/*` 7 个 crate + `vm/devices/pcie_remote_{protocol,device,resources}/`): 可直接 `git filter-repo` 抽出，新 repo 改 path dep 即可。
- **"openvmm 内部"依赖** (22 个 transitive crate): **不能搬**，必须外部引用——但**实际真用到的只有 4 个**（pcie_remote_protocol 是我们自己写的；pal_async 4 处 use；nvme_spec 拖 spec 常量；vmsocket 2 处 use）。其余 18 个全是 transitive。

最大不确定性：`pal_async` 是 openvmm 自定义的 async runtime，**没发到 crates.io**，必须 `git = ".../openvmm"` path dep 或者改写换成 tokio。

## 1. 当前耦合度真值

### 1.1 已经独立 (workspace exclude)

`openvmm/Cargo.toml` exclude 段：
```
exclude = [
  "docs/superpowers/examples/pcie_remote_noop_host",
  "docs/superpowers/examples/pcie_remote_userspace_sdk",
  "docs/superpowers/examples/pcie_remote_nvme_userspace",
  "docs/superpowers/examples/pcie_remote_rng_userspace",
  "docs/superpowers/examples/pcie_vfio_user_sdk",
  "docs/superpowers/examples/nvme_of_tcp_target",
  "docs/superpowers/examples/vmrs_log_scanner",
]
```

7 个 example crate **不是 workspace member**，本来就是"独立 crate 借 path dep 借入 openvmm"。这意味着：
- `cargo build` 主仓时不 build 它们
- 它们各自的 `Cargo.lock` 独立
- 搬出去仅需改 path dep；不需改 workspace 结构

### 1.2 在主仓的"我们的代码"

```
vm/devices/pcie_remote_protocol/   # commit db6acf8d 我们引入
vm/devices/pcie_remote_device/     # PCIe Remote v2 实现
vm/devices/pcie_remote_resources/  # 资源类型
```

这三个 crate 是 **本分支** 引入，main 没有。如果搬出去，原 openvmm 主仓就完全干净。

### 1.3 真正"借自 openvmm"的 4 个

| openvmm crate | 用法 | 行数 | 替换难度 |
|--------------|------|------|---------|
| `pal_async` | async runtime 抽象 (Driver/PolledSocket/PolledTimer) — 4 处 use | 9 149 | **大**: 选 tokio? 自己 fork? 直接 git dep? |
| `vmsocket` | Hyper-V vsock 包装 — 2 处 use | 620 | 中: vsock 本就有 crates.io 包，但 openvmm 风格定制 |
| `nvme_spec` | NVMe spec 常量 + struct (CDW0/SQE/CQE 等) — 8 处 use | 1 218 | **小**: 可直接 vendor 进新 repo (BSD-style 复制) |
| `storage_string` | UTF-8 padded fixed-length string helper | 103 | **小**: 可直接 vendor |

剩下 18 个 transitive (mesh / inspect / guid / pal / ...) **都是被 `pal_async` 拖进来的**。一旦切换 async runtime，整团消失。

## 2. 三条可选路径

### 路径 A: 全 git dep (零代码改动，最快)

```toml
# 新 repo Cargo.toml
[dependencies]
pal_async = { git = "https://github.com/microsoft/openvmm", rev = "<sha>" }
pcie_remote_protocol = { git = "...", rev = "<sha>" }
nvme_spec = { git = "...", rev = "<sha>" }
storage_string = { git = "...", rev = "<sha>" }
vmsocket = { git = "...", rev = "<sha>" }
```

**代价**: ≈ 0 代码改动。每个 dep 写 `git = ...` + 锁 rev。

**风险**:
- Cargo `git = ...` 比 path dep **慢得多** (每次 cargo update 都 fetch)
- openvmm 上游若 break API → 我们 pin 的 rev 永远卡老版
- 6+ 个 git dep 全锁同一 rev (因 workspace inheritance: `edition.workspace = true` 这种字段在 git dep 拉单独 crate 时**会 fail** — 见下方坑)

**实测过**: openvmm 的 dep 用 `edition.workspace = true` + `mesh.workspace = true` 之类，意味着**单独 git dep 一个 sub-crate 拉不下来** — Cargo 期待整个 workspace 在场。要么 git 整个 openvmm (~ 64 K 行 transitive)，要么先 patch 这些字段。

### 路径 B: vendor 小依赖 + fork pal_async (中等)

- `nvme_spec` (1 218 行) / `storage_string` (103 行) → 直接 vendor 进新 repo
- `pcie_remote_protocol` (210 行) → 就是我们的，搬过去就行
- `vmsocket` (620 行) → vendor 或换 crates.io `vsock` crate
- `pal_async` (9 149 行) → fork 出最小子集（只我们用的 Driver/PolledSocket/PolledTimer）

**代价**: 中等 (≈ 1-2 天)。pal_async 最痛苦——它自己 transitive 拖了 mesh / inspect / guid 等。Minimal fork 需要 strip 掉大量未用 feature。

**收益**: 新 repo 完全独立，0 git dep；cargo 速度快；可以自由演化。

### 路径 C: 换 tokio runtime (大，建议长期)

`pal_async` → `tokio`。需改：
- `Driver` trait → `tokio::runtime::Handle`
- `PolledSocket` → `tokio::net::TcpStream`
- `PolledTimer` → `tokio::time::Sleep`

`nvme_of_tcp_target` 已经全 tokio (V8e refactor)，只剩 `pcie_remote_userspace_sdk` + `pcie_remote_nvme_userspace` 还用 pal_async。

**代价**: 大 (≈ 3-5 天)。但 `nvme_of_tcp_target` 早已 tokio，证明这条路工程上 OK；剩下两 crate 也都是 worker loop + socket 模式，迁移直接。

**收益**:
- 完全脱离 openvmm runtime 生态
- 新贡献者门槛骤降 (tokio 是 Rust 生态主流)
- `pcie_vfio_user_sdk` 已经全 tokio，统一栈

## 3. 推荐路径 (按 phase)

### Phase X1 — 试水 (1 day)

只搬 `nvme_of_tcp_target` 一个 crate（最新最完整，且已 tokio）。看 git dep 路径 A 的真实痛点 + 文档化坑。

### Phase X2 — pal_async fork minimal 或 tokio 替换 (3-5 day)

对 `pcie_remote_userspace_sdk` + `pcie_remote_nvme_userspace` 换 runtime。这是单 phase 内能搞定的最大瓶颈。

### Phase X3 — vendor 剩余 + 新 repo 立 (1 day)

`nvme_spec` + `storage_string` + `vmsocket` vendor，`pcie_remote_protocol` + `pcie_remote_device` + `pcie_remote_resources` 用 `git filter-repo` 抽出 commit 历史。

```bash
git filter-repo \
  --path docs/superpowers/examples \
  --path docs/superpowers/plans \
  --path docs/superpowers/specs \
  --path vm/devices/pcie_remote_protocol \
  --path vm/devices/pcie_remote_device \
  --path vm/devices/pcie_remote_resources \
  --path-rename docs/superpowers/examples/:./crates/ \
  --path-rename docs/superpowers/plans/:./docs/plans/ \
  --path-rename docs/superpowers/specs/:./docs/specs/ \
  --path-rename vm/devices/:./vendor/
```

### Phase X4 — openvmm 升级路径 (若需要回过头)

如果想把搬出去的东西**反向贡献回 openvmm** (e.g. NVMe-oF TCP target 进 openvmm 成为标准 backend)，因为 `vm/devices/pcie_remote_*` 早已是干净的 add，rebase / cherry-pick 容易。

## 4. 关键风险 / 决策点

### R-1: `pal_async` 是否 fork 还是换 tokio?

**强烈建议换 tokio**。理由：
- `nvme_of_tcp_target` 已证明这条路工程 OK
- 减少教学读者认知成本 (tokio 是行业标准)
- 长期维护成本: fork pal_async = 持续追 openvmm 主线 vs tokio = stable 接口
- 见 [LESSONS §3 async + locks 灾难](LESSONS.md)，我们已经吃过 pal_async 的苦了

但 `pcie_remote_userspace_sdk` 是 OpenHCL VTL2 跑的，**runtime 限于 VTL2 paravisor 支持**。VTL2 不能跑 tokio (no_std-ish, 用 pal_async)。这意味着：
- VTL2 路径必须保留 pal_async
- 用户态路径（OpenVMM dev / vfio-user / nvme-of-tcp target）可换 tokio
- 折中: pal_async 用 feature flag 切

### R-2: 仓库归属

- **option 1**: 全搬出，新 repo `pcie-userspace-toolkit` (或类似名)，独立 maintained
- **option 2**: 留 `vm/devices/pcie_remote_*` 在 openvmm 主仓（它们就是 VTL2 device），只搬 example crates 出去
- **option 3**: 反向把 example crates 也送进 openvmm 主仓（`examples/` 目录），不搬出去

**推荐 option 2**: 主仓留 VTL2 device，新 repo 装"用户态 PCIe device toolkit"。两边 pcie_remote_protocol 共享（git dep 或 vendor）。

### R-3: 文档归属

我们 ROADMAP / PRINCIPLES / LESSONS / DECISIONS 4 份 + 多个 phase plan + spec 都在 `docs/superpowers/`。搬出去时这些都要走，因为它们记的是 nvme/pcie 设计；openvmm 主仓不需要。`git filter-repo` `--path docs/superpowers/` 一锅端。

## 5. 工程量估算 (诚实)

| 路径 | 代价 | 风险 | 维护成本 |
|------|------|------|---------|
| A (全 git dep) | 0.5 day | 高 (Cargo workspace inheritance) | 高 (rev pin 老化) |
| B (vendor + fork pal_async) | 2-3 day | 中 (pal_async minimal subset 边界难定) | 中 (要追 openvmm pal_async 变更) |
| C (换 tokio + vendor) | 4-6 day | 低 (nvme-of 已证) | 低 (tokio stable) |

## 6. 副作用 / 利好

搬出后:
- ✅ 新贡献者门槛骤降 (不用 build openvmm 64 K 行 deps)
- ✅ CI 快 5-10x (cargo build 时间)
- ✅ 文档 + 代码同一仓库，audit 容易
- ✅ 教学价值上升 ("看一个独立项目" vs "看 openvmm 一角")
- ❌ 失去 openvmm IDE 跨 crate 跳转 (但 git dep 也能跳)
- ❌ 反向贡献 (nvme-of 进 openvmm) 多走一层 vendor 同步

## 7. 推荐节奏

如果用户决定要搬：

1. **现在**: 不搬，但**所有新工作按"将来要搬"的预设写** — 比如新 dep 优先选 crates.io、避免新增 `mesh::*` use、`pal_async` use 集中到少数文件 (易于将来 swap)
2. **下个月**: 跑 Phase X1 试水 (`nvme_of_tcp_target` git dep 路径)
3. **季度末**: 评估 X1 痛点，决定 B vs C
4. **半年内**: 完成迁移；老仓只留 pcie_remote_*（VTL2 device）

不推荐"一次大爆炸"——逐 crate 切，每步可验证。
