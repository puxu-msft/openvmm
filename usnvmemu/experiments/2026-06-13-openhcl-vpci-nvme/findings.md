# Spike #03 OpenHCL Baseline — Initial Findings

**实验日期**: 2026-05-29
**OpenVMM 版本**: user 手 build (master branch), `openhcl-x64.bin` 19,910,068 bytes (ship release)
**测试机**: xpu-3.local (WS2025 build 26100)
**evidence**: `evidence/baseline/`

## 1. 实验目的

验证 OpenHCL paravisor + VTL2 SCSI-to-NVMe translation 路径在 Hyper-V 上能否让 **guest 内出现 emulated NVMe controller** (vpci.sys 提供, stornvme attach).

## 2. 实验步骤 + Deploy 路径迭代

| Version | 步骤 | 结果 |
|---------|------|------|
| v1 | Set-ItemProperty AllowFirmwareLoadFromFile=1 + Stop xdp-ws25-1 + Set-OpenHCLFirmware (in-place) + Start | ✓ VM Running, 但无 NVMe device 配置 |
| v2 | + Add SCSI ctrl 1 + Set-VmScsiControllerTargetVtl 2 + Set-VMBusRedirect + Set-Vtl2Settings JSON | **✗ Set-Vtl2Settings ReturnCode 32795 (0x801B)** |
| v3 (WS25) | **destroy + recreate** xdp-ws25-1 (保留 VHDX) + New-VM -GuestStateIsolationType OpenHCL + 全配置 + Vtl2Settings | ✓ host-side success (Vtl2Settings APPLIED). **✗ Guest boot loop crash** |
| Linux | 新 VM `xdp-openhcl-linux-test`, fedora41 VHDX 复制 | ✓ **boot 成功 无 crash** |

## 3. 关键 evidence

### 3.1 Host-side 配置全部成功 (v3)

```
GuestFeatureSet           = 513 (0x201, OpenHCL enabled)
FirmwareFile              = C:\spike03\openhcl-x64.bin
VMBusMessageRedirection   = 1
Backing SCSI controller GUID = 81fc9728-2c27-48ae-8649-bb8e9ab2e29b
Guest NVMe controller GUID   = e481dfaf-633e-4679-a4a0-294eca61cb79
Guest NSID                = 1
```

### 3.2 Guest behavior 鲜明对比

| Guest OS | OpenHCL boot |
|----------|--------------|
| **WS2025** | **CRASH loop**: Hyper-V event 18610 Critical "fatal virtual firmware error" + 18560 "triple fault" + 18590 "fatal error reported by guest OS" + 40001 crash dump 写盘. 每 2 min reset 一次 |
| **Fedora 41** | **BOOT OK**: 5 polls Running, Uptime 单调增加 (45s → 1m45s), 无 reset, Hyper-V event 18609 Information (vs WS25 18610 Critical) |

### 3.3 SSH 限制 (Linux guest 未验证 NVMe device)

fedora41 VHDX 是 user 自有 VM 复制 (SSH key 未配 xpu-3-vm proxy 路径). Hyper-V Heartbeat=NoContact + KVP 不可达 (Linux 端 hv_utils 与 OpenHCL VMBus 中介通信问题). **未能 SSH into guest 验证 `nvme list` 是否出现 emulated NVMe** — guest-side evidence 缺.

## 4. 结论 (update 2026-05-29 debug 后)

### ✅ 已实证

1. **OpenHCL paravisor 在 WS2025 host build 26100 work** (development support 可用)
2. **Set-Vtl2Settings 应用成功** (前提: destroy + recreate VM, in-place Set-OpenHCLFirmware 32795)
3. **paravisor 自己 (VTL2 Linux mini-OS) 启动 OK**, VMBus channel offer 进展正常

### ⚠️ 实证两个 root cause (Hyper-V Event 18590 paravisor panic trace)

**Root cause #1**: VTL2 settings JSON 字段大小写错

> ```
> json parsing failed: unknown variant `NVMe`, expected one of
>     `UNKNOWN`, `SCSI`, `IDE`, `NVME`, `scsi`, `ide`
> ```

我们 JSON 用 `"protocol": "NVMe"` 但 OpenHCL Rust 期望 `"NVME"` (全大写). 文档 `storage_configuration.md` 写 "NVMe" 是错的 (文档 vs 实际反序列化不一致). **已修复** (sed 3 个 ps1 script).

**Root cause #2**: OpenHCL build 不含 vpci feature

> ```
> failed to start VM error=built without vpci support
> ```

默认 recipe `x64` 的 `openvmm_hcl_features` 只 `Tpm`. `openhcl/openvmm_hcl/Cargo.toml:27` 定义 `nvme = ["openvmm_hcl_resources/nvme", "vpci"]` 但**默认不启**.

需要重 build 时加 feature flag:
```bash
cargo +1.95 xflowey build-igvm x64 --release --override-openvmm-hcl-feature nvme
```

### ❌ 此前关于 "WS25 guest crash 是 Windows guest support 不完整" 的推测 完全错

实际上两个 guest (WS25 / Linux) 都是 **paravisor 自己 panic** 触发 Hyper-V "guest fatal error", 不是 guest OS crash. paravisor panic 后 VM reset, 看起来像 guest crash 但实际是 paravisor 死.

修两个 root cause 后预期 WS25 + Linux 都应能正常 boot 看到 NVMe device.

### 时间线

| 时段 | 事件 |
|------|------|
| v1 | Set-OpenHCLFirmware in-place, paravisor 加载 OK 但无 device |
| v2 | + Set-Vtl2Settings, host 报 32795 (in-place 路径 GET 未 init) |
| v3 (WS25) | destroy + recreate VM, host-side success. **paravisor JSON parse fail "NVMe" → panic** |
| Linux baseline | 同 v3 (fedora41 VHDX), 同 paravisor JSON parse fail panic |
| **NVMe→NVME fix** | paravisor 过 JSON parse, but 报 "built without vpci support" → panic |
| **rebuild with --override-openvmm-hcl-feature nvme** | TODO user re-build, 预期 paravisor 启动 NVMe device, guest 真出现 stornvme/nvme attach |

## 5. 推测原因 + Next path 选项

### Windows guest crash 推测 (历史 推测 已被实证为错, 见 §4)

| 假设 | 评估 |
|------|------|
| H1. OpenHCL paravisor 对 Windows guest 不完全支持 | **强** (Microsoft 公开测试只用 Linux, Azure Boost 也 Linux) |
| H2. KDNET BCD 配置与 paravisor 引导冲突 | 中 (VHDX 内有 KDNET; paravisor 可能重写 boot path) |
| H3. SecureBoot Off + Microsoft template 不兼容 | 中 (doc 推荐 SecureBoot Off, 但 Windows 可能要 SB) |
| H4. Windows VHDX 内 spike #01/02 driver 残留与 OpenHCL 冲突 | 弱 (boot crash 发生在 driver load 之前) |

### Next path

1. **完成 Linux baseline** — install Ubuntu 25.04 cloud img 验证 `nvme list` 出现 emulated NVMe. 确认 OpenHCL 架构 work 但**不直接解决 Windows stornvme.sys attach** 目标
2. **修 Windows guest crash** — fresh Windows install (无 KDNET BCD 污染) + 验证 OpenHCL Windows guest 是否实际支持
3. **完全放弃 OpenHCL Windows guest path** — 接受 Microsoft 当前 OpenHCL Windows guest support 不完整, 重评 ADR 0001 (FPGA / 项目终止)

## 6. 已 commit 内容

- `baseline/spike03-deploy-baseline.ps1` (v1 in-place Set-OpenHCLFirmware)
- `baseline/spike03-deploy-v2-nvme-translation.ps1` (v2 NVMe translation, 32795 失败)
- `baseline/spike03-recreate-vm-openhcl.ps1` (v3 destroy + recreate, success host-side)
- `baseline/spike03-linux-baseline.ps1` (Linux baseline, boot success)
- `evidence/baseline/` (vssd.txt, guids.txt, vtl2-settings.json, vm.txt, scsi.txt)
- `baseline/payload/` (openhcl-x64.bin, hyperv.psm1, utilities.psm1)
