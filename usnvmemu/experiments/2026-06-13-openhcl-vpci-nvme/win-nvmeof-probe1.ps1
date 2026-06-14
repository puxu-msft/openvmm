Write-Output "=== OS ==="
(Get-ComputerInfo | Select-Object OsName,OsVersion,OsBuildNumber,WindowsProductName | Format-List | Out-String).Trim()

Write-Output "`n=== 1. INF mentioning NVMe-oF / fabrics / nvmf ==="
$hits = Select-String -Path "$env:WINDIR\inf\*.inf" -Pattern "nvmeof|nvme-of|nvmf|fabricsclient|nvmefabric|BusTypeNvmeof" -ErrorAction SilentlyContinue
if ($hits) { $hits | ForEach-Object { "$($_.Filename): $($_.Line.Trim())" } | Select-Object -First 30 } else { "NO inf hit" }

Write-Output "`n=== 2. drivers in System32\drivers matching nvme/nvmf/fabric ==="
Get-ChildItem "$env:WINDIR\System32\drivers\*.sys" -ErrorAction SilentlyContinue | Where-Object { $_.Name -match "nvme|nvmf|fabric|storufs" } | Format-Table Name,Length,LastWriteTime -AutoSize | Out-String -Width 200

Write-Output "`n=== 3. services/drivers named nvme/nvmf/fabric ==="
Get-CimInstance Win32_SystemDriver -ErrorAction SilentlyContinue | Where-Object { $_.Name -match "nvme|nvmf|fabric" } | Format-Table Name,State,StartMode,PathName -AutoSize | Out-String -Width 200

Write-Output "`n=== 4. pnputil enum-drivers grep nvme/fabric ==="
$pn = & pnputil.exe /enum-drivers 2>$null | Out-String
($pn -split "`r?`n") | Select-String -Pattern "nvme|fabric|nvmf" | Select-Object -First 20 | ForEach-Object { $_.Line.Trim() }

Write-Output "`n=== 5. 现有真 NVMe 盘的 BusType (确认 stornvme=NVMe 基线) ==="
Get-PhysicalDisk -ErrorAction SilentlyContinue | Select-Object DeviceId,FriendlyName,BusType,MediaType,@{N='GB';E={[int]($_.Size/1GB)}} | Format-Table -AutoSize | Out-String -Width 200

Write-Output "`n=== 6. nvme CLI / windows nvme management present? ==="
@("nvme.exe","nvmecli.exe") | ForEach-Object { $c = Get-Command $_ -ErrorAction SilentlyContinue; if ($c) { "FOUND: $($c.Source)" } else { "absent: $_" } }
