$vm = Get-VM xdp-ws25-1
$vm | Format-List Name,Generation,MemoryStartup,ProcessorCount,Path,SmartPagingFilePath,Notes
Write-Output '--- Network ---'
Get-VMNetworkAdapter -VMName xdp-ws25-1 | Format-List Name,SwitchName,MacAddress,VlanSetting
Write-Output '--- HardDisks ---'
Get-VMHardDiskDrive -VMName xdp-ws25-1 | Format-Table ControllerType,ControllerNumber,ControllerLocation,Path
Write-Output '--- Firmware ---'
$vm | Get-VMFirmware | Format-List BootOrder,SecureBoot,SecureBootTemplate
Write-Output '--- ComPorts ---'
Get-VMComPort -VMName xdp-ws25-1 | Format-Table Number,Path
Write-Output '--- DvdDrive ---'
Get-VMDvdDrive -VMName xdp-ws25-1 | Format-List ControllerNumber,ControllerLocation,Path
Write-Output '--- Integration ---'
Get-VMIntegrationService -VMName xdp-ws25-1 | Format-Table Name,Enabled
