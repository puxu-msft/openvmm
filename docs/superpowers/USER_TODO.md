# 需要用户配合的事项

> Claude 无人值守跑到的边界。所有不需用户配合的部分都已推进；本表列出剩下的人工操作。

## 强烈推荐：让 WSL 直连 Windows Hyper-V (`/dev/mshv`)

**为什么**：这是 OpenVMM/OpenHCL 在 WSL 内最原生的开发方案 —— 用 Microsoft Hypervisor (不是 KVM)，与 Windows host 上跑 OpenVMM 行为一致；KVM 是 Linux-only，行为差异较大。

**用户操作**：
1. Windows 11 22H2+ 主机
2. PowerShell（管理员）启用 Hyper-V：
   ```powershell
   Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All
   ```
3. 在 `%USERPROFILE%\.wslconfig` 加：
   ```ini
   [wsl2]
   nestedVirtualization=true
   kernelCommandLine=hyperv_default_partition=1
   ```
4. `wsl --shutdown` 后重开 WSL
5. 验证：`ls /dev/mshv` 应能看到

**效果**：之后 `target/debug/openvmm --hypervisor mshv ...` 就能用真硬件加速跑 guest，不依赖 KVM。

---

## 次选：用本机 KVM（更快但与产品场景差异大）

### 用户操作
```bash
sudo usermod -aG kvm $USER
newgrp kvm   # 或重新登录 WSL
```
验证：`ls -la /dev/kvm` 应该读写权限里有自己。

---

## 跨编 Windows openvmm.exe（如果想在 Windows 上原生跑）

### 推荐：直接在 Windows 上原生编译（不需要 xwin）

既然你已经有 VS Enterprise 2022 + Windows SDK，**最简单**：
- 用 PowerShell 直接到仓库目录 `cd \\wsl$\Ubuntu\home\xp\refs\openvmm`（或把代码 copy 到 Windows 盘）
- `cargo build -p openvmm`（会自动用 `x86_64-pc-windows-msvc` target）
- 产出 `target\debug\openvmm.exe`

VS Build Tools 已经把 `link.exe` / `INCLUDE` / `LIB` 环境配好了。

### 备选：WSL 跨编（需要 xwin 模拟 SDK 环境）

用户的 Windows SDK 装在 Windows 文件系统里，**WSL 跨编不能直接复用**，原因：

1. **路径混乱**：Windows SDK 用 `C:\Program Files\...` 路径，WSL 看到的是 `/mnt/c/...`；SDK 内部 .props/.targets 文件互相用 Windows 路径引用，cargo/rustc 无法解析。
2. **大小写敏感**：Linux fs 大小写敏感，`#include <Windows.h>` vs 实际 `windows.h` 不匹配；WSL 9P 协议虽然不区分但会引发其他冲突。
3. **环境变量集合**：rustc 需要 `INCLUDE`、`LIB`、`LIBPATH`、`WINDOWSSDKDIR`、`UCRTVersion` 等十几个 Windows 风格变量正确指向 SDK；用户不该手工拼。

**xwin 的作用**：从 Microsoft 公开下载 SDK + MSVC libs（不需要 VS license），解压到 Linux 友好的目录（规范大小写、生成 symlink），输出 `.cargo/config.toml` 片段把 `linker = "lld-link"` 和路径都配好。**等价于"在 Linux 上准备一份纯净的 Windows toolchain"**。

如果你想走这条：
```bash
cargo install xwin    # 已装
xwin --accept-license splat --output ~/.xwin
# 配 .cargo/config.toml 指向 ~/.xwin
cargo build --target x86_64-pc-windows-msvc -p openvmm
```

但**直接在 Windows 编**更省事。

> 不装 mingw 的替代：在 Windows 上原生 `cargo build`（用 Visual Studio Build Tools），不需要跨编。这是仓库默认推荐路径。

---

## 在 Windows host 上跑生产 Hyper-V Path C（真 vsock + IGVM）

### 用户操作
1. 在 Windows host 启用 Hyper-V Platform（同上 mshv 步骤 2）
2. 创建一个 VM 配 OpenHCL：
   ```powershell
   New-VM -Name MyExpVM -Generation 2 -MemoryStartupBytes 2GB
   # ... 按 OpenHCL 文档配 IGVM ...
   ```
3. 添加占位 NVMe controller（不绑磁盘）：
   ```powershell
   Add-VMNvmeController -VMName MyExpVM
   $Ctrl = (Get-VMNvmeController -VMName MyExpVM)[0]
   $NvmeGuid = $Ctrl.Id
   Write-Host "Use this GUID in OpenHCL cmdline: $NvmeGuid"
   ```
4. 一次性注册 vsock service GUID + ACL：
   ```powershell
   .\docs\superpowers\scripts\setup-pcie-remote.ps1 -VsockPort 50000
   ```
5. OpenHCL cmdline 注入（需要 IGVM build 时 cmdline policy=APPEND_CHOSEN）：
   ```
   OPENHCL_PCIE_REMOTE_INSTANCE=<NvmeGuid>:50000
   ```
6. 在 Windows 编译并启动 host 实验程序：
   ```powershell
   cd docs\superpowers\examples\pcie_remote_noop_host
   cargo run
   ```
   （它默认 bind 127.0.0.1:48914 是 TCP loopback，需要适配 vsock —— Phase 10 的扩展，spec K-20）
7. 启动 VM，guest dmesg 应看到 PCIe 设备。

---

## 何时不需要用户配合

- 所有 30 个单测/集成测试可以一直跑通（`cargo test -p pcie_remote_device -p pcie_remote_protocol`）
- 所有 cargo check / clippy / build 都能本地完成
- spec / plan / 文档 / setup.ps1 / host SDK 示例都已落盘

## 联系点

如果以上任一项遇到问题，把错误粘给 Claude 继续处理。
