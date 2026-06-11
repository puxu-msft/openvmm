# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# POC-7：host 侧 ReadProcessMemory 读 vmwp.exe 扫 guest RAM marker。
#
# 验"host 进程能否够到运行中 Hyper-V OpenHCL VM 的 guest RAM"——纠之前
# 纯研究+推理得出"host 够不到"的过宽结论。subagent 调研已列其为最易 POC
# 路径(机制 #1，classic LiveCloudKd 2010 方法)。
#
# Oracle: GPA 0x100000 处此刻有 marker 0x9E66_0000_0000_9E66（VTL2 侧 POC-6
# 真机写入，VTL2 侧 readback 已确认仍在）。host 侧若 RPM 扫到这 8 字节，则
# host 进程确实能访问 guest RAM；否则若 OpenProcess 被 PPL/ACL 阻断，那本
# 身也是真信号（确认 RPM 路不可行 → 须走 hypercall/驱动）。
#
# 用法（admin PowerShell on Windows host）：
#   .\poc7_rpm_vmwp.ps1 -VmName pcie-remote-exp
# 或 WSL 内：
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File \
#     /mnt/c/temp/pcie_remote_exp/poc7_rpm_vmwp.ps1 -VmName pcie-remote-exp

param(
    [string]$VmName = "pcie-remote-exp",
    [int]$MaxRegionMB = 8192,    # 单 region 上限；guest RAM 通常一大块
    [int]$ChunkMB = 4             # 每次 RPM 读多大
)

# Marker（little-endian 8 字节）：0x9E66_0000_0000_9E66
$Marker = [byte[]]@(0x66, 0x9E, 0x00, 0x00, 0x00, 0x00, 0x9E, 0x66)

function Log($m) { Write-Host "[poc7] $m" }

# 0. admin 自检（OpenProcess vmwp + 读 CmdLine 都需要）
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Log "ERROR: 须以 admin 运行（OpenProcess vmwp + 读 CmdLine 均受 ACL 保护）。"
    Log "用法：管理员 PowerShell 中："
    Log "  cd C:\temp\pcie_remote_exp; .\poc7_rpm_vmwp.ps1 -VmName pcie-remote-exp"
    exit 4
}
Log "running as Administrator ✓"

# 1. 定位 pcie-remote-exp 的 vmwp PID（按 CmdLine 含 VM GUID 匹配；失败 fallback 试所有 vmwp）
$vm = Get-VM -Name $VmName -ErrorAction Stop
$vmId = $vm.Id.Guid
Log "VM: $VmName  GUID: $vmId  State: $($vm.State)"

$vmwpCandidates = @()
$matched = Get-CimInstance Win32_Process -Filter "Name='vmwp.exe'" |
           Where-Object { $_.CommandLine -match $vmId }
if ($matched) {
    $vmwpCandidates = @($matched.ProcessId)
    Log "vmwp PID (GUID match) = $($vmwpCandidates -join ',')"
} else {
    $allVmwp = Get-Process vmwp -EA SilentlyContinue | Select -Expand Id
    $vmwpCandidates = $allVmwp
    Log "GUID 匹配未成；fallback 试所有 vmwp PID = $($vmwpCandidates -join ',')"
}
if (-not $vmwpCandidates) {
    Log "FAIL: 找不到任何 vmwp.exe 进程"; exit 1
}

# 2. P/Invoke OpenProcess + VirtualQueryEx + ReadProcessMemory
Add-Type -Namespace Mem -Name K32 -MemberDefinition @'
[StructLayout(LayoutKind.Sequential)]
public struct MBI {
    public IntPtr BaseAddress;
    public IntPtr AllocationBase;
    public uint AllocationProtect;
    public ushort PartitionId;
    public IntPtr RegionSize;
    public uint State;
    public uint Protect;
    public uint Type;
}
[DllImport("kernel32.dll", SetLastError=true)]
public static extern IntPtr OpenProcess(uint dwDesiredAccess, bool bInheritHandle, int dwProcessId);
[DllImport("kernel32.dll", SetLastError=true)]
public static extern IntPtr VirtualQueryEx(IntPtr hProcess, IntPtr lpAddress, ref MBI lpBuffer, IntPtr dwLength);
[DllImport("kernel32.dll", SetLastError=true)]
public static extern bool ReadProcessMemory(IntPtr hProcess, IntPtr lpBaseAddress, byte[] lpBuffer, IntPtr nSize, out IntPtr lpNumberOfBytesRead);
[DllImport("kernel32.dll")]
public static extern bool CloseHandle(IntPtr hObject);
[DllImport("kernel32.dll")]
public static extern uint GetLastError();
'@

# PROCESS_QUERY_INFORMATION (0x0400) | PROCESS_VM_READ (0x0010)
$ACCESS = 0x0400 -bor 0x0010

foreach ($vmwpPid in $vmwpCandidates) {
    Log "================================================================"
    Log "trying vmwp PID = $vmwpPid"
    $h = [Mem.K32]::OpenProcess($ACCESS, $false, $vmwpPid)
    if ($h -eq [IntPtr]::Zero) {
        $err = [Mem.K32]::GetLastError()
        Log "  OpenProcess FAIL: err=$err (0x$($err.ToString('X')))"
        if ($err -eq 5) {
            Log "  ERROR_ACCESS_DENIED → vmwp 是 PPL/受 NT VIRTUAL MACHINE ACL 保护，RPM 路被阻断。"
            Log "  这本身是真信号：纯用户态 RPM 不可行 → 须走 hypercall/驱动(livekd / WinHvReadGpa)。"
        }
        continue
    }
    Log "  OpenProcess OK (handle=$h)"

    # 3. VirtualQueryEx 遍历，对每个 MEM_COMMIT 大 region 扫 marker
    $MEM_COMMIT = 0x1000
    $addr = [IntPtr]::Zero
    $mbi = New-Object Mem.K32+MBI
    $mbiSize = [IntPtr][System.Runtime.InteropServices.Marshal]::SizeOf([type][Mem.K32+MBI])
    $totalScanned = 0L
    $hits = @()
    $regionCount = 0

    while ($true) {
        $r = [Mem.K32]::VirtualQueryEx($h, $addr, [ref]$mbi, $mbiSize)
        if ($r -eq [IntPtr]::Zero) { break }
        $regionCount++

        $regionSize = [int64]$mbi.RegionSize
        $base = [int64]$mbi.BaseAddress

        if ($mbi.State -eq $MEM_COMMIT -and $regionSize -ge 1MB -and $regionSize -le ($MaxRegionMB * 1MB)) {
            $chunkBytes = $ChunkMB * 1MB
            $buf = New-Object byte[] $chunkBytes
            $off = 0L
            while ($off -lt $regionSize) {
                $toRead = [Math]::Min($chunkBytes, $regionSize - $off)
                $bytesRead = [IntPtr]::Zero
                $ok = [Mem.K32]::ReadProcessMemory($h, [IntPtr]($base + $off), $buf, [IntPtr]$toRead, [ref]$bytesRead)
                if ($ok) {
                    $n = [int]$bytesRead
                    $totalScanned += $n
                    for ($i = 0; $i -le $n - 8; $i++) {
                        if ($buf[$i] -eq 0x66 -and $buf[$i+1] -eq 0x9E -and `
                            $buf[$i+2] -eq 0x00 -and $buf[$i+3] -eq 0x00 -and `
                            $buf[$i+4] -eq 0x00 -and $buf[$i+5] -eq 0x00 -and `
                            $buf[$i+6] -eq 0x9E -and $buf[$i+7] -eq 0x66) {
                            $hitAddr = $base + $off + $i
                            $hits += $hitAddr
                            Log ("  ✓ HIT @ vmwp VA 0x{0:X16}  (region base 0x{1:X16} + off 0x{2:X})" -f $hitAddr, $base, ($off + $i))
                            if ($hits.Count -ge 5) { break }
                        }
                    }
                }
                $off += $toRead
                if ($hits.Count -ge 5) { break }
            }
        }

        $next = [int64]$mbi.BaseAddress + [int64]$mbi.RegionSize
        if ($next -le [int64]$addr) { break }
        $addr = [IntPtr]$next
        if ($hits.Count -ge 5) { break }
    }

    [Mem.K32]::CloseHandle($h) | Out-Null
    Log ("  regions visited: $regionCount   total scanned: {0:N0} bytes ({1:N1} MiB)" -f $totalScanned, ($totalScanned/1MB))
    Log "  marker hits: $($hits.Count)"
    if ($hits.Count -gt 0) {
        Log "POC-7 PASSED ✓ — host 进程 RPM(PID=$vmwpPid) 读到 guest RAM 里的 VTL2-写入 marker。"
        Log "⟹ '任何 host 进程都够不到 guest RAM' 这个过宽结论被推翻。topology B 需重判。"
        exit 0
    }
}

Log "================================================================"
Log "所有 vmwp 候选都未扫到 marker。可能原因："
Log "  (a) OpenProcess 全部 ACCESS_DENIED → RPM 路不通 → 须用 livekd / 驱动 (确认结论)"
Log "  (b) marker 已被 guest OS 覆写"
Log "  (c) guest RAM 大块超过 MaxRegionMB($MaxRegionMB MB) 被跳过"
Log "  (d) vmwp 私有区不含 guest RAM（VM 用了 GPA-direct/SLAT 而非 vmwp VA 映射）"
exit 3
