# usnvmemu — Userspace NVMe Firmware Emulator

> **用户态 NVMe firmware**（Phase A→S7 完整 NVMe 2.0 spec 覆盖），通过统一的
> `trait Transport` 5 原语，挂接到任意 hypervisor / userspace 程序。

```text
                ┌────────────────────────────────────┐
                │     用户态 NVMe firmware (core)     │
                │            nvme_firmware            │
                │  + nvme_of_tcp_target (NVMe-oF 层)  │
                │ ────────────────────────────────── │
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

详 [docs/PROJECT_VISION.md](docs/PROJECT_VISION.md)。

## 目录结构

```
usnvmemu/
├── README.md                              ← 本文件 (项目入口)
├── crates/                                ← 6 个 Rust crate
│   ├── nvme_firmware/                      — 用户态 NVMe firmware 主体
│   ├── nvme_of_tcp_target/                 — NVMe-oF TCP target (firmware + wire 包装)
│   ├── pcie_device_sdk/                    — PcieDevice SDK + trait Transport
│   ├── vfio_user_transport/                — vfio-user transport backend (QEMU 接管)
│   ├── rng_device_example/                 — 第二个 PcieDevice example (RNG, 教学)
│   └── pcie_remote_test_harness/           — 协议 e2e harness (PCIe Remote 验证)
├── docs/                                  ← 项目文档
│   ├── PROJECT_VISION.md                  — 项目愿景 + 架构图
│   ├── ROADMAP.md                         — 短/中/长期 phase (Tier 1/2/3 优先级)
│   ├── PRINCIPLES.md                      — 不变约束 + coding policy
│   ├── LESSONS.md                         — 17 条踩坑教训
│   ├── DECISIONS.md                       — 9 条 ADR
│   ├── PCIE_REMOTE_SESSION_LOG.md         — PCIe Remote 阶段日志 (2026-05-29..05-31)
│   ├── PCIE_REMOTE_HYPERV_RUNBOOK.md      — Hyper-V 部署 runbook
│   ├── PCIE_REMOTE_HOTPLUG_DESIGN.md      — K-20 hotplug 设计 (已 shipped)
│   ├── PCIE_REMOTE_MSHV_DIAGNOSIS.md      — Path B mshv 排查归档
│   ├── PCIE_REMOTE_USER_TODO_LEGACY.md    — 阶段性 user TODO 快照
│   ├── PCIE_REMOTE_REVIEW_PENDING.md      — 待用户决定的 LOW 级建议
│   ├── plans/                             — 15 个 phase 详细计划
│   └── specs/                             — 4 个 wire reference / design spec
└── scripts/                               ← 部署 / 跨编脚本
    ├── build-windows-cross.sh             — WSL → Windows MSVC cross-build
    ├── setup-pcie-remote.ps1              — vsock GUID 注册
    ├── noop_host_vsock.ps1                — vsock client 调试
    └── hyperv/                            — Hyper-V VM 创建 + IGVM 加载
```

## 6 个 crate 的关系 + 各自作用

### 主线: firmware ↔ transport

| crate | 作用 | 类型 |
|------|------|------|
| `pcie_device_sdk` | **PcieDevice SDK** — 定义 `trait Transport` (5 原语 dma_read/write/fire_and_forget/fire_interrupt)。任何用户态 PCIe 设备都基于它。 | Library |
| `nvme_firmware` | **用户态 NVMe firmware 本体** — 完整 NVMe 2.0 spec coverage (Phase A→S7)；transport-agnostic (controller core 只 use `std/zerocopy/super/crate/pcie_device_sdk`)。 | Library + Bin |
| `nvme_of_tcp_target` | **NVMe-oF TCP target** — 在 NVMe firmware 之上包 NVMe-oF TCP wire 协议 (V1-V8e + V-followup-tls/auth/dhchap)，让 Linux nvme-cli 直接连。 | Bin |
| `vfio_user_transport` | **vfio-user transport backend** — 把 `trait Transport` 接到 vfio-user UNIX socket，让 QEMU/Cloud Hypervisor/SPDK 接管。 | Library |

### 教学 / harness

| crate | 作用 |
|------|------|
| `rng_device_example` | **第二个 PcieDevice example** (~360 LOC)，最小 RNG 设备；证明 SDK 不限 NVMe，可作新设备教学模板。 |
| `pcie_remote_test_harness` | **PCIe Remote 协议 e2e harness**，两个 bin (TCP + vsock) 主动 connect server 验协议；不是 device。 |

> **关于命名 (2026-06-08 已重构)**: 历史上这些 crate 叫 `pcie_remote_*`
> （最初项目叫 "OpenHCL/OpenVMM 远程 PCIe 实验设备"）。从 firmware-as-core
> 视角看 "remote" 是 transport 的事，跟 firmware 没关系。已按 ROADMAP Tier 2
> `V-followup-firmware-rename` 重构完成:
> - `pcie_remote_nvme_userspace` → **`nvme_firmware`** (强调"是 firmware")
> - `pcie_remote_userspace_sdk` → **`pcie_device_sdk`** (去 "remote"，强调通用 PCIe 设备 SDK)
> - `pcie_vfio_user_sdk` → **`vfio_user_transport`** (它是 transport backend 不是 SDK)
> - `pcie_remote_rng_userspace` → **`rng_device_example`** (教学 example)
> - `pcie_remote_noop_host` → **`pcie_remote_test_harness`** (协议 e2e harness, 保留 pcie_remote_ 因它专测该协议)
> - `nvme_of_tcp_target` 保留 (名字已准确)

## 快速上手

### 1. 跑 NVMe-oF TCP target (最热路径，无需 Hyper-V)

```bash
cd usnvmemu/crates/nvme_of_tcp_target
python3 -c "open('/tmp/ns1.img','wb').write(b'\\0'*1024*1024*1024)"  # 1 GiB
cargo run --release -- --listen 127.0.0.1:4420 --backing-file /tmp/ns1.img
```

然后从另一台 Linux (或 WSL2):
```bash
sudo nvme discover -t tcp -a 127.0.0.1 -s 4420
sudo nvme connect -t tcp -a 127.0.0.1 -s 4420 -n nqn.2014-08.org.nvmexpress:teaching:disk
sudo nvme list
```

### 2. 跑教学 controller behind vfio-user (QEMU 接管)

详 `crates/nvme_firmware/QEMU_VFIO_USER.md`。

### 3. 跑 OpenHCL VTL2 真 PCIe Remote 路径

详 `docs/PCIE_REMOTE_HYPERV_RUNBOOK.md`。

## 状态 (2026-06-08)

- **代码 + 测试**: 306 tests + clippy 0 warning (`nvme_of_tcp_target`)；其他 5 crate 各自 build pass
- **已 verified 真 host**: Linux nvme-cli plaintext discover + connect + IO；真 Hyper-V e2e (PCIe Remote 路径 v20 NVMe 完全闭环)
- **未 verified 真 host**: TLS/mTLS/CHAP (只 lib test + Python harness)；vfio-user QEMU e2e (只 lib unit test)

下一步 HIGH (见 [ROADMAP §1](docs/ROADMAP.md)):
1. NVMe-oF real-host CHAP interop (`--dhchap-secret`)
2. vfio-user QEMU 真 e2e harness (scripts/qemu_interop/)
3. NVMe TLS PSK kernel-CI 五元组 anchor

## 关于本目录跟 openvmm 主仓的关系

本目录 (`usnvmemu/`) 物理位置在 openvmm 仓库 root 下，但**不是 openvmm
workspace member** (在 `<root>/Cargo.toml` 的 `exclude = [...]` 里)。

- `crates/*/Cargo.toml` 通过 `path = "../../../support/..."` 引 openvmm 主仓内
  部 crate (pal_async / mesh / nvme_spec / 等)
- 未来 (Phase X, [ADR-008](docs/DECISIONS.md)) 计划独立成新 repo
  `userspace-nvme-firmware`；目前耦合度调研已完成，留待 V-followup-firmware-rename
  后一起做

## 许可

MIT — 与上游 openvmm 仓库一致。
