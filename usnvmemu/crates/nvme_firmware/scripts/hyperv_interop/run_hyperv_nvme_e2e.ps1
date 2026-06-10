<#
.SYNOPSIS
  OpenHCL / Hyper-V end-to-end harness for the userspace `nvme_firmware` device
  over the pcie_remote (vsock) transport.

.DESCRIPTION
  This is the pcie_remote/OpenHCL counterpart to the vfio-user `qemu_interop`
  harness and the NVMe-oF `interop_py` harness. It drives the REAL, current
  `nvme_firmware.exe` against a REAL Hyper-V OpenHCL VM and a REAL Windows guest,
  exercising the full data path end to end:

      nvme_firmware.exe (host, vsock client)
        -> AF_HYPERV vsock
        -> OpenHCL VTL2 pcie_remote worker
        -> vpci publish to VTL0
        -> guest nvme.sys
        -> guest format + 4 MiB file IO (drives the controller's PRP-list path)
        -> NVMe WRITE back over the same path
        -> host backing file

  Two INDEPENDENT oracles confirm correctness:
    1. guest-side: write 4 MiB (embedding $Marker), flush, read back, compare.
    2. host-side : raw byte-scan the backing file for $Marker, after stopping the
       controller (its mmap holds the file lock). A different code path than the
       guest readback (catches NTFS-cache false positives).

  Flow is "deploy-or-reuse": if the VM exists it is restarted (so VTL2 re-spawns
  its pcie_remote listener); otherwise it is created from the IGVM + guest VHDX.
  The controller is started with `--retries 0` (infinite) BEFORE the VM boots, so
  it is already waiting when the VTL2 listener appears (handshake window is short,
  see HYPERV_RUNBOOK K-19).

.NOTES
  Guest credentials come from a SINGLE source ($AdminUser/$AdminPass). The
  pre-built guest.vhdx uses Administrator/PcieRemote123!. Freshly provisioned
  guests should use a/1 (see build_winserver_vhdx provisioning).

  Exit codes: 0 = PASS; 2 = guest never became reachable; 3 = no NVMe disk in
  guest; 4 = guest IO mismatch; 5 = VM/deploy error.
#>
[CmdletBinding()]
param(
    [string]$VmName   = 'pcie-remote-exp',
    [string]$WorkDir  = 'C:\temp\pcie_remote_exp',
    [string]$ExePath,
    [string]$IgvmFile,
    [string]$Vhdx,
    [string]$AdminUser = 'Administrator',
    [string]$AdminPass = 'PcieRemote123!',
    [string]$BackingImg,
    [long]  $BackingSizeBytes = 1073741824,
    [string]$Marker  = "USNVMEMU-OPENHCL-REVERIFY",
    [int]   $Port    = 50000,
    [switch]$ForceDeploy,
    [int]   $BootWaitSec = 40,
    [int]   $GuestReadyTimeoutSec = 180
)
$ErrorActionPreference = 'Continue'
if (-not $ExePath)    { $ExePath    = Join-Path $WorkDir 'nvme_firmware.exe' }
if (-not $IgvmFile)   { $IgvmFile   = Join-Path $WorkDir 'openhcl-pcie-v14-8s.bin' }
if (-not $Vhdx)       { $Vhdx       = Join-Path $WorkDir 'guest.vhdx' }
if (-not $BackingImg) { $BackingImg = Join-Path $WorkDir 'nvme_reverify_ns1.img' }
$OutLog = Join-Path $WorkDir 'reverify_ctrl.out'
$ErrLog = Join-Path $WorkDir 'reverify_ctrl.err'
$ESC = [char]27

function Section($m){ Write-Host "`n======== $m ========" }
function Strip($lines){ $lines | ForEach-Object { $_ -replace "$ESC\[[0-9;]*m","" } }
# Stop the controller then exit. A `--retries 0` controller launched with
# -NoNewWindow inherits this process's stdout handle, so leaving it alive would
# keep the launching pipe/redirect open and hang the caller. The guest has
# already flushed its writes (Write-VolumeCache → NVMe WRITE → controller →
# mmap); the OS writes the mmap dirty pages back to the backing file when the
# controller exits, so run_e2e.sh's host oracle re-scans with a short retry to
# ride out that flush. $ctrl is $null on early deploy errors — guard it.
function Finish($code,$msg){
    if ($msg) { Write-Host $msg }
    if ($script:ctrl) { Stop-Process -Id $script:ctrl.Id -Force -EA SilentlyContinue }
    exit $code
}

# Raw byte-scan a (large, binary) file for an ASCII marker, in 8 MiB chunks with a
# carry to catch boundary-spanning matches. Host-side oracle, independent of the
# guest's NTFS readback (raw bytes, not via the guest filesystem). `findstr` /
# `Select-String` choke on binary long lines, hence the manual scan.
function Find-MarkerInFile($path, $marker) {
    if (-not (Test-Path $path)) { return $false }
    # OpenRead throws IOException while the controller still holds the file open
    # (mmap) — the caller retries after stopping it, so just return $false here.
    try { $fs = [System.IO.File]::OpenRead($path) } catch { return $false }
    try {
        $buf = New-Object byte[] (8 * 1024 * 1024)
        $carry = ''
        while (($n = $fs.Read($buf, 0, $buf.Length)) -gt 0) {
            $s = $carry + [System.Text.Encoding]::ASCII.GetString($buf, 0, $n)
            if ($s.Contains($marker)) { return $true }
            $keep = [Math]::Min($marker.Length - 1, $s.Length)
            $carry = $s.Substring($s.Length - $keep)
        }
    } finally { if ($fs) { $fs.Close() } }
    return $false
}

# --- 1. ensure VM (deploy-or-reuse) ---
function Deploy-Vm {
    Section "Deploy fresh OpenHCL VM '$VmName' (IGVM=$IgvmFile)"
    if (-not (Test-Path $IgvmFile)) { Finish 5 "RESULT=FAIL_NO_IGVM $IgvmFile" }
    if (-not (Test-Path $Vhdx))     { Finish 5 "RESULT=FAIL_NO_VHDX $Vhdx" }
    $old = Get-VM -Name $VmName -ErrorAction SilentlyContinue
    if ($old) { if ($old.State -ne 'Off') { Stop-VM $VmName -TurnOff -Force; Start-Sleep 2 }; Remove-VM $VmName -Force }
    $vm = New-VM -Name $VmName -Generation 2 -GuestStateIsolationType OpenHCL -MemoryStartupBytes 4GB
    Set-VM -VM $vm -ProcessorCount 2 -AutomaticCheckpointsEnabled $false
    Set-VMFirmware -VM $vm -EnableSecureBoot Off
    Add-VMHardDiskDrive -VM $vm -Path $Vhdx -ControllerType SCSI -ControllerNumber 0 -ControllerLocation 0
    $bootEntry = Get-VMFirmware -VM $vm | Select-Object -ExpandProperty BootOrder | Where-Object { $_.BootType -eq 'Drive' } | Select-Object -First 1
    if ($bootEntry) { Set-VMFirmware -VM $vm -FirstBootDevice $bootEntry.Device }
    $setup = Join-Path $WorkDir 'Set-OpenHCL-HyperV-VM.ps1'
    & $setup -VM $vm -Path $IgvmFile
}

$exists = [bool](Get-VM -Name $VmName -ErrorAction SilentlyContinue)
if ($ForceDeploy -or -not $exists) { Deploy-Vm } else { Section "Reusing existing VM '$VmName'" }
$vmid = (Get-VM $VmName).Id.Guid
Section "VM $VmName id=$vmid"

# --- 2. stop stale controllers ---
Get-Process nvme_firmware,pcie_remote_nvme_userspace -EA SilentlyContinue | Stop-Process -Force
Start-Sleep 2

# --- 3. fresh zeroed backing img (proves fresh writes + clean host oracle) ---
if (Test-Path $BackingImg) { Remove-Item $BackingImg -Force }
fsutil file createnew $BackingImg $BackingSizeBytes | Out-Null
Section "Fresh zeroed backing img $BackingImg ($BackingSizeBytes bytes)"

# --- 4. stop VM so VTL2 re-spawns the pcie_remote listener on next boot ---
if ((Get-VM $VmName).State -ne 'Off') { Stop-VM $VmName -TurnOff -Force; Start-Sleep 3 }

# --- 5. start controller FIRST with --retries 0 so it waits for VTL2 ---
if (-not (Test-Path $ExePath)) { Finish 5 "RESULT=FAIL_NO_EXE $ExePath" }
Remove-Item $OutLog,$ErrLog -EA SilentlyContinue
$ctrl = Start-Process -FilePath $ExePath `
    -ArgumentList @('--vm-id',$vmid,'--port',"$Port",'--retries','0','--retry-ms','500','--backing-file',$BackingImg) `
    -PassThru -RedirectStandardOutput $OutLog -RedirectStandardError $ErrLog -NoNewWindow
Section "Controller PID=$($ctrl.Id) exe=$ExePath"

# --- 6. boot VM + handshake check ---
Start-VM $VmName
Section "VM booting; ${BootWaitSec}s for VTL2 + handshake"
Start-Sleep $BootWaitSec
$ctrlLines = Strip (Get-Content $OutLog -EA SilentlyContinue)
Section "Controller log (last 16)"; $ctrlLines | Select-Object -Last 16
$hs = ($ctrlLines | Where-Object { $_ -match 'HelloAck|entering main loop|namespace registered' }).Count
Section "Handshake markers = $hs ; controller alive = $([bool](Get-Process -Id $ctrl.Id -EA SilentlyContinue))"

# --- 7. wait for guest PSDirect readiness ---
$cred = New-Object System.Management.Automation.PSCredential($AdminUser, (ConvertTo-SecureString $AdminPass -AsPlainText -Force))
$deadline=(Get-Date).AddSeconds($GuestReadyTimeoutSec); $ready=$false
do {
  try { Invoke-Command -VMId $vmid -Credential $cred -ScriptBlock { $env:COMPUTERNAME } -EA Stop | Out-Null; $ready=$true; break }
  catch { Start-Sleep 10 }
} while ((Get-Date) -lt $deadline)
Section "Guest PSDirect ready = $ready"
if (-not $ready) { Finish 2 "RESULT=FAIL_GUEST_NOT_READY" }

# --- 8. guest NVMe IO oracle ---
$sb = {
    param($Marker)
    # NVMe enumeration can lag WinRM readiness — force a storage rescan + poll
    'rescan' | diskpart | Out-Null
    $nvme = @()
    for ($i=0; $i -lt 18; $i++) {
        $nvme = @(Get-Disk | Where-Object { $_.BusType -eq 'NVMe' })
        if ($nvme.Count -gt 0) { break }
        Start-Sleep 5
    }
    "guest: NVMe disk count = $($nvme.Count) (after rescan+poll)"
    Get-Disk | Format-Table Number,FriendlyName,BusType,Size,OperationalStatus,PartitionStyle -AutoSize | Out-String
    if ($nvme.Count -eq 0) { "guest: NO_NVME_DISK"; return }
    foreach ($d in $nvme) {
        if ($d.OperationalStatus -ne 'Online') { Set-Disk -Number $d.Number -IsOffline $false -EA SilentlyContinue; Set-Disk -Number $d.Number -IsReadOnly $false -EA SilentlyContinue }
        if ($d.PartitionStyle -eq 'RAW') { Initialize-Disk -Number $d.Number -PartitionStyle GPT -Confirm:$false }
        $p = Get-Partition -DiskNumber $d.Number -EA SilentlyContinue | Where-Object DriveLetter | Select-Object -First 1
        if (-not $p) {
            $p = New-Partition -DiskNumber $d.Number -UseMaximumSize -AssignDriveLetter
            Format-Volume -DriveLetter $p.DriveLetter -FileSystem NTFS -NewFileSystemLabel "NVME$($d.Number)" -Confirm:$false | Out-Null
        }
        $dl = $p.DriveLetter
        $path = "${dl}:\reverify.dat"
        $bytes = New-Object byte[] (4*1024*1024)        # 4 MiB -> drives PRP-list path (> 8 KiB single IO)
        (New-Object Random 12345).NextBytes($bytes)
        [System.Text.Encoding]::ASCII.GetBytes($Marker).CopyTo($bytes,0)
        [System.IO.File]::WriteAllBytes($path,$bytes)
        Write-VolumeCache -DriveLetter $dl              # force NTFS -> nvme.sys -> NVMe WRITE -> host file
        $rb = [System.IO.File]::ReadAllBytes($path)
        $rbMarker = [System.Text.Encoding]::ASCII.GetString($rb[0..($Marker.Length-1)])
        "guest: disk $($d.Number) '$($d.FriendlyName)' drive=${dl}: wrote+flushed 4 MiB, readback len=$($rb.Length) markerMatch=$($rbMarker -eq $Marker)"
    }
}
Section "Guest NVMe IO (format + 4 MiB write + readback)"
# PSDirect Invoke-Command can throw a transient "Unspecified Error" right after
# boot; retry a few times before declaring failure.
$io = ""
for ($attempt = 1; $attempt -le 3; $attempt++) {
    try { $io = Invoke-Command -VMId $vmid -Credential $cred -ScriptBlock $sb -ArgumentList $Marker -EA Stop | Out-String; break }
    catch { Write-Host "guest IO attempt $attempt failed: $_"; $io = ""; Start-Sleep 8 }
}
if (-not $io) { Finish 4 "RESULT=FAIL_PSDIRECT (all attempts)" }
Write-Host $io
if ($io -match 'NO_NVME_DISK') { Finish 3 "RESULT=FAIL_NO_NVME_DISK" }
if ($io -notmatch 'markerMatch=True') { Finish 4 "RESULT=FAIL_IO_MISMATCH" }

# --- 9. host oracle: raw byte-scan the backing file for the marker. INDEPENDENT
#        of the guest readback (raw bytes, not via the guest FS).
#        The controller holds the backing file open (mmap), so a separate reader
#        gets IOException AND can't see its writes until it's stopped (which both
#        releases the lock and flushes the mmap dirty pages on terminate). Stop it
#        FIRST, then scan Windows-side (no WSL 9p cache), retrying briefly for the
#        terminate-flush to settle. ---
if ($script:ctrl) { Stop-Process -Id $script:ctrl.Id -Force -EA SilentlyContinue; $script:ctrl = $null }
Start-Sleep 3
Section "Host oracle: raw byte-scan backing file for marker"
$hostFound = $false
for ($h = 1; $h -le 12; $h++) {
    if (Find-MarkerInFile $BackingImg $Marker) { $hostFound = $true; break }
    Start-Sleep 5
}
if ($hostFound) { Write-Host "HOST ORACLE: marker found in raw backing file (after $h attempt(s))" }
else { Finish 6 "RESULT=FAIL_HOST_ORACLE marker not found after retries" }

# --- 10. controller log scan (confirm PRP-list path exercised) ---
$ctrlLines = Strip (Get-Content $OutLog -EA SilentlyContinue)
Section "Controller PRP-list / large-IO markers"
$ctrlLines | Where-Object { $_ -match 'PRP-list|PrpList|dual-PRP|full_len' } | Select-Object -Last 6
Write-Host "`n======== Controller WARN/ERROR (post-handshake) ========"
$ctrlLines | Where-Object { $_ -match 'WARN|ERROR' } | Where-Object { $_ -notmatch '10049|10060|connect failed' } | Select-Object -Last 6

Finish 0 "`nRESULT=PASS marker=$Marker backing=$BackingImg vmid=$vmid"
