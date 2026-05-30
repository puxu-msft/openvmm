# Inspect a VM's VirtualSystemSettingData (vssd).
# 用法：./inspect_vm.ps1 [-VmName <name>]
#
# 默认 VM 名 'pcie-remote-exp'。验证 -GuestStateIsolationType 是 'OpenHCL'
# (而不是 'Disabled') 是 OpenHCL VM 创建正确的关键指标。

param([string]$VmName = 'pcie-remote-exp')

$ns = 'root\virtualization\v2'
$vssd = Get-CimInstance -Namespace $ns -Query "SELECT * FROM Msvm_VirtualSystemSettingData WHERE ElementName='$VmName' AND VirtualSystemType='Microsoft:Hyper-V:System:Realized'"
if (-not $vssd) {
    Write-Host "VM '$VmName' not found"
    exit 1
}
Write-Host "=== $VmName vssd ==="
Write-Host ("GuestStateIsolationType  : " + $vssd.GuestStateIsolationType + "  (期望 'OpenHCL'; 'Disabled' = retrofit 路径，加载不上 IGVM)")
Write-Host ("GuestFeatureSet          : 0x" + ('{0:X}' -f $vssd.GuestFeatureSet))
Write-Host ("FirmwareFile             : '" + $vssd.FirmwareFile + "'")
Write-Host ("FirmwareParameters       : " + ($vssd.FirmwareParameters | Measure-Object).Count + " bytes")
Write-Host ("BootSourceOrder          : " + ($vssd.BootSourceOrder -join ','))
Write-Host ("SecureBootEnabled        : " + $vssd.SecureBootEnabled)
Write-Host ("GuestStateFile           : '" + $vssd.GuestStateFile + "'")
Write-Host ("VMBusMessageRedirection  : " + $vssd.VMBusMessageRedirection + "  (非必需; 用 -GuestStateIsolationType OpenHCL 创建后 0/1 都 OK)")
Write-Host ("Version                  : " + $vssd.Version)
Write-Host ("ConfigurationID          : " + $vssd.ConfigurationID)
Write-Host "=== All non-empty string/UInt32/Bool props ==="
$vssd.CimInstanceProperties | Where-Object {
    $_.Value -ne $null -and $_.Value -ne '' -and ($_.CimType -eq 'String' -or $_.CimType -eq 'UInt32' -or $_.CimType -eq 'Boolean')
} | Select-Object Name,Value,CimType | Format-Table -AutoSize | Out-String | Write-Host
