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
| Phase 7+ OpenHCL Path C (NVMe takeover) | ✅ | `4ecd7b9b` create_storage_controllers_from_vtl2_settings 分流 |
| Phase 8 host SDK + setup.ps1 + Guide | ✅ | host stub 改为 client 角色 |
| Phase 9 真 KVM 端到端 | ✅ | `2917dfbd` boot grace period + 持久 worker |
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

详见 [USER_TODO.md](USER_TODO.md)。简要：

1. **OpenHCL 真 VTL2 验证** —— 需要 Windows 11 host 的 Hyper-V 或 WSL2 启用 mshv（`nestedVirtualization=true`）。
2. **生产 Hyper-V 上 Path C 真验证** —— 需要 Windows 管理员权限运行 Hyper-V cmdlets + setup-pcie-remote.ps1。
3. **跨编 openvmm.exe** —— 用户在 Windows 原生 `cargo build` 更简单（VS Build Tools 已装）。
4. **CVM 端到端** —— 需要 SNP/TDX/VBS 真机。

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
