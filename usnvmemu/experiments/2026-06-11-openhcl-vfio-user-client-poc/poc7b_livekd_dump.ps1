# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# POC-7b：livekd 路径 — Sysinternals 现成工具，底层调 HvReadGpa hypercall
# 把活 Hyper-V VM 的 guest 物理内存写到 raw dump 文件。然后 host 侧扫 marker
# 0x9E66_0000_0000_9E66（POC-6 在 VTL2 写入 GPA 0x100000 的，仍在）。
#
# 优势 vs POC-7(RPM-on-vmwp)：livekd 是 supported 工具、走 hypercall 路径，
# 不依赖 vmwp 内部 VA 布局；最快坐实"host 能读 guest RAM"。
#
# 须 admin。用法：
#   管理员 PowerShell：cd C:\temp\pcie_remote_exp; .\poc7b_livekd_dump.ps1

param(
    [string]$VmName = "pcie-remote-exp",
    [string]$LiveKd = "C:\temp\pcie_remote_exp\livekd64.exe",
    [string]$DumpFile = "C:\temp\pcie_remote_exp\poc7b_dump.bin",
    [int]$DumpSizeMB = 8     # 抓低 8 MiB 即可覆盖 GPA 0x100000 处 marker
)

function Log($m) { Write-Host "[poc7b] $m" }

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) { Log "ERROR: 须 admin 运行"; exit 4 }

# livekd 需要 _NT_SYMBOL_PATH 或 dbghelp，但生成 raw dump 不强制；先试 -o
Log "VM: $VmName"
Log "livekd: $LiveKd"
if (-not (Test-Path $LiveKd)) { Log "FAIL: $LiveKd 不存在"; exit 1 }

# 试 -hv -p -o：暂停 VM 拿一致快照，dump 到文件
# livekd 默认想交互启动 windbg；对 raw dump 用 -o 落盘。注意：livekd 仍可能
# 要求 dbghelp.dll；若失败，备选 LiveCloudKd 路径(机制 #1 子路)。
$args = @("-hv", $VmName, "-p", "-o", $DumpFile, "-y", "srv*")
Log "running: livekd64.exe $($args -join ' ')"
& $LiveKd @args 2>&1 | Tee-Object -Variable LiveKdOut
if (-not (Test-Path $DumpFile)) {
    Log "FAIL: dump 文件未生成；livekd 输出见上方。常见原因："
    Log "  - 缺 dbghelp.dll/Debugging Tools for Windows (livekd 拒启)"
    Log "  - 该 VM 是 CVM/受保护(HvReadGpa 失败)"
    Log "  - VM 名/GUID 不被 livekd 识别"
    Log "若 livekd 路不通，转 POC-7 (RPM-on-vmwp) 或写驱动直调 WinHvReadGpa。"
    exit 2
}
$dumpSize = (Get-Item $DumpFile).Length
Log ("dump 文件: {0} ({1:N0} bytes, {2:N1} MiB)" -f $DumpFile, $dumpSize, ($dumpSize/1MB))
Log "→ 在 WSL 扫 marker：xxd -s 0x100000 -l 16 $($DumpFile -replace 'C:\\','/mnt/c/' -replace '\\','/')"
