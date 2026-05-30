# 需要用户配合的事项 (2026-05-31 更新)

> **🎉 Path C + userspace PCIe/NVMe 全闭环！** 真 Hyper-V 上 OpenHCL VTL2 +
> pcie_remote + vsock host + **用户态 NVMe SDK** 端到端验证成功：guest Windows
> 真 format FAT32 + 读写文件，backing file 真持久化字节。具体见
> [SESSION_LOG.md](SESSION_LOG.md) "v20 NVMe 完全闭环" 段。

## 当前状态

| 项 | 状态 | 备注 |
|---|---|---|
| 加入 Hyper-V Administrators | ✅ 用户完成 | `Get-VM` 可用 |
| 跨编 ohcldiag-dev.exe (WSL → Windows) | ✅ Claude 完成 | 用仓库官方 cross-compile 方案；不需要 xwin |
| 注册 vsock service GUID | ✅ Claude 完成 | gsudo cache 一次性 elevation |
| 创建 OpenHCL VM (正确方法) | ✅ Claude 完成 | `New-VM -GuestStateIsolationType OpenHCL`（关键！）|
| 自建 IGVM + Set-OpenHCL-HyperV-VM.ps1 加载 | ✅ Claude 完成 | `cargo xflowey build-igvm x64 --release --override-manifest <json>` |
| OpenHCL VTL2 diag_server | ✅ ohcldiag-dev 完全工作 | `ohcldiag-dev pcie-remote-exp inspect vm` 返回完整 inspect 树 |
| pcie_remote 真 vsock 握手 e2e | ✅ **vsock handshake ok, worker spawned** | host noop_host_vsock.exe ↔ VTL2 |
| K-8/K-11/K-15/K-17/K-18/K-19 spec gaps | ✅ 全部清零 | 48 tests pass |
| inspect 暴露 device state + Lost/Revive 诊断 | ✅ 2026-05-30 | `ohcldiag-dev inspect pcie_remote` 节点直接看到 `Connecting`/`Live`/`Lost` + `last_lost_at_ms` / `last_revive_at_ms` / `revive_count` / `last_lost_reason`（位标记 READ_ERR=1 / WRITE_ERR=2 / DISPATCH_FAIL=4 / WORKER_EXIT=8）|
| DMA 速率限制 env override | ✅ 2026-05-30 | `OPENHCL_PCIE_REMOTE_DMA_BPS=<bytes/sec>`：`0`=禁用、`>0`=自定义、未设置=64 MiB/s。启动期读一次缓存，K-20 swap 复活不重读 |
| **userspace 写 PCIe 设备闭环 (SDK + NVMe example)** | ✅ 2026-05-31 | `pcie_remote_userspace_sdk` ~500 行 + `pcie_remote_nvme_userspace` ~1600 行 reference impl；真 Hyper-V guest 完整 FAT32 format + 文件读写 + backing file 字节持久化；44 unit + 3 e2e + clippy -D warnings 全绿 |

## ✅ 已闭环路径

**OpenVMM 路径 (Linux KVM)：** 真 KVM 集成测试通过（前期工作）。

**OpenHCL 路径 (Hyper-V)：** 真 Hyper-V end-to-end 通过：
- 自建 OpenHCL IGVM 加载 ✅
- VTL2 pcie_remote 初始化 ✅
- VTL2 vsock listener 启动 ✅
- Host AF_HYPERV vsock client 连入 ✅
- Hello / HelloAck 协议交换 ✅
- Worker spawn ✅
- K-19 timeout 上限被实际触发拒绝 ✅
- §3.10 absent fallback 被实际触发 ✅
- **用户态 NVMe (vsock → SDK → guest)**：guest format FAT32 + 读写文件 + backing file 字节持久化 ✅✅✅

## 仍待用户参与的事项

### §1 (可选) CVM 真机验证

代码路径已覆盖 (AbsentPcieDevice 对 CVM 默认 + cvm_skip_pcie_remote
flag)，但 SNP/TDX/VBS 隔离 VM 上的真机端到端需要 confidential VM 硬件。
没有这类设备时跳过即可。

### §2 ✅ VTL0 guest OS 中真的 PCI 探测 — **已完成 (2026-05-31)**

不仅 enumerate，guest 真 format FAT32 + 读写文件 + backing file 持久化字节。
完整证据见 [SESSION_LOG.md](SESSION_LOG.md) "v20 NVMe 完全闭环" 章节。

历史脚本（仍可用于回归 / 新 OS 镜像验证）：

1. **自建 VHDX**：用 `build_winserver_vhdx.ps1` 从 SEAL ISO 装 Windows Server
   （已在 `C:\temp\pcie_remote_exp\guest.vhdx` 完成 16GB 镜像）。
2. **挂盘 + 校验脚本**：

```powershell
# 一键挂 + 验证（已封装）
.\docs\superpowers\scripts\hyperv\attach_vhdx_and_verify_lspci.ps1 `
    -VMName pcie-remote-exp `
    -VhdxPath C:\temp\pcie_remote_exp\guest.vhdx `
    -OhcldiagDevPath C:\path\to\ohcldiag-dev.exe
```

脚本执行：Stop-VM → Add-VMHardDiskDrive (idempotent) → 设硬盘首启 →
Start-VM → PSSession 等就绪 → guest 内 `Get-CimInstance Win32_PnPEntity`
过滤 `VEN_1414&DEV_C0DE`。退出码 0 = guest 真见到设备。

> 用 `Win32_PnPEntity` 而不是 `lspci`（Windows 没自带），结果包含
> `Status` / `ConfigManagerErrorCode` 便于诊断 driver bind 状态。

### §2.1 想自己写一个 PCIe 设备？

仓库自带 userspace SDK：

- **SDK**：`docs/superpowers/examples/pcie_remote_userspace_sdk/`
  - `src/lib.rs` — crate-level doc + 30 行最小例子骨架
  - `src/device.rs` — `trait PcieDevice` 7 方法签名 + `DeviceCtx` API
    （3 必须：`describe` / `mmio_read` / `mmio_write`；4 可选：
    `cfg_write_side_effect` / `reset` / `tick` / `on_dma_complete`）
- **Reference impl**：`docs/superpowers/examples/pcie_remote_nvme_userspace/`
  完整 NVMe 1.4 子集（Admin + IO SQ/CQ + Identify + NVM Read/Write
  含 dual-PRP + FLUSH + VWC），~1600 行 Rust。
- 设计要点见 spec §10 K-NEW-I / K-NEW-J / K-NEW-K。

### §3 (可选) WSL2 /dev/mshv

对当前 Path C **不必要**（Path C 已闭环）。详见 `MSHV_DIAGNOSIS.md`。
保留作为未来想法。

---

## 历史 (已完成，保留参考)

### §A 把当前 Windows 用户加入 `Hyper-V Administrators`
```powershell
net localgroup "Hyper-V Administrators" puxu /add
# 然后注销重登 Windows
```

### §B 跨编 ohcldiag-dev.exe (WSL → Windows)
```bash
sudo apt install clang-tools-20  # 提供 clang-cl-20
mkdir -p ~/.local/bin
ln -sf $(rustup which rust-lld) ~/.local/bin/lld-link-20
./docs/superpowers/scripts/build-windows-cross.sh ohcldiag-dev
```

### §C Service GUID 注册
```powershell
gsudo cache on --duration 00:05:00
gsudo -d powershell -NoProfile -ExecutionPolicy Bypass `
  -File docs\superpowers\scripts\hyperv\register_openhcl_diag_guids.ps1
```

### §D 创建 OpenHCL VM (正确方法 — 关键！)

```powershell
# 1. 启 firmware-from-file
Set-ItemProperty 'HKLM:/Software/Microsoft/Windows NT/CurrentVersion/Virtualization' `
  -Name AllowFirmwareLoadFromFile -Value 1 -Type DWORD

# 2. 用 -GuestStateIsolationType OpenHCL **必须在创建时指定！**
$vm = New-VM -Name pcie-remote-exp -Generation 2 `
  -GuestStateIsolationType OpenHCL `
  -MemoryStartupBytes 2GB
Set-VM -VM $vm -AutomaticCheckpointsEnabled $false
Set-VMFirmware -VM $vm -EnableSecureBoot Off

# 3. 设 firmware file
& .\openhcl\Set-OpenHCL-HyperV-VM.ps1 -VM $vm -Path C:\path\to\openhcl-x64.bin
```

> **重要教训**：用 `New-CustomVM`（无 isolation type）+ 之后 vssd 字段编辑
> **不会**让 Hyper-V 加载自定义 IGVM。必须 `-GuestStateIsolationType OpenHCL`。

---

## 联系点

Path C 已闭环。如有新需求或想验证 CVM 真机，请提示 Claude。
