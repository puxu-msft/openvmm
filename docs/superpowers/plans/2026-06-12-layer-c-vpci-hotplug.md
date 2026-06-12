# Layer C — emulated vfio-user NVMe 设备 guest 运行时热插拔 实现计划

> **For agentic workers:** 用 superpowers:subagent-driven-development 逐 task 执行。
> 设计已经过 ecc:architect 评审（纠正了 2 个 CRITICAL 错误，见下），承重 POC（源码）+
> 前置条件（ChipsetDevices/StateUnits 运行时留存）已核验。**C0 是真机 POC 门，必须先过。**

**Goal**：usnvmemu（VTL2 内 vfio-user server）启动 → 设备在 OpenHCL guest 里**热出现**
（PnP add，NVMe 盘出现）；usnvmemu 停 → 设备**热消失**（PnP remove）。即运行时 hot-add/
remove，且 boot 时 usnvmemu 没起则 guest 启动无盘（冷插）。

**Architecture**：device shim（`VfioUserPciDevice` + worker + reconnect connector + irq
tasks）**boot 时装配一次、长存**；只有薄壳 **VpciBus 层**在 Live/Lost 边沿经
`ChipsetDevices::add_dyn_device` / `DynamicDeviceUnit::remove` 动态 add/remove。一个
reconcile task 订阅 device 的 `SharedState`（Live↔Lost）驱动 add/remove。

**Tech Stack**：`vmm_core/vmotherboard` 的 `ChipsetDevices::add_dyn_device` +
`DynamicDeviceUnit::remove`（运行时设备管理 API）；生产蓝图 = `vpci_relay`
（`vm/devices/pci/vpci_relay/src/lib.rs`，已用此 API 做 VpciBus runtime add/remove）。

---

## 关键设计事实（architect 评审纠正 + 核验）

- **CRITICAL 纠正 1：不要碰 `build_vpci_device`**（`vmm_core/src/device_builder.rs`）。它是
  boot-time `ChipsetBuilder` 路径，`build()` 后 consume，**运行时无移除单个 unit 的能力**，
  且 2 个 caller（underhill `worker.rs:3476` / openvmm `dispatch.rs:2402`）改了会附带损害。
  → 用运行时 API `ChipsetDevices::add_dyn_device`（`vmotherboard/src/chipset/builder/mod.rs:66`）
  + `DynamicDeviceUnit::remove`（`:99`）。
- **CRITICAL 纠正 2：拆除用 `DynamicDeviceUnit::remove`，不是 `SimpleDeviceHandle::revoke`**。
  `revoke` 只 await vmbus offer task，**不碰** chipset device unit + 2 个 MMIO config 区域 →
  每周期泄漏。`DynamicDeviceUnit::remove` 连 chipset device + MMIO 一起拆（drop VpciBus →
  `SimpleDeviceHandle` Drop = rescind；drop config MMIO → unmap，无泄漏）。
- **盲点纠正：device shim 长存，只 bus 层 add/remove**。每周期重建 device shim 会丢
  in-flight read / MSI-X 表 / 重启 worker+connector+eventfd（毁 C-3）。device shim 用
  `with_external_pci`（`device_builder.rs:59`）可独立长存，bus 薄壳重建无状态损失。
  **这是 Layer C 区别于 vpci_relay（每次全新设备）的核心。**
- **前置条件已核验**：underhill 长存 `ChipsetDevices`（`dispatch/mod.rs:149`）+ `StateUnits`
  （`:165`），reconcile 可在 dispatch 对象里运行时 add_dyn_device。
- **VpciBus::new 内 offer 不可延迟**（`bus.rs:167` offer 是 new 强制末步）；但"Live 才整体
  add bus、Lost 才整体 remove"等价实现按需 offer/revoke，无 channel 闪现。
- **C-2/C-3 去留**：C-3（reconnect 重发 set_irqs/dma_map）全留；C-2 的 **MMIO-Live-门控全留**
  （offer 后、worker 转 Lost 的竞态窗口里 guest MMIO 必须返 Err 不 Defer，防 hang，291d8645）；
  C-2 的 **cfg-恒呈真保留但更新动机注释**（offer 改在 Live 发生，不再为 assemble-time 枚举骗术）。
- **offer-on-Live 顺带解掉 disable/enable 缺口**：guest 对着已就绪控制器枚举，盘自动出现。

---

## Build order（C0 是真机 POC 门，必须先过）

### Task C0 — 真机 POC 门：手动 add/remove bus 验 guest PnP 行为

**承重未知**（OpenVMM 源码不可知 Windows guest 行为，必须真机先验）：vmbus 通道 rescind →
guest 是否干净 PnP 移盘？re-offer 同 instance_id → guest 是否重新枚举出盘 + 能再 IO？
in-flight IO 时 rescind 安全吗？

- **Files**: `openhcl/underhill_core/src/worker.rs`（把 vfio_user 实例从 `vpci_devices`
  静态循环 `:3467-3500` 摘出）；`vtl2_settings_worker.rs:1927`（不 push 进 vpci_devices）；
  新 `openhcl/underhill_core/src/emuplat/vfio_user_hotplug.rs`（C0 用最小手动触发）。
- **做法**：device shim 仍 boot 装配（Connecting，worker+connector 跑）但**不**走
  build_vpci_device。VpciBus 改经 `add_dyn_device`（device shim unit 已存在则只 add bus；
  照 `vpci_relay/src/lib.rs:326-357` 的两次 add 顺序：先 device 后 bus，bus 闭包捕获 device
  Arc + interrupt_mapper）。C0 触发用**可控信号**（最简：wire 到 usnvmemu Lost/Live，我从
  host `pkill -f`/重起控制；无 debounce）：Live→add bus，Lost→`bus_unit.remove().await`。
  `add_dyn_device` 后须 `state_units.start_stopped_units().await`（`vpci_relay:368`）。
- **真机验证**（用 committed harness `scripts/hyperv_vfio_user_interop/` 改造）：usnvmemu 起
  → guest 盘出现 + IO（oracle）；`pkill -f usnvmemu` → guest PnP 移盘（`Get-Disk` 消失）；
  重起 usnvmemu → 盘重新出现 + 能再 IO + host backing oracle。
- **门**：若 guest 不接受 rescind/re-offer（蓝屏/stuck/不重枚举）→ 整个路径 A 假设崩，
  回退重评估（[[poc-before-settling-design]]）。**过了才做 C1+。**

### Task C1 — reconcile task 骨架（边沿订阅，无 debounce）
- 给 `SharedState` 加 Live 边沿 channel（仿 `worker.rs:187` lost_tx，新增 live_tx 在 `:273`
  `store(Live)` 旁）。reconcile task（`emuplat/vfio_user_hotplug.rs`，照 `netvsp.rs:1181-1265`
  worker 模式）：串行 select { live→add bus / lost→remove bus }，持 `Option<bus_unit>` +
  重建上下文（device shim Arc + msi/vtom/instance_id/driver_source/vmbus + `&ChipsetDevices`
  + `&mut StateUnits`）。standalone loopback 测：usnvmemu 起→add；kill→remove；重起→re-add。

### Task C2 — debounce + 抖动加固
- Lost→Live settle 窗口（`PolledTimer` backoff，仿 `netvsp.rs:1240-1252` VfReconfigBackoff），
  防 usnvmemu 抖动引发 add/remove 风暴打爆 guest PnP。竞态加固（add 进行中又 Lost 等）。
  真机：usnvmemu 反复重启压测，guest 盘稳定 appear/disappear，PnP 不卡。

### Task C3 — at-boot-absent 冷插完整化
- 确认 boot 时 usnvmemu 没起则 **不** add bus（guest 启动无盘）；运行时起 usnvmemu →
  reconcile add → guest 冷插出盘。这是 Layer C 完整目标。

---

## 风险（architect 评审）
- guest PnP 对 rescind/re-offer 的实际行为（C0 门验）。
- surprise-remove 时 guest 未决 IO 的收尾（C0 验；underhill 侧 go_lost drain 已 complete_error 不 hang）。
- re-offer 同 instance_id guest 是否认新设备（C0 验）。
- reconcile 必须串行（单 task），不并发 add/remove。

## 参考 file:line
- 蓝图 `vm/devices/pci/vpci_relay/src/lib.rs:114-118,326-368`
- API `vmm_core/vmotherboard/src/chipset/builder/mod.rs:66,99`；`state_unit/src/lib.rs:962`
- 摘出点 `openhcl/underhill_core/src/worker.rs:3467-3500`；`dispatch/vtl2_settings_worker.rs:1927`
- 留存 `openhcl/underhill_core/src/dispatch/mod.rs:149,165`
- reconcile 范本 `openhcl/underhill_core/src/emuplat/netvsp.rs:1181-1265,1240-1252`
- C-2/C-3 `vm/devices/pci/vfio_user_pci_device/src/device.rs:21-43,187,231`；`worker.rs:187,273,476`
