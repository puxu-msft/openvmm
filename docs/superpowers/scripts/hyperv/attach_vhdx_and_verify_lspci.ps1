# Attach an OS VHDX to the pcie-remote-exp VM and verify that the guest
# Windows enumerates the pcie_remote device (vendor 0x1414, device 0xc0de).
#
# 步骤：
#   1. Stop VM if running
#   2. Add-VMHardDiskDrive (idempotent — 若已附则 skip)
#   3. Set boot order: hdd first
#   4. Start VM
#   5. Wait for guest 可登录（PowerShell Direct credential check）
#   6. 在 guest 内跑 Get-PnpDevice + pnputil / Get-WmiObject Win32_PnPEntity
#      过滤 vendor "1414" / device "C0DE" 显示发现的实例
#   7. 跑 ohcldiag-dev inspect 比对 host 侧 worker_stats（state=Live, mmio_read_results>0 等）
#
# 用法：
#   .\attach_vhdx_and_verify_lspci.ps1 -VMName pcie-remote-exp `
#                                       -VhdxPath C:\temp\pcie_remote_exp\guest.vhdx `
#                                       -AdminPassword PcieRemote123! `
#                                       -OhcldiagDevPath C:\path\to\ohcldiag-dev.exe
#
# 退出码：0 = guest 真见到 pcie_remote；1 = guest 没看到；2 = guest 没起来；3 = VM/VHDX 异常

[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$VMName,
    [Parameter(Mandatory=$true)][string]$VhdxPath,
    [string]$AdminPassword = 'PcieRemote123!',
    [string]$OhcldiagDevPath = '',  # 可选；找不到则跳过 host 侧对比
    [int]$BootTimeoutSec = 600
)

$ErrorActionPreference = 'Stop'

function Write-Step($msg) {
    Write-Host ""
    Write-Host "================ $msg ================" -ForegroundColor Cyan
}

function Fail($code, $msg) {
    Write-Host "[FAIL] $msg" -ForegroundColor Red
    exit $code
}

# --- 1. VM 状态预检 ---
Write-Step "1. 预检 VM '$VMName'"
$vm = Get-VM -Name $VMName -ErrorAction SilentlyContinue
if (-not $vm) { Fail 3 "VM '$VMName' 不存在；先用 setup_openhcl_vm.ps1 创建" }
if ($vm.State -eq 'Running') {
    Write-Host "VM 在跑，先停"
    Stop-VM -Name $VMName -Force -TurnOff
    Start-Sleep -Seconds 3
}

if (-not (Test-Path $VhdxPath)) {
    Fail 3 "VHDX '$VhdxPath' 不存在；先跑 build_winserver_vhdx.ps1"
}

# --- 2. Add-VMHardDiskDrive (idempotent) ---
Write-Step "2. 挂 VHDX → VM"
$existing = Get-VMHardDiskDrive -VMName $VMName -ErrorAction SilentlyContinue
$alreadyAttached = $existing | Where-Object { $_.Path -eq $VhdxPath }
if ($alreadyAttached) {
    Write-Host "VHDX 已挂在 SCSI $($alreadyAttached.ControllerNumber):$($alreadyAttached.ControllerLocation)；skip"
} else {
    Add-VMHardDiskDrive -VMName $VMName -Path $VhdxPath
    Write-Host "已挂"
}

# --- 3. 设 boot order: 硬盘优先 ---
Write-Step "3. 设 boot order (硬盘优先)"
$bootDevs = Get-VMFirmware -VMName $VMName | Select-Object -ExpandProperty BootOrder
$hddBoot = $bootDevs | Where-Object { $_.BootType -eq 'Drive' -and $_.Device.Path -eq $VhdxPath }
if ($hddBoot) {
    Set-VMFirmware -VMName $VMName -FirstBootDevice $hddBoot.Device
    Write-Host "硬盘设为首启动"
} else {
    Write-Host "找不到 BootOrder entry；维持默认顺序"
}

# --- 4. 启动 VM ---
Write-Step "4. 启动 VM"
Start-VM -Name $VMName
Write-Host "等 guest 起来（PowerShell Direct credential check）..."

$secPwd = ConvertTo-SecureString $AdminPassword -AsPlainText -Force
$cred = New-Object System.Management.Automation.PSCredential('administrator', $secPwd)

$deadline = (Get-Date).AddSeconds($BootTimeoutSec)
$session = $null
do {
    Start-Sleep -Seconds 10
    try {
        $session = New-PSSession -VMName $VMName -Credential $cred -ErrorAction Stop
        Write-Host "PSSession 建立成功" -ForegroundColor Green
        break
    } catch {
        Write-Host "  ... 还没起来 ($(($deadline - (Get-Date)).TotalSeconds)s remaining)"
    }
} while ((Get-Date) -lt $deadline)

if (-not $session) { Fail 2 "guest 在 $BootTimeoutSec 秒内没接受 PSSession（可能没装好 OS）" }

# --- 5. guest 内枚举 PCI 设备 ---
Write-Step "5. guest 内枚举 PCIe 设备"
$pciDevices = Invoke-Command -Session $session -ScriptBlock {
    # vendor 0x1414 (Microsoft), device 0xc0de
    Get-CimInstance -ClassName Win32_PnPEntity -Filter "PNPDeviceID like 'PCI%VEN_1414%DEV_C0DE%'" |
        Select-Object Name, Status, PNPDeviceID, DeviceID, ConfigManagerErrorCode
}

if (-not $pciDevices) {
    Write-Host "guest 没在 Win32_PnPEntity 看到 VEN_1414&DEV_C0DE；列全部 PCI 设备求证：" -ForegroundColor Yellow
    $allPci = Invoke-Command -Session $session -ScriptBlock {
        Get-CimInstance -ClassName Win32_PnPEntity -Filter "PNPDeviceID like 'PCI%'" |
            Select-Object -ExpandProperty PNPDeviceID
    }
    $allPci | ForEach-Object { Write-Host "  $_" }
    Remove-PSSession $session
    Fail 1 "guest 未看到 pcie_remote 设备"
}

Write-Host "guest 看到 $($pciDevices.Count) 个 VEN_1414&DEV_C0DE 实例：" -ForegroundColor Green
$pciDevices | Format-Table -AutoSize

# --- 6. host 侧 ohcldiag-dev 对比（可选）---
if ($OhcldiagDevPath -and (Test-Path $OhcldiagDevPath)) {
    Write-Step "6. host 侧 ohcldiag-dev inspect 对比"
    $stats = & $OhcldiagDevPath $VMName inspect 'vm/openhcl/openhcl_core/devices' 2>&1
    Write-Host "$stats"
} else {
    Write-Host "[INFO] OhcldiagDevPath 未提供或路径无效；跳过 host 侧对比"
}

# --- 7. cleanup ---
Remove-PSSession $session
Write-Step "完成"
Write-Host "guest 看到 pcie_remote 设备 ✅" -ForegroundColor Green
exit 0
