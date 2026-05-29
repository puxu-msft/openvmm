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

### 用户操作
```bash
sudo apt install mingw-w64
```

然后我可以：
```bash
cargo build --target x86_64-pc-windows-gnu -p openvmm
```
产出 `target/x86_64-pc-windows-gnu/debug/openvmm.exe`，复制到 Windows 跑。

### 为什么需要 mingw（用户疑问回答）

OpenVMM 的 **Rust 源码**确实是纯 Rust，**rustc 自己只产生 .o 对象文件**。把 .o 链成可执行 .exe 需要：

| 目标 | 链接器需求 |
|------|-----------|
| `x86_64-unknown-linux-gnu` | 系统 `ld` / `gcc`（Linux 自带） |
| `x86_64-unknown-linux-musl` | `ld` + musl libc（rustup target 自带 musl lib） |
| `x86_64-pc-windows-msvc` | MSVC `link.exe` + Windows SDK（仅 Windows） |
| `x86_64-pc-windows-gnu` | **mingw-w64 的 `x86_64-w64-mingw32-gcc`**（在 Linux 上跨编 .exe 的标准选择） |

另外 OpenVMM 还透过 `windows-sys` / `socket2` / 部分 native 依赖引用 Windows 的 system import lib（如 `kernel32.lib`、`ws2_32.lib`），这些 .lib 文件 mingw-w64 也提供了 GNU 风格的等价品。

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
