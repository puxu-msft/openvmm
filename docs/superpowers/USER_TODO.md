# 需要用户配合的事项

> 已经验证：你的 Windows host 上 Hyper-V 已装并 running（vmms+vmcompute 服务在跑），
> Windows sudo / gsudo 也都装了，PowerShell 5.1 在 WSL 内可直接调用 (`/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe`)。
>
> **当前阻塞点：从 WSL 调出的 PowerShell 是 medium IL token，UAC 把管理员权限剥离了，因此 `Get-VM` 静默无输出。** 一次提权操作即可彻底解决（见 §1）。

---

## §1【一次性，强烈推荐】把当前 Windows 用户加入 `Hyper-V Administrators`

这是消除"每次操作 Hyper-V 都需要弹 UAC"的根本解。做完这一步后，**Claude 可以从 WSL 完全无人值守地控制 Hyper-V**，包括创建 VM、添加 NVMe controller、查状态、删除等。

### 操作（一次性）

1. **Windows host 开管理员 PowerShell**（Win+X → "Terminal (Admin)"，或开始菜单搜 "Powershell" → 右键管理员运行）
2. 跑：

   ```powershell
   net localgroup "Hyper-V Administrators" puxu /add
   ```

3. **完全注销当前 Windows 用户再登录**（仅重启 PowerShell 不够，组成员资格在登录时被锁进 token）。
4. 验证（重登后，从 WSL 调即可，不需要 elevation）：

   ```bash
   /mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe -NoProfile -Command "Get-VMHost | Select-Object Name"
   ```

   能输出主机名 = 成功。

### 等价的图形界面操作（如果不想敲命令）

控制面板 → 用户账户 → 管理其他账户 → 改账户类型 ❌ 不行，要走：

`compmgmt.msc` (Computer Management) → System Tools → Local Users and Groups → Groups → `Hyper-V Administrators` → 右键 Properties → Add → 输入 `puxu` → OK。然后注销重登。

---

## §2 创建一个测试用 Hyper-V VM（让 Claude 试 OpenHCL Path C）

完成 §1 之后，**Claude 可以从 WSL 完全自动**做这一步。但也提供给你看清楚到底要建什么。

详细 runbook：[`HYPERV_RUNBOOK.md`](HYPERV_RUNBOOK.md)。

---

## §3【可选】启用 WSL2 `/dev/mshv`

让 WSL 内直接用 Microsoft Hypervisor（与 Hyper-V 共享 root partition），不需要切到 Windows 跑 VM。OpenHCL 的 VTL2 在 WSL 内就能跑（比 KVM 强：KVM 不支持 VTL2）。

### 操作

1. **Windows host 管理员 PowerShell**：

   ```powershell
   # 1. 确认 Hyper-V 平台 + nested virt 都已启用（你机器看起来已经够了）
   Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All -All
   Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform -All
   # 上一步可能要求重启。

   # 2. 编辑 %USERPROFILE%\.wslconfig（如不存在则创建）
   notepad $env:USERPROFILE\.wslconfig
   ```

2. `.wslconfig` 加（或合并）：

   ```ini
   [wsl2]
   nestedVirtualization=true
   kernelCommandLine=hyperv_default_partition=1
   ```

3. PowerShell 跑 `wsl --shutdown`，然后重开 WSL。

4. 验证：在 WSL 里 `ls -la /dev/mshv`，能看到 char device 即成功。

完成后告诉 Claude，会自动切到 mshv backend 启 OpenHCL（替代当前 KVM 路径）。

---

## §4【生产 Hyper-V Path C】仅在做完 §1 后由 Claude 自动执行

完成 §1 之后，Claude 会自动跑以下序列（不再需要人工操作）：

1. `New-VM` 创建一个 Gen2 VM（256MB / 1 vCPU 即够）
2. `Add-VMNvmeController` 加占位 NVMe controller（不绑磁盘）
3. 跑 `setup-pcie-remote.ps1 -VsockPort 50000` 注册 service GUID + ACL
4. `Set-VMFirmware` 指向自建 IGVM `\\wsl$\Ubuntu\home\xp\refs\openvmm\flowey-out\artifacts\build-igvm\ship\x64\openhcl-x64.bin`
5. 在 WSL 里编一个 host SDK 实验程序（vsock client）
6. `Start-VM`，guest dmesg 看到 pcie_remote 设备

但**第 4 步设 IGVM 路径需要本机 Hyper-V 支持自定义 IGVM 加载**，这是 OpenHCL preview 功能；如果 Microsoft VM 模板不让传 custom IGVM，要走 `mshv` 或 OpenVMM-hosted OpenHCL 路径。

---

## §5【可选】跨编 openvmm.exe 在 Windows 跑

xwin 已在 WSL 安装好（`~/.xwin/sdk` + `~/.xwin/crt`），但还差 `lib.exe` / `clang-cl`。
**更简单**：直接在 Windows 上 `cd \\wsl$\Ubuntu\home\xp\refs\openvmm` + `cargo build -p openvmm`，会自动用 VS Build Tools 已装好的 link.exe。

---

## 不需用户配合 / 已完成

- ✅ 30+ 单测 / 集成测试 全部通过
- ✅ `cargo build -p openvmm` (Linux KVM 可跑)
- ✅ `cargo xflowey build-igvm x64 --release` 完整 IGVM (19MB ship)
- ✅ 真 KVM 端到端验证（OpenVMM + UEFI + PCIe RC + pcie_remote 设备完整加载）
- ✅ AbsentPcieDevice 兜底验证（host 不在 → VM 仍正常启动，spec §3.10 layer-2）
- ✅ Path C (NVMe takeover) 代码完整
- ✅ host SDK example (client mode) 已可用

详见 [SESSION_LOG.md](SESSION_LOG.md)。

## 联系点

完成 §1 即可解锁后续所有自动化。完成后告诉 Claude 一声，剩下的事情我会接着做。
