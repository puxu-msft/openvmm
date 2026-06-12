<#
.SYNOPSIS
  Guest-side (VTL0) driver for the OpenHCL + vfio-user NVMe (W6b/W6c) e2e harness.

.DESCRIPTION
  Runs INSIDE the loop driven by run_w6c_e2e.sh. Assumes the OpenHCL VM is booted
  and the usnvmemu vfio-user server is already Live in VTL2 (the bash side handles
  push/launch/Live). This script, over PowerShell Direct:
    1. waits for guest PSDirect readiness,
    2. if the NVMe devnode is in 'Error' (stornvme probed before usnvmemu went
       Live during boot), disable/enable it to force re-init against the now-Live
       + DMA-mapped controller (W6c),
    3. confirms enumeration as PCI\VEN_1414&DEV_00A9 + an NVMe disk appears,
    4. oracle-1: writes 4 MiB (embedding $Marker, > 8 KiB so it drives the
       PRP-list path), flushes (NTFS -> stornvme -> NVMe WRITE -> usnvmemu ->
       zero-copy DMA -> backing file), reads it back, compares.

  The INDEPENDENT host/VTL2-side oracle-2 (raw backing-file byte scan) is done by
  the bash driver via streaming `ohcldiag-dev file -p` -- NOT here.

  Emits machine-parseable lines for the bash driver:
    READY=<bool>  DEVNODE=<status>  HWID=<instanceid>  NVME_DISK=<count>
    ORACLE1=<PASS|FAIL|NO_DISK>

.NOTES
  Guest creds default Administrator / PcieRemote123! (the pre-built guest.vhdx).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$VmId,
    [string]$Marker = "USNVMEMU-W6C-E2E",
    [string]$AdminUser = 'Administrator',
    [string]$AdminPass = 'PcieRemote123!',
    [int]$ReadyTimeoutSec = 180
)
$ErrorActionPreference = 'Continue'
$cred = New-Object System.Management.Automation.PSCredential($AdminUser, (ConvertTo-SecureString $AdminPass -AsPlainText -Force))

# --- 1. wait for guest PSDirect readiness ---
$ready = $false; $deadline = (Get-Date).AddSeconds($ReadyTimeoutSec)
do {
    try { Invoke-Command -VMId $VmId -Credential $cred -ScriptBlock { $env:COMPUTERNAME } -EA Stop | Out-Null; $ready = $true; break }
    catch { Start-Sleep 8 }
} while ((Get-Date) -lt $deadline)
Write-Host "READY=$ready"
if (-not $ready) { Write-Host "ORACLE1=FAIL"; exit 2 }

# --- 2-4. re-init (if needed) + enumerate + 4 MiB IO oracle-1 ---
$out = Invoke-Command -VMId $VmId -Credential $cred -ScriptBlock {
    param($Marker)
    $ErrorActionPreference = 'Continue'
    # The vfio-user NVMe controller advertises VEN_1414&DEV_00A9 (see
    # vfio_user_pci_device identity). Find its devnode.
    $dev = Get-PnpDevice | Where-Object { $_.InstanceId -match 'VEN_1414&DEV_00A9' } | Select-Object -First 1
    if (-not $dev) { Write-Output "HWID=NONE"; Write-Output "ORACLE1=NO_DISK"; return }
    Write-Output "HWID=$($dev.InstanceId)"
    # W6c: if stornvme probed during boot BEFORE usnvmemu went Live, the controller
    # init failed (MMIO Live-gated -> Err) and the devnode is 'Error'. Now that the
    # controller is Live + DMA-mapped, disable/enable forces a clean re-init.
    if ($dev.Status -ne 'OK') {
        Disable-PnpDevice -InstanceId $dev.InstanceId -Confirm:$false -EA SilentlyContinue
        Start-Sleep 2
        Enable-PnpDevice  -InstanceId $dev.InstanceId -Confirm:$false -EA SilentlyContinue
        Start-Sleep 8
    }
    Write-Output "DEVNODE=$((Get-PnpDevice | Where-Object { $_.InstanceId -eq $dev.InstanceId }).Status)"
    'rescan' | diskpart | Out-Null; Start-Sleep 2
    $d = Get-Disk | Where-Object { $_.BusType -eq 'NVMe' } | Select-Object -First 1
    $cnt = @(Get-Disk | Where-Object { $_.BusType -eq 'NVMe' }).Count
    Write-Output "NVME_DISK=$cnt"
    if (-not $d) { Write-Output "ORACLE1=NO_DISK"; return }
    Write-Output "DISK='$($d.FriendlyName)' $($d.Size)B $($d.OperationalStatus)"
    if ($d.PartitionStyle -eq 'RAW') { Initialize-Disk -Number $d.Number -PartitionStyle GPT -Confirm:$false }
    $p = Get-Partition -DiskNumber $d.Number -EA SilentlyContinue | Where-Object DriveLetter | Select-Object -First 1
    if (-not $p) {
        $p = New-Partition -DiskNumber $d.Number -UseMaximumSize -AssignDriveLetter
        Format-Volume -DriveLetter $p.DriveLetter -FileSystem NTFS -NewFileSystemLabel 'W6C' -Confirm:$false | Out-Null
    }
    $dl = $p.DriveLetter; $path = "${dl}:\w6c.dat"
    $b = New-Object byte[] (4*1024*1024)             # 4 MiB -> drives PRP-list path
    (New-Object Random 2026).NextBytes($b)
    [System.Text.Encoding]::ASCII.GetBytes($Marker).CopyTo($b, 0)
    [System.IO.File]::WriteAllBytes($path, $b)
    Write-VolumeCache -DriveLetter $dl               # force NTFS -> NVMe WRITE -> usnvmemu DMA -> backing
    $rb = [System.IO.File]::ReadAllBytes($path)
    $match = [System.Text.Encoding]::ASCII.GetString($rb[0..($Marker.Length-1)]) -eq $Marker
    Write-Output ("ORACLE1=" + $(if ($match) { 'PASS' } else { 'FAIL' }))
} -ArgumentList $Marker
$out | Out-String
