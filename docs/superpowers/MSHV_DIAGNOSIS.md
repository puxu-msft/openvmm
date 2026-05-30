# `/dev/mshv` 排查总结

> **2026-05-30 更新：Path C 已端到端闭环，不再需要 mshv。** 本文档保留作为
> 历史诊断记录 + 未来想跑 Linux + mshv backend OpenVMM 时的参考。

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

## 当前决策

**Path C（Hyper-V on Windows + OpenHCL IGVM）已经完整闭环**，详见
[HYPERV_RUNBOOK.md](HYPERV_RUNBOOK.md) 和 [SESSION_LOG.md](SESSION_LOG.md)。
该路径不依赖 WSL 内的 `/dev/mshv`，所有 Hyper-V 操作都通过
`/mnt/c/Windows/System32/.../powershell.exe` 完成。

**结论**：mshv 不再是当前阻塞，可保持现状。

## 历史方案（如未来需要 mshv 再启用）

### A. 编自定义 WSL kernel 加 `CONFIG_MSHV_ROOT`

1. clone [microsoft/WSL2-Linux-Kernel](https://github.com/microsoft/WSL2-Linux-Kernel)
2. 找 `Microsoft/config-wsl` 看是否含 `CONFIG_MSHV_ROOT`；没有就加：
   ```
   CONFIG_MSHV_ROOT=y
   CONFIG_HYPERV=y
   ```
3. `make -j$(nproc)` → 得 `arch/x86/boot/bzImage`
4. `.wslconfig` 加 `kernel = C:\path\to\bzImage`
5. `wsl --shutdown` 重启

工期：2–4 小时（首次编 WSL kernel）；可靠性不保证（Microsoft 不官方支持）。

### B. Hyper-V 起完整 Linux VM 在里面跑 OpenVMM+mshv

1. Hyper-V 起 Ubuntu Server VM（不是 WSL）
2. 启用 nested virt
3. VM 内装支持 mshv 的 distro / 自编内核
4. VM 内跑 OpenVMM with mshv backend

工期：数小时；多一层 VM nesting。

## 何时再考虑 mshv

- 想在 WSL 内直接跑 OpenVMM + VTL2，不通过 Hyper-V WMI（避免每次都跳 PowerShell）
- 想跑 OpenVMM 自带 CVM/SNP 测试，且没有真 SNP 硬件（mshv 可以模拟）
- 想脱离 Hyper-V 主管 VM lifecycle

当前 Path C 都覆盖了上述场景的可用替代，所以不需要 mshv。
