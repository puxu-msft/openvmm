# Hyper-V VM 创建 + OpenHCL + pcie_remote Runbook

> **2026-05-30 更新：Path C 已端到端验证通过。** 本 runbook 经实测核对，
> 删除了所有不准确的"未确定/可能"段落。请优先阅读本 runbook 而不是历史
> 段落如 USER_TODO 旧版备份。

完成 [USER_TODO.md](USER_TODO.md) §A（加 Hyper-V Administrators 组）后，
Claude 可从 WSL 用 `/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe`
驱动所有命令，**不再需要人工**。

---

## 路径选择

| 路径 | 适用 | 当前状态 |
|------|------|---------|
| **A. OpenVMM in WSL (linux-gnu, KVM)** | 快速本地实验；OpenVMM 进程内嵌 RC + pcie_remote | ✅ 已验证 |
| **B. OpenVMM in WSL (linux-gnu, /dev/mshv)** | 替代 KVM；能跑 OpenHCL VTL2 | ❌ WSL 默认 kernel 无 `CONFIG_MSHV_ROOT`（详见 [MSHV_DIAGNOSIS.md](MSHV_DIAGNOSIS.md)）；**Path C 已闭环，不需要这条**|
| **C. Hyper-V on Windows + OpenHCL IGVM + vsock** | 最贴近生产 | ✅ **已验证**（本 runbook 主体）|
| **D. OpenVMM.exe on Windows + WHP** | Windows guest 测试 | Windows 端 `cargo build -p openvmm` |

下面 **路径 C** 完整步骤。

---

## 路径 C — 完整步骤（已验证）

### 0. 前置条件

```powershell
# 从 WSL 调：
# /mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe -NoProfile -Command "..."
# (Windows PowerShell 5.x；如果装了 PowerShell 7+，路径是
#  /mnt/c/Program Files/PowerShell/7/pwsh.exe，参数兼容。)

Get-Service vmms,vmcompute | Format-Table
# 期望：vmms = Automatic+Running，vmcompute = Manual+Running

Get-VMHost | Select-Object Name,VirtualMachinePath,VirtualHardDiskPath | Format-List
# 期望：能输出（如果输出空，说明 USER_TODO §A 没完成 — 加入 Hyper-V Administrators 组）

[Environment]::OSVersion.Version
# 实测 OK：10.0.26200.x（Windows 11 24H2 build 26100+）
# 文档要求 24H2+；旧版没有 -GuestStateIsolationType OpenHCL 支持。

# 开启自定义 IGVM 加载（仅需一次，Admin 权限）
Set-ItemProperty 'HKLM:/Software/Microsoft/Windows NT/CurrentVersion/Virtualization' `
  -Name AllowFirmwareLoadFromFile -Value 1 -Type DWORD
```

### 1. 准备 OpenHCL IGVM 文件

```bash
# WSL 内（仓库根目录）。先 build 标准 release IGVM：
cargo xflowey build-igvm x64 --release

# 输出位置（默认 build label 是 'x64'）：
# /home/xp/refs/openvmm/flowey-out/artifacts/build-igvm/ship/x64/openhcl-x64.bin
```

如果需要给 cmdline 注入 pcie_remote 配置，用自定义 manifest + `--override-manifest`：

```bash
cat > /tmp/openhcl-x64-pcie.json <<'JSON'
{
    "guest_arch": "x64",
    "guest_configs": [{
        "guest_svn": 1,
        "max_vtl": 2,
        "isolation_type": "none",
        "image": {
            "openhcl": {
                "command_line": "OPENHCL_PCIE_REMOTE_INSTANCE=11111111-2222-3333-4444-555555555555:50000,handshake_timeout_ms=2000",
                "memory_page_count": 131072,
                "uefi": true
            }
        }
    }]
}
JSON

cargo xflowey build-igvm x64 --release --override-manifest /tmp/openhcl-x64-pcie.json -o pcie-test
# 输出 -> flowey-out/artifacts/build-igvm/ship/pcie-test/openhcl-pcie-test.bin
```

把产物拷到 Windows 可访问位置：

```bash
cp flowey-out/artifacts/build-igvm/ship/pcie-test/openhcl-pcie-test.bin /mnt/c/temp/pcie_remote_exp/
```

> ⚠ **handshake_timeout_ms 上限 = config_timeout/2**（K-19）。
> 默认 config_timeout = 5s，所以 handshake_timeout_ms ≤ 2500。
> 设过大会被 underhill_core 启动期拒绝，日志：
> `pcie_remote: ... rejected (handshake_timeout_ms=N > max M=config_timeout/2); skipping`。

### 2. 创建 VM —— **必须用 `-GuestStateIsolationType OpenHCL`**

> 🔥 **真根因发现**：用 plain `New-VM` 或 petri 的 `New-CustomVM` 创建后，
> 再编辑 vssd 的 `GuestFeatureSet` / `FirmwareFile` **Hyper-V 会完全
> 静默忽略**，加载 stock Msvm UEFI，VTL2 根本不存在。诊断只看到
> "No bootable devices configured" + ohcldiag-dev WSA 10060。
>
> 正解参 `Guide/src/user_guide/openhcl/run/hyperv.md`（仓库自带）。

```powershell
$VmName = 'pcie-remote-exp'
# 如果你只跑了 §1 第一条 cargo xflowey build-igvm 命令，IGVM 是 'openhcl-x64.bin'；
# 如果跑了 --override-manifest 的第二条，IGVM 是 'openhcl-pcie-test.bin'。
$IgvmFile = 'C:\temp\pcie_remote_exp\openhcl-pcie-test.bin'

# 如果有旧的，先删
$old = Get-VM -Name $VmName -ErrorAction SilentlyContinue
if ($old) {
    if ($old.State -ne 'Off') { Stop-VM -Name $VmName -TurnOff -Force }
    Remove-VM -Name $VmName -Force
}

# 关键：-GuestStateIsolationType OpenHCL 必须在创建时指定
$vm = New-VM -Name $VmName -Generation 2 `
    -GuestStateIsolationType OpenHCL `
    -MemoryStartupBytes 2GB
Set-VM -VM $vm -AutomaticCheckpointsEnabled $false
Set-VMFirmware -VM $vm -EnableSecureBoot Off

# 用仓库自带脚本把 IGVM file 设到 vssd
# (Set-OpenHCL-HyperV-VM.ps1 内部走 ModifySystemSettings)
& C:\temp\pcie_remote_exp\Set-OpenHCL-HyperV-VM.ps1 -VM $vm -Path $IgvmFile

# 检查
Get-VM $VmName | Select-Object Name,Version,State,Generation,IsolationType
```

> 📝 `Set-OpenHCL-HyperV-VM.ps1` 是仓库根 `openhcl/Set-OpenHCL-HyperV-VM.ps1`。
> 拷到 Windows 上跑（因为它要 `Set-ExecutionPolicy Bypass` + 写 vssd）。

### 3. (可选) 注册 vsock service GUID

> ⚠ **实测发现：不是必需的**。`-GuestStateIsolationType OpenHCL` 创建的 VM
> 在 vsock 路径上由 Hyper-V 自动处理 routing；ohcldiag-dev 直接走 AF_HYPERV
> 高 VTL channel 工作，不依赖 GuestCommunicationServices reg key。
>
> 如果你的 host 端程序是本仓库提供的 `pcie_remote_noop_host_vsock.exe`，
> AF_HYPERV connect 内核会自动处理 ACL，**跳过本节即可**。本节只在你想
> 给**任意第三方用户态 service** 注册 GUID 白名单时才需要。

```powershell
& C:\temp\pcie_remote_exp\setup-pcie-remote.ps1 -VsockPort 50000
# 输出 'Registered service GUID: 0000c350-facb-11e6-bd58-64006a7986d3'
```

### 4. 启动 VM

```powershell
Start-VM -Name $VmName
Start-Sleep -Seconds 5

# 验证 OpenHCL VTL2 起来了
C:\temp\pcie_remote_exp\ohcldiag-dev.exe $VmName inspect vm | Select-Object -First 10
# 期望看到 inspect tree (battery / chipset / vm / ...);
# 如果返回 'error: ... timeout / 10060'，回查 §2 (没用 -GuestStateIsolationType OpenHCL)
```

### 5. 跑 host vsock client (pcie_remote 对端)

```powershell
$vmid = (Get-VM pcie-remote-exp).Id
# AF_HYPERV 客户端（vsock_main.rs 跨编自 docs/superpowers/examples/pcie_remote_noop_host）
C:\temp\pcie_remote_exp\pcie_remote_noop_host_vsock.exe `
    --vm-id $vmid --port 50000 --retries 60 --retry-ms 500
```

### 5b. (可选) Stress 模式 — 验证 K-NEW-C rate limit + A4 Lost

noop_host 加了两个 stress 参数，可主动触发健壮性边界：

```powershell
# 验证 K-NEW-C: DMA 速率限制 (64 MiB/s)
# burst 发 2048 个 64KB ReadGpa = 128 MiB → 1024 个应被 rate limit 拒绝
pcie_remote_noop_host_vsock.exe --vm-id $vmid --port 50000 `
    --stress-dma-count 2048
# 然后看 ohcldiag-dev pcie-remote-exp inspect "vm/pcie_remote_vmbus:.../worker_stats"
# 应见 dma_rate_limit_rejects 接近 1024

# 验证 A4: 连续 ≥4 OOB InterruptFire → worker 进 Lost
pcie_remote_noop_host_vsock.exe --vm-id $vmid --port 50000 `
    --stress-bad-frames 8
# 应见 consecutive_bad_frames=4，OpenHCL kmsg "going Lost consecutive=0x4"
```

不指定 stress 参数 = 默认 periodic 模式（每 5s InterruptFire，每 15s ReadGpa，每 20s WriteGpa）。

### 6. 验证端到端

```powershell
# host 端日志（noop_host_vsock 自身 stdout）
# INFO pcie_remote_noop_host_vsock: vsock client connecting vm_id=... port=50000
# INFO pcie_remote_noop_host_vsock: connected
# INFO pcie_remote_noop_host_vsock: received Hello magic=0x52504345 version=1
# INFO pcie_remote_noop_host_vsock: sent HelloAck

# OpenHCL VTL2 端日志（从 host 调 ohcldiag-dev）
C:\temp\pcie_remote_exp\ohcldiag-dev.exe pcie-remote-exp kmsg | Select-String pcie_remote
# 期望（实测真 Hyper-V 输出）：
# [2.346256] pcie_remote_device::handshake_spawn:
#   INFO  pcie_remote: vsock handshake ok, worker spawned
#   id=11111111-2222-3333-4444-555555555555
```

✅ 看到这两条 = Path C 闭环。

### 7. (可选) 让 guest OS 真"看到"PCI 设备

到这里 VTL2 已经组装 `PcieRemoteDevice` 并 publish 给 VTL0 的 vpci，
但 VTL0 此时是 UEFI shell（VM 没 VHD/OS），无法跑 `lspci`。
要看 guest OS 探测：

```powershell
# 给 VM 加 VHDX
Add-VMHardDiskDrive -VMName pcie-remote-exp `
    -Path C:\path\to\ubuntu-25.04-server.vhdx
# 重启 VM → guest 启动 → 进 guest 跑：
#   sudo lspci -nn | grep -i 1414
# 期望见到 vendor_id 1414 device_id c0de 的 emulated PCIe device
```

### 8. 清理

```powershell
Stop-VM -Name pcie-remote-exp -TurnOff -Force
Remove-VM -Name pcie-remote-exp -Force
```

---

## 路径 A — OpenVMM in WSL (KVM)，已验证

```bash
# Terminal 1: host stub (TCP server mode)
cd /home/xp/refs/openvmm
cargo run --manifest-path docs/superpowers/examples/pcie_remote_noop_host/Cargo.toml --bin pcie_remote_noop_host_tcp

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
add_device device="pcie:rc0rp0-pcie_remote_tcp"
host stub: received Hello ... sent HelloAck
```

---

## 故障排查

| 现象 | 原因 | 解决 |
|------|------|------|
| `Get-VM` 静默无输出 | 不在 Hyper-V Administrators 组 | USER_TODO §A |
| `New-VM` 没有 `-GuestStateIsolationType` 参数 | Windows < 24H2 | 升级到 build 26100+ |
| VM start 后 ohcldiag-dev 10060 timeout + 事件日志 `No bootable devices` | **创建时没加 `-GuestStateIsolationType OpenHCL`**（最常见！）| 见 §2；vssd 编辑不能补救，必须重建 VM |
| ohcldiag-dev `10049 EADDRNOTAVAIL` | vsock service GUID 没注册 | 跑 §3 `setup-pcie-remote.ps1` |
| ohcldiag-dev `10061 ECONNREFUSED` | host 路由 OK 但 VTL0 没 listener（vsock 默认到 VTL0） | 确认 ohcldiag-dev 用 HIGH_VTL；或自定义 vsock client 调 `set_high_vtl(true)` |
| pcie_remote `rejected (handshake_timeout_ms=N > max M)` | K-19 — handshake_timeout > config_timeout/2 | IGVM cmdline 改小 handshake_timeout_ms（≤2500ms） |
| pcie_remote `vsock handshake timeout; device absent` | host client 没在 VTL2 listener 启动后 2s 内连入 | 先启 VM，立刻并行启 host client，retries=60 retry-ms=500 |
| OpenVMM `vtl2 is not supported on this hypervisor` | KVM 不支持 VTL2 | 走 Hyper-V (path C) |
| TCP bind failed Address already in use | host stub 还在跑 | `pkill -f pcie_remote_noop_host` |

---

## 工件清单（仓库相对路径）

| 工件 | 用途 |
|---|---|
| `openhcl/Set-OpenHCL-HyperV-VM.ps1` | Microsoft 官方：设 vssd `GuestFeatureSet=0x201` + `FirmwareFile`（已被 §2 New-VM 创建成功后才有意义）|
| `docs/superpowers/scripts/setup-pcie-remote.ps1` | 注册 vsock service GUID + ACL |
| `docs/superpowers/scripts/hyperv/` | 历史 PS 脚本（部分已过时，见各文件头部 deprecation note）|
| `docs/superpowers/examples/pcie_remote_noop_host/` | host 端 TCP 与 vsock client（OpenVMM / OpenHCL 对端）|
| `docs/superpowers/examples/vmrs_log_scanner/` | `.vmrs` RAM 字符串扫描器（OpenHCL 启动失败时诊断用；已用于定位本节"真根因发现"中的 retrofit 路径加载失败问题，详见 [SESSION_LOG.md](SESSION_LOG.md)）|
| `Guide/src/user_guide/openhcl/run/hyperv.md` | Microsoft 官方 OpenHCL on Hyper-V 文档（本 runbook 的依据）|
