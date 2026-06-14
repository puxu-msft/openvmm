Import-Module C:\spike03\hyperv.psm1 -Force

# Start the VM first
Start-VM -Name xdp-ws25-1 -ErrorAction SilentlyContinue
Start-Sleep -Seconds 10
Get-VM xdp-ws25-1 | Format-List Name,State

# Now try GetManagementVtlSettings standalone
$gms = Get-VmGuestManagementService
Write-Output 'GuestManagementService:'
$gms | Format-List Name,Caption,ElementName

Write-Output '--- Try GetManagementVtlSettings Namespace=Base ---'
try {
  $vmid = (Get-VM xdp-ws25-1).Id.ToString()
  Write-Output "VmId=$vmid"
  $result = $gms | Invoke-CimMethod -MethodName GetManagementVtlSettings -Arguments @{ VmId = $vmid; Namespace = 'Base' }
  Write-Output "ReturnValue: $($result.ReturnValue)"
  Write-Output "CurrentUpdateId: $($result.CurrentUpdateId)"
  Write-Output "Settings byte count: $(if ($result.Settings) { $result.Settings.Length } else { 'null' })"
} catch {
  Write-Output "err: $_"
}
