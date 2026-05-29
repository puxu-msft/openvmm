# `/dev/mshv` 排查总结

> 2026-05-30：你配了 `kernelCommandLine = "hyperv_default_partition=1"` 但 `/dev/mshv` 没出现。完整诊断 + 解决路径如下。

## 诊断结果

```
WSL version       : 2.7.3.0
WSL kernel        : 6.6.114.1-1  (microsoft-standard-WSL2)
Windows version   : 10.0.26200.8390
kernel cmdline    : ... hyperv_default_partition=1   ← 已生效

dmesg 警告:
  "Unknown kernel command line parameters ... hyperv_default_partition=1,
   will be passed to user space."
                                                     ← 内核不认识，所以无效

modprobe mshv     : FATAL: Module mshv not found
/lib/modules/.../kernel/ 内不存在 mshv.ko

/sys/module 实有: hv_balloon hv_netvsc hv_sock hv_storvsc hv_utils hv_vmbus
                  ← 这些是 guest 端 VSC drivers（任何 WSL2 都有），
                    NOT root partition driver
```

**根因**：WSL2 自带的 `6.6.114.1-1-microsoft-standard-WSL2` 是 Microsoft 维护的精简内核（guest 用），**未编入 `CONFIG_MSHV_ROOT` 选项**，因此：

1. `hyperv_default_partition=1` 这个 kernel arg 是给 mshv driver 用的，但 driver 不存在 → 被忽略并 warn
2. `mshv` 模块文件根本没生成 → `/dev/mshv` 永不会创建

**与 Windows 端无关** —— 你的 Windows host Hyper-V Platform / HypervisorPlatform / VirtualMachinePlatform feature 都已 Installed，bug 在 WSL guest 内核。

## 解决路径

### A. 用 dev WSL kernel（短期，不可靠）

Microsoft Linux WSL kernel repo (github.com/microsoft/WSL2-Linux-Kernel) 的某些 branch 可能带 `CONFIG_MSHV_ROOT=y`，但 main release 不带。需要：

1. clone repo + 找 `Microsoft/config-wsl-arm64` 或 `Microsoft/config-wsl` 看是否含 `CONFIG_MSHV_ROOT`
2. 没有的话，自己改 `.config` 加：
   ```
   CONFIG_MSHV_ROOT=y
   CONFIG_HYPERV=y
   ```
3. `make -j$(nproc)` 编译，得到 `arch/x86/boot/bzImage`
4. 在 `.wslconfig` 加 `kernel = C:\\path\\to\\bzImage`
5. `wsl --shutdown` → 重启 WSL

工期：2~4 小时（首次编 WSL kernel）；可靠性低（Microsoft 不官方支持）。

### B. 跳过 mshv 改用 Hyper-V WMI 路径（推荐）

**这正是我们现在的 Path C 已经在做的事**：
- 不需要 WSL 内 `/dev/mshv`
- 直接从 WSL 通过 `/mnt/c/Windows/System32/powershell.exe` 调 Windows Hyper-V cmdlets / WMI
- 已经成功：创建 OpenHCL VM、加载 IGVM、配 service GUID、跨编 ohcldiag-dev.exe

**当前阻塞点**（USER_TODO.md §阻塞）：自建 IGVM 的 OpenHCL diag_server 没响应。这跟 mshv 无关。

### C. 用 Hyper-V 起一个完整 Linux VM 在里面跑 OpenVMM-with-mshv（最绕但最干净）

1. Hyper-V 起一个 Ubuntu Server VM（不是 WSL）
2. 该 VM 启用 nested virt
3. 在 VM 内装支持 mshv 的 Linux distro / 编内核
4. 内嵌 KVM 或 mshv backend 跑 OpenVMM/OpenHCL

工期：数小时；远离用户当前流程。

## 我的建议

**不要折腾 mshv**。当前 Path C 用 WSL → Windows PowerShell → Hyper-V 已经走通 95%。最后 5%（OpenHCL diag listener timeout）是独立技术问题，跟 mshv 无关，应该用 **USER_TODO §A/B** 路径继续：

- §A：让 Claude 写 `.vmrs` 解析工具抽 VTL2 boot log
- §B：你跑 `openhcl/Set-OpenHCL-HyperV-VM.ps1` 验证 baseline

请确认是否继续走 Path C，或者明确说 "我就是要 /dev/mshv"，我帮你编 WSL kernel。
