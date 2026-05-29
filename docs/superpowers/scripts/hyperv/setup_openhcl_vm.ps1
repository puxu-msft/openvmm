# Properly configure vm-ws25-1 as OpenHCL VM using petri's powershell module.
# Stops VM, sets GuestFeatureSet (OpenHCL bit), sets IGVM, sets cmdline.

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
