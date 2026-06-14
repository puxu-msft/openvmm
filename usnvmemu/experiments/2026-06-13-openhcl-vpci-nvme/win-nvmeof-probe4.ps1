Write-Output "=== nvmeofutil connect -? (看 transport 参数 tcp/rdma) ==="
(& cmd /c "nvmeofutil.exe connect -? 2>&1") | Out-String
Write-Output "=== nvmeofutil add -? (adapter transport?) ==="
(& cmd /c "nvmeofutil.exe add -? 2>&1") | Out-String
Write-Output "=== nvmeofutil host -? ==="
(& cmd /c "nvmeofutil.exe host -? 2>&1") | Out-String
