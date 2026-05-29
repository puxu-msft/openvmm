# Inspect vm-ws25-1 VirtualSystemSettingData
$ns = 'root\virtualization\v2'
$vssd = Get-CimInstance -Namespace $ns -Query "SELECT * FROM Msvm_VirtualSystemSettingData WHERE ElementName='vm-ws25-1' AND VirtualSystemType='Microsoft:Hyper-V:System:Realized'"
if (-not $vssd) {
    Write-Host "VM not found"
    exit 1
}
Write-Host "=== vm-ws25-1 VSSD ==="
Write-Host ("FirmwareFile        : '" + $vssd.FirmwareFile + "'")
Write-Host ("FirmwareParameters  : " + ($vssd.FirmwareParameters | Measure-Object).Count + " bytes")
Write-Host ("BootSourceOrder     : " + ($vssd.BootSourceOrder -join ','))
Write-Host ("SecureBootEnabled   : " + $vssd.SecureBootEnabled)
Write-Host ("GuestStateFile      : '" + $vssd.GuestStateFile + "'")
Write-Host ("GuestStateIsolationType : " + $vssd.GuestStateIsolationType)
Write-Host ("Version             : " + $vssd.Version)
Write-Host ("ConfigurationID     : " + $vssd.ConfigurationID)
Write-Host "=== All non-empty string/UInt32 props ==="
$vssd.CimInstanceProperties | Where-Object {
    $_.Value -ne $null -and $_.Value -ne '' -and ($_.CimType -eq 'String' -or $_.CimType -eq 'UInt32' -or $_.CimType -eq 'Boolean')
} | Select-Object Name,Value,CimType | Format-Table -AutoSize | Out-String | Write-Host
