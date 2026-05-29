# Switch existing VM to a different OpenHCL IGVM via WMI ModifySystemSettings.
# Uses Get-CimInstance (modern CIM) so CimSerializer works.

param(
    [string]$VmName = 'pcie-remote-exp',
    [Parameter(Mandatory=$true)][string]$IgvmFile,
    [uint32]$Vtl2RangeMb = 1024,
    [uint32]$Vtl2MmioMb = 512
)
$ErrorActionPreference = 'Stop'
$ns = 'root\virtualization\v2'

# Stop VM if needed
$vm = Get-VM -Name $VmName
if ($vm.State -ne 'Off') {
    Stop-VM -Name $VmName -Force -TurnOff
    Start-Sleep 2
}

$vmid = $vm.Id.Guid
$vssd = Get-CimInstance -Namespace $ns -Query "SELECT * FROM Msvm_VirtualSystemSettingData WHERE ConfigurationID='$vmid' AND VirtualSystemType='Microsoft:Hyper-V:System:Realized'"

Write-Host "Before:"
Write-Host "  FirmwareFile=$($vssd.FirmwareFile)"
Write-Host "  Vtl2AddressRangeSize=$($vssd.Vtl2AddressRangeSize)"
Write-Host "  GuestFeatureSet=0x$('{0:X}' -f $vssd.GuestFeatureSet)"

$vssd.FirmwareFile = $IgvmFile
$vssd.GuestFeatureSet = 0x201
$vssd.Vtl2AddressSpaceConfigurationMode = 1
$vssd.Vtl2AddressRangeSize = $Vtl2RangeMb
$vssd.Vtl2MmioAddressRangeSize = $Vtl2MmioMb

$cimSerializer = [Microsoft.Management.Infrastructure.Serialization.CimSerializer]::Create()
$serialized = $cimSerializer.Serialize($vssd, [Microsoft.Management.Infrastructure.Serialization.InstanceSerializationOptions]::None)
$ss = [System.Text.Encoding]::Unicode.GetString($serialized)

$vmms = Get-CimInstance -Namespace $ns -ClassName Msvm_VirtualSystemManagementService
$r = $vmms | Invoke-CimMethod -Name ModifySystemSettings -Arguments @{ SystemSettings = $ss }
Write-Host "ModifySystemSettings return: $($r.ReturnValue) (0=ok, 4096=async)"

if ($r.ReturnValue -eq 4096) {
    $job = Get-CimInstance -InputObject $r.Job
    while ($job.JobState -in 2,3,4,5) {
        Start-Sleep -Milliseconds 250
        $job = Get-CimInstance -InputObject $r.Job
    }
    Write-Host "Job final state: $($job.JobState), error=$($job.ErrorDescription)"
}

# Verify
$vssd2 = Get-CimInstance -Namespace $ns -Query "SELECT * FROM Msvm_VirtualSystemSettingData WHERE ConfigurationID='$vmid' AND VirtualSystemType='Microsoft:Hyper-V:System:Realized'"
Write-Host ""
Write-Host "After:"
Write-Host "  FirmwareFile=$($vssd2.FirmwareFile)"
Write-Host "  Vtl2AddressRangeSize=$($vssd2.Vtl2AddressRangeSize)"
Write-Host "  GuestFeatureSet=0x$('{0:X}' -f $vssd2.GuestFeatureSet)"
