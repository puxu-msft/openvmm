# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# W6b realize-only：把指定的 OpenHCL IGVM + OPENHCL_VFIO_USER_NVME cmdline 装到一台
# Hyper-V OpenHCL VM 上并启动。用于验证 underhill 集成 vfio_user_nvme 设备后 boot 不挂
# （firmware 缺席时 connect 超时 → AbsentPcieDevice 兜底 → boot 继续）。
param(
  [string]$VmName='pcie-remote-exp',
  [string]$IgvmFile='C:\temp\pcie_remote_exp\openhcl-vfio-user.bin',
  [string]$Instance='11111111-2222-3333-4444-555555555555',
  [string]$SockPath='/tmp/vfio_nvme.sock',
  [string]$PsModule='C:\temp\pcie_remote_exp\hyperv.psm1'
)
Import-Module $PsModule -Force -ErrorAction Stop
$vm = Get-VM -Name $VmName -ErrorAction Stop
if ($vm.State -ne 'Off') { Write-Host "stopping $VmName ($($vm.State))"; Stop-VM $VmName -TurnOff -Force; Start-Sleep 3 }
Set-OpenHCLFirmware -Vm $vm -IgvmFile $IgvmFile          # vssd.FirmwareFile + GuestFeatureSet
$cmd = "OPENHCL_VFIO_USER_NVME=$Instance`:$SockPath"
Set-VmCommandLine -Vm $vm -CommandLine $cmd               # vssd.FirmwareParameters = VTL2 cmdline
Write-Host "FirmwareFile = $IgvmFile"
Write-Host "cmdline      = $(Get-VmCommandLine -Vm $vm)"
Start-VM $VmName
Write-Host "STARTED $VmName at $(Get-Date -Format o)"
