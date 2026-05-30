# Enable VMBusMessageRedirection on the VM via WMI.
#
# ⚠ 半 DEPRECATED (2026-05-30): 本脚本基于"VMBusMessageRedirection=1 是 OpenHCL
#   diag vsock 必需"的旧假设。该假设**对 vsock/diag 路径不成立** —— 用
#   `New-VM -GuestStateIsolationType OpenHCL` 正确创建的 VM 默认
#   VMBusMessageRedirection=0，ohcldiag-dev 依然正常工作。
#
#   **但**：当 OpenHCL 配置 vpci 设备（cmdline pcie_remote 注入 / NVMe
#   takeover 等），VMBusMessageRedirection=1 **是必需的**：OpenHCL 启动会
#   报 `vpci devices require vmbus redirection to be enabled`。
#
#   所以本脚本在 vpci 场景仍有效；在纯 diag 场景则非必要。当前推荐：
#   - 没用 pcie_remote 等 vpci 设备 → 不需要本脚本
#   - 用了 vpci 设备 → 本脚本 OR 在 deploy 时一并设 vssd
#
#   旧 deprecation note "无法补救" 已被纠正：retrofit 编辑 vssd 的
#   VMBusMessageRedirection 字段对**已经用 -GuestStateIsolationType
#   OpenHCL 创建的 VM** 是有效的；retrofit 不行的是 isolation type 本身。

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
