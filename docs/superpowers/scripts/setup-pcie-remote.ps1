# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# 一次性注册 pcie_remote vsock service GUID 并配 ACL（仅 Admin / SYSTEM）。
# 用法（必须在管理员 PowerShell 运行）:
#   ./setup-pcie-remote.ps1 -VsockPort 50000

param(
    [Parameter(Mandatory = $true)]
    [uint32]$VsockPort
)

# HV_GUID_VSOCK_TEMPLATE: vsock port 嵌入 AF_HYPERV service GUID 的模板
$BaseGuid = '00000000-facb-11e6-bd58-64006a7986d3'
$ServiceGuid = "{0:x8}-{1}" -f $VsockPort, $BaseGuid.Substring(9)

$RegPath = "HKLM:\Software\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices\$ServiceGuid"
New-Item -Path $RegPath -Force | Out-Null
Set-ItemProperty -Path $RegPath -Name 'ElementName' -Value "pcie_remote vsock:$VsockPort"

# SDDL: Protected DACL, 仅 BUILTIN\Administrators (BA) + LocalSystem (SY)
# - D:  DACL section
# - P:  Protected（不继承父对象 ACE）
# - (A;;GA;;;BA): Allow Generic All to BUILTIN\Administrators
# - (A;;GA;;;SY): Allow Generic All to LocalSystem
$Sddl = 'D:P(A;;GA;;;BA)(A;;GA;;;SY)'
$Acl = Get-Acl -Path $RegPath
$Acl.SetSecurityDescriptorSddlForm($Sddl)
Set-Acl -Path $RegPath -AclObject $Acl

# Verify protected flag
$Verify = (Get-Acl -Path $RegPath).Sddl
if ($Verify -notlike 'D:PAI*' -and $Verify -notlike 'D:P*') {
    Write-Warning "DACL not protected as expected; got: $Verify"
} else {
    Write-Host "OK: DACL protected, Admin/SYSTEM only"
}

Write-Host ""
Write-Host "Service GUID registered:"
Write-Host "  GUID:     $ServiceGuid"
Write-Host "  RegPath:  $RegPath"
Write-Host "  ACL:      $Sddl"
Write-Host ""
Write-Host "Next steps:"
Write-Host "  1. 启动 host 实验程序，让它 AF_HYPERV connect 到 (vm_id, $ServiceGuid)"
Write-Host "  2. OpenHCL CLI 加 --pcie-remote-takeover <nvme_guid>:$VsockPort"
Write-Host "     或   --pcie-remote-instance <new_guid>:$VsockPort"
