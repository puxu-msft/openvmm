# Boot VM and stream COM1 named pipe for $TimeoutSec seconds.
# Uses async BeginRead with cancellation via WaitOne.
#
# ⚠ 几乎无用 (2026-05-30):
#   1. OpenHCL boot_logger 只写 COM3，不写 COM1。
#      参 openhcl/openhcl_boot/src/boot_logger.rs:81 — `com3_serial_available`
#      被 true 才用 serial，否则 in-memory only。
#   2. Win11 26200 stock Hyper-V Gen2 VM 不支持 COM3 暴露为命名管道
#      （仅 Insider Canary ≥27813 用 EnableAdditionalComPorts 才行）。
#   3. 即使能拿到 COM1 pipe，里面也是空的（无任何 OpenHCL 输出）。
#
#   **正解**：用 ohcldiag-dev.exe 读 VTL2 kernel log：
#     ohcldiag-dev.exe <vm> kmsg | Select-String pcie_remote
#
#   本脚本保留作 NamedPipeClientStream 异步读 + cancellation 代码范例。

param(
    [string]$VmName = 'pcie-remote-exp',
    [int]$TimeoutSec = 30
)
$ErrorActionPreference = 'Continue'

$vm = Get-VM -Name $VmName
$vmid = $vm.Id.Guid

if ($vm.State -ne 'Off') {
    Stop-VM -Name $VmName -Force -TurnOff
    Start-Sleep 2
}

Start-VM -Name $VmName
Write-Host "Started VM (id=$vmid)"
Start-Sleep 1

# Connect to named pipe (vmms creates the server side when VM is running)
$pipeName = "$vmid-1"
Write-Host "Connecting to \\.\pipe\$pipeName (timeout ${TimeoutSec}s)..."
try {
    $client = New-Object System.IO.Pipes.NamedPipeClientStream('.', $pipeName, [System.IO.Pipes.PipeDirection]::In, [System.IO.Pipes.PipeOptions]::Asynchronous)
    $client.Connect(5000)
    Write-Host "Connected. Reading..."
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    $total = 0
    while ((Get-Date) -lt $deadline) {
        $buf = New-Object byte[] 4096
        $iar = $client.BeginRead($buf, 0, $buf.Length, $null, $null)
        if (-not $iar.AsyncWaitHandle.WaitOne(1000)) { continue }
        $n = $client.EndRead($iar)
        if ($n -eq 0) { Write-Host '[EOF]'; break }
        $text = [System.Text.Encoding]::UTF8.GetString($buf, 0, $n)
        Write-Host -NoNewline $text
        $total += $n
    }
    Write-Host ""
    Write-Host "[$total bytes total]"
    $client.Close()
} catch {
    Write-Host "Pipe error: $_"
}

$vm2 = Get-VM -Name $VmName
Write-Host "VM final: State=$($vm2.State), Uptime=$($vm2.Uptime), CPU=$($vm2.CPUUsage)%"
