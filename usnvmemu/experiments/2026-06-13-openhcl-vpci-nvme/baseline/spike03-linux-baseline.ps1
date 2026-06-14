# spike03-linux-baseline.ps1 — OpenHCL + Linux guest (fedora41) baseline
#
# 用户 2026-05-29 决策: Linux guest 验证 OpenHCL baseline 是否 work
# (Windows guest WS25 在 OpenHCL 下 boot loop crash)
#
# 不动 xdp-ws25-1 / fedora41 原 VM
# 新建 xdp-openhcl-linux-test VM, fedora41 VHDX 复制作 boot disk

$ErrorActionPreference = "Stop"
$Dir = "C:\spike03"
$TestVmName = "xdp-openhcl-linux-test"
$SrcVhdx = "C:\hv\fedora41\Virtual Hard Disks\fedora41.vhdx"
$TestVhdx = "$Dir\$TestVmName-os.vhdx"
$NvmeBackingVhdx = "$Dir\spike03-nvme-backing.vhdx"
$IgvmFile = "$Dir\openhcl-x64.bin"
$HypervPsm = "$Dir\hyperv.psm1"

function Phase($n) { Write-Output ""; Write-Output "=== $n ===" }

Import-Module $HypervPsm -Force

Phase "0. cleanup 之前 test VM (if any)"
$existing = Get-VM $TestVmName -ErrorAction SilentlyContinue
if ($existing) {
    if ($existing.State -ne "Off") {
        Stop-VM -Name $TestVmName -TurnOff -Force
        while ((Get-VM $TestVmName).State -ne "Off") { Start-Sleep -Seconds 1 }
    }
    Remove-VM -Name $TestVmName -Force
    Write-Output "Removed previous $TestVmName"
}

Phase "1. Copy fedora41 VHDX (10.5GB)"
if (-not (Test-Path $TestVhdx)) {
    Write-Output "Copying $SrcVhdx → $TestVhdx (may take 2-5 min)"
    Copy-Item $SrcVhdx $TestVhdx
    Write-Output "Copy done, size = $((Get-Item $TestVhdx).Length / 1GB) GB"
} else {
    Write-Output "Reuse existing $TestVhdx"
}

Phase "2. New-VM with -GuestStateIsolationType OpenHCL"
$vm = New-VM -Name $TestVmName -Generation 2 -GuestStateIsolationType OpenHCL `
    -VHDPath $TestVhdx -BootDevice VHD `
    -MemoryStartupBytes 4GB -SwitchName vs-openwrt -Path $Dir
Set-VM -VM $vm -ProcessorCount 2 `
    -AutomaticStartAction Nothing -AutomaticCheckpointsEnabled $false -CheckpointType Disabled
Write-Output "VM created"

Phase "3. SecureBoot Off (Linux 通常 fail with MS SB template)"
Set-VMFirmware -VM $vm -EnableSecureBoot Off
Write-Output "SecureBoot Off"

Phase "4. Set-OpenHCLFirmware"
Set-OpenHCLFirmware -Vm $vm -IgvmFile $IgvmFile
Write-Output "OpenHCL IGVM set"

Phase "5. NVMe backing SCSI controller + Set-VmScsiControllerTargetVtl 2"
if (-not (Test-Path $NvmeBackingVhdx)) {
    New-VHD -Path $NvmeBackingVhdx -SizeBytes 4GB -Dynamic | Out-Null
}
$ctrl = Add-VMScsiController -VM $vm -Passthru
$ctrlNum = $ctrl.ControllerNumber
$hostLun = 0
Add-VMHardDiskDrive -VM $vm -ControllerType SCSI -ControllerNumber $ctrlNum -ControllerLocation $hostLun -Path $NvmeBackingVhdx
Set-VMBusRedirect -Vm $vm -Enable $true
Set-VmScsiControllerTargetVtl -Vm $vm -ControllerNumber $ctrlNum -TargetVtl 2
$backingControllerId = [guid](Get-VmScsiControllerIdByNumber -Vm $vm -ControllerNumber $ctrlNum)
Write-Output "Backing controller GUID = $backingControllerId"

Phase "6. Set-Vtl2Settings (NVMe protocol)"
$guestNvmeControllerId = [guid]::NewGuid()
$settings = @{
    version = "V1"
    dynamic = @{
        storage_controllers = @(
            @{
                instance_id = $guestNvmeControllerId.ToString()
                protocol = "NVME"
                luns = @(
                    @{
                        location = [uint32]1
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
$settingsFile = "$Dir\vtl2-settings-linux-test.json"
$settings | ConvertTo-Json -Depth 10 | Set-Content -Path $settingsFile -Encoding UTF8
Set-Vtl2Settings -VmId $vm.Id -Namespace "Base" -SettingsFile $settingsFile
Write-Output "Vtl2Settings applied (guest NVMe GUID = $guestNvmeControllerId)"

Phase "7. Start VM + 等 2 分钟"
Start-VM -Name $TestVmName
Write-Output "VM started, waiting 120s for guest boot..."
Start-Sleep -Seconds 30

# 5 次轮询 state + heartbeat
for ($i = 1; $i -le 5; $i++) {
    Start-Sleep -Seconds 15
    $v = Get-VM $TestVmName
    Write-Output "[$i/5] State=$($v.State) Uptime=$($v.Uptime) Heartbeat=$($v.Heartbeat)"
    if ($v.State -ne "Running") {
        Write-Output "VM not Running anymore! crash?"
        break
    }
}

Phase "8. Recent Hyper-V events"
Get-WinEvent -FilterHashtable @{LogName="Microsoft-Windows-Hyper-V-Worker-Admin"; StartTime=(Get-Date).AddMinutes(-5)} -MaxEvents 15 -ErrorAction SilentlyContinue | Where-Object { $_.Message -match $TestVmName } | Format-Table TimeCreated,Id,LevelDisplayName -AutoSize | Out-String -Width 200 | Write-Output

Write-Output ""
Write-Output "=== DONE ==="
Write-Output "Test VM: $TestVmName state = $((Get-VM $TestVmName).State)"
