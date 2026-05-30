# Create OpenHCL VM the CORRECT way (using -GuestStateIsolationType OpenHCL).
#
# 这是 2026-05-30 验证通过的正确流程，取代旧的 setup_openhcl_vm.ps1 +
# create_and_boot_openhcl.ps1 + switch_igvm.ps1（那些用 New-CustomVM 路径，
# Hyper-V 实测会静默忽略 retrofit 的 vssd OpenHCL 字段，VM 启动到 stock
# Msvm UEFI，不会加载 OpenHCL VTL2）。
#
# 真根因 + 详细排查见 docs/superpowers/SESSION_LOG.md "🎉🎉🎉 真 Hyper-V
# 端到端验证" 段；用户视角操作说明见 docs/superpowers/HYPERV_RUNBOOK.md §2。
#
# Prereqs:
#   - Windows >= 24H2 (build 26100+, 提供 -GuestStateIsolationType OpenHCL)
#   - 当前用户在 Hyper-V Administrators 组
#   - HKLM\Software\Microsoft\Windows NT\CurrentVersion\Virtualization\AllowFirmwareLoadFromFile = 1 (DWORD)
#   - openhcl/Set-OpenHCL-HyperV-VM.ps1 (仓库自带) 已拷到 $SetupScript 指定路径
#   - IGVM 文件已 build (cargo xflowey build-igvm x64 --release [--override-manifest ...]) 并拷到 $IgvmFile
#
# Usage:
#   .\create_openhcl_vm_correct.ps1
#   .\create_openhcl_vm_correct.ps1 -VmName foo -IgvmFile C:\path\openhcl-x64.bin

param(
    [string]$VmName = 'pcie-remote-exp',
    [string]$IgvmFile = 'C:\temp\pcie_remote_exp\openhcl-pcie-test.bin',
    [int]$MemoryGB = 2,
    [string]$SetupScript = 'C:\temp\pcie_remote_exp\Set-OpenHCL-HyperV-VM.ps1'
)
$ErrorActionPreference = 'Stop'

# 0. 验证 IGVM 文件存在
if (-not (Test-Path $IgvmFile)) {
    throw "IGVM file not found: $IgvmFile`n请先 cargo xflowey build-igvm x64 --release 然后 cp 到此路径。"
}
if (-not (Test-Path $SetupScript)) {
    throw "Set-OpenHCL-HyperV-VM.ps1 not found: $SetupScript`n请从仓库 openhcl/Set-OpenHCL-HyperV-VM.ps1 拷过去。"
}

# 1. 验证 AllowFirmwareLoadFromFile reg
$reg = Get-ItemProperty 'HKLM:\Software\Microsoft\Windows NT\CurrentVersion\Virtualization' `
    -Name AllowFirmwareLoadFromFile -ErrorAction SilentlyContinue
if (-not $reg -or $reg.AllowFirmwareLoadFromFile -ne 1) {
    Write-Warning "AllowFirmwareLoadFromFile 不是 1。需要 Admin 运行："
    Write-Warning "  Set-ItemProperty 'HKLM:\Software\Microsoft\Windows NT\CurrentVersion\Virtualization' -Name AllowFirmwareLoadFromFile -Value 1 -Type DWORD"
}

# 2. 删除旧 VM（如果存在）
$old = Get-VM -Name $VmName -ErrorAction SilentlyContinue
if ($old) {
    Write-Host "Removing existing VM '$VmName' (state: $($old.State))..."
    if ($old.State -ne 'Off') {
        Stop-VM -Name $VmName -TurnOff -Force
        Start-Sleep -Seconds 2
    }
    Remove-VM -Name $VmName -Force
}

# 3. 关键步骤：必须在 New-VM 时指定 -GuestStateIsolationType OpenHCL。
#    后期 ModifySystemSettings 改 vssd 的 GuestFeatureSet / FirmwareFile **无效**。
Write-Host "Creating VM '$VmName' with -GuestStateIsolationType OpenHCL..."
$vm = New-VM -Name $VmName -Generation 2 `
    -GuestStateIsolationType OpenHCL `
    -MemoryStartupBytes (${MemoryGB} * 1GB)

Set-VM -VM $vm -AutomaticCheckpointsEnabled $false
Set-VMFirmware -VM $vm -EnableSecureBoot Off
Write-Host "VM Version: $($vm.Version) (要求 >= 12.0)"

# 4. 把 OpenHCL IGVM 文件路径写到 vssd
#    Set-OpenHCL-HyperV-VM.ps1 内部走 ModifySystemSettings 设 vssd.FirmwareFile + GuestFeatureSet=0x201
Write-Host "Setting OpenHCL firmware file: $IgvmFile"
& $SetupScript -VM $vm -Path $IgvmFile

# 5. 验证 vssd
$vmid = $vm.Id
$cimvm = Get-CimInstance -namespace root/virtualization/v2 `
    -query "select * from Msvm_ComputerSystem where Name = '$vmid'"
$vssd = $cimvm | Get-CimAssociatedInstance `
    -ResultClass Msvm_VirtualSystemSettingData `
    -Association Msvm_SettingsDefineState

Write-Host ""
Write-Host "=== vssd verification ==="
Write-Host "GuestFeatureSet:         0x$('{0:X}' -f $vssd.GuestFeatureSet)"
Write-Host "FirmwareFile:            $($vssd.FirmwareFile)"
Write-Host "VirtualSystemSubType:    $($vssd.VirtualSystemSubType)"
Write-Host "SecureBootEnabled:       $($vssd.SecureBootEnabled)"
Write-Host ""
Write-Host "Done. Next:"
Write-Host "  Start-VM -Name $VmName"
Write-Host "  Start-Sleep -Seconds 5"
Write-Host "  C:\temp\pcie_remote_exp\ohcldiag-dev.exe $VmName inspect vm"
