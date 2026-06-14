# spike03-deploy-baseline.ps1 — in-place enable OpenHCL on xdp-ws25-1
#
# 跑在 xpu-3.local (host with Hyper-V)
# 前置: openhcl-x64.bin 已 scp 到 C:\spike03\openhcl-x64.bin
#       hyperv.psm1 已 scp 到 C:\spike03\hyperv.psm1
#
# 目标: 在不 destroy 现有 xdp-ws25-1 VM 的前提下, 启用 OpenHCL paravisor
#       guest (WS2025) 内 vpci.sys 看到 OpenHCL 模拟的 NVMe controller + stornvme attach

$ErrorActionPreference = "Stop"
$Dir = "C:\spike03"
$VmName = "xdp-ws25-1"
$IgvmFile = "$Dir\openhcl-x64.bin"
$HypervPsm = "$Dir\hyperv.psm1"

function Phase($n) { Write-Output ""; Write-Output "=== $n ===" }

Phase "0. 环境 baseline 记录"
Get-VM $VmName | Format-List Name,State,Generation,Path | Out-Host
$origState = (Get-VM $VmName).State
Write-Output "Original state: $origState"

Phase "1. 启用 AllowFirmwareLoadFromFile (允许加载未签名 IGVM)"
$key = "HKLM:\Software\Microsoft\Windows NT\CurrentVersion\Virtualization"
$prev = (Get-ItemProperty -Path $key -Name AllowFirmwareLoadFromFile -ErrorAction SilentlyContinue).AllowFirmwareLoadFromFile
Write-Output "Previous AllowFirmwareLoadFromFile = $prev"
Set-ItemProperty -Path $key -Name "AllowFirmwareLoadFromFile" -Value 1 -Type DWORD
$now = (Get-ItemProperty -Path $key -Name AllowFirmwareLoadFromFile).AllowFirmwareLoadFromFile
Write-Output "Current AllowFirmwareLoadFromFile = $now"

Phase "2. 验证 IGVM 文件 + import hyperv.psm1"
if (-not (Test-Path $IgvmFile)) { throw "IGVM file missing: $IgvmFile" }
$sz = (Get-Item $IgvmFile).Length
Write-Output "openhcl-x64.bin size = $sz bytes"

if (-not (Test-Path $HypervPsm)) { throw "hyperv.psm1 missing: $HypervPsm" }
Import-Module $HypervPsm -Force
$cmd = Get-Command Set-OpenHCLFirmware -ErrorAction SilentlyContinue
if (-not $cmd) { throw "Set-OpenHCLFirmware 未导入" }
Write-Output "Set-OpenHCLFirmware OK"

Phase "3. Stop $VmName (if Running)"
if ((Get-VM $VmName).State -ne "Off") {
    Stop-VM -Name $VmName -TurnOff -Force
    while ((Get-VM $VmName).State -ne "Off") { Start-Sleep -Seconds 1 }
}
Write-Output "VM stopped"

Phase "4. 备份当前 VM config (for rollback)"
$backupDir = "$Dir\backup-$VmName-$(Get-Date -Format yyyyMMdd-HHmmss)"
New-Item -ItemType Directory -Path $backupDir -Force | Out-Null
Get-VM $VmName | Get-VMFirmware | Format-List | Out-File "$backupDir\vmfirmware.txt"
Get-VM $VmName | Format-List * | Out-File "$backupDir\vm.txt"
# VHDX 不复制, 太大; 关键是 VM config 备份够 rollback
Write-Output "Backup: $backupDir"

Phase "5. Set-OpenHCLFirmware on $VmName"
$vm = Get-VM $VmName
Set-OpenHCLFirmware -Vm $vm -IgvmFile $IgvmFile
Write-Output "Set-OpenHCLFirmware DONE"

Phase "6. Start $VmName"
Start-VM -Name $VmName
$startTime = Get-Date
while ((Get-VM $VmName).State -ne "Running" -and ((Get-Date) - $startTime).TotalSeconds -lt 60) {
    Start-Sleep -Seconds 1
}
$state = (Get-VM $VmName).State
Write-Output "VM state after Start: $state"

Phase "7. 收 baseline evidence"
$Ev = "$Dir\evidence-baseline-$(Get-Date -Format yyyyMMdd-HHmmss)"
New-Item -ItemType Directory -Path $Ev -Force | Out-Null
Get-VM $VmName | Get-VMFirmware | Format-List | Out-File "$Ev\vmfirmware-after-openhcl.txt"
Get-VM $VmName | Format-List * | Out-File "$Ev\vm-after-openhcl.txt"
# 等 guest OS 启动 + SSH 起来 后再 query stornvme - 这个 script 不做 (需要 wait guest boot)
Write-Output "Evidence: $Ev"
Write-Output ""
Write-Output "=== DONE — baseline phase 1 (host side) ==="
Write-Output "下一步: ssh xdp-ws25-1 -> query stornvme / pnp / nvme list"
