# 需要用户配合的事项（更新版）

> **2026-05-30 更新：** K-IDs 全部清零（K-8/11/15/17/18/19，共 48 tests 通过）。Path C 仍卡在 `VMBusMessageRedirection=1` 设置后 diag_server 10060；准备进入 A 路线（.vmrs 抽取 boot_log）继续无人值守推进。

## 当前状态

| 项 | 状态 | 备注 |
|---|---|---|
| 加入 Hyper-V Administrators | ✅ 用户完成 | `Get-VM` 可用 |
| 跨编 ohcldiag-dev.exe（WSL → Windows） | ✅ Claude 完成 | 用仓库官方 cross-compile 方案；不需要 xwin |
| 注册 vsock service GUID 1+2 | ✅ Claude 完成 | gsudo cache 一次性 elevation |
| 创建 OpenHCL VM (pcie-remote-exp) | ✅ Claude 完成 | 用 petri 自带 `hyperv.psm1::New-CustomVM` |
| 设置 `FirmwareFile` + `GuestFeatureSet=0x201` | ✅ Claude 完成 | WMI ModifySystemSettings |
| 设置 `AllowFirmwareLoadFromFile=1` reg | ✅ 已存在 | 接受自建 IGVM |
| 设置 `VMBusMessageRedirection=1` | ✅ Claude 完成 | VTL2 vsock listener 必需 |
| 自定义 dev IGVM（含 boot_log debug pool） | ✅ Claude 完成 | `openhcl-x64-com1.bin` 74MB |
| vsock client 跨编 | ✅ Claude 完成 | `pcie_remote_noop_host_vsock.exe` |
| VM Start | ✅ Running, HCS state=Created | VP0 HLT 0.1% VP1 0% |
| **OpenHCL VTL2 diag_server 连接** | ❌ **timeout 10060** | **当前阻塞点** |
| **K-8/11/15/17/18/19 spec gaps** | ✅ **全部清零** | **48 tests pass** |

## 阻塞点：VMBusMessageRedirection=1 后仍 10060

`ohcldiag-dev.exe pcie-remote-exp inspect /` 仍返回 `os error 10060`。可能原因：
- VTL2 内部 underhill_core 早期 panic（COM3 不可读因 Win11 26200 stock 不支持 Hyper-V Gen2 COM3）
- diag_server 没 publish 到 redirected vmbus channel
- 仍缺一个 WMI 字段或 registry key

### Claude 已准备的下一步（无人值守）

**A. .vmrs boot_log 抽取**（subagent 已设计，~80 LOC Rust）
- 抽出 VTL2 boot_log buffer（8KB at fixed GPA）
- 读出 OpenHCL 启动早期日志，确定 panic 位置
- 工期：约 2 小时

**B. 验证 spec §3 完整性**（被加在 K-IDs 之外补的兜底）
- 再检查一次 inspect 树 + tracing 覆盖
- 工期：30 分钟

### 用户可选的协作（任一无要求）

**用户在 Windows 上跑 `Set-OpenHCL-HyperV-VM.ps1`**（openhcl/Set-OpenHCL-HyperV-VM.ps1，仓库自带）
- 这个脚本是 Microsoft 推荐的最小配置法
- 验证我们的 IGVM 是否能用基线脚本启动
- 工期：用户 5 分钟

**用户下载 Microsoft 官方 OpenHCL 预编译 IGVM**
- 不公开发布；只能从 Windows Insider channel 拿
- 工期：未知

---

## 一次性已完成（保留参考）

### §1【已完成】把当前 Windows 用户加入 `Hyper-V Administrators`

```powershell
net localgroup "Hyper-V Administrators" puxu /add
# 然后 注销重登 Windows
```

### §2【已完成】OpenHCL VM 创建与启动

由 Claude 通过 PowerShell 自动跑：
- `setup_openhcl_vm.ps1` 改造现有 VM
- `create_and_boot_openhcl.ps1` 完整重建
- 见 [scripts/hyperv/](scripts/hyperv/)

### §3【已完成】Service GUID 注册

```powershell
# gsudo cache 提权一次（用户首次需点 UAC）
gsudo cache on --duration 00:05:00
gsudo -d powershell -NoProfile -ExecutionPolicy Bypass -File register_openhcl_diag_guids.ps1
```

### §4【已完成】跨编 ohcldiag-dev.exe（WSL → Windows）

仓库官方 cross-compile，无 xwin：
```bash
sudo apt install clang-tools-20  # 提供 clang-cl-20
mkdir -p ~/.local/bin
ln -sf $(rustup which rust-lld) ~/.local/bin/lld-link-20
# 然后
./docs/superpowers/scripts/build-windows-cross.sh ohcldiag-dev
```

---

## §5【可选】启用 WSL2 `/dev/mshv`

让 WSL 内直接用 Microsoft Hypervisor。**对当前 path C 不必要**，但有了就能让 Claude 完全在 WSL 内做实验，免 Windows 跳转。详见旧 USER_TODO §3。

---

## §6【可选】跨编 openvmm.exe 在 Windows 跑

同 §4，把 `ohcldiag-dev` 换 `openvmm`：
```bash
./docs/superpowers/scripts/build-windows-cross.sh openvmm
```

---

## 联系点

Claude 继续无人值守推进（A 路线 + 文档清理）。如用户需要介入选项，参考上面"可选"段。
