# Enable VMBusMessageRedirection on the VM via WMI.
#
# ⚠ DEPRECATED (2026-05-30): 本脚本基于"VMBusMessageRedirection=1 是 OpenHCL
#   diag vsock 必需"的旧假设。实测发现：用
#   `New-VM -GuestStateIsolationType OpenHCL` 正确创建的 VM 默认
#   VMBusMessageRedirection=0，**ohcldiag-dev 依然正常工作**。
#
#   真实必要条件是 VM 创建时带 -GuestStateIsolationType OpenHCL（见
#   create_openhcl_vm_correct.ps1）。retrofit vssd 字段（含本脚本）对
#   "vssd 缺 isolation type" 的根因无法补救。
#
#   旧脚本保留作 ModifySystemSettings + CimSerializer + 异步 Job 等待
#   的代码范例参考。

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
