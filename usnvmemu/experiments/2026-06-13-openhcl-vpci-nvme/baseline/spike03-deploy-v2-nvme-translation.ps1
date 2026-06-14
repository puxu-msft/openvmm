# spike03-deploy-v2-nvme-translation.ps1 — OpenHCL SCSI-to-NVMe translation
#
# 前置: openhcl-x64.bin + hyperv.psm1 已在 C:\spike03\
#       xdp-ws25-1 已 Set-OpenHCLFirmware (v1 已做)
#
# 本 v2 改动:
#   - host 端 添加 新 SCSI controller + VHDX (作 backing storage)
#   - Set-VmScsiControllerTargetVtl 2 → host SCSI 给 VTL2 OpenHCL 看
#   - Set-Vtl2Settings JSON protocol=NVMe → OpenHCL VTL2 内翻译成 NVMe controller
#   - 启动 + guest 内 stornvme attach NVMe controller (vpci.sys 提供)
#
# 关键 schema (官方 docs):
#   storage_controllers[].protocol = "NVME"  (guest 看到 NVMe controller)
#   storage_controllers[].luns[].location  (NVMe namespace ID, 不能 0/!0)
#   storage_controllers[].luns[].physical_devices.device.device_type = "vscsi" 或 "nvme"

$ErrorActionPreference = "Stop"
$Dir = "C:\spike03"
$VmName = "xdp-ws25-1"
$HypervPsm = "$Dir\hyperv.psm1"
$VhdPath = "$Dir\spike03-nvme-backing.vhdx"
$NvmeBackingSizeGB = 4

function Phase($n) { Write-Output ""; Write-Output "=== $n ===" }

Phase "0. import hyperv.psm1 + 检查 VM 已 Set-OpenHCLFirmware"
Import-Module $HypervPsm -Force
$vm = Get-VM $VmName
$vssd = Get-VmSystemSettings $vm
Write-Output "GuestFeatureSet = 0x$($vssd.GuestFeatureSet.ToString('X'))"
Write-Output "FirmwareFile    = $($vssd.FirmwareFile)"
if ($vssd.GuestFeatureSet -ne 0x201) {
    throw "OpenHCL 未启用 (GuestFeatureSet 应 0x201). 先跑 v1 deploy."
}

Phase "1. Stop xdp-ws25-1"
if ((Get-VM $VmName).State -ne "Off") {
    Stop-VM -Name $VmName -TurnOff -Force
    while ((Get-VM $VmName).State -ne "Off") { Start-Sleep -Seconds 1 }
}
Write-Output "VM stopped"

Phase "2. cleanup 旧非默认 SCSI controller"
$existingCtrls = Get-VMScsiController -VMName $VmName
foreach ($ctrl in $existingCtrls) {
    if ($ctrl.ControllerNumber -gt 0) {
        Write-Output "Removing SCSI controller $($ctrl.ControllerNumber)"
        Remove-VMScsiController -VMName $VmName -ControllerNumber $ctrl.ControllerNumber
    }
}

Phase "3. 创建 OR 复用 NVMe backing VHDX ($NvmeBackingSizeGB GB)"
if (-not (Test-Path $VhdPath)) {
    New-VHD -Path $VhdPath -SizeBytes ($NvmeBackingSizeGB * 1GB) -Dynamic | Out-Null
    Write-Output "Created $VhdPath"
} else {
    Write-Output "Reuse existing $VhdPath ($((Get-Item $VhdPath).Length / 1MB) MB)"
}

Phase "4. 添加 新 SCSI controller (作 VTL2 backing) + 挂 VHDX"
$ctrl = Add-VMScsiController -VM $vm -Passthru
$ctrlNum = $ctrl.ControllerNumber
$hostLun = 0
Write-Output "Added SCSI controller #$ctrlNum"
Add-VMHardDiskDrive -VM $vm -ControllerType SCSI -ControllerNumber $ctrlNum -ControllerLocation $hostLun -Path $VhdPath
Write-Output "Attached $VhdPath to controller $ctrlNum LUN $hostLun"

Phase "5. VMBUS Redirect ON (storage relay 必需)"
Set-VMBusRedirect -Vm $vm -Enable $true
Write-Output "VMBus redirect enabled"

Phase "6. 把 controller $ctrlNum 指向 VTL2 (TargetVTL=2)"
Set-VmScsiControllerTargetVtl -Vm $vm -ControllerNumber $ctrlNum -TargetVtl 2
Write-Output "Controller $ctrlNum target VTL = 2 (host -> OpenHCL VTL2)"

Phase "7. 获取 controller 的 VMBus instance GUID (作 backing device_path)"
$backingControllerId = [guid](Get-VmScsiControllerIdByNumber -Vm $vm -ControllerNumber $ctrlNum)
if (-not $backingControllerId) { throw "无法获取 controller $ctrlNum 的 GUID" }
Write-Output "Backing SCSI controller GUID = $backingControllerId"

Phase "8. 构造 Vtl2Settings JSON (protocol=NVMe, backing=vscsi)"
$guestNvmeControllerId = [guid]::NewGuid()
$guestNamespaceId = 1   # NVMe NSID (不能 0 不能 !0=0xFFFFFFFF)
Write-Output "Guest NVMe controller GUID = $guestNvmeControllerId"
Write-Output "Guest NVMe namespace ID    = $guestNamespaceId"

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
Write-Output "Settings written to $settingsFile"
Get-Content $settingsFile | Out-Host

Phase "9. Apply Vtl2 Settings"
Set-Vtl2Settings -VmId $vm.Id -Namespace "Base" -SettingsFile $settingsFile
Write-Output "Set-Vtl2Settings DONE"

Phase "10. Start VM"
Start-VM -Name $VmName
$startTime = Get-Date
while ((Get-VM $VmName).State -ne "Running" -and ((Get-Date) - $startTime).TotalSeconds -lt 60) {
    Start-Sleep -Seconds 1
}
Write-Output "VM state: $((Get-VM $VmName).State)"

Phase "11. record evidence (host side)"
$Ev = "$Dir\evidence-v2-$(Get-Date -Format yyyyMMdd-HHmmss)"
New-Item -ItemType Directory -Path $Ev -Force | Out-Null
$settings | ConvertTo-Json -Depth 10 | Set-Content "$Ev\vtl2-settings.json" -Encoding UTF8
"Backing SCSI controller GUID: $backingControllerId" | Out-File "$Ev\guids.txt"
"Guest NVMe controller GUID: $guestNvmeControllerId" | Out-File "$Ev\guids.txt" -Append
"Guest NVMe namespace ID: $guestNamespaceId" | Out-File "$Ev\guids.txt" -Append
Get-VM $VmName | Get-VMScsiController | Format-List | Out-File "$Ev\scsi-controllers.txt"

Write-Output ""
Write-Output "=== DONE — v2 NVMe translation 配好 ==="
Write-Output "Evidence: $Ev"
Write-Output "下一步: 等 guest 启动 + ssh xdp-ws25-1 + Get-PnpDevice 看 NVMe controller"
