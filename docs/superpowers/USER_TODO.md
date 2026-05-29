# 需要用户配合的事项 (2026-05-30 更新)

> **🎉 Path C 已闭环！** 真 Hyper-V 上 OpenHCL VTL2 + pcie_remote + vsock host
> 端到端验证成功。具体见 [SESSION_LOG.md](SESSION_LOG.md) "🎉🎉🎉 真 Hyper-V
> 端到端验证" 段。

## 当前状态

| 项 | 状态 | 备注 |
|---|---|---|
| 加入 Hyper-V Administrators | ✅ 用户完成 | `Get-VM` 可用 |
| 跨编 ohcldiag-dev.exe (WSL → Windows) | ✅ Claude 完成 | 用仓库官方 cross-compile 方案；不需要 xwin |
| 注册 vsock service GUID | ✅ Claude 完成 | gsudo cache 一次性 elevation |
| 创建 OpenHCL VM (正确方法) | ✅ Claude 完成 | `New-VM -GuestStateIsolationType OpenHCL`（关键！）|
| 自建 IGVM + Set-OpenHCL-HyperV-VM.ps1 加载 | ✅ Claude 完成 | `cargo xflowey build-igvm x64 --release --override-manifest <json>` |
| OpenHCL VTL2 diag_server | ✅ ohcldiag-dev 完全工作 | `ohcldiag-dev pcie-remote-exp inspect vm` 返回完整 inspect 树 |
| pcie_remote 真 vsock 握手 e2e | ✅ **vsock handshake ok, worker spawned** | host noop_host_vsock.exe ↔ VTL2 |
| K-8/K-11/K-15/K-17/K-18/K-19 spec gaps | ✅ 全部清零 | 48 tests pass |

## ✅ 已闭环路径

**OpenVMM 路径 (Linux KVM)：** 真 KVM 集成测试通过（前期工作）。

**OpenHCL 路径 (Hyper-V)：** 真 Hyper-V end-to-end 通过：
- 自建 OpenHCL IGVM 加载 ✅
- VTL2 pcie_remote 初始化 ✅
- VTL2 vsock listener 启动 ✅
- Host AF_HYPERV vsock client 连入 ✅
- Hello / HelloAck 协议交换 ✅
- Worker spawn ✅
- K-19 timeout 上限被实际触发拒绝 ✅
- §3.10 absent fallback 被实际触发 ✅

## 仍待用户参与的事项

### §1 (可选) CVM 真机验证

代码路径已覆盖 (AbsentPcieDevice 对 CVM 默认 + cvm_skip_pcie_remote
flag)，但 SNP/TDX/VBS 隔离 VM 上的真机端到端需要 confidential VM 硬件。
没有这类设备时跳过即可。

### §2 (可选) VTL0 guest OS 中真的 PCI 探测

当前实验 VM 没装 OS（用 `MemoryStartupBytes 2GB` 直接跑 OpenHCL/UEFI）。
要观察 VTL0 OS 真的把 emulated PCIe device "看到"，需要：

```powershell
# 加 VHD
$vmOsDisk = "C:\path\to\ubuntu-25.04-server.vhdx"
Set-VM -VM $vm -AutomaticCheckpointsEnabled $false
Add-VMHardDiskDrive -VMName pcie-remote-exp -Path $vmOsDisk
```

然后启动后在 guest 内 `lspci -nn | grep 1414:c0de` 应可见。

### §3 (可选) WSL2 /dev/mshv

对当前 Path C **不必要**（Path C 已闭环）。详见 `MSHV_DIAGNOSIS.md`。
保留作为未来想法。

---

## 历史 (已完成，保留参考)

### §A 把当前 Windows 用户加入 `Hyper-V Administrators`
```powershell
net localgroup "Hyper-V Administrators" puxu /add
# 然后注销重登 Windows
```

### §B 跨编 ohcldiag-dev.exe (WSL → Windows)
```bash
sudo apt install clang-tools-20  # 提供 clang-cl-20
mkdir -p ~/.local/bin
ln -sf $(rustup which rust-lld) ~/.local/bin/lld-link-20
./docs/superpowers/scripts/build-windows-cross.sh ohcldiag-dev
```

### §C Service GUID 注册
```powershell
gsudo cache on --duration 00:05:00
gsudo -d powershell -NoProfile -ExecutionPolicy Bypass -File register_openhcl_diag_guids.ps1
```

### §D 创建 OpenHCL VM (正确方法 — 关键！)

```powershell
# 1. 启 firmware-from-file
Set-ItemProperty 'HKLM:/Software/Microsoft/Windows NT/CurrentVersion/Virtualization' `
  -Name AllowFirmwareLoadFromFile -Value 1 -Type DWORD

# 2. 用 -GuestStateIsolationType OpenHCL **必须在创建时指定！**
$vm = New-VM -Name pcie-remote-exp -Generation 2 `
  -GuestStateIsolationType OpenHCL `
  -MemoryStartupBytes 2GB
Set-VM -VM $vm -AutomaticCheckpointsEnabled $false
Set-VMFirmware -VM $vm -EnableSecureBoot Off

# 3. 设 firmware file
& .\openhcl\Set-OpenHCL-HyperV-VM.ps1 -VM $vm -Path C:\path\to\openhcl-x64.bin
```

> **重要教训**：用 `New-CustomVM`（无 isolation type）+ 之后 vssd 字段编辑
> **不会**让 Hyper-V 加载自定义 IGVM。必须 `-GuestStateIsolationType OpenHCL`。

---

## 联系点

Path C 已闭环。如有新需求或想验证 CVM 真机，请提示 Claude。
