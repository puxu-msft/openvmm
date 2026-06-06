# 项目愿景 — 用户态 NVMe Firmware 为核心，三种接入

> 状态：vision 文档（2026-06-06 用户明确）。本文记录长期产品定位 + 当前架构匹配度 + 待补缺口。
>
> 关联：
> - [Phase X 外部化调研](2026-06-06-phase-x-extract-from-openvmm-survey.md) — 仓库拆分调研
> - [ADR-008](DECISIONS.md) — 外部化路径决策
> - [ROADMAP.md](ROADMAP.md) — 短中长期 phase

---

## 1. 一句话愿景

**一份用户态 NVMe firmware**（教学到 production 平滑过渡），让任何 hypervisor / userspace 程序通过三条标准接入暴露 NVMe 盘:

```
                ┌────────────────────────────────────┐
                │     用户态 NVMe firmware (core)     │
                │  pcie_remote_nvme_userspace        │
                │  + nvme_of_tcp_target (NVMe-oF 层) │
                │  ─────────────────────────────────  │
                │  trait Transport (5 原语)           │
                └─────────┬───────────┬───────────────┘
                          │           │
              ┌───────────┼───────────┴────────────┐
              │           │                        │
       ┌──────┴──┐  ┌─────┴────┐         ┌─────────┴──────┐
       │ OpenHCL │  │  OpenVMM │         │  QEMU /        │
       │  VTL2   │  │   dev    │         │  Cloud         │
       │  vsock  │  │  TCP     │         │  Hypervisor /  │
       │  (PCIe  │  │  (PCIe   │         │  SPDK          │
       │  Remote)│  │  Remote) │         │  (vfio-user)   │
       └─────────┘  └──────────┘         └────────────────┘

                       第 4 条 (隔了 NVMe-oF 抽象层):
                       ┌────────────────────┐
                       │ Linux nvme-cli /   │
                       │ Windows nvme       │
                       │ initiator          │
                       │ (NVMe-oF TCP wire) │
                       └────────────────────┘
```

## 2. 当前架构匹配度 ✅

**好消息**: 教学项目已经按这个方向走，**核心边界已对齐**：

| 组件 | 当前路径 | 状态 |
|------|---------|------|
| Firmware core (NVMe controller) | `pcie_remote_nvme_userspace` | ✅ Phase A→S7 完整 |
| `trait Transport` (5 原语) | `pcie_remote_userspace_sdk/src/device.rs` | ✅ 已抽出 |
| PCIe Remote transport | `pcie_remote_userspace_sdk/src/openhcl_transport.rs` | ✅ vsock + TCP 双 backend |
| vfio-user transport | `pcie_vfio_user_sdk/src/transport.rs` | ✅ Phase U1-U5 + U-followup |
| NVMe-oF TCP wire 层 | `nvme_of_tcp_target` | ✅ V1..V8e + V-followup-tls/auth/dhchap |

**真依赖 grep 验证**:
- NVMe controller core (`controller/*.rs`) — runtime-agnostic，只 use `std/zerocopy/super/crate/pcie_remote_userspace_sdk`
- `pal_async` 只出现在 `main.rs` (bin 入口) 和 controller 的一行 comment 里

意味着: **controller 已经可移植**。换 transport 不用改 controller。换 runtime 只需改 bin。

## 3. 三条接入的现状 + 缺口

### 3.1 OpenHCL VTL2 (vsock + PCIe Remote)

**现状**: ✅ 真 Hyper-V e2e 验证过 (commit `4fe6bec1` v20 NVMe 完全闭环；guest format FAT32 + 文件读写)。

**缺口**: pal_async 与 VTL2 paravisor 强绑定；无法换 tokio。这是设计约束，**不是问题**。

### 3.2 OpenVMM dev (TCP + PCIe Remote)

**现状**: ✅ TCP backend (`pcie_remote_noop_host_tcp` + bin TCP 模式)；测试覆盖完整。

**缺口**: 同 3.1 受 pal_async 约束。

### 3.3 QEMU / Cloud Hypervisor / SPDK (vfio-user)

**现状**: ✅ U1-U5 完成；U-followup commit `2d284030` "NVMe controller behind vfio-user (QEMU-接管模式)" 真 e2e。

**缺口**:
- vfio-user spec 演进 (Live Migration / 新 region type) 未跟
- 测试只有 lib unit；缺**真 QEMU e2e** Python harness (相当于 nvme-of-tcp 的 V-interop-7 角色)
- README 提及但没有 e2e 命令脚本 (`QEMU_VFIO_USER.md` 有，但散在 nvme userspace crate 下)

### 3.4 NVMe-oF TCP wire (隐含第 4 条)

**现状**: ✅ Linux nvme-cli plaintext discover + connect + IO 真互通；TLS/mTLS/CHAP 全栈代码 + lib test + Python harness。

**缺口**: 见 [ROADMAP §1](ROADMAP.md):
- real-host CHAP interop (跑真 Linux nvme-cli `--dhchap-secret`)
- kernel-CI tls-psk 五元组 anchor
- TLS PSK 注入待 rustls upstream

## 4. 愿景下的下一阶段重点

按 firmware-as-core 视角重排 ROADMAP §1 优先级：

### Tier 1 (firmware 本体 + 真 e2e 验证)

1. **NVMe-oF real-host CHAP interop** (跑真 Linux nvme-cli `--dhchap-secret`) — *验第 4 条接入*
2. **vfio-user QEMU e2e harness** — *验第 3 条接入还活着*；类比 nvme-of-tcp 的 V-interop-7 写 Python `scripts/qemu_interop/`
3. **NVMe firmware kernel-CI tls-psk vector** — *验 firmware crypto 算法对齐 kernel*

### Tier 2 (firmware 演进 + 文档统一)

4. **统一 firmware crate 命名**：当前 `pcie_remote_nvme_userspace` 名字带 transport 暗示（pcie_remote）。建议改 `nvme_firmware` 或 `nvme_controller`，让"是个 firmware"显式
5. **`trait Transport` doc + example**：写一份 "如何加第 5 条 transport (e.g. iSCSI / NBD)" 教程
6. **`pcie_remote_userspace_sdk` 改名 `pcie_device_sdk`**：去掉 "remote" 暗示，强调它是通用 PCIe 设备 SDK

### Tier 3 (外部化 + 长期)

7. **Phase X (ADR-008)**: 仓库拆分；新仓 ` userspace-nvme-firmware` (建议名)，主仓只留 VTL2 device
8. **runtime swap**: 用户态路径 pal_async → tokio (除 VTL2 path)

## 5. 仓库重组建议 (Phase X 之后形态)

```
userspace-nvme-firmware/                # 新仓
├── README.md                           # "用户态 NVMe firmware + 3 transport"
├── crates/
│   ├── nvme_firmware/                  # 原 pcie_remote_nvme_userspace 改名
│   ├── nvme_of_tcp/                    # 原 nvme_of_tcp_target 改名
│   ├── pcie_device_sdk/                # 原 pcie_remote_userspace_sdk 改名
│   ├── pcie_vfio_user_sdk/             # 不变
│   ├── pcie_protocol/                  # 原 pcie_remote_protocol 改名
│   └── examples/
│       ├── rng_device/                 # 原 pcie_remote_rng_userspace
│       └── noop_host/                  # 原 pcie_remote_noop_host
├── docs/
│   ├── PROJECT_VISION.md (本文)
│   ├── ARCHITECTURE.md                 # firmware ↔ transport 边界图
│   ├── ROADMAP.md / PRINCIPLES.md / LESSONS.md / DECISIONS.md
│   ├── plans/                          # phase plans
│   └── specs/                          # wire references
├── scripts/
│   ├── interop_py/                     # nvme-of-tcp Python harness
│   └── qemu_interop/                   # 新: vfio-user QEMU harness
└── vendor/                             # vendor 进来的 openvmm 小 dep
    ├── nvme_spec/
    └── storage_string/

openvmm/                                # 原仓
└── vm/devices/
    ├── pcie_remote_protocol/           # 已搬走，改 path = "../../userspace-nvme-firmware/crates/pcie_protocol" (或 git dep)
    ├── pcie_remote_device/             # 留这边，VTL2 paravisor 自己消费
    └── pcie_remote_resources/          # 留这边
```

## 6. 与现有路径的对齐

本文档相对 [Phase X survey](2026-06-06-phase-x-extract-from-openvmm-survey.md):
- **survey** 答 "可不可行 / 多大代价"
- **本文** 答 "搬完之后长什么样 / 优先做什么"

相对 [ROADMAP](ROADMAP.md):
- **ROADMAP** 是 phase by phase 计划
- **本文** 给 ROADMAP 重排优先级 (firmware-as-core 视角 vs feature-by-feature 视角)

相对 [DECISIONS](DECISIONS.md):
- ADR-008 已记 Phase X 决策
- 待加 ADR-009 "Firmware-as-core 愿景确认 + 命名重构"

## 7. 关键架构原则 (firmware-as-core 视角)

1. **NVMe controller core 必须 runtime-agnostic** — 现已对齐，必须保持
2. **任何 transport 加入只走 `trait Transport`** — 不许 transport 反向 use controller 内部
3. **wire 协议层 (NVMe-oF / PCIe / vfio-user) 是 transport 的一部分，不属 firmware** — 现已对齐
4. **教学/生产边界不能模糊**: firmware 的每个 feature 都要标 "spec-strict-mode" or "教学版简化"
5. **测试金字塔**: lib test (controller 单测) → integration test (transport + firmware 端到端) → Python harness (跨进程 wire 实证) → 真 host (Linux nvme-cli / QEMU / Hyper-V)
