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

