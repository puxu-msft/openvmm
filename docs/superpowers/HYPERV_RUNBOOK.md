# Hyper-V VM 创建 + OpenHCL + pcie_remote Runbook

> 这是一份**可逐行复制**的操作手册。所有 PowerShell 片段都在 Windows host 上跑。
> 完成 `USER_TODO.md §1`（把 puxu 加入 Hyper-V Administrators）后，**Claude 可以从 WSL 自动跑这些命令**，不再需要人工操作。

---

## 路径选择（先决定走哪条）

| 路径 | 适用场景 | 需要 |
|------|---------|------|
| **A. OpenVMM in WSL (linux-gnu, KVM)** | 已验证可跑（spec path D）；OpenVMM 进程内嵌 PCIe RC + pcie_remote | 已就绪，无需任何额外操作 |
| **B. OpenVMM in WSL (linux-gnu, /dev/mshv)** | 取代 KVM，能用 OpenHCL VTL2 | 需要 USER_TODO §3 启用 mshv |
| **C. Hyper-V on Windows + OpenHCL IGVM + NVMe takeover** | 这份 runbook 主体；最贴近生产 | 需要 USER_TODO §1（加组） |
| **D. OpenVMM.exe on Windows + WHP** | 适合 windows guest 测试 | 在 Windows 上 `cargo build -p openvmm` |

下面按 **路径 C** 展开（最复杂、最有 OpenHCL 实战价值的一种）。

---

## 路径 C — 完整步骤

### 0. 前置条件（验证）

```powershell
# 从 WSL：
# /mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe -NoProfile -Command "..."

Get-Service vmms,vmcompute | Format-Table
# 期望：vmms = Automatic+Running，vmcompute = Manual+Running

Get-VMHost | Select-Object Name,VirtualMachinePath,VirtualHardDiskPath | Format-List
# 期望：能输出（如果输出空，说明 USER_TODO §1 没完成）

# Windows 版本（NVMe controller 需 Windows 11 24H2+ / Server 2025+）
[Environment]::OSVersion.Version
# 期望：>= 10.0.26100
```

### 1. 准备 IGVM 文件

```powershell
# 从 WSL 仓库根目录，先 build：
# cargo xflowey build-igvm x64 --release

# 在 Windows 上读 IGVM 文件路径（通过 \\wsl$\）：
$Igvm = '\\wsl.localhost\Ubuntu\home\xp\refs\openvmm\flowey-out\artifacts\build-igvm\ship\x64\openhcl-x64.bin'
Test-Path $Igvm
# 期望：True；显示文件大小约 19 MB

# 如果 WSL 发行版名不是 Ubuntu，用 wsl -l -q 查
```

> ⚠ Hyper-V 默认 VM firmware 是 MSVM UEFI；OpenHCL IGVM 通过 `Set-VMFirmware -FirmwareFile` 或更新的 `Set-VMFirmware -IgvmFile` 加载。**注意**：该字段在 Microsoft 内部 PowerShell module 才有；公版 Hyper-V cmdlet 可能没有。如果 Set-VMFirmware 拒绝，需要走 Microsoft "OpenHCL on Hyper-V" 的另一套配置（见步骤 9 备选）。

### 2. 创建一个新 VM（最小配置）

```powershell
$VmName = 'PcieRemoteExp'
$VhdPath = (Get-VMHost).VirtualHardDiskPath + "\$VmName.vhdx"
# 1G 内存、Gen2、Linux/Windows 都可
New-VM -Name $VmName -Generation 2 -MemoryStartupBytes 1GB -NewVHDPath $VhdPath -NewVHDSizeBytes 8GB
Set-VMProcessor -VMName $VmName -Count 2
Set-VMMemory -VMName $VmName -DynamicMemoryEnabled $false -StartupBytes 1GB

# 默认 secure boot 是开的；OpenHCL preview 不一定支持，关掉以稳妥：
Set-VMFirmware -VMName $VmName -EnableSecureBoot Off

# 把 vmbus 默认网络断开（不需要）
Get-VMNetworkAdapter -VMName $VmName | Remove-VMNetworkAdapter
```

### 3. 添加占位 NVMe controller（spec §3.1 Path C 的核心）

```powershell
# 24H2+ 才有 Add-VMNvmeController
$cmd = Get-Command Add-VMNvmeController -ErrorAction SilentlyContinue
if (-not $cmd) {
    Write-Host '此 Windows 版本没有 Add-VMNvmeController。走 path B (mshv) 或升级 Windows。'
    return
}

Add-VMNvmeController -VMName $VmName
$Ctrl = (Get-VMNvmeController -VMName $VmName)[0]
$NvmeGuid = $Ctrl.Id
Write-Host "Takeover GUID = $NvmeGuid"
# 把这个 GUID 抄下来，下面给 OpenHCL cmdline 用
```

### 4. 一次性注册 vsock service GUID + ACL

```powershell
# 从 WSL 路径运行 setup-pcie-remote.ps1
$Setup = '\\wsl.localhost\Ubuntu\home\xp\refs\openvmm\docs\superpowers\scripts\setup-pcie-remote.ps1'
& $Setup -VsockPort 50000
# 输出 'Registered service GUID: ... (ACL: Admin/SYSTEM only)'

# 验证：
Get-Item 'HKLM:\Software\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices' |
  Get-ChildItem | Where-Object { $_.PSChildName -like '*facb*' }
```

### 5. 把 OpenHCL IGVM 配到 VM

> ⚠ 这一步是**最不确定**的，因 Hyper-V 自定义 IGVM 加载是 Microsoft 私有 cmdlet (`Set-VMFirmwareFile`) 或需要 `vmwp.exe` 的特殊配置。

```powershell
# 方法 A：如果有 Set-VMFirmware -IgvmFile（preview build 才有）
try {
    Set-VMFirmware -VMName $VmName -IgvmFile $Igvm
} catch {
    Write-Host 'Set-VMFirmware -IgvmFile 不支持。'
    Write-Host '试 Set-VmCommandLine（Microsoft Internal）...'
}

# 方法 B：通过注册表（hack；仅供实验，不推荐生产）
# (Microsoft 内部走 vmwp.exe + IGVM 文件路径。开源 Hyper-V 上需要其它机制。)
```

### 6. 设置 OpenHCL cmdline（注入 PCIe Remote takeover）

OpenHCL cmdline 来自 IGVM measured cmdline（build 时编进去）+ host append（如果 IGVM 的 policy 是 APPEND_CHOSEN）。

#### 选项 A：自建 IGVM 时把 cmdline 编进去（最稳）

```bash
# 在 WSL 重新 build IGVM，加 --extra-cmdline
cargo xflowey build-igvm x64 --release -- \
  --extra-cmdline "OPENHCL_PCIE_REMOTE_TAKEOVER=$NvmeGuid:50000"
# (xflowey 是否支持 --extra-cmdline 由 igvmfilegen manifest 决定，可能要改 manifest YAML)
```

#### 选项 B：通过 Hyper-V 注册表 / WMI 注入

```powershell
# Hyper-V 6.0+ 把 host append cmdline 存在 VM 设置里。具体字段名因 build 不同。
# 一般通过 Get-VM | Select-Object * 找含 "CommandLine" 的字段。
Get-VM -Name $VmName | Get-Member | Select-String CommandLine
```

如果两种都不通，**回退到 path B**（mshv）—— OpenVMM 启 OpenHCL 时通过环境变量直接传 cmdline，最干净。

### 7. 启动 host 端实验程序（vsock client）

在 WSL 端跑（**这部分是 Claude 自动**）：

```bash
cd /home/xp/refs/openvmm/docs/superpowers/examples/pcie_remote_noop_host
# 当前 noop_host 是 TCP loopback；vsock client 版本待补（USER_TODO §4 由 Claude 做）
PORT=50000 cargo run
```

### 8. 启动 VM

```powershell
Start-VM -Name $VmName
# 等 5 秒
Start-Sleep 5
Get-VM -Name $VmName | Select-Object Name,State,Uptime
# 抓 guest serial / kvp 输出
```

### 9. 验证 guest 看到了 pcie_remote 设备

```powershell
# Hyper-V KVP（IC）：guest 内 lspci 输出不能直接抓，只能进 guest 看
# 进 vmconnect 或 ssh 进 guest：
vmconnect localhost $VmName
# guest 内：lspci 应看到 vendor 1414 / device c0de 的设备
```

### 10. 清理

```powershell
Stop-VM -Name $VmName -Force
Remove-VM -Name $VmName -Force
Remove-Item $VhdPath -Force
```

---

## 路径 A — OpenVMM in WSL (KVM)，已验证

```bash
# Terminal 1: host stub (client mode)
cd /home/xp/refs/openvmm
cargo run --manifest-path docs/superpowers/examples/pcie_remote_noop_host/Cargo.toml

# Terminal 2: OpenVMM with --hv (KVM)
sg kvm "target/debug/openvmm \
    --processors 1 --memory 512M --hv \
    --uefi --uefi-firmware flowey-persist/flowey_lib_hvlite__download_uefi_mu_msvm/extracted/RELEASE-X64-VS2022-artifacts.tar.gz/FV/MSVM.fd \
    --pcie-root-complex rc0,segment=0,start_bus=0,end_bus=255,low_mmio=4M,high_mmio=1G \
    --pcie-root-port rc0:rc0rp0 \
    --pcie-remote rc0rp0,socket=127.0.0.1:48914"
```

预期日志：
```
pcie_remote_device::handshake_spawn: pcie_remote: TCP handshake ok, worker spawned
add_device device="pcie:rc0rp0-pcie_remote_tcp"  ← 真 PcieRemoteDevice 加载
host stub: received Hello ... sent HelloAck
```

---

## 故障排查

| 现象 | 原因 | 解决 |
|------|------|------|
| `Get-VM` 静默无输出 | 不在 Hyper-V Administrators 组 | USER_TODO §1 |
| `Add-VMNvmeController` not found | Windows < 24H2 | 升级 Windows，或走路径 B (mshv) |
| `setup-pcie-remote.ps1` 报权限 | 注册表写入需要 Admin | 用提权的 PowerShell 跑 setup.ps1 |
| OpenVMM `vtl2 is not supported on this hypervisor` | KVM 不支持 VTL2 | 走 mshv（path B）或 Hyper-V（path C） |
| `pcie_remote handshake missing; serving AbsentPcieDevice` | host stub 没在 OpenVMM bind 之后 connect 上 | host stub 启动后等 2s 再启 OpenVMM；或扩大 handshake_timeout_ms |
| TCP bind failed Address already in use | 你之前忘了关 host stub | `pkill -f pcie_remote_noop_host` |
