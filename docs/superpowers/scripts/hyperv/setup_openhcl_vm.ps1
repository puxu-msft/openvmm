# Properly configure vm-ws25-1 as OpenHCL VM using petri's powershell module.
# Stops VM, sets GuestFeatureSet (OpenHCL bit), sets IGVM, sets cmdline.
#
# ⚠ DEPRECATED (2026-05-30): 本脚本试图在**现有** VM 上 retrofit OpenHCL
#   配置（修 vssd 字段）。实测 Hyper-V 会静默忽略：原 VM 没有用
#   -GuestStateIsolationType OpenHCL 创建，怎么改 vssd 也加载不上 OpenHCL
#   IGVM，VM 启动到 stock Msvm UEFI，VTL2 不存在。
#
#   正确流程：用同目录 create_openhcl_vm_correct.ps1 **重建** VM。
#
#   旧脚本保留作历史 + petri Set-OpenHCLFirmware cmdlet 使用范例。

param(
    [string]$VmName = 'vm-ws25-1',
    [string]$IgvmFile = 'C:\temp\pcie_remote_exp\openhcl-x64.bin',
    [string]$CommandLine = ''
)

$ErrorActionPreference = 'Stop'
Import-Module C:\temp\pcie_remote_exp\hyperv.psm1 -Force

$vm = Get-VM -Name $VmName
if ($vm.State -ne 'Off') {
    Write-Host "Stopping VM..."
    Stop-VM -Name $VmName -Force -TurnOff
    Start-Sleep 2
}

# Disable Secure Boot
Set-VMFirmware -VMName $VmName -EnableSecureBoot Off
Write-Host "Secure Boot: disabled"

# Apply OpenHCL firmware via petri module
Set-OpenHCLFirmware -Vm $vm -IgvmFile $IgvmFile -IncreaseVtl2Memory
Write-Host "OpenHCL firmware applied: $IgvmFile (with 1GB VTL2 address space)"

# Set cmdline if provided
if ($CommandLine -ne '') {
    Set-VmCommandLine -Vm $vm -CommandLine $CommandLine
    Write-Host "Cmdline set: $CommandLine"
}

# Verify
$vssd = Get-VmSystemSettings $vm
Write-Host ""
Write-Host "=== Verify ==="
Write-Host "FirmwareFile: $($vssd.FirmwareFile)"
Write-Host "GuestFeatureSet: 0x$('{0:X}' -f $vssd.GuestFeatureSet)"
Write-Host "Vtl2AddressRangeSize: $($vssd.Vtl2AddressRangeSize)"
Write-Host "Vtl2MmioAddressRangeSize: $($vssd.Vtl2MmioAddressRangeSize)"
Write-Host "FirmwareParameters: $([System.Text.Encoding]::UTF8.GetString($vssd.FirmwareParameters))"

Write-Host ""
Write-Host "Done. Run: Start-VM -Name $VmName"
