# Enable VMBusMessageRedirection on the VM via WMI.
# This is REQUIRED for OpenHCL diag vsock (port 1/2) to be reachable from host.
# Without it, OpenHCL doesn't even create a VmbusServer (see openhcl/underhill_core/src/worker.rs:1697).

param([string]$VmName = 'pcie-remote-exp')
$ErrorActionPreference = 'Stop'
$ns = 'root\virtualization\v2'

$vm = Get-VM -Name $VmName
$vmid = $vm.Id.Guid
if ($vm.State -ne 'Off') { Stop-VM -Name $VmName -Force -TurnOff; Start-Sleep 2 }

$vssd = Get-CimInstance -Namespace $ns -Query "SELECT * FROM Msvm_VirtualSystemSettingData WHERE ConfigurationID='$vmid' AND VirtualSystemType='Microsoft:Hyper-V:System:Realized'"
Write-Host "Before: VMBusMessageRedirection=$($vssd.VMBusMessageRedirection)"

$vssd.VMBusMessageRedirection = $true

$cimSerializer = [Microsoft.Management.Infrastructure.Serialization.CimSerializer]::Create()
$serialized = $cimSerializer.Serialize($vssd, [Microsoft.Management.Infrastructure.Serialization.InstanceSerializationOptions]::None)
$ss = [System.Text.Encoding]::Unicode.GetString($serialized)

$vmms = Get-CimInstance -Namespace $ns -ClassName Msvm_VirtualSystemManagementService
$r = $vmms | Invoke-CimMethod -Name ModifySystemSettings -Arguments @{ SystemSettings = $ss }
Write-Host "ModifySystemSettings return: $($r.ReturnValue)"
if ($r.ReturnValue -eq 4096) {
    $job = Get-CimInstance -InputObject $r.Job
    while ($job.JobState -in 2,3,4,5) { Start-Sleep -Milliseconds 250; $job = Get-CimInstance -InputObject $r.Job }
    Write-Host "Job state: $($job.JobState), error=$($job.ErrorDescription)"
}

$vssd2 = Get-CimInstance -Namespace $ns -Query "SELECT * FROM Msvm_VirtualSystemSettingData WHERE ConfigurationID='$vmid' AND VirtualSystemType='Microsoft:Hyper-V:System:Realized'"
Write-Host "After: VMBusMessageRedirection=$($vssd2.VMBusMessageRedirection)"
Write-Host ""
Write-Host "Now start VM: Start-VM -Name $VmName ; then ohcldiag-dev <vm> inspect /"
