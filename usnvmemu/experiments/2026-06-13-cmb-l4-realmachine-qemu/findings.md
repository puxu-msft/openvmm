# CMB L4 真机 e2e 实测结果（QEMU + 真 Linux guest）

**日期**：2026-06-13　**环境**：QEMU 11.0.1 + Ubuntu 6.8.0-31 guest kernel + busybox initramfs
**harness**：`usnvmemu/crates/vfio_user_transport/scripts/qemu_interop/run_qemu_vfio_guest_cmb.py`
（`CMB_MODE=trap|map`，三 oracle：guest IO PASS + host backing marker + firmware `CMB-RESIDENT-ACCESS`）
**firmware 观测性**：`controller/cmb.rs::note_first_cmb_access`（首次 CMB-resident 访问打日志）

## 结论：L4 部分达成 —— **CMB enable 握手 + 真 IO 已对真驱动验证；SQ-in-CMB 数据放置卡在 guest p2pdma**

### ✅ 已验证（firmware-correctness 关键部分）
真 Linux `nvme` 驱动对本 firmware**完整走通 CMB 发现 + 启用握手**（trap 与 map 两模一致）：
1. 读 CAP → 见 CMBS 位（`build_cap` 真置）→ 决定支持 CMB；
2. 写 `CMBMSC.CRE=1`（offset 0x50 = 0x1）→ firmware `CMBMSC programmed cre=true`；
3. 读 `CMBSZ`(0x3c) = `0x2001f0` → SZ=512×4KiB=2 MiB、**SQS|CQS|LISTS|RDS|WDS 全置**（驱动看到 CMB 可放队列）；
4. 读 `CMBLOC`(0x38) = `0x2` → BIR=2（CMB 在 BAR2）；
5. 写 `CMBMSC = 0xfe800003` → **CMSE=1 + CBA=0xfe800000**（驱动启用 CMB 内存空间 + 重定位到 BAR2 GPA）。
6. 真 NVMe IO（write/flush/read marker）经 vfio-user 全程 PASS + host backing 裸读独立核到。

→ **firmware 的 CMB 寄存器/启用序列对真驱动的确切时序成立**（in-process 测不出"驱动真按 CAP.CMBS→CRE→CMBSZ→CMBLOC→CMSE 这个序列操作"）。这是 L4 的核心 firmware-correctness 价值。

### ⚠️ 未达成：驱动把 IO SQ 放进了 **host RAM 而非 CMB**
firmware 日志 `Create IO SQ gpa=0x1ffdb000 / 0x1e78000 / 0x3108000`——全在 guest 512 MiB host RAM 内，**非** CMB(0xfe800000)。故 SQE-fetch 走 DMA、`CMB-RESIDENT-ACCESS` 不触发。

**根因（guest 侧条件，非 firmware bug）**：Linux `nvme` 驱动用 `pci_alloc_p2pmem` 从 CMB 分配 SQ，需 **CONFIG_PCI_P2PDMA + BAR 可作 p2p 资源**。驱动启用了 CMB(CMSE)，但 SQ 分配从 CMB 失败 → 静默回退 host RAM。

**下一步 lever（未做，需用户定夺，跨切+不确定）**：
- **CMB BAR 改 64-bit prefetchable**（现疑为 32-bit non-prefetchable；真 CMB BAR 通常 64-bit prefetchable，spec-aligned）——可能让 guest p2pdma 接纳该 BAR。但 64-bit BAR 占 2 槽(BAR2+BAR3)，影响 P3b resolver / OpenHCL client / in-process e2e，**跨 crate 改 + 有回归面**，且即便改了 p2pdma 仍可能受 guest CONFIG_PCI_P2PDMA / QEMU vfio-user-pci p2p 暴露其它条件限制——多轮不确定。
- 或换一个明确带 P2PDMA + p2p-capable BAR 的 guest 环境。

### 净判
L4 把"真驱动真用 CMB"拆成两半：**(a) 发现+启用握手 + 真 IO** = ✅ 真机已证；**(b) SQ/data 物理落 CMB** = ⚠️ 卡 guest p2pdma，是 guest 环境/BAR-类型条件而非 firmware 正确性。harness + 观测性已落库,改 64-bit prefetchable BAR 后可一键复跑验证 (b)。

## 追加：① 64-bit prefetchable CMB BAR 实测（2026-06-13）

按用户 ①，把 CMB BAR 从 32-bit non-prefetchable 改为 **64-bit prefetchable**（`describe()` `BarKind::Mmio64 + prefetchable:true`；真 NVMe CMB BAR 即此形态，spec-aligned）。in-process 测试全绿（228+6+2+33 / transport 78）。真机复跑（trap）：

### ✅ 实质进展：p2pdma **资源注册成功**（32-bit 时没有）
guest dmesg 新增：`nvme 0000:00:03.0: added peer-to-peer DMA memory 0xfe000000-0xfe1fffff`
→ **64-bit prefetchable BAR 让 Linux `pci_p2pdma_add_resource` 成功**（32-bit 时此步静默失败、无此行）。这是真驱动对"CMB BAR 可作 p2p 资源"的认可——firmware 侧 BAR 形态现对真驱动 p2p 路径成立。

### ⚠️ 仍卡：`pci_alloc_p2pmem` 返 NULL → SQ 回退 host RAM
`Create IO SQ gpa=0x1ffdb000`（host RAM,非 CMB 0xfe000000）。强制 `nvme.use_cmb_sqes=1`（harness `GUEST_EXTRA_CMDLINE`）**仍不变**。即:p2p 资源已注册,但驱动从该 p2p 池**分配** SQ 内存(`pci_alloc_p2pmem`)返 NULL → `cmb_use_sqes` 回落 false → SQ 落 host RAM。

**根因（firmware-external,不可从我方修）**:`pci_alloc_p2pmem` 在 QEMU vfio-user-pci 模拟设备上分配失败——疑 p2pdma 的 provider/client distance 计算 / ACS / 模拟拓扑判定"无 p2p 路径",或 emulated BAR 的 p2p 池发布限制。这是 **Linux p2pdma 分配层 + QEMU 模拟交互**的限制,非 firmware 正确性问题。

### 净结论（① 后）
- **firmware CMB 现完全 spec-aligned 且对真驱动 p2p 注册成立**（64-bit prefetchable BAR 是真改进,已 keep）。
- **SQ 物理落 CMB** 卡在 `pci_alloc_p2pmem`(QEMU/p2pdma 分配层),**firmware 无杠杆**。三个 lever 试毕(64-bit BAR ✅注册 / use_cmb_sqes ✗ / 都不改变分配失败)。
- **建议**:L4 在此收束于"握手+真 IO+p2p 注册已真机证";SQ-in-CMB 需换一个 p2pdma 分配能成的环境(真硬件 NVMe-CMB,或 p2p 拓扑更完整的 guest/hypervisor),非 QEMU-vfio-user-emulated 可达。
- **follow-up**:OpenHCL client(`vfio_user_pci_device` P3b)的 BAR2 也应改 64-bit prefetchable 以与 firmware 一致(当前 OpenHCL CMB 路径未真机验证,记此待办)。

## 修订（独立 subagent 审计后，2026-06-13）—— 根因归因纠正

上文 ① 节把根因写成"`pci_alloc_p2pmem` 在内核 p2pdma 分配层失败 / distance·ACS·拓扑"——**独立审计据 Linux v6.8 源码推翻此归因**，纠正如下（与审计协商一致）：

- **内核源码反证**：`nvme_alloc_sq_cmds`(pci.c) 里设备从**自己**的 p2p pool 分配 SQ（`pci_alloc_p2pmem(pdev,…)`）**不走** distance/ACS/拓扑判定（那些只在给*别的*设备找 provider 的 `calc_map_type_and_dist`）；且 `add_resource` 已成功（pool 已建、2 MiB 已 memremap），8 KiB 不会耗尽 → **`pci_alloc_p2pmem` 在此场景没有合理的返 NULL 路径**。原"内核分配器限制"归因是从"SQ 落 host RAM"的**错误反推**。
- **真正的烟枪（原分析漏看）**：QEMU log 有明确报错——`BAR 2: failed to create dma-buf: PCI BAR IOMMU mappings may fail: Invalid argument`（map run）/ `vfio_container_dma_unmap(…,0xfe000000,0x800000) = -22`（trap run）。→ 洞在 **QEMU vfio-user 把 CMB BAR 暴露为"可作 DMA 目标 / IOMMU 可映射的 MMIO"这层**（介于 BAR 寄存器暴露[已过] 与内核纯分配[按源码应成功] 之间）。`pci_p2pmem_virt_to_bus` 返 0（bus 地址链路异常）比"分配器返 NULL"更贴 QEMU 报错。
- **map 模式（真 memfd 共享 BAR）同样失败** → 排除"trap 假 BAR 无 struct page"解释；坐实洞在 BAR-DMA 暴露层而非 backing 性质。
- **根因目前仍是推测，未坐实**：需 ① guest 侧 `nvme` dyndbg / ftrace `nvme_alloc_sq_cmds` 直接抓是 `pci_alloc_p2pmem` NULL 还是 `virt_to_bus` 0 + `/sys/.../p2pmem/`；② **QEMU 自带 emulated `nvme`(`-device nvme,cmb_size_mb=…`) 同 guest 对照**——其 SQ 也不落 CMB ⟹ QEMU 模拟-BAR p2pdma 通病(fork 自家 vfio-user 白搭);落了 ⟹ 洞在 vfio-user 路径(fork 才对路)。**此对照须先于 fork 决策。**

### fork-QEMU 裁定（修订后，与审计一致）
- **当前不该先 fork**——但理由从"洞在内核"修正为"洞虽指回 QEMU vfio-user 的 BAR-DMA 暴露,但根因未坐实,且具体动作不是改已通过的 BAR 寄存器暴露,而是深水区的 vfio-user dma-buf/IOMMU-mappable 路径,确定性低成本高"。
- **决策前置实验**:先 QEMU-emulated-nvme 对照 + 内核侧观测,坐实洞在 vfio-user 还是 QEMU 模拟通病,再谈 fork。

## S6 对照实验：QEMU emulated nvme —— **决定性结果（fork 裁定反转）**

harness：`qemu_interop/run_qemu_nvme_cmb_control.py`（QEMU 自带 `-device nvme,cmb_size_mb=2`，同 guest，`-trace pci_nvme_create_sq`）。

### 结果：QEMU emulated nvme 的 **IO SQ 落在 CMB**
QEMU trace（真 oracle = `create_sq` 的 addr，**非** `pci_nvme_map_addr_cmb`——后者是 DMA-data 路径、SQ-fetch 不走它，我**一度误用**，读 trace 后纠正）：
```
pci_nvme_create_sq addr=0x1ffd8000 sqid=1 qsize=255   ← admin SQ, host RAM
pci_nvme_create_sq addr=0xfe000000 sqid=1 qsize=1023  ← IO SQ 在 CMB!
pci_nvme_create_sq addr=0xfe010000 sqid=2 qsize=1023  ← IO SQ 在 CMB!
```
（p2p 注册区 `0xfe000000-0xfe1fffff`；IO SQ addr=0xfe000000 正落其中。）

### 对照
| 设备侧 | IO SQ 落点 | SQ-in-CMB |
|---|---|---|
| **QEMU emulated nvme**（native BAR）| `0xfe000000`（CMB）| ✅ |
| **我们的 vfio-user firmware** | `0x1ffdb000`（host RAM）| ❌ |

**同一 guest、同一 kernel、同一 CMB 2MiB**——唯一变量是**设备暴露路径**。

### 裁定（反转 S5 的"先别 fork"）
- **guest + kernel 完全有能力把 SQ 放进 CMB**（emulated nvme 实证），**不是**环境/内核限制。
- **洞专属 vfio-user 路径**：QEMU vfio-user client 把设备 BAR 暴露为"可作 DMA 目标/dma-buf"这步失败（`failed to create dma-buf` on CMB BAR），导致 Linux `pci_p2pmem_virt_to_bus` 拿不到 bus 地址 → SQ 回退 host RAM。
- → **fork/改 QEMU vfio-user 的 BAR-as-DMA-target / dma-buf 暴露路径,是让 SQ-in-CMB 在我们这条 transport 上成立的对路杠杆。** （回答了最初"qemu 官方 vfio-user 不支持,能否 fork and improve"——是,且对照已证 guest/kernel 不是瓶颈。）
- **范围**:这是 QEMU vfio-user **client** 的洞(也可能含我们 firmware 的 region_info 缺某属性让 client 据以建 dma-buf——两侧都属"vfio-user 路径",需进一步定位是 client-only 还是 server-hint)。

### 下一步（若投入）
1. 定位是 QEMU vfio-user client 单方面缺 dma-buf 支持，还是我们 region_info 缺让 client 建 dma-buf 的 hint/flag（读 QEMU `hw/vfio-user/` + libvfio-user dma-buf 协议）。
2. 据定位 fork QEMU vfio-user（或补 firmware region_info）打通 BAR-as-DMA-target。
