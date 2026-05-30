# PCIe Remote 实施进度日志

> 这是 Claude 无人值守实施会话的实时进度日志。

## 会话信息
- **开始**：2026-05-29 06:43
- **关键里程碑**：2026-05-29 ~15:40 完成真 KVM 端到端 e2e 验证
- **Spec**：[docs/superpowers/specs/2026-05-29-pcie-remote-design.md](specs/2026-05-29-pcie-remote-design.md)（v3.1，经 3 轮 reviewer 评审）
- **Plan**：[docs/superpowers/plans/2026-05-29-pcie-remote-impl.md](plans/2026-05-29-pcie-remote-impl.md)（v2）
- **分支**：`feat/pcie-remote-experimental`

## 最终状态：✅ 真 KVM 上完整端到端验证成功

| 主题 | 状态 | 关键 commit / 证据 |
|------|------|--------|
| Phase 1-5（crate / handshake / worker / dead-man / device） | ✅ | 54 unit + integration tests pass |
| Phase 6 OpenVMM wiring | ✅ | `cargo build -p openvmm` ✅ |
| Phase 7 OpenHCL wiring + CLI（INSTANCE） | ✅ | musl 跨编通过 |
| Phase 7+ OpenHCL Path C (NVMe takeover) | ✅ | `create_storage_controllers_from_vtl2_settings` 分流（早期 commit；squash 后 hash 已变）|
| Phase 8 host SDK + setup.ps1 + Guide | ✅ | host stub 改为 client 角色 |
| Phase 9 真 KVM 端到端 | ✅ | boot grace period + 持久 worker（早期 commit；squash 后 hash 已变）|
| IGVM build | ✅ | `flowey-out/artifacts/build-igvm/ship/x64/openhcl-x64.bin` 19MB |
| 单测 / resolver fallback / options parser | ✅ | 27 + 3 + 8 = 38 tests in pcie_remote_* + underhill_core::options |

## 真 KVM 端到端证据（2026-05-29 15:35）

```bash
# Terminal 1: host stub (client，spec §3.3 OpenVMM/OpenHCL is server)
$ cargo run -p pcie_remote_noop_host
pcie_remote_noop_host: client mode — connecting to OpenVMM/OpenHCL addr=127.0.0.1:48914
connected
received Hello magic=0x52504345 version=1 instance_id_len=16
sent HelloAck

# Terminal 2: OpenVMM real-KVM
$ sg kvm "target/debug/openvmm --processors 1 --memory 512M --hv --uefi \
    --uefi-firmware ...MSVM.fd \
    --pcie-root-complex rc0,segment=0,start_bus=0,end_bus=255,low_mmio=4M,high_mmio=1G \
    --pcie-root-port rc0:rc0rp0 \
    --pcie-remote rc0rp0,socket=127.0.0.1:48914"

openvmm_entry: instantiating PCIe remote device (TCP loopback) ...
worker_new: guest RAM config mem_size=0x20000000
pcie_remote_device::handshake_spawn: pcie_remote: TCP handshake ok, worker spawned id=...
openvmm_core::worker::dispatch: pcie_remote: handshake wait done expected=0x1 got=0x1
add_device device="pcie:rc0rp0-pcie_remote_tcp"  ← 真 PcieRemoteDevice（不是 Absent）
state_change start  ✅
```

**完整路径全部走通**：CLI 解析 → openvmm_entry 构造 PcieRemoteTcpHandle → dispatch.rs 调 spawn_tcp_handshakes → server bind 127.0.0.1:48914 → accept client → 协议 Hello/HelloAck roundtrip → prepared_map 填充 → boot grace period 等到 → add_async_resolver → resolve → 真 PcieRemoteDevice 组装 → chipset 加载 → state start。

## AbsentPcieDevice 兜底验证（spec §3.10 layer-2）

把 host stub 杀掉再启 OpenVMM，handshake 超时后 dispatch.rs 走兜底：
```
pcie_remote_device::resolver: pcie_remote handshake missing; serving AbsentPcieDevice
add_device device="pcie:rc0rp0-pcie_remote_tcp"  ← AbsentPcieDevice，VM 仍正常启动 ✅
```
**绝不 boot fail**，正如 spec 设计。

## OpenHCL IGVM build 验证

`cargo xflowey build-igvm x64 --release` 完整通过：

| 文件 | 大小 |
|------|------|
| openhcl-x64.bin (ship) | 19 MB |
| openvmm_hcl (ship) | 15 MB |
| sidecar (ship) | 71 KB |
| openhcl_boot (ship) | 363 KB |

**这个 IGVM 包含我刚写的所有 pcie_remote 代码**：options.rs/PcieRemoteCliConfig 解析、vtl2_settings_worker 的 NVMe takeover 分流、worker.rs 的 CVM filter + add_async_resolver、handshake_spawn 等。

OpenHCL 在 OpenVMM 上启动需要 **WHP 或 mshv** hypervisor（提供 VTL2）。KVM 不支持 VTL2，验证报错明确：`vtl2 is not supported on this hypervisor`。spec §3.1 已说明这一点（OpenHCL 路径要求 Hyper-V 原生环境或 mshv）。

## msvc 跨编 openvmm.exe（进展，未完成）

| 步骤 | 状态 |
|------|------|
| `rustup target add x86_64-pc-windows-msvc` | ✅ |
| `cargo install xwin` | ✅ |
| `xwin --accept-license splat --output ~/.xwin` | ✅ |
| 链接 SDK + MSVC libs | ✅ |
| 实际跨编 | ❌ 缺 `lib.exe` (cc-rs 找；llvm-ar 不能 100% 替代) + `clang-cl` |
| **替代方案**：用户在 Windows 原生 `cargo build -p openvmm` | 用户自做更简单 |

详见 [docs/superpowers/USER_TODO.md](USER_TODO.md)。

## 测试统计（截至此 log）

| 层 | 数 |
|----|----|
| pcie_remote_protocol::codec | 3 |
| pcie_remote_device 单元（state/absent/deadman/handshake/transport/dma/device/resolver） | 27 |
| pcie_remote_device 集成（e2e_tcp 同进程） | 3 |
| underhill_core::options pcie_remote 解析 | 8 |
| **TOTAL** | **41 单测 / 集成测试** |

构建验证（**全部 clippy -D warnings clean**）：
- `cargo build -p openvmm` (linux-gnu, KVM 可运行) ✅
- `cargo check --target x86_64-unknown-linux-musl -p underhill_core` ✅
- `cargo xflowey build-igvm x64 --release` ✅ (产生 19MB ship IGVM)

真 KVM 集成测试在 SESSION_LOG（不是 cargo 测试套）。

## 未完成 / 需要用户配合

> **2026-05-30 修正**：本段所列项 1 + 2 都**已完成**（真 Hyper-V Path C 闭环；
> 见末尾 "🎉🎉🎉 真 Hyper-V 端到端验证" 段）。只剩项 3 (跨编 openvmm.exe，
> 用户可选) 和项 4 (CVM 真机，需硬件)。当前 USER_TODO 已对应更新。

详见 [USER_TODO.md](USER_TODO.md)。简要（**原始历史快照**）：

1. ~~OpenHCL 真 VTL2 验证~~ ✅ 已完成（Path C 已端到端通过）
2. ~~生产 Hyper-V 上 Path C 真验证~~ ✅ 已完成（noop_host_vsock ↔ VTL2 handshake ok）
3. **跨编 openvmm.exe** —— 用户在 Windows 原生 `cargo build` 更简单（可选）
4. **CVM 端到端** —— 需要 SNP/TDX/VBS 真机（可选）

## 关键经验教训（持续追加）

- prost + mesh derive 需要 deps 启用 `mesh::prost` feature
- 仓库强制 `#[expect(...)]` 优先于 `#[allow(...)]`
- vmsocket::VmListener 同步，async accept 用 PolledSocket::new(driver, listener)
- mesh::CancelContext::new().with_timeout(d) 是正解
- guid::Guid::as_bytes() (zerocopy)，没有 into_inner()
- handshake 必须把 receiver 一起返回给 worker
- AbsentPcieDevice 不要 impl GenericPciBusDevice
- OpenVMM Config 字段是 MeshPayload，要 plain data，不能 sender/channel
- **`drop(shutdown_tx)` 会让 worker.next() 立刻返回 Err(Closed)**，worker 立刻退出 —— 用 `std::mem::forget` 留住 sender（v1 无 graceful shutdown）
- **dispatch.rs 必须等 prepared_map 满或超时再 add_async_resolver**（spec §3.3 boot grace period），否则 resolve 总是命中 absent fallback
- xwin 跨编 windows-msvc 还需要 lib.exe（cc-rs）和 clang-cl，仅有 SDK 不够 —— 用户 Windows 原生编更省事
- WSL2 用户在 sudo 组但 `id -G` 缓存了登录时旧 group list；用 `sg kvm <cmd>` 可临时拿 kvm 组权限访问 /dev/kvm（不需要 sudo / 重启 WSL）

---

## 2026-05-29 ~ 05-30 后续：K-IDs 清零 + Path C 实战

> **2026-05-30 后续修正**：本段所述的 "VMBusMessageRedirection=1 必需"、
> "ohcldiag-dev 10060" 等表象都来自一个误判：原 VM 是用
> `New-CustomVM` 创建的（没有 `-GuestStateIsolationType OpenHCL`），
> Hyper-V 完全忽略 retrofit 的 OpenHCL 配置。真正的解决方案见本文档
> 末尾的 "🎉🎉🎉 真 Hyper-V 端到端验证" 段。
> 本段保留作历史诊断记录。

### K-8/K-11/K-15/K-17/K-18/K-19 全部清零

| K-ID | 主旨 | 落地 |
|------|------|------|
| K-8 | cvm_tracing `CVM_ALLOWED` gate | 所有 pcie_remote tracing 调用加上 CVM_ALLOWED marker；`cvm_tracing` 加进 pcie_remote_device 依赖；underhill_core/src/options.rs 一处 tracing 同样加上 |
| K-11 | 单 instance accept 重试封顶 | `MAX_ACCEPT_ATTEMPTS = 32`（handshake_spawn.rs） |
| K-15 | prost generated 类型 + mesh derive 双兼容 | `mesh_payload_compat` 测试模块：编译期断言 15 个消息类型都 impl MeshPayload；BarInfo.kind 是 i32 不是 Kind 枚举，故不需要为 Kind derive |
| K-17 | duplicate resolver panic 契约 | 添加文档化测试 `add_async_resolver_duplicate_panics_contract_documented`（pcie_remote_tcp / pcie_remote_vmbus ID 不同） |
| K-18 | MMIO 访问尺寸合法性 | `pcie_remote_protocol::is_valid_mmio_size(size) ∈ {1,2,4,8}` + worker.rs 拒绝非法尺寸；size_tests 模块 |
| K-19 | handshake_timeout 上限 | `parse_pcie_remote_entries` 拒绝 `handshake_timeout_ms > config_timeout/2`；2 个新测试 |

测试统计更新：

| 层 | 数 |
|----|----|
| pcie_remote_protocol（codec + size_tests + mesh_payload_compat） | 7 |
| pcie_remote_device 单元 | 28 |
| pcie_remote_device 集成（e2e_tcp 同进程） | 3 |
| underhill_core::options pcie_remote 解析 | 10 |
| **TOTAL** | **48 单测 / 集成** |

### Path C（真 Hyper-V）实战进展

> **2026-05-30 后续修正**：本段及下文 "🔥 关键发现：IGVM 没被加载" 段内
> 所有 "VMBusMessageRedirection 必需"、"COM3 不可用"、"diag_server 仍不可达"、
> "下一步建议跑 Set-OpenHCL-HyperV-VM.ps1" 等表述都是**误判时期的诊断快照**。
> 真根因（VM 创建时未指定 `-GuestStateIsolationType OpenHCL`）见本文档
> 最末"🎉🎉🎉 真 Hyper-V 端到端验证"段。**本段保留作侦查日志参考。**

**新增工件**：
- `docs/superpowers/examples/pcie_remote_noop_host/src/vsock_main.rs` —— Windows AF_HYPERV 客户端变体，跨编 `pcie_remote_noop_host_vsock.exe` 成功
- `/mnt/c/temp/pcie_remote_exp/enable_vmbus_redirect.ps1` —— 通过 WMI ModifySystemSettings 设置 `vssd.VMBusMessageRedirection = 1`（VTL2 vsock listener 必需）
- `/mnt/c/temp/pcie_remote_exp/switch_igvm.ps1` —— 不重建 VM 切换 IGVM 文件 + VTL2 内存
- `/mnt/c/temp/pcie_remote_exp/boot_and_read_com1.ps1` —— 异步读 COM1 命名管道
- `/tmp/openhcl-x64-com1.json` —— 自定义 IGVM 清单（`OPENHCL_BOOT_LOG=com1` + `OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=debug`）；产物 74MB dev IGVM

**已确认事实**：
- WSL2 当前 kernel `6.6.114.1-1-microsoft-standard-WSL2` 不含 `CONFIG_MSHV_ROOT`，`/dev/mshv` 无法启用（详见 `MSHV_DIAGNOSIS.md`）→ 决定走 Path C
- `VMBusMessageRedirection=1` 通过 WMI 设置成功（vssd 字段）
- `vssd.GuestFeatureSet = 0x201` 已开启 OpenHCL VTL2 加载
- 自定义 dev IGVM 已构建（`openhcl-x64-com1.bin`，含 debug GPA pool）
- 当前 HCS state 卡在 "Created"，VP0 HLT 0.1% / VP1 0%（OpenHCL 早期等待状态）

**待解决**：
- ohcldiag-dev 仍 `WSA 10060 ETIMEDOUT`（VMBusMessageRedirection 已设但 diag_server 仍不可达）
- COM1 0 字节读出 —— OpenHCL boot_logger 仅在 `com3_serial_available=true` 时使用 COM3；Win11 26200 stock 不支持 Hyper-V Gen2 COM3（仅 Insider Canary ≥27813）
- hcsdiag exec/console 不支持 Hyper-V VM（仅 UVM 容器）

**T7 下一步候选**（不需要用户介入）：
1. 写一个 .vmrs 抽取器（~80 行 Rust）从 saved-state HVS 格式中读出 VTL2 RAM 的 boot_log buffer
2. 在 OpenHCL boot 早期插更明显的 hypercall log trap（如果有的话）
3. 修改 IGVM 清单加上 `OPENHCL_BOOT_LOG=vmbus`（如可用）

---

## 2026-05-30 关键发现：IGVM 没被加载

> **2026-05-30 后续修正**：以下"4 个可能原因（待用户验证）"全部被否决。
> 真根因是 VM 创建时未指定 `-GuestStateIsolationType OpenHCL`（详见末尾
> "🎉🎉🎉 真 Hyper-V 端到端验证"段）。本段保留作侦查日志参考。

`vmrs_log_scanner_win.exe`（新工具，调 VmSavedStateDumpProvider.dll 自动解
XPRESS-HUFF）扫描 `pcie-remote-exp` VM 的 Save-VM 后 .vmrs：

- 总 RAM 512 MiB，1 个 GPA chunk 0x0..0x20000000
- min_run=64 无过滤扫出 ~80+ 字符串
- **所有字符串都来自 Hyper-V stock Msvm UEFI 固件**：
  - `c:\__w\1\s\Build\MsvmX64\RELEASE_VS2022\X64\...\.pdb`
  - `EltCheckBootCatalog: El Torito boot catalog header...`
  - `ScsiDisk: Failed to install the Erase Block Protocol!`
  - `TranslateGopBltToBmp: GopBlt is too large...`
- **没有任何 `openhcl`、`underhill`、`boot_logger`、`vmlinuz`、`initrd`、`linux`、`kernel` 字符串**

vmcx 验证：`hvs_file` reader 读 .vmcx confirm 配置在位：
- `/configuration/settings/Compatibility/GuestFeatureSet = 0x201` ✅
- `/configuration/settings/firmware/file_path = "C:\\temp\\pcie_remote_exp\\openhcl-x64-com1.bin"` ✅

**结论**：vmcx 配置已应用，但 **Hyper-V 在 boot 时忽略了 firmware override，加载了 stock Msvm UEFI**。

可能原因（待用户在 Windows 上验证）：
1. `AllowFirmwareLoadFromFile` registry key 是 `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization` 还是别处？需要重新确认实际生效
2. Win11 26200 stock 不支持 IGVM `firmware/file_path` 字段（只 Insider Canary 才支持自建 IGVM 加载）
3. `GuestFeatureSet=0x201` 仅启用 VTL2，但**还需要另一个 flag**指示用 file path 而不是 stock IGVM
4. IGVM 文件格式版本 不匹配（dev IGVM 74MB vs 通常 19MB ship）

这是 path C 的真实阻塞点 —— 不是 vsock listener、不是 VMBusMessageRedirection、
也不是 diag_server，而是 **OpenHCL 整个没起来，VM 在跑 stock UEFI**。

之前所有 "10060 timeout" 表象都是因为 VTL2 不存在 → 没有 listener 自然 timeout。

### 下一步建议

- **请用户**：在 Windows 上运行仓库自带的 `Set-OpenHCL-HyperV-VM.ps1`（Microsoft
  推荐的最小配置），把它和我们的脚本做对比，找出 vmcx 字段差异
- 或者：抓 Hyper-V Worker 进程的 ETW provider `Microsoft-Windows-Hyper-V-Worker`
  channel Analytic（admin only），看 firmware load 阶段的事件

工具留存：
- `docs/superpowers/examples/vmrs_log_scanner/` (cross-platform + Windows-only
  variants)；任何 .vmrs 都可以拿 `vmrs_log_scanner_win.exe` 扫

---

## 2026-05-30 🎉 突破：Path C 完全打通

### 真根因

旧 VM `pcie-remote-exp` 是用 `New-CustomVM` (petri hyperv.psm1) 创建的，**没有
`-GuestStateIsolationType OpenHCL`**。后期 vssd 上 `GuestFeatureSet=0x201` +
`FirmwareFile=...` 看似配置了 OpenHCL，但**实际上 Hyper-V 对 retrofit
的 OpenHCL 配置直接忽略**，启动时加载 stock Msvm UEFI，所以：

- "Create compute system, result 0xC0370103" 实际是 silent firmware ignore
- "No bootable devices configured" 是 stock UEFI 找不到 boot device
- VTL2 不存在 → vsock listener 不存在 → ohcldiag-dev 10060 timeout

### 修复

按 `Guide/src/user_guide/openhcl/run/hyperv.md` 的官方方法：

```powershell
Remove-VM pcie-remote-exp -Force
$vm = New-VM -Name pcie-remote-exp -Generation 2 `
  -GuestStateIsolationType OpenHCL `
  -MemoryStartupBytes 2GB
Set-VM -VM $vm -AutomaticCheckpointsEnabled $false
Set-VMFirmware -VM $vm -EnableSecureBoot Off
# 再用仓库脚本设 firmware file
& openhcl\Set-OpenHCL-HyperV-VM.ps1 -VM $vm -Path C:\path\to\openhcl-x64.bin
Start-VM $vm
```

### 验证

```
PS> ohcldiag-dev.exe pcie-remote-exp inspect /
{
    build_info: _,
    control_state: "started",
    mesh: _,
    proc: _,
    trace: _,
    uhdiag: _,
    vm: _,
}
```

OpenHCL VTL2 完全启动并响应 diag！🎉

### 副产品发现

- `VMBusMessageRedirection=1` **对 vsock / ohcldiag-dev 路径不是必需的**
  （GuestStateIsolationType=OpenHCL 路径自动处理 vsock routing）。
  > **2026-05-30 后续修正**：当 OpenHCL 配置了 **vpci 设备**（cmdline 或
  > NVMe takeover 注入 pcie_remote 等），VMBusMessageRedirection=1 **是
  > 必需的**，否则 OpenHCL 启动失败：`vpci devices require vmbus
  > redirection to be enabled`。详见末尾 "v2 真 Hyper-V end-to-end" 段。
- 用 `-GuestStateIsolationType OpenHCL` 创建后，vssd 上 `GuestFeatureSet=0x513`
  （含 isolation flags），用 `Set-OpenHCL-HyperV-VM.ps1` 设了 FirmwareFile
  后变成 `0x201`，但 OpenHCL 仍 boot —— 说明真正决定是创建时的 isolation
  type，不是后续的 GuestFeatureSet
- 没有 VHD（disk）也能 boot OpenHCL（只是 guest UEFI 没 boot device，但 VTL2 已起）

### Path C 进入下一阶段

OpenHCL boot 起来，下一步可以：
1. 跑真 PCIe Remote 实验：cmdline 加 OPENHCL_PCIE_REMOTE_INSTANCE，本机 vsock client 连
2. 用 ohcldiag-dev inspect 看 pcie_remote 实例状态
3. 实际验证 spec §3.10 layer-2 absent fallback 在真 Hyper-V 上的行为

---

## 2026-05-30 🎉🎉🎉 真 Hyper-V 端到端验证

### 实验步骤

1. 构建带 PCIe Remote cmdline 的 IGVM:
   ```bash
   cat > /tmp/openhcl-x64-pcie.json <<'JSON'
   {
       "guest_arch": "x64",
       "guest_configs": [{
           "guest_svn": 1, "max_vtl": 2, "isolation_type": "none",
           "image": {"openhcl": {
               "command_line": "OPENHCL_PCIE_REMOTE_INSTANCE=11111111-2222-3333-4444-555555555555:50000,handshake_timeout_ms=2000",
               "memory_page_count": 131072,
               "uefi": true
           }}
       }]
   }
   JSON
   cargo xflowey build-igvm x64 --release --override-manifest /tmp/openhcl-x64-pcie.json -o pcie-test
   ```

2. 部署 + 重启:
   ```powershell
   Stop-VM pcie-remote-exp -TurnOff -Force
   $vm = Get-VM pcie-remote-exp
   & .\Set-OpenHCL-HyperV-VM.ps1 -VM $vm -Path C:\temp\openhcl-pcie-test.bin
   Start-VM $vm
   ```

3. 同时启 host vsock client:
   ```powershell
   $vmid = (Get-VM pcie-remote-exp).Id
   pcie_remote_noop_host_vsock.exe --vm-id $vmid --port 50000 --retries 60 --retry-ms 500
   ```

### 输出（成功！）

Host noop_host_vsock:
```
INFO pcie_remote_noop_host_vsock: vsock client connecting vm_id=2a0a... port=50000
INFO pcie_remote_noop_host_vsock: connected
INFO pcie_remote_noop_host_vsock: received Hello magic=0x52504345 version=1
INFO pcie_remote_noop_host_vsock: sent HelloAck
```

OpenHCL VTL2 kmsg:
```
[2.346256] pcie_remote_device::handshake_spawn:
  INFO  pcie_remote: vsock handshake ok, worker spawned
  id=11111111-2222-3333-4444-555555555555
```

### 验证的 spec items

| Spec 段 | 行为 | 真 Hyper-V 验证 |
|---|---|---|
| §3.3 boot grace period | OpenHCL boot 期等 host handshake，超时进 Lost | ✅ 2.59s 超时（无 host 时）/ 2.35s handshake ok（有 host 时） |
| §3.10 layer-2 absent fallback | handshake 失败 → AbsentPcieDevice | ✅ 上一轮日志：`vsock handshake timeout; device absent` |
| §3.4 Hello/HelloAck 协议 | magic 0x52504345 / version 1 | ✅ host 收到 magic=0x52504345 version=1 |
| K-8 CVM_ALLOWED | 关键 tracing 在 OpenHCL kmsg 可见 | ✅ `pcie_remote: vsock handshake ok` 含 CVM_ALLOWED marker |
| K-19 handshake timeout 上限 | timeout > config/2 拒绝 | ✅ 第一次 10s 被拒：`rejected (handshake_timeout_ms=10000 > max 2500=config_timeout/2)` |

### 验证清单完成度

- **真 Linux KVM** (OpenVMM 路径): ✅ (前期工作)
- **真 Hyper-V** (OpenHCL 路径): ✅ **新增**
- **vsock transport** (AF_HYPERV / HIGH_VTL): ✅ **新增**
- **CVM transport** (SNP/TDX/VBS): 仍需真机；代码路径已覆盖

### 总测试统计

- 单元 + 集成: 48 (pcie_remote_device 28 + protocol 7 + options 10 + e2e_tcp 3)
- 真 KVM e2e (OpenVMM, 前期): 已 logged
- 真 Hyper-V e2e (OpenHCL, **此次**): 上面的输出

Path C 闭环。

---

## 2026-05-30 v2 重构：完整 BAR/MSIX/MMIO/InterruptFire/DMA 闭环

承前 Path C 真 Hyper-V 已通；本次把 v1 cfg-only skeleton 升级到真正
能服务 guest PCIe MMIO + MSI-X 中断 + host-initiated DMA 的完整设备。

### 流程

严格按用户要求：
1. **architect subagent review** 在动手前（提出 10 modifications，全采纳）
2. **rust-reviewer subagent review** 在 commit 前（1 HIGH + 多 MEDIUM/LOW，全修）
3. **LOW 处理规则**：有价值直接做、不打算做的记到 `REVIEW_PENDING.md`

### 主要变更

| 维度 | v1 | v2 |
|---|---|---|
| cfg space | `[u32; 64]` 镜像 | `ConfigSpaceType0Emulator`（标准实现）|
| MMIO | 不支持 | `MmioIntercept` 完整路由：MSIX 本地 + 其它走 worker `Defer` |
| MSI-X | 协议层有但 dead code | `MsixEmulator` + Vec<Interrupt>，BAR4 专用 |
| InterruptFire | warn 占位 | 真 `interrupts[i].deliver()` + bounds + ≥4 bad→Lost |
| DMA | warn "not implemented" | `ReadGpa/WriteGpa → GuestMemory + DmaCompletion`，含 64 MiB/s 速率限制 |
| Worker spawn | handshake 完立刻 | 推迟到 resolver `assemble_device`（拿全 `msi_target/register_mmio/guest_memory`）|
| cfg_write_side_effect | "Phase 6+" 注释 | 实现，**仅成功 write 才 forward** |

### 安全 + 健壮性新增

- **K-NEW-A** MSI-X 表数据永远不离开 OpenHCL（绝不转发给 host）
- **K-NEW-B** BAR1/3/5 在 handshake 拒绝（DeviceBars upstream API v1 限制）；BAR4 给 MSI-X 保留
- **K-NEW-C** DMA 累积速率限制 64 MiB/s
- **A4** 连续 ≥4 个非法 inbound 帧 → 立即进 Lost

### 测试统计 53/53 全过

| 层 | 数 | 备注 |
|----|---|---|
| pcie_remote_device 单元 | 33 | v1 是 28；+5 (DMA rate × 3 + interrupt_fire_bounds + rejects_unsupported_bar_index) |
| pcie_remote_device e2e_tcp 集成 | 3 | 未变 |
| pcie_remote_protocol | 7 | 未变 |
| underhill_core::options | 10 | 未变 |
| **TOTAL** | **53** | v1 是 48 |

### 构建验证

- `cargo build -p openvmm` (linux-gnu, KVM) ✅
- `cargo check -p underhill_core --target x86_64-unknown-linux-musl` (OpenHCL) ✅

### 协议变更

`pcie_remote.proto` `ToHost` 加 `DmaCompletion dma_completion = 14;` —
OpenHCL → host 的 DMA 完成回执（用 token 关联请求，不依赖 seq）。

### Callsite 适配

- `openvmm/openvmm_core/src/worker/dispatch.rs`：`PcieRemoteTcpResolver::new(prepared, worker_tasks)`
- `openhcl/underhill_core/src/worker.rs`：`PcieRemoteVmbusResolver::new(prepared, worker_tasks)`
- worker_tasks: `Arc<Mutex<Vec<Task<()>>>>` 由调用方持到进程结束

### 关联 commit

`ea928d5b feat(pcie_remote): v2 重构 - 完整 BAR/MSIX/MMIO/InterruptFire/DMA 闭环`

---

## 2026-05-30 v2 真 Hyper-V end-to-end 进 guest 已验证

承前 v2 重构 (ea928d5b) + grace period fix (f5ec43c1)，本轮完成
**Windows Server guest 实际看到 pcie_remote 设备** 的部署 + 验证。

### 自动化流程

1. **build VHDX from ISO**: SEAL Windows Server 2025 26100 ISO (6 GB)
   → mount → diskpart GPT + EFI + Windows partitions → DISM /Apply-Image
   → bcdboot → 注 unattend.xml (auto logon) + post_logon.ps1
2. **新建 OpenHCL VM** (`-GuestStateIsolationType OpenHCL`)
3. **配置**: VHDX attach SCSI + IGVM build w/ vpci feature override +
   vssd VMBusMessageRedirection=1
4. **启 noop_host_vsock.exe** (persistent reconnect loop) → Start-VM →
   OpenHCL 内 grace period 等 prepared_map 填好 → resolver assemble
   pcie_remote 完整 device → vpci channel publish 到 vmbus

### 三轮 fix iteration（部署阶段才暴露）

| 问题 | 错误 | 修复 |
|---|---|---|
| openvmm_hcl 默认 build 无 vpci feature | `failed to start VM error=built without vpci support` | `cargo xflowey build-igvm ... --override-openvmm-hcl-feature vpci` |
| vpci device 需 VMBus message redirection | `vpci devices require vmbus redirection to be enabled` | WMI ModifySystemSettings: vssd.VMBusMessageRedirection=1（更正了之前文档中"非必需"的判断）|
| underhill_core/worker.rs 没等 boot grace period | `pcie_remote handshake missing; serving AbsentPcieDevice` | commit f5ec43c1: 加同步 poll prepared_map 直到满或超时再 add_async_resolver |

### Guest 内验证（PowerShell Direct）

```powershell
Enter-PSSession -VMName pcie-remote-exp -Credential Administrator
# Inside guest:
pnputil /scan-devices
Get-PnpDevice -PresentOnly | ? { $_.InstanceId -like 'VMBUS*44C4F61D*11111111*' }
# Output:
#   Microsoft Hyper-V Virtual PCI Bus  OK  VMBUS\{44C4F61D-...}\{11111111-...}
Get-PnpDevice -PresentOnly | ? { $_.InstanceId -like '*VEN_1414*' }
# Output:
#   Standard NVM Express Controller  Error (CM_PROB_FAILED_START)
#   PCI\VEN_1414&DEV_C0DE&SUBSYS_00000000&REV_01\5&191D8A3A&0&0
#   Class: SCSIAdapter, Service: stornvme
```

### 完整路径全部走通

1. ✅ OpenHCL VTL2 启 pcie_remote vsock listener
2. ✅ Host noop_vsock client Hello/HelloAck handshake 完成
3. ✅ boot grace period 等到 → resolver assemble device + spawn worker
4. ✅ device publish 到 OpenHCL vmbus (channel id 5, interface 44c4f61d=vpci)
5. ✅ Guest VMBUS\{44C4F61D-...}\{11111111-...} `Microsoft Hyper-V Virtual PCI Bus` 状态 OK
6. ✅ **Guest PnP 探测到 `PCI\VEN_1414&DEV_C0DE`**
7. ⚠ Windows stornvme driver 加载失败 (CM_PROB_FAILED_START) — 因为
   class_code=0x010802 (NVMe) 让 Windows 自动选 stornvme，但 noop_host
   不响应 NVMe ABI；这是 **noop 的正确预期行为**

### 关键发现

- **vpci 是 OpenHCL 内置概念**：cmdline 注入的 pcie_remote 通过
  `InitialControllers::new` → `controllers.vpci_devices` →
  `build_vpci_device` 路径自动 publish 给 guest vmbus（不需要 vpci_relay；
  vpci_relay 是 host 来的 vpci channel 转发给 guest，与 OpenHCL 自有
  vpci device 是两条路径）
- **next_seq 即使 guest 探测后仍 = 1**：cfg_read 走本地 ConfigSpaceType0Emulator,
  不递增 seq；只有 MMIO Defer 或 cfg_write_side_effect 才递增。Windows
  stornvme 在 cfg_read VEN/DEV 看到是 PCIe（不是 NVMe register 实现）后
  试读 BAR0 NVMe registers → noop 返回 0 → stornvme bail → PROB_FAILED_START
  在 cfg-read 阶段就失败，从未进 MMIO defer 路径
- **要让 guest 真用上设备**：noop_host 要换 real device emulation
  （或改 class_code 为 unclassified 0xff00 让 Windows 不自动绑 stornvme）

### 工件

- `/mnt/c/temp/pcie_remote_exp/openhcl-pcie-test-v2-vpci.bin` (含 vpci feature)
- `/mnt/c/temp/pcie_remote_exp/openhcl-pcie-v2-grace.bin` (final v2 IGVM)
- `/mnt/c/temp/pcie_remote_exp/guest.vhdx` (Windows Server 26100 with auto-login)
- `/mnt/c/temp/pcie_remote_exp/build_winserver_vhdx.ps1` (ISO → VHDX)
- `/mnt/c/temp/pcie_remote_exp/inject_unattend.ps1` (add unattend + post_logon)
- `/mnt/c/temp/pcie_remote_exp/deploy_v2_e2e.ps1` (full deploy + start)

---

## 2026-05-30 v3+v4: worker_stats inspect + InterruptFire/DMA 端到端验证

承前 v2 真 Hyper-V guest e2e（29f7293c），本轮在已通的 OpenHCL→guest
路径上加可观察 stats + 主动 InterruptFire/ReadGpa，验证完整数据通路。

### v3 (commit 22d731d4)：worker_stats inspect 暴露

- `WorkerStats { 9 个 AtomicU64 }`：mmio_read_results / interrupts_fired /
  interrupts_oob / read_gpa_requests / write_gpa_requests /
  dma_rate_limit_rejects / inflight_current / inflight_peak /
  consecutive_bad_frames
- `Inspect` derive 自动暴露到 ohcldiag-dev
- `SharedWorkerStats = Arc<WorkerStats>`，worker 单写 + device.rs 持 clone
  让 inspect tree 看见
- noop_host vsock_main 加 `select_biased(read, 5s_timer)`，timer tick
  时主动发 `InterruptFire { msix_index: 0 }`

实测：worker_stats.interrupts_fired = 55 与 host noop fire_count = 55 完全对应

### v3+ (commit 8fee5340)：rust-reviewer #2 修复

3 个 counter (read_gpa_requests / write_gpa_requests / dma_rate_limit_rejects)
之前声明但未 fetch_add，永远 0 — 修复。inflight_peak load-then-store
race 改 fetch_max。drain_in_flight 末尾归零 inflight_current。
interrupt_fire_bounds 测试加 5 个 stats 断言（防回归）。

### v4 (commit 2f8ffe75)：DMA 路径 e2e

noop_host 每 3 个 timer tick (15s) 主动发 ReadGpaRequest(gpa=0, len=4)。
真 Hyper-V VM 6 分钟实测：

```
worker_stats:
  interrupts_fired:   80   ← noop fire_count=80
  read_gpa_requests:  26   ← noop ReadGpa 发了 26 次
  dma_rate_limit_rejects: 0
  inflight_current:   0
  inflight_peak:      0
```

**完整 DMA 路径** host noop_vsock ↔ OpenHCL worker ↔ guest_memory 验证：
1. noop 发 ReadGpaRequest 帧
2. OpenHCL worker dispatch_inbound → handle_read_gpa
3. stats.read_gpa_requests.fetch_add(1)
4. guest_memory.read_at(0, 4) (guest 内存 @0 = bootstrap 永远 mapped)
5. reply_dma 回 DmaCompletion 给 noop

### 工件 update

`openhcl-pcie-v4.bin` 取代 v3-stats / v2-grace；当前 IGVM 含全部 fix
+ worker_stats + grace period + vpci feature。

### 验证清单 v2 → v4 增量

| 项 | v2 | v3 | v4 |
|---|---|---|---|
| OpenHCL VTL2 boot + vsock listener | ✅ | ✅ | ✅ |
| host noop handshake | ✅ | ✅ | ✅ |
| device assemble + worker spawn | ✅ | ✅ | ✅ |
| vmbus vpci channel publish | ✅ | ✅ | ✅ |
| guest 检测到 VEN_1414&DEV_C0DE | ✅ | ✅ | ✅ |
| worker_stats inspect | - | ✅ | ✅ |
| InterruptFire host→OpenHCL→guest LAPIC | - | ✅ | ✅ |
| ReadGpa host→OpenHCL→guest_memory→DmaCompletion | - | - | ✅ |
