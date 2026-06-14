# Spike #03 — OpenHCL paravisor 路径 (Hyper-V VPCI VSP synthetic NVMe)

**子项目**: 2B
**状态**: 启动 — baseline spike 进行中
**ADR**: 0001 OPT-1 (host kernel) 终止 (spike #02 MS-A v1 C4 实证); user 2026-05-29 接受 OpenHCL paravisor 路径

## 核心架构 (与 spike #01/02 host-side 模式截然不同)

```
+--------------------------------------------------+
| Host (xpu-3.local, WS2025 build 26100)           |
|  Hyper-V hypervisor (vmms / vmwp.exe, stock)     |  ← 不改这里
+---+----------------------------------------------+
    | hypervisor (Hyper-V VMBus + VTL)
    v
+==================================================+
| Guest VM (New-VM -GuestStateIsolationType OpenHCL)|
|                                                  |
|  +--------------------------------------------+  |
|  | VTL2 paravisor — OpenHCL (microsoft/openvmm)|  |  ← fork + 改这里
|  |   - VPCI VSP (vm/devices/pci/vpci/, Rust) |  |
|  |   - NVMe controller (vm/devices/storage/  |  |
|  |     nvme/, Rust)                          |  |
|  +--------------------------------------------+  |
|                  ↕  VTL transition / VMBus      |
|  +--------------------------------------------+  |
|  | VTL0 guest OS (Windows 11 / WS2025 / Linux)|  |
|  |   vpci.sys (inbox) → stornvme.sys ✓       |  |  ← guest 视角看到 NVMe
|  +--------------------------------------------+  |
+==================================================+
```

## 环境验证 (2026-05-28)

| 检查 | 状态 |
|------|------|
| xpu-3.local OS | WS2025 build 26100 (= 24H2 base) ✓ |
| Hyper-V | Installed, vmms Running ✓ |
| `New-VM -GuestStateIsolationType OpenHCL` 支持 | ✅ 在 valid values 中 (TrustedLaunch / VBS / **OpenHCL** / SNP / TDX / Disabled) |
| `AllowFirmwareLoadFromFile` 注册表 | 待 set (`HKLM:\Software\Microsoft\Windows NT\CurrentVersion\Virtualization` = 1 DWORD) |
| OpenHCL prebuilt .bin | 不存在 (Microsoft 无 GitHub release / 无 CI artifact 公开) |
| Build 工具 | 必须 Linux/WSL2 (Win 无法 build); 本机 WSL Ubuntu-24.04 + rustup 在装 |

## 工作步骤 (M0-M4)

### M0: Baseline spike (1-2 天) — **进行中**

目标: 用 stock OpenHCL .bin 跑通 Hyper-V VM, 验证 guest 内 stornvme 绑 emulated NVMe.

步骤:
1. ✅ Clone microsoft/openvmm to `refs/openvmm/` (40MB)
2. ⏳ 本地 WSL Ubuntu-24.04 安装 rustup (后台跑)
3. ⏳ `cargo xflowey build-igvm x64 --release` (估 30min-2h 首次)
4. ⏳ scp `openhcl-x64.bin` 到 xpu-3 `C:\Windows\System32\` (vmwp.exe 可读)
5. ⏳ xpu-3 set `AllowFirmwareLoadFromFile = 1`
6. ⏳ 准备 guest OS disk (Ubuntu cloud img 推荐 — Linux 调 debug 容易, stornvme 等价行为)
7. ⏳ `New-VM ... -GuestStateIsolationType OpenHCL` + `Set-OpenHCLFirmware`
8. ⏳ 启 VM + guest 内观察 nvme list / lsblk

### M1: 跑 OpenVMM 测试套件 (1-3 天)

`cargo xflowey vmm-tests-run` — 验证我们的 build 与 Microsoft CI 一致.

### M2: 理解 vm/devices/storage/nvme/ 接口 (3-5 天)

阅读 NVMe controller Rust 代码 (PCI MMIO + MSI-X + admin queue + completion queue + 后端 disk backend). 找出 customization extension points.

### M3: 自定义 NVMe behavior PoC (2-4 周)

实施 user-specified 自定义 NVMe 行为 (待 user 明确需求 — 可能是 namespace 模型 / 自定义命令集 / 后端存储类型 / 特定厂商 quirk).

### M4: deploy + findings + escalate (1 周)

完整 docs + handbook + findings + commit.

## 目录布局

```
spikes/03-openhcl-vpci-nvme/
├── README.md            (本文)
├── baseline/            (M0 - stock OpenHCL deploy 工具)
├── custom-nvme/         (M3 - 自定义 NVMe code fork)
└── evidence/
    ├── baseline/        (M0 - guest 内 nvme list / lsblk / stornvme attach)
    └── custom-nvme/     (M3 - 自定义行为 evidence)
```

## 当前未知 / 风险

1. **OpenHCL build 实际时间** — Microsoft doc 说"first build will take some time" 没具体数. 可能 30min 也可能 4h+, 取决于 Linux kernel + Rust dependencies 下载/编译.
2. **WS2025 dev support** — doc 仅明确说 Win11 24H2; WS2025 "Instructions coming soon". 但 `New-VM` 参数已支持 OpenHCL, 应能跑 — 待 baseline 验证.
3. **VTL2 paravisor 调试** — KDNET 在 VTL2 中是否工作未知. 调试可能需要 `ohcldiag-dev` tool.
4. **自定义 NVMe 接口稳定性** — OpenVMM `vm/devices/storage/nvme/` API 是否稳定 (Microsoft 在 inner-loop 开发)? Customization 是否会被下次 main pull 冲突?
