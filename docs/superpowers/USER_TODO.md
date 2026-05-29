# 需要用户配合的事项（更新版）

> **2026-05-29 20:00 更新：** Hyper-V Administrators 组生效，Claude 已经从 WSL 完成大量自动化工作；现在卡在 OpenHCL VTL2 diag listener 未响应这一具体技术问题。

## 当前状态

| 项 | 状态 | 备注 |
|---|---|---|
| 加入 Hyper-V Administrators | ✅ 用户完成 | `Get-VM` 可用 |
| 跨编 ohcldiag-dev.exe（WSL → Windows） | ✅ Claude 完成 | 用仓库官方 cross-compile 方案；不需要 xwin |
| 注册 vsock service GUID 1+2 | ✅ Claude 完成 | gsudo cache 一次性 elevation |
| 创建 OpenHCL VM (pcie-remote-exp) | ✅ Claude 完成 | 用 petri 自带 `hyperv.psm1::New-CustomVM` |
| 设置 `FirmwareFile` + `GuestFeatureSet=0x201` | ✅ Claude 完成 | WMI ModifySystemSettings |
| 设置 `AllowFirmwareLoadFromFile=1` reg | ✅ 已存在 | 接受自建 IGVM |
| VM Start | ✅ Running, HCS state=Created | 但 VTL2 没响应 diag |
| **OpenHCL VTL2 diag_server 连接** | ❌ **timeout 10060** | **当前阻塞点** |

## 阻塞点：自建 IGVM 的 diag_server 不响应

`ohcldiag-dev.exe pcie-remote-exp inspect /` 返回 `os error 10060` (timeout)。意味着：
- service GUID 注册生效了（不是 10049 也不是 10061）
- AF_HYPERV + HIGH_VTL 通到 VTL2，但 VTL2 内部没有进程接受 + 回 ttrpc 握手
- 可能原因：
  - 自建 IGVM (`cargo xflowey build-igvm x64 --release`) 启动早期 panic
  - underhill_core 启动了但 diag_server 没注册
  - vmms 提供的 vmbus channels 与自建 OpenHCL 不兼容

### 可能的下一步（请用户选）

**A. 让 Claude 写 .vmrs 解析工具**（subagent #2 给了完整方案，~80 LOC Rust）
- 抽出 VTL2 boot_log buffer（8KB at fixed GPA）
- 读出 OpenHCL 启动早期日志，确定 panic 位置
- 工期：2 小时左右

**B. 用户在 Windows 上跑 `Set-OpenHCL-HyperV-VM.ps1`** （openhcl/Set-OpenHCL-HyperV-VM.ps1，仓库自带）
- 这个脚本是 Microsoft 推荐的最小配置法
- 验证我们的 IGVM 是否能用基线脚本启动
- 工期：用户 5 分钟

**C. 用户下载 Microsoft 官方 OpenHCL 预编译 IGVM**
- 不公开发布；只能从 Windows Insider channel 拿
- 工期：未知

**D. Claude 修改 OpenHCL 自建配置加更多 trace**（用 `--debug` build + 启用更多 log）
- 重 build IGVM（~10 分钟）
- 工期：30 分钟

**推荐顺序**：B（5 分钟验证）→ A（如果 B 也失败，需 .vmrs 解析）→ D（refine）。

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

下一步：选 A/B/C/D 之一回复，Claude 继续。
