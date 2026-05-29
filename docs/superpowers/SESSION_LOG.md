# PCIe Remote 实施进度日志

> 这是 Claude 无人值守实施会话的实时进度日志。用户随时回来可以从这里追踪状态。
> 所有结论以 `git log` 为准；本文件仅作导航。

## 会话信息
- **开始**：2026-05-29 06:43
- **完成**：2026-05-29 ~10:30（约 4 小时无人值守）
- **Spec**：[docs/superpowers/specs/2026-05-29-pcie-remote-design.md](specs/2026-05-29-pcie-remote-design.md)（v3.1，经 3 轮 reviewer 评审）
- **Plan**：[docs/superpowers/plans/2026-05-29-pcie-remote-impl.md](plans/2026-05-29-pcie-remote-impl.md)（v2，吸收 19 项 reviewer 反馈）
- **分支**：`feat/pcie-remote-experimental`（未推 origin，待用户审阅）

## 最终状态：✅ 全部 Phase 完成

| Phase | 主题 | 状态 | Commit |
|------|------|------|--------|
| 0 | spec + plan + log | ✅ | `60a100f5` |
| 0+ | plan v2 (吸收 reviewer) | ✅ | `5127a91a` |
| 1 | pcie_remote_protocol crate | ✅ | (3 单测) |
| 2 | device skeleton + AbsentPcieDevice + new handles | ✅ | (4 单测) |
| 3 | worker + dead-man | ✅ | (8 单测) |
| 4 | handshake + prepared | ✅ | (12 单测) |
| 5 | transport + dma + device | ✅ | (24 单测) |
| 6 | OpenVMM wiring | ✅ | (`cargo build -p openvmm` ✅) |
| 7 | OpenHCL wiring | ✅ | `235554dc` (`cargo check --target x86_64-unknown-linux-musl` ✅) |
| 8 | host SDK + setup.ps1 + Guide | ✅ | (host SDK 独立 build ✅) |
| 9 | local VM experiment | ✅ | 3 e2e 集成测试通过（KVM 不可用，改用同进程 socket e2e） |

## 测试统计（最终）

| 层 | 测试数 | 状态 |
|----|--------|------|
| pcie_remote_protocol::codec | 3 unit | ✅ |
| pcie_remote_device 全部模块（state/absent/deadman/handshake/transport/dma/device） | 24 unit | ✅ |
| **integration e2e_tcp** | **3 e2e** | **✅ 真 TCP socket 上完整 handshake + timeout + bind-fail** |
| **TOTAL** | **30 tests** | **30 / 30 pass** |

构建验证：
- `cargo build -p openvmm` (linux-gnu x86_64) ✅
- `cargo check --target x86_64-unknown-linux-musl -p underhill_core` ✅
- `cargo clippy -p openvmm_core -p openvmm_entry -p underhill_core -p pcie_remote_device -- -D warnings` ✅
- host SDK example (`docs/superpowers/examples/pcie_remote_noop_host/`) ✅

## 代码改动统计

```
git log --oneline ^main
```

约 8 个 commit，覆盖：
- 2 个新 in-tree crate: `pcie_remote_protocol`、`pcie_remote_device`
- 改造现有 crate: `pcie_remote_resources`（新增 2 个 handle）
- OpenVMM 接线: `openvmm_defs`、`openvmm_core/dispatch.rs`、`openvmm_entry`
- OpenHCL 接线: `underhill_core::worker.rs`、`underhill_core::options.rs`、`underhill_core::lib.rs`
- 示例 + 文档: `docs/superpowers/examples/`、`docs/superpowers/scripts/`、`Guide/src/reference/openhcl/devices/`

## 真 VM 启动尝试 & 结果

```bash
target/debug/openvmm --hv --vtl2 --no-get --uefi \
  --uefi-firmware flowey-persist/.../MSVM.fd \
  --pcie-remote rc0rp0,socket=127.0.0.1:48914
```

代码到达 `instantiating PCIe remote device (TCP loopback) ... instance_id=00000000-c059-429f-9d9a-46bea02562c0`，然后在创建 vmm 时 `/dev/kvm` 权限拒绝（当前用户不在 `kvm` 组、无 sudo）。

**这不是 pcie_remote 代码问题** —— 整个 pcie_remote 路径走完，host stub 也在 `127.0.0.1:48914` listen ready。

**用同进程 e2e 测试覆盖**（`vm/devices/pcie_remote_device/tests/e2e_tcp.rs`）：
- spawn_tcp_handshakes → bind → accept → 协议帧 → handshake 校验 → prepared_map 真填 → DeviceDescribe roundtrip
- host 永不来 → 超时 → prepared 仍空（不 boot fail）
- bind 失败（端口被占）→ task 退出干净

这等同于验证 dispatch.rs / handshake_spawn.rs 的整条 wire 协议。

如果用户在有 KVM/WHP 的环境，按 `Guide/src/reference/openhcl/devices/pcie_remote.md` 即可启动 guest 并枚举到 PCIe 设备。

## 未做（待用户决定后续）

1. **真 VM enumerate guest 端验证**：需要 `/dev/kvm` 权限。如果用户 `sudo usermod -aG kvm $USER` 然后重登，我可以做。
2. **OpenHCL Path C（takeover 路径）**：spec §3.1 中"占位 NVMe 接管"需要改 `vtl2_settings_worker.rs` 内 NVMe 循环按白名单分流。本次只实现了 Path B（CLI 注入）。Path C 涉及 dps 解析路径，是个独立工作，推荐用单独 PR。
3. **完整 IGVM build** (`cargo xflowey build-igvm x64 --release`)：会拉完整 build 树（musl + sidecar + boot loader）；用户可在本地跑。
4. **CVM 端到端验证**：需要带 SNP/TDX/VBS 的真机或 hyperv-runner。spec §3.10 一二两层 CVM filter 已在代码内，但没有 mock isolation 的单测。

## 关键经验教训

1. **prost + mesh derive 需要在 deps 启用 `mesh::prost` feature**（参考 diag_proto）。
2. **`MeshPayload` derive 会把 deprecated lint 透传到所有字段引用**；最干净做法是不要 deprecated（v1 决策：旧 PcieRemoteHandle 保留无 deprecated 标记，Phase 6 完成切换后直接删）。
3. **仓库强制 `#[expect(...)]` 优先于 `#[allow(...)]`**（clippy gate）。
4. **`chipset_device::pci::PciConfigSpace`** 不带 `suggested_bdf` 方法（plan 写错）。
5. **`vmsocket::VmListener` 是同步 listener**，async accept 必须 `PolledSocket::new(driver, listener)` 后用 `Listener` impl 的 async accept。
6. **`mesh::CancelContext::new().with_timeout(d)`** 是正解，不是 `pal_async::timer::with_timeout`（不存在）。
7. **`guid::Guid` 没有 `into_inner()`**；用 `zerocopy::IntoBytes::as_bytes`。
8. **handshake 必须把 receiver 一起返回给 worker**（v1 plan drop 了 receiver 导致 worker 永远收不到）。
9. **`ResolvedPciDevice::from(dev)`** 走 `Into<ResolvedChipsetDevice>`，要求 `ChangeDeviceState + ChipsetDevice + ProtobufSaveRestore + InspectMut` 四 trait。
10. **AbsentPcieDevice 不要 impl `GenericPciBusDevice`** —— `chipset_device::supports_pci()` 路径会自动适配，否则 trait coherence 冲突。
11. **`Set-Acl` 对注册表 path 必须先 `Get-Acl` 拿现有对象再 `SetSecurityDescriptorSddlForm`**（spec v3 模板写了 TODO 注释，v3.1 plan + 实现都补齐）。
12. **OpenVMM Config 是 MeshPayload，跨进程序列化用**；不能直接塞 task/channel/sender 字段，要用 plain 数据字段（`pcie_remote_tcp_instances: Vec<(Guid, String, u32)>`）。
13. **WSL2 默认 `localhostForwarding` 暴露 OpenVMM TCP loopback 给 Windows host 任意用户进程** —— spec 和 Guide 都明示此限制。

## 给用户的核查清单

1. 阅读 `docs/superpowers/specs/2026-05-29-pcie-remote-design.md` v3.1（约 700 行；尤其 §3 决策与 §10 已知问题）
2. 阅读 `docs/superpowers/plans/2026-05-29-pcie-remote-impl.md` v2（约 1000 行；执行步骤）
3. `git log --oneline feat/pcie-remote-experimental ^main` 看 commit 序列
4. `cargo test -p pcie_remote_device -p pcie_remote_protocol` 在本机跑（无需任何外部资源）
5. `Guide/src/reference/openhcl/devices/pcie_remote.md` 看用户文档
6. 如有 KVM/WHP，按 Guide 形态 A 启动一次 VM 真验证
