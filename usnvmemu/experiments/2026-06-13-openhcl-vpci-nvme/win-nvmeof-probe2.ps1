Write-Output "=== A. stornvmeofi.inf 全文 ==="
Get-Content "$env:WINDIR\inf\stornvmeofi.inf" -ErrorAction SilentlyContinue

Write-Output "`n=== B. NVMe-oF / NVMe PowerShell cmdlets ==="
Get-Command "*nvme*","*nvmeof*","*fabric*" -ErrorAction SilentlyContinue | Format-Table CommandType,Name,Source -AutoSize | Out-String -Width 200

Write-Output "`n=== C. stornvmeofi 驱动文件 version 信息 ==="
$f = Get-Item "$env:WINDIR\System32\drivers\stornvmeofi.sys" -ErrorAction SilentlyContinue
if ($f) { $f.VersionInfo | Format-List FileVersion,ProductVersion,FileDescription,CompanyName }

Write-Output "`n=== D. nvmedisk.sys (新驱动) version ==="
$g = Get-Item "$env:WINDIR\System32\drivers\nvmedisk.sys" -ErrorAction SilentlyContinue
if ($g) { $g.VersionInfo | Format-List FileVersion,FileDescription }

Write-Output "`n=== E. stornvmeofi 支持的 transport (strings 扫 TCP/RDMA/discovery) ==="
$bytes = [System.IO.File]::ReadAllBytes("$env:WINDIR\System32\drivers\stornvmeofi.sys")
$ascii = -join ($bytes | ForEach-Object { if ($_ -ge 32 -and $_ -le 126) { [char]$_ } else { "`n" } })
($ascii -split "`n") | Where-Object { $_.Length -ge 4 } | Select-String -Pattern "tcp|rdma|discover|traddr|trsvcid|subnqn|hostnqn|nqn\.|fabric|keep.?alive|controller" -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Line -Unique | Select-Object -First 40

Write-Output "`n=== F. 是否有 WMI/MI class for nvmeof 管理 ==="
Get-CimClass -Namespace root\Microsoft\Windows\Storage -ClassName "*Nvme*" -ErrorAction SilentlyContinue | Select-Object CimClassName
