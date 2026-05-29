# Create a minimal OpenHCL VM from scratch (delete prior if exists), boot, capture serial.
param(
    [string]$VmName = 'pcie-remote-exp',
    [string]$IgvmFile = 'C:\temp\pcie_remote_exp\openhcl-x64.bin',
    [string]$CommandLine = '',
    [int]$SerialTimeoutSec = 20
)
$ErrorActionPreference = 'Stop'
Import-Module C:\temp\pcie_remote_exp\hyperv.psm1 -Force

# 1. Delete prior
$existing = Get-VM -Name $VmName -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host "Removing existing VM..."
    if ($existing.State -ne 'Off') {
        Stop-VM -Name $VmName -Force -TurnOff
        Start-Sleep 2
    }
    Remove-VM -Name $VmName -Force
}

# 2. Create using petri's New-CustomVM helper (full OpenHCL minimal config)
Write-Host "Creating OpenHCL VM '$VmName'..."
$args = @{
    VMName             = $VmName
    Generation         = 2
    SecureBootEnabled  = $false
    Memory             = 4GB
    VpCount            = 2
    FirmwareFile       = $IgvmFile
    Com1               = $true
    IncreaseVtl2Memory = $true
}
if ($CommandLine -ne '') {
    $args['FirmwareParameters'] = $CommandLine
}
New-CustomVM @args

# 3. Find COM port pipe paths. Gen2 VMs have 2 com ports; OpenHCL boot logs go
#    to COM1 if COM3 isn't present (boot_logger logic check com3_serial_available).
$vm = Get-VM -Name $VmName
$vmid = $vm.Id.Guid
Write-Host "COM1 pipe: \\.\pipe\$vmid-1"

# 4. Show VSSD
$vssd = Get-VmSystemSettings (Get-VM -Name $VmName)
Write-Host "FirmwareFile: $($vssd.FirmwareFile)"
Write-Host "GuestFeatureSet: 0x$('{0:X}' -f $vssd.GuestFeatureSet)"
Write-Host "Vtl2AddressRangeSize: $($vssd.Vtl2AddressRangeSize)"

# 5. Start VM
Write-Host "Starting VM..."
Start-VM -Name $VmName
Start-Sleep 2

# 6. Connect to COM1 (where OpenHCL falls back when COM3 unavailable).
Write-Host "Reading COM1 for $SerialTimeoutSec seconds..."
$pipeName = "$vmid-1"
try {
    $client = New-Object System.IO.Pipes.NamedPipeClientStream('.', $pipeName, [System.IO.Pipes.PipeDirection]::In, [System.IO.Pipes.PipeOptions]::Asynchronous)
    $client.Connect(5000)
    $deadline = (Get-Date).AddSeconds($SerialTimeoutSec)
    $total = 0
    while ((Get-Date) -lt $deadline) {
        $buf = New-Object byte[] 4096
        $iar = $client.BeginRead($buf, 0, $buf.Length, $null, $null)
        $signaled = $iar.AsyncWaitHandle.WaitOne(1000)
        if (-not $signaled) { continue }
        $n = $client.EndRead($iar)
        if ($n -eq 0) { Write-Host '[EOF]'; break }
        $text = [System.Text.Encoding]::UTF8.GetString($buf, 0, $n)
        Write-Host -NoNewline $text
        $total += $n
    }
    Write-Host ""
    Write-Host "[$total bytes read from COM3]"
    $client.Close()
} catch {
    Write-Host "COM3 read error: $_"
}

# 7. Final state
$vm = Get-VM -Name $VmName
Write-Host "Final state: $($vm.State), uptime: $($vm.Uptime)"
