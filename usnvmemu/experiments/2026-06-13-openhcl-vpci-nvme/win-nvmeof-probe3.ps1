Write-Output "=== nvmeofutil.exe 用法 (看支持的 transport: tcp/rdma) ==="
$o = & cmd /c "nvmeofutil.exe /? 2>&1"; if (-not $o) { $o = & cmd /c "nvmeofutil.exe -h 2>&1" }; if (-not $o) { $o = & cmd /c "nvmeofutil.exe 2>&1" }
$o | Out-String

Write-Output "`n=== stornvmeofi.sys: TCP vs RDMA transport 明确扫 ==="
$bytes = [System.IO.File]::ReadAllBytes("$env:WINDIR\System32\drivers\stornvmeofi.sys")
$ascii = -join ($bytes | ForEach-Object { if ($_ -ge 32 -and $_ -le 126) { [char]$_ } else { "`n" } })
$lines = ($ascii -split "`n") | Where-Object { $_.Length -ge 3 }
Write-Output "--- 含 'tcp' (大小写不敏感) ---"
$lines | Select-String -Pattern "tcp" -CaseSensitive:$false | Select-Object -ExpandProperty Line -Unique | Select-Object -First 25
Write-Output "--- 含 'transport' ---"
$lines | Select-String -Pattern "transport" -CaseSensitive:$false | Select-Object -ExpandProperty Line -Unique | Select-Object -First 15
Write-Output "--- 含 'wsk' (Winsock Kernel = TCP 栈标志) ---"
$lines | Select-String -Pattern "^Wsk|WskSocket|WskConnect" | Select-Object -ExpandProperty Line -Unique | Select-Object -First 10

Write-Output "`n=== nvmeofutil.exe version ==="
(Get-Item "$env:WINDIR\System32\nvmeofutil.exe").VersionInfo | Format-List FileVersion,FileDescription

Write-Output "`n=== 是否有独立 nvme-tcp transport 模块 ==="
Get-ChildItem "$env:WINDIR\System32\drivers\*.sys" | Where-Object { $_.Name -match "tcp|wsk|nvmf" } | Format-Table Name -AutoSize | Out-String
