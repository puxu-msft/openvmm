# spike03-recreate-vm-openhcl.ps1 — destroy + recreate xdp-ws25-1 with -GuestStateIsolationType OpenHCL
#
# 用户 2026-05-29 选 destroy + recreate (保留 VHDX) 因 in-place Set-OpenHCLFirmware 没正确初始化 GET
# (Set-Vtl2Settings 返回 32795 0x801B)
#
# 保留: VHDX C:\hv\xdp-ws25-1.vhdx + VM name xdp-ws25-1 + Memory 8GB dyn + 4 CPU
#       + Network vs-openwrt + MAC 00155D406508 + COM1 pipe
# 新加: GuestStateIsolationType OpenHCL + Set-OpenHCLFirmware + NVMe translation

$ErrorActionPreference = "Stop"
$Dir = "C:\spike03"
$VmName = "xdp-ws25-1"
$VmHvDir = "C:\hv"
$OsVhdx = "$VmHvDir\xdp-ws25-1.vhdx"
$NvmeBackingVhdx = "$Dir\spike03-nvme-backing.vhdx"
$IgvmFile = "$Dir\openhcl-x64.bin"
$HypervPsm = "$Dir\hyperv.psm1"
$SwitchName = "vs-openwrt"
$MacAddr = "00155D406508"
$ComPipe = "\\.\pipe\xdp-ws25-1"

function Phase($n) { Write-Output ""; Write-Output "=== $n ===" }

Import-Module $HypervPsm -Force

Phase "0. validate prereq"
if (-not (Test-Path $OsVhdx)) { throw "OS VHDX missing: $OsVhdx" }
if (-not (Test-Path $IgvmFile)) { throw "IGVM missing: $IgvmFile" }
if (-not (Test-Path $NvmeBackingVhdx)) {
    Write-Output "Create NVMe backing VHDX (4GB dynamic)"
    New-VHD -Path $NvmeBackingVhdx -SizeBytes 4GB -Dynamic | Out-Null
}

Phase "1. Stop + Remove old VM (VHDX preserved)"
$existing = Get-VM $VmName -ErrorAction SilentlyContinue
if ($existing) {
    if ($existing.State -ne "Off") {
        Stop-VM -Name $VmName -TurnOff -Force
        while ((Get-VM $VmName).State -ne "Off") { Start-Sleep -Seconds 1 }
    }
    $backupDir = "$Dir\backup-recreate-$(Get-Date -Format yyyyMMdd-HHmmss)"
    New-Item -ItemType Directory -Path $backupDir -Force | Out-Null
    $existing | Format-List * | Out-File "$backupDir\vm.txt"
    Write-Output "Backup: $backupDir"
    Remove-VM -Name $VmName -Force
}

Phase "2. New-VM with -GuestStateIsolationType OpenHCL"
$vm = New-VM -Name $VmName -Generation 2 -GuestStateIsolationType OpenHCL `
    -VHDPath $OsVhdx -BootDevice VHD `
    -MemoryStartupBytes 8GB -SwitchName $SwitchName -Path $VmHvDir
Write-Output "VM created"

Phase "3. CPU + dynamic memory + 关闭 checkpoint"
Set-VM -VM $vm -ProcessorCount 4 -DynamicMemory `
    -MemoryMinimumBytes 512MB -MemoryMaximumBytes 1TB `
    -AutomaticStartAction Nothing `
    -AutomaticCheckpointsEnabled $false `
    -CheckpointType Disabled
Write-Output "VM settings restored"

Phase "4. SecureBoot Off (Microsoft Windows template, 与原 VM 一致)"
Set-VMFirmware -VM $vm -EnableSecureBoot Off -SecureBootTemplate MicrosoftWindows
Write-Output "SecureBoot Off"

Phase "5. Network adapter MAC restore"
Set-VMNetworkAdapter -VMName $VmName -StaticMacAddress $MacAddr
Write-Output "MAC = $MacAddr"

Phase "6. COM1 pipe restore (KDNET via serial)"
Set-VMComPort -VMName $VmName -Number 1 -Path $ComPipe
Write-Output "COM1 = $ComPipe"

Phase "7. Set-OpenHCLFirmware"
Set-OpenHCLFirmware -Vm $vm -IgvmFile $IgvmFile
Write-Output "OpenHCL IGVM set"

Phase "8. 添加 SCSI controller (作 VTL2 backing) + 挂 NVMe backing VHDX"
$ctrl = Add-VMScsiController -VM $vm -Passthru
$ctrlNum = $ctrl.ControllerNumber
$hostLun = 0
Add-VMHardDiskDrive -VM $vm -ControllerType SCSI -ControllerNumber $ctrlNum -ControllerLocation $hostLun -Path $NvmeBackingVhdx
Write-Output "SCSI ctrl #$ctrlNum LUN $hostLun = $NvmeBackingVhdx"

Phase "9. VMBus Redirect ON + Set-VmScsiControllerTargetVtl 2"
Set-VMBusRedirect -Vm $vm -Enable $true
Set-VmScsiControllerTargetVtl -Vm $vm -ControllerNumber $ctrlNum -TargetVtl 2
Write-Output "Controller $ctrlNum → VTL 2"

Phase "10. 获取 backing controller GUID + 构造 Vtl2Settings"
$backingControllerId = [guid](Get-VmScsiControllerIdByNumber -Vm $vm -ControllerNumber $ctrlNum)
$guestNvmeControllerId = [guid]::NewGuid()
$guestNamespaceId = 1
Write-Output "Backing GUID = $backingControllerId"
Write-Output "Guest NVMe GUID = $guestNvmeControllerId NSID=$guestNamespaceId"

$settings = @{
    version = "V1"
    dynamic = @{
        storage_controllers = @(
            @{
                instance_id = $guestNvmeControllerId.ToString()
                protocol = "NVME"
                luns = @(
                    @{
                        location = [uint32]$guestNamespaceId
                        device_id = ([guid]::NewGuid()).ToString()
                        vendor_id = "OpenVMM"
                        product_id = "Disk"
                        product_revision_level = "1.0"
                        serial_number = "spike03-001"
                        model_number = "OpenHCL-NVMe"
                        physical_devices = @{
                            type = "single"
                            device = @{
                                device_type = "vscsi"
                                device_path = $backingControllerId.ToString()
                                sub_device_path = [uint32]$hostLun
                            }
                        }
                        is_dvd = $false
                        chunk_size_in_kb = 0
                    }
                )
            }
        )
    }
}
$settingsFile = "$Dir\vtl2-settings-nvme.json"
$settings | ConvertTo-Json -Depth 10 | Set-Content -Path $settingsFile -Encoding UTF8

Phase "11. Set-Vtl2Settings (这次新 OpenHCL VM 应正确 init GET)"
Set-Vtl2Settings -VmId $vm.Id -Namespace "Base" -SettingsFile $settingsFile
Write-Output "Vtl2Settings applied"

Phase "12. Start VM"
Start-VM -Name $VmName
$start = Get-Date
while ((Get-VM $VmName).State -ne "Running" -and ((Get-Date) - $start).TotalSeconds -lt 60) {
    Start-Sleep -Seconds 1
}
Write-Output "VM State = $((Get-VM $VmName).State)"

Phase "13. Evidence snapshot (host side)"
$Ev = "$Dir\evidence-recreate-$(Get-Date -Format yyyyMMdd-HHmmss)"
New-Item -ItemType Directory -Path $Ev -Force | Out-Null
$settings | ConvertTo-Json -Depth 10 | Set-Content "$Ev\vtl2-settings.json" -Encoding UTF8
"Backing GUID: $backingControllerId" | Out-File "$Ev\guids.txt"
"Guest NVMe GUID: $guestNvmeControllerId" | Out-File "$Ev\guids.txt" -Append
"Guest NSID: $guestNamespaceId" | Out-File "$Ev\guids.txt" -Append
Get-VM $VmName | Format-List * | Out-File "$Ev\vm.txt"
Get-VM $VmName | Get-VMScsiController | Format-List | Out-File "$Ev\scsi.txt"
(Get-VmSystemSettings $vm) | Format-List GuestFeatureSet,FirmwareFile,VMBusMessageRedirection | Out-File "$Ev\vssd.txt"

Write-Output ""
Write-Output "=== DONE — VM recreated with OpenHCL + NVMe translation ==="
Write-Output "Evidence: $Ev"
Write-Output "下一步: 等 guest boot + ssh xdp-ws25-1 + Get-PnpDevice"
