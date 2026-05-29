# Register OpenHCL diag service GUIDs (ports 1 + 2) in Hyper-V GuestCommunicationServices.
# REQUIRES ELEVATION. Run from an admin PowerShell:
#   sudo .\register_openhcl_diag_guids.ps1
# Or interactively click UAC when sudo opens new window.

$ErrorActionPreference = 'Stop'
$Base = '00000000-facb-11e6-bd58-64006a7986d3'.Substring(9)  # 'facb-11e6-bd58-64006a7986d3'
$RegRoot = 'HKLM:\Software\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices'

foreach ($port in 1, 2) {
    $guid = ('{0:x8}-' -f $port) + $Base
    $path = "$RegRoot\$guid"
    Write-Host "Registering $guid (port $port)..."
    New-Item -Path $path -Force | Out-Null
    Set-ItemProperty -Path $path -Name 'ElementName' -Value "OpenHCL diag vsock:$port"
    # ACL: allow Admin + SYSTEM only
    $sddl = 'D:P(A;;GA;;;BA)(A;;GA;;;SY)'
    $acl = Get-Acl -Path $path
    $acl.SetSecurityDescriptorSddlForm($sddl)
    Set-Acl -Path $path -AclObject $acl
    Write-Host "  done. SDDL=$((Get-Acl $path).Sddl)"
}
Write-Host ""
Write-Host "All registered. Now from any shell (not necessarily admin):"
Write-Host "  ohcldiag-dev.exe <vm-name> inspect /"
