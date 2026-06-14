# vfio-user-in-underhill — 概览与导览

> **本文体裁**：这是一篇**自上而下的导览（reader's guide）**，给第一次接触这条线的读者
> （人，或一个无上下文的新 Claude 会话）一个全貌入口——**是什么、整体怎么运转、现在到哪了、
> 想深入该读哪篇**。它**不**重复过程细节：每个里程碑的 commit/弯路在 [MILESTONES §3](MILESTONES.md)，
> 每阶段的设计在 `docs/superpowers/plans/`，每次真机验证在 `usnvmemu/experiments/*/RESULT.md`，
> 动态待办在 [ROADMAP](ROADMAP.md)，最新状态在 auto-memory `vfio-user-underhill-state`。
> 本文是把这些**串成一条线的伞**。
>
> **最后更新**：2026-06-13（W0→W6c 闭环 + Layer C Option B 收口 + W6 autostart 落地）。

---

## 1. 一句话

让一个真 OpenHCL VM 的 **guest（VTL0）枚举并驱动一个 NVMe 盘**，而这个盘**不由 host 提供、也不由
hypervisor 内置**，而是由 **VTL2 paravisor 内一个用户态进程（usnvmemu）当 vfio-user server** 模拟出来，
对 guest 内存做**真零拷贝 DMA**。已端到端真机 PROVEN：guest 出盘「OpenHCL Userspace NVMe v2.0」+
4 MiB 真 IO（双独立 oracle）。

## 2. 是什么（三词拆解）

- **vfio-user** —— 进程间设备模拟协议（QEMU 生态标准）。VFIO 原本是内核把物理设备透传给用户态；
  vfio-user 把「设备前端 ↔ 后端」搬到**两个用户态进程之间走 Unix socket**：一端是「客户端」
  （代表 hypervisor/guest），一端是「server」（模拟出 PCIe 设备：config space / BAR MMIO / 中断 / DMA）。
- **underhill** —— OpenHCL 的 **VTL2 paravisor 用户态运行时**（`openhcl/underhill_core`）。在 CVM/paravisor
  架构里，VTL2 比 guest（VTL0）更高特权、比 host 更受信任，负责给 guest 提供虚拟设备。
- **vfio-user-in-underhill** —— 把 vfio-user 的**「客户端」那一半实现进 underhill**，使 guest 看到的
  PCIe NVMe 设备的真实后端是 **VTL2 内的 usnvmemu 进程**。

**为什么有价值**：① 把设备模拟路径从不可信 host 移入 VTL2（更小 TCB，CVM 友好）；② 用户态 firmware
作可热迭代的设备后端（改 firmware 不碰 hypervisor）；③ 教学——展示 VPCI-over-VMBus 全链路 + 真零拷贝 DMA。

## 3. 全栈数据流（已真机 PROVEN）

```
   guest (VTL0, Windows)
       │  NVMe driver (stornvme) 把它当真硬件驱动
       ▼
   VPCI-over-VMBus                      ← guest 看到的「PCIe 设备」呈现层
       │
       ▼
   underhill (VTL2)
   ┌──────────────────────────────────────────────────────┐
   │ vfio_user_pci_device  (ChipsetDevice shim)            │ ← 把 guest 的 cfg/MMIO/IRQ
   │   ├ cfg space / MSI-X    本地应答                      │   翻成 vfio-user 消息
   │   └ 其它 BAR MMIO ──┐                                  │
   │ worker (全双工 split + in_flight HashMap<msg_id>)      │
   └────────────────────┼─────────────────────────────────┘
                        │  vfio-user wire（Unix socket；DMA fd 经 SCM_RIGHTS 传递）
                        ▼
   usnvmemu  (VTL2 内的用户态 vfio-user **server** = nvme_firmware)
       │  真 NVMe 2.0 controller；收 client 在 DMA_MAP 声明的 guest RAM fd
       ▼
   零拷贝 DMA 直接读写 guest RAM (/dev/mshv_vtl_low)
```

**关键点**：firmware 对 guest 内存做**真零拷贝 DMA**——不是 copy 进 VTL2 再搬回，而是直接映射 guest
物理页。DMA 数据路径的接通是 W6c（见 §7）。

## 4. 关键代码拓扑

| crate / 路径 | 角色 | workspace 归属 |
|---|---|---|
| [usnvmemu/crates/vfio_user_wire/](../crates/vfio_user_wire/) | sans-IO wire（6 模块，仅 zerocopy+thiserror+pcie_device_core） | **exclude**（保 standalone Linux 可测） |
| [usnvmemu/crates/vfio_user_device/](../crates/vfio_user_device/) | client（async；`async_socket.rs` 是唯一 unsafe 模块） | **exclude** |
| [vm/devices/pci/vfio_user_pci_device/](../../vm/devices/pci/vfio_user_pci_device/) | guest 呈现（ChipsetDevice shim + worker） | workspace member（path-dep 上面两个 exclude crate） |
| `vm/devices/pci/vfio_user_pci_resources` | 资源/resolver 类型 | workspace member |
| [openhcl/underhill_core/src/emuplat/vfio_user_hotplug.rs](../../openhcl/underhill_core/src/emuplat/vfio_user_hotplug.rs) | Layer C / Option B 生命周期逻辑 | underhill_core |
| [openhcl/underhill_init/src/lib.rs](../../openhcl/underhill_init/src/lib.rs) | VTL2 PID1 autostart（§7 W6 收尾） | underhill_init |
| [vm/devices/pci/vpci/src/device.rs](../../vm/devices/pci/vpci/src/device.rs) | VPCI 设备层（含 Option A POC 挖出的 config MMIO RAII 修复） | vpci |

> **铁律**：`vfio_user_wire` / `vfio_user_device` 是 `[workspace.exclude]` crate，**绝不**移进 members
> ——否则拖入 pci_core 全家桶、毁掉 W5a 的 static-musl standalone 基建。member crate 用 **path-dep**
> 引用它们（B-2 已坐实可行）。

underhill 集成是 **env-gated + CVM-gated**：未设 `OPENHCL_VFIO_USER_NVME=<guid>:<unix_path>` 时零行为变化。

## 5. 生命周期模型：Layer A / Layer C / Option B

「Layer」指 **usnvmemu 进程启停/重启时，设备在 guest 侧如何表现**的雄心层级。文档里显式命名的是
**Layer A** 与 **Layer C**（无独立 Layer B）：

| 层 | 目标 | 模型 | 状态 |
|---|---|---|---|
| **Layer A**（W6b reconnect，commit `8c47c4b9`） | usnvmemu 重启时控制器**透明恢复**，盘不消失 | **槽位恒在**（slot-always-declared）+ cfg-error 表 Live/Lost + transport 重连 | ✅ 真机 PROVEN |
| **Layer C**（plan `2026-06-12-layer-c-vpci-hotplug.md`） | usnvmemu 起→盘 **PnP 热出现**，停→盘 **PnP 热消失**（真 VPCI 总线级热插拔） | 运行时 `add_dyn_device` / `DynamicDeviceUnit::remove` | ⚠️ **核心模型真机证伪 → 改采 Option B** |

**Layer C 终局裁定**（2026-06-13，commit `b7bd5459d`）：
- **hot-remove/re-add 在 Windows guest 上不可达**——根因 Windows `pci.sys` **只在 guest 自己重上电
  bus FDO**（`FDO_D0_EXIT → FDO_D0_ENTRY`，如手动 disable/enable）时才重枚举 VPCI 子设备；VSP 侧任何
  push（同-instance re-offer / device_count / INVALIDATE_BUS / 新 instance_id）**都不触发重枚举**，
  四+一种方法真机全败。这是**平台行为，非代码 bug**——逐步真机调试坐实（源码不可知 Windows pci.sys 行为）。
- 故**弃热插拔、采 Option B**：把「usnvmemu 停了又起」重新建模为 **transient 后端停顿**（像真硬件 NVMe
  controller 短暂 reset），**设备恒在** + Layer A 式 C-3 透明重连恢复。本质 = 回到 Layer A「设备恒在」
  模型 + 接受**长停顿恢复需手动**（reboot / 设备管理器 rescan / disable-enable）。
- **Option A POC**（commit `36289f357`，code 已 revert）：换新 instance_id re-offer，证明 guest **接受
  新总线**（唯一克服 finding-⑦ 缓存的法子），但 child 在 offer 时仍不自动枚举 ⇒ 纯 VSP 侧自动长停顿
  恢复不可达。**但顺手挖出+修了一个真 RAII bug**（`VpciConfigSpace` 无 `Drop` → 运行时 rescind 时
  config MMIO 泄漏 → 加 `Drop→unmap`，保留）。
- **未采纳方案全部文档化**在 Layer C plan 的「已试方案与去留」表（C0/C2-0/C2-1/INVALIDATE_BUS/Option A
  逐方案真机结果 + 未来适用性），「也许未来会用到」。

> **通用迁移教训**：当平台不让你 re-add 一个已移除的设备时，把后端停顿建模为「设备恒在 + 透明重连」
> （贴真硬件 controller reset 语义），别硬做 remove/re-add。

## 6. 三个 CRITICAL 不变量

happy-path 看不见、但错了就真机翻车（同 DBBUF shadow-doorbell 一类）：

- **C-1** worker pair-swap 原子（全双工 split，两赋值间无 `await`）。
- **C-2** cfg space 恒呈**真 declared config**（含 Connecting/Lost，让 VPCI offer-latch 拿到真
  `DEV_00A9`）；**只有 MMIO 才按 Live 门控**，非 Live 返 `Err` **绝不** `Defer`。
- **C-3** reconnect 重发 `set_irqs` + `dma_map`（持久 eventfd/region owned-clone）。

## 7. 当前状态

**已真机 PROVEN（闭环）**：
- **W0–W5a**：sans-IO wire crate 抽取 + client + 真 VM 零拷贝（W5a 真机 e2e，真 firmware ELF 在真 VTL2 跑）。
- **W6a**：client async 化（照搬 vhost_user pattern）。
- **W6b**：underhill ChipsetDevice 设备 + reconnect Layer A + **首次真 guest 枚举**（finding-③ VPCI
  offer-latch 修复，commit `bbdfd7cc` 码 + `02150223` 归档）。
- **W6c**：DMA 数据路径接通（finding-④，策略 A：underhill `GuestMemoryAccess::sharing()` 逐 ram 段
  产 `ShareableRegion`，reconnect 时 `dma_map`）——**W6b 完整闭环**，commit `7cd8bfa4` 码 + `6fc6e500` /
  `1bed8c4f` 归档（双 oracle）。
- **Layer C**：真机证 Windows 平台硬限 → Option B 收口（见 §5）。
- **W6 收尾**：usnvmemu VTL2 自启动托管服务（commit `738ab808d`）——usnvmemu 烤进 IGVM initrd
  `/bin/usnvmemu` + `underhill_init`(PID1) env-gated 自启 → **零-operator 出盘真机 PASS**。一等
  `--with-vfio-user-nvme` build flag 已落地（commit `f5f00c98c`）。

**后续 todos**（[ROADMAP §1](ROADMAP.md) vfio-user-in-underhill 节，按价值挑、无固定主战场）：
- supervised restart（区分 graceful-exit vs crash，对齐 VTL2「非预期死即 fatal」哲学）。
- persistent backing（现 tmpfs backing 重启即失）。
- 多 vfio-user 设备 / 多 NS。
- **L3 真 guest-boot e2e**（从这个 vfio-user 盘启动 guest OS，须 Windows/QEMU，是目前唯一缺的真机档）。

## 8. 文档地图（深读指引）

| 想了解 | 读这篇 |
|---|---|
| **全貌 + 每步 commit + 带注解弯路** | [MILESTONES.md §3](MILESTONES.md)（W0 → W6c saga，§3.1–3.6 逐段） |
| **Layer C 为什么这样收口**（方案对比 + 终局裁定） | [plans/2026-06-12-layer-c-vpci-hotplug.md](../../docs/superpowers/plans/2026-06-12-layer-c-vpci-hotplug.md)「已试方案与去留」表 |
| **Layer C finding-⑦ 深挖 + Option A POC + Option B 裁定** | [experiments/2026-06-12-layer-c-c0-real-vm/RESULT.md](../experiments/2026-06-12-layer-c-c0-real-vm/RESULT.md) |
| **W6b 设计**（全双工 worker / member path-dep） | [plans/2026-06-12-w6b-vfio-user-pci-device-underhill.md](../../docs/superpowers/plans/2026-06-12-w6b-vfio-user-pci-device-underhill.md) |
| **Layer A reconnect 设计** | [plans/2026-06-12-w6b-reconnect-usnvmemu-lifecycle.md](../../docs/superpowers/plans/2026-06-12-w6b-reconnect-usnvmemu-lifecycle.md) + 同名 spec |
| **W6c DMA 零拷贝真机验证** | [experiments/2026-06-12-w6c-dma-poc/RESULT.md](../experiments/2026-06-12-w6c-dma-poc/RESULT.md) |
| **W6 autostart 设计 + 真机 PASS** | [plans/2026-06-13-w6-usnvmemu-vtl2-autostart.md](../../docs/superpowers/plans/2026-06-13-w6-usnvmemu-vtl2-autostart.md) |
| **总体架构 spec** | [specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md](../../docs/superpowers/specs/2026-06-11-openhcl-vfio-user-vtl2-firmware-design.md) |
| **当前 live 状态 / 下一步** | [ROADMAP.md](ROADMAP.md) §1 + auto-memory `vfio-user-underhill-state` |

## 9. 怎么跑（零-operator 最小路径）

```bash
# 1) cross-build usnvmemu（static-musl，零改 firmware 源）
cd usnvmemu/crates/nvme_firmware
cargo build --bin nvme_firmware --no-default-features --features vfio-user \
  --target x86_64-unknown-linux-musl --release

# 2) 烤进 IGVM（一等 flag 自动 push usnvmemu_fs.config + 设 OPENHCL_USNVMEMU_PATH）
cargo xflowey build-igvm x64 --override-openvmm-hcl-feature vpci \
  --with-vfio-user-nvme \
  usnvmemu/crates/nvme_firmware/target/x86_64-unknown-linux-musl/release/nvme_firmware

# 3) VM kernel cmdline 设两个 env（值必须 space-free——经 cmdline tokenizer 按空白切分）
#    设备（underhill_core 识别）：
#      OPENHCL_VFIO_USER_NVME=<guid>:<sock>
#    启动器（underhill_init 识别；sock 自动从设备 env 派生，不重复）：
#      OPENHCL_VFIO_USER_NVME_AUTOSTART=<size_mb>:<backing>      例 256:/tmp/nvme_backing.img
#
# 4) boot → init 自启 /bin/usnvmemu + 建 backing → device shim 连上 →
#    guest 自动出盘「OpenHCL Userspace NVMe v2.0」。零 operator 介入。
```

真机部署/诊断细节见 [RUNBOOK_HOST_ROOT.md](RUNBOOK_HOST_ROOT.md) 与各 `experiments/*/RESULT.md`。
