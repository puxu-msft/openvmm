# Layer C C0 真机 POC 结果 — VPCI 运行时热插拔的 guest-PnP 行为

**日期**：2026-06-12　**VM**：pcie-remote-exp（真 OpenHCL Hyper-V VM）
**IGVM**：Layer C C0 代码（commit `5f871cd6` + re-add 修 `4494df06`）+ `--override-openvmm-hcl-feature vpci`

## 目的

C0 是 Layer C 的真机 POC 门：在写完整 reconcile/debounce 前，先用最小实现验证**承重的
guest-PnP 行为**——OpenVMM 源码无法预知 Windows guest 对 VPCI 通道 offer/rescind/re-offer
的实际反应（[[poc-before-settling-design]]：评审验设计内部一致性，不验平台外部真相）。
C0 实现 = device shim 长存（`add_dyn_device`+`with_external_pci`），VpciBus 层随 device
的 Live/Lost 在 dispatch select-arm reconcile 里 `add_dyn_device`/`DynamicDeviceUnit::remove`。

## ✅ PROVEN（C0 核心机制成立）

1. **boot-absent**：usnvmemu 未起时 device shim 装配但 **VpciBus 不 offer**（kmsg
   `long-lived device shim assembled (bus not yet offered)`），guest 无功能设备（disk=0）。
2. **offer-on-Live → guest 自动出盘（核心 win）**：起 usnvmemu → worker `reconnected, Live`
   → reconcile `VpciBus offered` → vmbus `sending offer to guest {11111111…}` + guest
   `opened channel` → guest 枚举 `DEV_00A9` **devnode status=OK + NVMe disk「OpenHCL
   Userspace NVMe v2.0」256MB Online，全程无需手动 disable/enable**（解掉了 W6c 的手动缺口
   ——offer 发生在 Live 之后，guest 对着已就绪控制器枚举）。**真 4MiB IO：oracle-1 guest
   readback markerMatch=True + oracle-2 独立 raw backing 扫 @ byte 22577152**。
3. **revoke-on-Lost → guest 移盘**：`pkill -f usnvmemu` → worker `going Lost` → reconcile
   `VpciBus removed` → vmbus `revoking channel`+`rescinding channel from guest` → guest
   devnode **present=False, disk=0**（PnP 移除）。
4. **re-add 在 underhill 层修复**（见下 device_id bug）。

## 🐞 re-add device_id bug（已修，`4494df06`）

**现象**：kill→重起 usnvmemu 后，underhill 2nd Live 到达但**无 2nd "VpciBus offered"**，
guest 盘不回来。**根因**（kmsg 抓到被 swallow 的 dispatch ERROR）：每个 Live 边沿 `add_bus`
都 `partition.build_virtual_device(device_id)` 重建虚拟设备，但持久 `msi_conn`/
`interrupt_mapper` 仍持有上一次的 → device_id 未释放 → 第二次 `build(device_id)` 撞
**"device id 572666672 is already in use"**。**修**：虚拟设备 + device_id 是分区级资源，
移到 assemble **一次性**建，存持久 `VpciInterruptMapper`（Arc，Clone），每个 add_bus 只
`clone()`，不重建。**真机复验**：boot-absent→add→remove→re-add **2nd offered +
already-in-use errors=0**（underhill 层 re-add 通）。

## ⚠️ finding-⑥：surprise-remove 挂载+脏数据卷会崩 guest（intermittent）

**现象**：热-add 盘 → 格式化 NTFS + 写 4MiB + flush → kill usnvmemu（surprise-remove）→
PSDirect "remote session ended" → **整个 VM reboot**（VTL2+VTL0 uptime 重置、/tmp 清空）。
**但同一 surprise-remove 在盘保持 RAW（未格式化/未挂载）时安全**（无崩，guest disk 干净
归 0）。故根因 = **surprise-removal 一个挂载且有脏/在途 IO 的 NTFS 卷** → guest 卷栈
bugcheck（BSOD→reboot）。intermittent（C0 第一轮 remove 没崩，本轮崩）——取决于移除瞬间
是否有 dirty/lazy-writer 活动。

**这正是 architect 预警的 "surprise-remove + in-flight IO" real-VM 未知。** 正确做法是
**graceful PnP removal**（query-remove / `VpciBusEvent::PrepareForRemoval`，让 guest 先
flush+dismount 再撤通道），而非当前 C0 的直接 `DynamicDeviceUnit::remove`→drop→rescind。
→ C2 硬化项。

## ⚠️ finding-⑦：re-offer 同 instance_id（surprise-remove 后）guest 不重枚举

**现象**：re-add 在 underhill 层成功（2nd offer，无 error），但 guest **devnode 恒
present=False、disk=0，连 `pnputil /scan-devices` 也唤不回**。即 surprise-rescind 后用
**同一 instance_id** re-offer vmbus 通道，guest vmbus/PnP **不干净地重新接纳**该设备。

**这正是 architect 预警的 "re-add 同 instance_id guest 行为未知"（unknown #8）。** 疑因：
surprise-rescind 后 guest 对该 channel key（`{44c4f61d…}-{11111111…}`）的 vmbus/PnP 状态
未完全释放，同 key re-offer 被当作"已知/已移除"而不重新 PDO 化。

**含义**：当前 C0 的 surprise-rescind/re-offer 路径对**首次 add 完美**，但 **remove（崩）+
re-add（不重枚举）有 guest 侧硬伤**。干净的运行时热插拔需要：① graceful removal（先让
guest 释放 devnode）+ ② 干净 re-add（要么换 instance_id，要么走 architect 路径 B 的
BUS_RELATIONS2 变长设备列表——标准 VPCI 热插拔语义——而非 vmbus 通道级 rescind/re-offer）。
→ C2/C3 设计课题，由本 POC 的硬数据驱动。

## 结论

C0 **核心机制 PROVEN**（offer-on-Live 自动出盘 + IO 双 oracle = 最有价值的 win，且解掉了
W6c disable/enable 缺口）。三个 architect 预警的 guest-PnP 未知**全部由真机 POC 答复**：
首次 add 干净；surprise-remove 挂载卷崩（⑥）；re-offer 同 instance 不重枚举（⑦）。后两者
重塑 remove/re-add 设计——下阶段 C2 上 graceful removal + 干净 re-offer/路径 B。这是 POC
门的全部价值：在铺完整 reconcile 前，用最小实现把"平台真相"挖出来。

## 复跑（C0 机制；注意 surprise-remove 挂载卷有崩风险，验机制用 RAW 盘）
```bash
# rebuild: cargo xflowey build-igvm x64 --override-openvmm-hcl-feature vpci
# boot(无 usnvmemu)→guest 无盘；起 usnvmemu→guest 自动出盘+IO；kill(RAW 盘)→移盘；重起→re-add
```

---

## ✅ C2-0 真机门 0 — graceful EJECT 缓解 finding-⑥（commit b2e8ab41e）

**日期**：2026-06-12（upstream merge d1f26a05 后）。C2-0 在撤通道前先发 graceful PnP
EJECT 给 guest（query-remove 窗口），等 EJECT_COMPLETE（1500ms 超时兜底）再 rescind。

**门 0 测试**：复现 finding-⑥ 崩溃条件——热-add → 格式化 NTFS + 写 4MiB + Write-VolumeCache
flush（挂载+脏卷）→ `pkill usnvmemu`（触发 graceful remove）→ 看 guest 是否崩。

**结果（2/2 guest 存活）**：两轮都 — kmsg `vpci: sending graceful EJECT to guest` → 
`graceful EJECT timed out; proceeding to rescind`（1500ms）→ `VpciBus removed`；guest **VM
uptime 不重置（89→99 / 55→64）+ PSDirect 全程响应 + disk 干净归 0**。对比 C0 同条件
（surprise-rescind）intermittent BSOD→reboot，**C2-0 graceful EJECT 下 guest 不崩**。

**承重 nuance（诚实记）**：guest **未回 EJECT_COMPLETE**（恒超时）。根因 = finding-⑥ 触发场景
是 usnvmemu **被 kill（后端已死）** → device shim Lost → guest 收到 EJECT 后做 query-remove
要 flush，但 flush 打到 Lost device（MMIO 返 Err）→ flush 失败 → eject 不完成。但 guest 的
**有序 query-remove teardown（接受 flush 失败）仍避免了 surprise-yank 的 BSOD**。即 C2-0 是
"EJECT heads-up + 超时 fallback rescind"，对**后端已死**的非计划移除是能达到的最好结果
（无法向死后端 flush）；对**计划移除**（operator 先 EJECT 再停 usnvmemu，后端尚活）才能拿到
完整 EJECT_COMPLETE 的干净 flush——那是未来 operator-workflow，非 C2-0 reactive 模型范畴。

**2/2 非铁证（⑥ intermittent），但 EJECT heads-up 的机制是确定性的**（每次都先给 guest
query-remove 信号再 rescind），故合理判定门 0 PASS：**graceful EJECT 缓解 ⑥**。

**仍未做 = finding-⑦**（re-offer 同 instance_id 不重枚举）：C2-0 只改了 remove 路径加 EJECT，
**没动 re-add**（仍 channel rescind/re-offer）→ ⑦ 未修，re-add 后 guest 盘不回来。→ **C2-1**：
device_count 0↔1 模型（VpciBus 通道恒在，重发变长 BUS_RELATIONS2）修 ⑦。


---

## 🔬 finding-⑦ 深挖 — C2-1 / C2-1-fix 真机失败 + 根因确认（2026-06-12 续）

C2-1（device_count 0↔1，commit `a3122b3af`）+ C2-1-fix（`INVALIDATE_BUS`）真机门 1 **均失败**。
逐步真机调试（**非**靠源码推断——源码不可知 Windows pci.sys 行为）确认了根因。

### 真机证据链（决定性）

1. **C2-1 device_count push 失败**：kill usnvmemu → graceful EJECT + `SetPresent(false)`
   → `device_count=0`（盘移除，guest **存活**，graceful 缓解⑥再确认）；重起 usnvmemu →
   `SetPresent(true)` → 主动 push `BUS_RELATIONS2 device_count=1` → **guest 不出盘**。
   bus FDO（`Microsoft Hyper-V Virtual PCI Bus {11111111…}`）present 且健康，但无 child PDO。
2. **H2（identity/ghost）否定**：清掉滞留的 `OpenHCL Userspace NVMe` 幽灵 devnode
   （`pnputil /remove-device`，2 个全清）后，**同 serial** 的 `device_count=1` push **仍不出盘**。
   故根因**不是** child PDO identity 撞幽灵（bump serial 不会有用）。
3. **决定性 — FDO 重上电生效**：手动 `Disable-PnpDevice` + `Enable-PnpDevice` bus FDO
   → **同 serial 盘立即回来 + IO 正常**。证明：device_count=1 **已正确送达**（设备确在总线上）、
   后端/identity/config **全 OK**；唯一缺的是**让 guest 重查 relations 的触发**。
4. **C2-1-fix INVALIDATE_BUS 失败**：`SetPresent(true)` 改发标准 `INVALIDATE_BUS`（“总线
   relations 变了，请重查”，header-only 4 字节）→ **guest 仍不出盘**；紧接着再 disable/enable
   FDO → 盘立即回来。证明 `INVALIDATE_BUS` **不**触发 Windows guest 重查（≠ FDO 重上电）。

### 根因（已确认，平台行为）

**Windows pci.sys 只在 guest 自己重上电 bus FDO（`FDO_D0_EXIT`→`FDO_D0_ENTRY`）/ 首次开通道
时才枚举 VPCI 子设备。** 对 VSP 侧任何 push **都不重枚举**：
- 同-instance 通道 rescind + re-offer（C0 finding-⑦）；
- unsolicited `BUS_RELATIONS2` device_count push（C2-1）；
- `INVALIDATE_BUS`（C2-1-fix）。

而 **VSP 无法在恒定通道上迫使 guest 重上电 FDO**。故 C2「通道恒在 + device_count 切换」
（路径 B）的**核心前提在 Windows 上不成立**——非换个信号能修。

### 架构含义 + 方向（用户定：先验证 Option B）

「Lost 时 hot-remove、Live 时 hot-re-add」模型在 Windows 不可闭环（remove 经 EJECT 可行，
re-add 让 guest 重出盘不可行）。出路：
- **Option A**：re-add 换**新 instance_id** 重建 VpciBus（guest 见全新总线→建新 FDO→`FDO_D0_ENTRY`
  →枚举）。几乎必然生效，但每周期漏一个 phantom 总线 devnode。plan 预登记的兜底。
- **Option B（用户选先验证）**：**改变模型**——usnvmemu Lost 视为 transient 后端停顿，**不**移除
  设备；设备对 guest 恒在，靠长存 device shim 的 **C-3 reconnect** 透明恢复（重发 set_irqs+dma_map，
  如真硬件 controller 短暂 reset，盘不消失、在途 IO 超时重试后恢复）。**根本不触发**需 guest
  重枚举的 re-add 路径，绕开⑦。无 phantom 债，最贴近真硬件。验证点：guest 能否容忍 Lost 窗口
  （NVMe driver timeout 内）。永久移除仍可用 `hide_device` graceful EJECT（保留为 scaffolding）。

C2-1-fix 的 `INVALIDATE_BUS` 改动**已 revert**（真机证伪、未 commit）；委员 device.rs 维持
committed C2-1。Option B 实现 = `vfio_user_hotplug.rs` 的 `process` Lost 边沿改为 no-op（保设备在位）。

---

## ✅ Option B 真机验证结果（2026-06-13）— transient-stall 模型成立（fast restart）

实现：`process` 的 Lost 边沿（`(false, true, true)`）从 `hide_device()` 改为 **no-op**
（保设备在位 + log）；`hide_device` 转 `#[expect(dead_code)]`（留作未来 operator 显式永久移除）。
device.rs 维持 committed C2-1（device_count 机制在 Option B 下休眠，re-add 臂运行时不可达但编译可达，
无 dead_code 告警）。`state.rs` 的 "Lost 为终态" stale doc 一并改正（Lost↔Live 经 C-3 reconnect 反复）。

**真机（Option B IGVM，fresh boot → 首 add 格式化+写 marker）：**
- **fast restart（~5s 停顿）→ 盘存活**：kill usnvmemu → kmsg `keeping device present (transient
  stall)` → 重起 → C-3 reconnect `reconnected, Live` → **盘 Healthy + 旧 marker 持久 + 新 4MiB IO OK**，
  guest **全程无 remove/re-add**（无需重枚举，绕开⑦）。
- **long outage（~10s 停顿）→ guest 有序移盘**：停顿超 guest 容忍窗口 → guest 自身 IO 超时后
  **有序**移除盘（**无⑥崩溃**——guest 主导的 orderly removal，非 surprise-rescind）；reconnect 后
  盘**不自动回来**（re-add 不可行），需 guest FDO 重上电（reboot/rescan）恢复——FDO disable/enable
  验证盘确可回（Healthy），证设备本体无恙、纯 guest 侧 stall-tolerance 限制。

**结论**：Option B 对**快重启**（usnvmemu crash+auto-restart 的主可靠性场景）**透明无缝**，
**与真硬件 NVMe 行为一致**（controller 短暂 reset 盘不消失；长时间消失 OS 移盘）。无 phantom 债、
无⑥风险。长 outage 的不可自动恢复是 Windows 平台硬限（VSP 无法迫 guest 重枚举）+ 真硬件同理，
作为已知限制记录（未来若需可加 Option-A new-instance_id 兜底强制重枚举，代价 phantom 债）。

**采纳 Option B 为 Layer C 的设备-存在模型**：usnvmemu 是 transient 后端，非可热插拔设备；
首 add（冷插/boot-absent→起）+ 透明 reconnect 覆盖主场景；永久移除留 `hide_device` graceful EJECT scaffolding。
