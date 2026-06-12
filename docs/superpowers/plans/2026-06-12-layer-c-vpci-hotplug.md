# Layer C — emulated vfio-user NVMe 设备 guest 运行时热插拔 实现计划

> **⚠️ 终局状态（2026-06-13，commit `b7bd5459d`）：本计划的「hot-remove/re-add」核心模型
> 经真机证伪——Windows pci.sys 只在 guest 自己重上电 bus FDO 时才重枚举 VPCI 子设备，VSP 侧
> 任何 push（C0 同-instance re-offer / C2-1 device_count / C2-1-fix INVALIDATE_BUS）都不重枚举，
> 四方法真机全败。已弃热插拔模型、改采 Option B「usnvmemu 视为 transient 后端停顿：设备恒在 +
> C-3 透明重连」（real-VM 验证通过）。完整结论见
> `usnvmemu/experiments/2026-06-12-layer-c-c0-real-vm/RESULT.md`（⑦深挖 + Option B 节）+
> memory `vfio-user-underhill-state`。下文 C2/C3 的 device_count/re-offer 设计**已作废**，仅留作
> 调试历程记录。**

## 已试方案与去留（未采纳的留作未来参考——"也许未来会用到"）

承重平台约束（真机坐实，所有方案的前提）：**Windows pci.sys 只在 guest 自己重上电 bus FDO
（`FDO_D0_EXIT`→`FDO_D0_ENTRY`）/ 首次开通道时才枚举 VPCI 子设备。VSP 侧无法在恒定通道上迫
guest 重查 relations。** 据此评估每个方案：

| 方案 | 机制 | 真机结果 | 去留 / 未来适用性 |
|------|------|---------|------------------|
| **C0 同-instance 通道 re-offer** | rescind VpciBus + 用**同** instance_id re-offer | ❌ 失败（finding-⑦）：guest 不重枚举，连 `pnputil /scan-devices` 也唤不回 | **永久弃**。同 instance_id → guest vmbus/PnP 状态未释放，当"已知/已移除"不重 PDO 化。 |
| **C2-0 graceful EJECT** | 撤通道前发 `EJECT`(slot0) 给 guest query-remove 窗口 | ✅ 缓解⑥（脏卷不崩）；但 guest 不回 EJECT_COMPLETE（后端死无法 flush） | **保留为 scaffolding**（`hide_device`，`#[expect(dead_code)]`）。**未来 operator 显式永久移除**时用：先 EJECT 让 guest 有序 dismount 再撤。 |
| **C2-1 device_count 0↔1 push** | 通道恒在，unsolicited 重发变长 `BUS_RELATIONS2`（device_count 0↔1） | ❌ re-add 失败：device_count=1 已送达但 guest 不出盘（remove 经 EJECT 可行） | **弃**（机制留在 device.rs 作 scaffolding，Option B 下休眠）。Windows 不响应 unsolicited relations push。 |
| **C2-1-fix INVALIDATE_BUS** | re-add 时发标准 `INVALIDATE_BUS`（"relations 变了请重查"） | ❌ 失败：guest 不重查（≠ FDO 重上电）。已 revert | **弃**。Windows pci.sys 不响应 VSP 发的 INVALIDATE_BUS。 |
| **Option A：换 NEW instance_id re-offer** | rescind 旧 bus + 用**全新** instance_id 重建+offer（guest 见全新总线→建**新 FDO**→`FDO_D0_ENTRY`→枚举） | ⚠️ **未验证**（C0 只验了同 instance_id；新 instance_id 是不同情形） | **未采纳，留作未来首选恢复路径**。理论应生效（新 FDO = guest 自己重上电的等价物）。代价：每次每周期漏一个 phantom 总线 devnode（Windows 会 GC）。**未来若要长停顿自动恢复，先 POC 此方案**（承重假设未验证，按项目规矩先 POC 再定）。 |
| **Option B（已采纳）** | usnvmemu 停=transient 后端停顿，**不**移除设备；设备恒在，C-3 reconnect 透明恢复 | ✅ real-VM 通过：fast restart 盘无缝存活+IO；多次快重启稳定；long outage guest 有序移盘（无⑥崩） | **采纳**。HW-accurate（真硬件 controller reset 同理），无 phantom 债。**唯一缺口=长停顿不自动恢复**（guest 超时移盘后需 reboot/rescan，或未来上 Option A）。 |

**未来增强路径（若需要）**：Option B + Option A 混合——快重启走 Option B 透明（主场景）；
检测到长停顿（Lost 超 guest 容忍窗口 ~8s，需计时）→ reconnect 时走 Option A（rescind 旧 bus
先清掉 guest 可能残留的盘 + 用新 instance_id 重 offer 强制重枚举）。**先 POC 验证 Option A 的
"新 instance_id 重枚举" 承重假设**再定混合设计。代价 = 长停顿（罕见）时的 phantom 总线债。

---

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

### C0 + C1 ✅ 已完成（commits 5f871cd6 / 4494df06，真机 POC）
C0 把 C1（live_tx 边沿订阅 + dispatch select-arm add/remove）一并做了。真机证：**offer-on-Live
→ guest 自动出盘 + IO 双 oracle（核心 win，解掉 W6c disable/enable 缺口）**；re-add device_id
bug 修（虚拟设备 assemble **一次性**建、`VpciInterruptMapper.clone()` 复用）。**但暴露 2 个
guest-PnP 硬伤**（归档 `usnvmemu/experiments/2026-06-12-layer-c-c0-real-vm/RESULT.md`）：
- finding-⑥：surprise-remove 一个**挂载+脏数据**的 NTFS 卷 → guest BSOD→reboot（intermittent；RAW 盘安全）。
- finding-⑦：surprise-rescind 后用**同 instance_id** re-offer → guest 不重枚举（pnputil 也唤不回）。

### C2 — 干净热插拔（据 ⑥⑦，architect 评审定 **路径 B + graceful EJECT**）
**路径 B**（standard VPCI PnP：BUS_RELATIONS2 device_count 0↔1，**vmbus 通道恒在**）从根修⑦
（通道/instance_id 恒定 = 同盘 in/out，无 phantom devnode），优于路径 A 换 instance_id（永久
phantom 债、错抽象）。两路都需 **graceful EJECT** 修⑥（device_count=0 不保证优雅，Windows 可能
仍 surprise）。**最小侵入**：VpciChannel 只需 "0/1 设备"（非 N），加**可选** command receiver；
4 个现有 `VpciBus::new` 调用方传 `None` = 零变化（blast radius 受控，architect grep 确认）。

- **C2-0（真机门 0，最先——最便宜验最危险的未知）**：仅 graceful EJECT，不改 device_count 模型。
  - `vpci/src/device.rs`：`parse_packet`（:309-511）认入站 `EJECT_COMPLETE`（现落 `UnknownType`
    杀通道，**必补**）；`ReadyState`（:615）加外部 command receiver，`run`（:749 `queue.read().await`）
    换 `select!`（**`None` 分支另一臂 `pending()`；借用/pending 正确性 unit test 锁死**——最敏感处）；
    命令 `Eject` → 发 `EJECT{slot:0}` 等 `EJECT_COMPLETE`（超时兜底）。
  - `VpciChannel::new`/`VpciBusDevice::new`/`VpciBus::new` 加 `Option<Receiver<HotplugCommand>>`。
  - `vfio_user_hotplug.rs` `remove_bus`：先经 channel 发 `Eject` 等 complete/超时 → **再**
    `bus_unit.remove()`。
  - **门 0**：热-add → 格式化 NTFS+写+flush（**造⑥脏卷条件**）→ graceful remove → **guest 不
    BSOD/reboot**？最微妙点：⑥触发时 usnvmemu 已 kill（后端死）→ guest flush 打到 Lost device
    shim（C-2 MMIO 返 Err）→ 观察 guest 能否仍干净 dismount。**过→C2-1；不过→生产侧 EJECT 无效，回退。**
- **C2-1（真机门 1——验⑦修）**：`device_present` 0↔1 模型 + `send_child_device` 变长 device_count +
  外部臂**主动重发 BUS_RELATIONS2**（**非** INVALIDATE_DEVICE——两侧均未实现，复用 C0 已验的
  send_child_device）。VpciBus **恒 add 一次不 remove**；Live/Lost 改发 `SetPresent(true/false)`。
  - **门 1**：add → graceful remove → **re-add → guest 重枚举 + 盘回来 + 再 IO + host oracle**？
    **过→⑥⑦双修**；不过→才考虑换 instance_id 兜底。
- **C2-2（抖动加固）**：debounce（Lost→Live settle，仿 `netvsp.rs:1240-1252`）+ 命令序列化（EJECT
  进行中又 SetPresent(true) 竞态）。usnvmemu 反复重启压测，盘稳定 appear/disappear、无 phantom、PnP 不卡。

### C3 — at-boot-absent 冷插完整化（C2 后）
boot 无 usnvmemu → guest 无盘；运行时起 → 冷插出盘。

## 必验真机未知（C2，源码不可知 Windows guest 行为）
- guest 对**生产侧** EJECT 的反应（本仓只有 `vpci_client` 消费侧 EJECT 先例，生产侧 VSP `device.rs` 从未发过）。
- device_count 1→0 优雅性 + 0→1 重枚举（路径 B 修⑦核心假设）。
- EJECT_COMPLETE 超时 + **usnvmemu-已死时 graceful flush 的收尾**（⑥最微妙点）。
- 盘符/挂载点跨 re-add 稳定性（路径 B 保 instance_id+serial_num 恒定，Windows 持久化策略未知）。

## blast radius（architect grep 确认受控）
`VpciBus::new` 4 调用方：`device_builder.rs:80` / `openvmm dispatch.rs:2469` / `vpci_relay:342`
→ 传 `None` 零变化；`vfio_user_hotplug:326` → 传 `Some` 启用。`VpciChannel` 仅 `bus.rs:128`
+ 测试构造（无第三方直接构造）。

## 参考 file:line
- 蓝图 `vm/devices/pci/vpci_relay/src/lib.rs:114-118,326-368`；EJECT 消费侧先例
  `vpci_client/src/lib.rs:1092-1109,163-180`；协议 EJECT/INVALIDATE/BUS_RELATIONS2
  `vpci_protocol/src/lib.rs:50-111`（PdoMessage :803-810）。
- VpciChannel 单设备硬编码 `vpci/src/device.rs`：`ReadyState`:615-619 / `send_child_device`:674-723
  (device_count 写死 695/708) / `run`:725-783(唯一 await 749) / slot 检查 816-819 / parse_packet 309-511。
- API `vmm_core/vmotherboard/src/chipset/builder/mod.rs:66,99`；reconcile 范本
  `netvsp.rs:1181-1265,1240-1252`；C-2/C-3 `vfio_user_pci_device/src/device.rs:21-54`。
