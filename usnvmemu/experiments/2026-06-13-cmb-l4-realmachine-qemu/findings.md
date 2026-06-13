# CMB L4 真机 e2e 实测结果（QEMU + 真 Linux guest）

**日期**：2026-06-13　**环境**：QEMU 11.0.1 + Ubuntu 6.8.0-31 guest kernel + busybox initramfs
**harness**：`usnvmemu/crates/vfio_user_transport/scripts/qemu_interop/run_qemu_vfio_guest_cmb.py`
（`CMB_MODE=trap|map`，三 oracle：guest IO PASS + host backing marker + firmware `CMB-RESIDENT-ACCESS`）
**firmware 观测性**：`controller/cmb.rs::note_first_cmb_access`（首次 CMB-resident 访问打日志）

## 结论：**L4 完全达成（trap+map 双模真机 PASS）** —— 真根因是两个 firmware 寄存器 bug，非 dma-buf/fork

> **重大更新（S8）**：S2–S7 一路以为"SQ 落 host RAM"卡在 vfio-user dma-buf 暴露层、需 fork QEMU。
> guest-ftrace（S7 part-1）+ 修复实测**推翻**此结论：真根因是**两个 firmware 自身的寄存器 bug**
> （CMBSZ 位布局 + CMBMSC 跨 Controller Reset 生命周期），都是 **self-consistent trap**（in-process
> 测试 + 误读 spec 两端自洽通过，只有真 Linux 驱动作独立 oracle 才暴露）。**修两个 bug 后 L4
> trap+map 双模真机全 PASS**（guest IO + host backing + firmware CMB-RESIDENT-ACCESS 三 oracle 一致）。
> **不需要 fork QEMU、不需要 dma-buf**——SQ-in-CMB 的 firmware 自读 backing 路径与 host dma-buf 无关。
> 下方 S2–S7 保留为**弯路存档**（教训价值：见 self-consistent trap 与 oracle 选择）。

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

### 裁定（反转 S5 的"先别 fork"——但 fork **对象**仍未定，见独立审计）
- **① guest + kernel 完全有能力把 SQ 放进 CMB** = ✅ **真机坐实**。emulated nvme 的 IO SQ addr=0xfe000000 落 CMB 区；QEMU 取 SQE 走 `nvme_addr_read`→`nvme_addr_is_cmb` 命中→`memcpy(n->cmb.buf)`（device CMB backing），绕开 DMA。即"addr 在 CMB 区"经 QEMU 语义等价于"SQE 真从 CMB backing 取"。**非**环境/内核限制。
- **② 瓶颈在 vfio-user 的 BAR-as-DMA-target 暴露链** = ✅ **成立**。同 guest 同 kernel，native-BAR 路径成功、vfio-user-BAR 路径失败（QEMU `failed to create dma-buf` / `vfio_container_dma_unmap = -22 EINVAL`）；+ map 模式（真 memfd 共享 BAR）也失败 ⟹ 排除"trap 假 BAR 无 struct page"。三条锁定瓶颈在这条链。
- **③ fork 对象未定**（审计纠正我**过早收敛**）：EINVAL 既可能是 **QEMU vfio-user client 不支持** BAR-dma-buf，也可能是**我们 firmware 的 region_info 缺 flag/hint**（如未标 dma-buf-able capability）让 client 据以建 dma-buf——**现有证据无法区分**。这区别决定该 **fork QEMU** 还是**改 firmware（几行 region_info，远省）**。故**不能把 fork 钉死在 QEMU**，更不能说"应先 fork QEMU"。
- → **正确下一动作 = 低成本定位实验（S7），不是 fork**。"动 vfio-user 这条链是对的方向"成立；"必须 fork QEMU"**未被证据指定**。
- ⚠️ 残留风险：EINVAL 来自 `vfio_container_dma_unmap`（IOMMU 容器层，非纯 dma-buf 创建）暗示要改的可能不止一层；fork 若真要做，深度可能超预期。

### 下一步（定位实验 S7，决定 fork 对象 / 是否需 fork）
见下方 ## S7 章节——已做协议侧源码溯源（决定性）。

## S7 定位实验：QEMU vfio-user dma-buf 源码溯源 —— **决定性，且推翻"region_info flag"廉价解**

### 协议侧（源码坐实，QEMU 11.0.1 + 用户 fork 11.0.50 双版核对）
`-22 EINVAL` 的精确来源 = **QEMU client 自身短路，根本没发 vfio-user 消息也没到 host kernel**：
1. `hw/vfio/region.c:288 vfio_region_create_dma_buf` 在 **BAR mmap 时**（设备 setup，非 guest 建 IOMMU 映射时）对每个 BAR 尝试建 dma-buf，使该 BAR 的 RAMBlock 带 fd → 可作 DMA target。失败只 `error_report` 不中断 BAR 映射（故 BAR 仍可访问、p2pdma 仍注册，但无 dma-buf）。
2. 它发 `VFIO_DEVICE_FEATURE_GET | VFIO_DEVICE_FEATURE_DMA_BUF` 走 `vfio_device_get_feature`→`io_ops->device_feature`。**brew 11.0.1 的 `vfio_user_device_io_ops_sock` 无 `.device_feature` 成员** → NULL 钩子 → 当场 `return -EINVAL`。后续 `vfio_container_dma_unmap = -22` 是该 BAR 退化为无-fd 映射的连锁后果。

### ❌ 推翻审计的"廉价解 A"（region_info 多发个 flag）
dma-buf 路径**不读任何 region capability**（`vfio_region_create_dma_buf` 只用标准 MMAP+sparse-mmap 的 `nr_mmaps`/offset/size）。能力协商走 **device-feature 通道（cmd 16）**，不是 region cap。→ **server 在 region_info 多发 flag 不可能解**。审计的 (A) 证伪。

### 三层缺口（每层源码锚定）
| 层 | 现状 | 缺口 |
|---|---|---|
| host kernel VFIO | `VFIO_DEVICE_FEATURE_DMA_BUF` 已进 mainline（`drivers/vfio/pci/vfio_pci_dmabuf.c`）| ✅ 有能力 |
| QEMU client | `region.c` 会尝试建 dma-buf（commit 8cfaf22668，11.0.1 已有）；device_feature 转发 = commit **e2358af583**（master，11.0.1 **无**）| ❌ 11.0.1 缺转发 |
| server（我们 firmware）| 自有 vfio-user server（非 libvfio-user），未实现 `VFIO_USER_DEVICE_FEATURE` 的 DMA_BUF GET | ❌ 缺实现 |

### 用户 fork `/home/xp/src/qemu-fork`（11.0.50，master-based，HEAD=vfio-user-win devdoc）的确切状态
- ✅ **已含 device_feature 转发**：`hw/vfio-user/device.c:78 vfio_user_device_io_device_feature` + `:476 .device_feature=…` 已挂进 `vfio_user_device_io_ops_sock`（泛化转发任意 feature，含 DMA_BUF）。即 e2358af583 已在此 fork。client 短路缺口**已闭合**。
- ❌ **但 dma-buf 仍被门控跳过**：`region.c:297` 的门 `if (!(io_ops->capabilities & VFIO_IO_CAP_DMA_BUF)) return;`，而 `vfio_user_device_io_ops_sock` **未设** `.capabilities = VFIO_IO_CAP_DMA_BUF`（默认 0）→ 对 vfio-user 设备**直接跳过 dma-buf 创建**（故 fork 下连 -22 都不会有，但也永远不建 dma-buf）。

### 净裁定（S7，源码级，反转并细化 S6 的"对象未定"）
让"vfio-user BAR 作 SQ-in-CMB 的 DMA target"成立，需 **(B client) + (C server) 叠加，排除 (A region-flag)**，具体到用户 fork 是**有界三步**（非从零 fork）：
1. **QEMU fork**：`vfio_user_device_io_ops_sock.capabilities |= VFIO_IO_CAP_DMA_BUF`（~1 行，过 region.c:297 门）。
2. **firmware server**：实现 `VFIO_USER_DEVICE_FEATURE` GET + DMA_BUF —— 把指定 BAR region 导出为 host vfio-pci 可当 dma-buf 的 fd（map 模式已有 memfd backing，正好可作 fd 来源）。
3. **真机验残留 (D)**：host `vfio_pci_dmabuf.c::validate_dmabuf_input` 是否肯为"无真实 PCI BAR 资源背书"的软件 vfio-user 设备导出 dma-buf —— 这是唯一**未坐实的承重假设**，须先 POC 再大改。
- **fork 方向 = 用 `/home/xp/src/qemu-fork`（已闭合 client 短路），加 1 行 capability，不从零开始。** 但 (D) 不验证就实现 server 侧 = 押未验证承重假设，违项目"先 POC"原则。

### S7 part-1（guest 侧 ftrace `pci_alloc_p2pmem` vs `virt_to_bus`）
进行中（子 agent）——定位 guest 回退在哪个内核函数，与 host-side dma-buf 缺口对账（reconcile 是 guest 自身 pool 问题，还是 host dma-buf 缺失的下游 guest-visible 后果）。

## S8 真根因定位 + 修复 + L4 完全达成（决定性，推翻 S2–S7 的 fork 方向）

### guest-ftrace（S7 part-1）暴露真根因 ①：firmware CMBSZ 位布局 bug
boot-time kretprobe 抓 `pci_alloc_p2pmem` **命中 0 次** → guest 根本没进 p2pmem 分支（判定 (c)：`cmb_use_sqes=false`）。根因：firmware `regs.rs::cmbsz` 常量**非 spec-aligned**：

| 字段 | NVMe spec §3.1.14 / Linux nvme.h / QEMU | firmware（错） |
|---|---|---|
| SQS | **bit 0** | bit 4 |
| CQS/LISTS/RDS/WDS | bit 1/2/3/4 | bit 5/6/7/8 |
| SZU | **bits 11:8** | bits 3:0 |

firmware 意图广告全能力却写出 `0x002001f0`，Linux 按 spec 解码成 **SQS=0** → 驱动拒绝 SQ-in-CMB。`cmbsz_field_offsets_match_spec` 测试**反而 enshrine 了错误布局**（断言 `SQS==1<<4`）——典型 self-consistent trap。

### 真根因 ②：CMBMSC 跨 Controller Reset 生命周期 bug
修 ① 后 guest 真把 SQ 放进 CMB（`Create IO SQ gpa=0xfe000000`），但 firmware `guest_read`→`cmb_hit` 返 **Miss** → `dma_read 0xfe000000 not in any DMA region` → 起不来。debug 日志坐实：
```
CMBMSC programmed cmse=true cba=0xfe000000   ← driver 启用 CMB
CC.EN 1→0 disabling                          ← Controller Reset
CC.EN 0→1 enabling                           ← 重新 enable，driver 不重编程 CMBMSC
Create IO SQ gpa=0xfe000000 → fetch cmb_hit Miss（cmse 被 reset 清了）
```
firmware `disable()`（CC.EN→0）**误清** CMBMSC 的 cre/cmse/cba。真 Linux `nvme_map_cmb` 仅编程一次（`if (dev->cmb_size) return` 守卫）、init 的 CC.EN 周期后不重编程，依赖 CMBMSC **跨 Controller Reset 持久**。**QEMU `nvme_ctrl_reset(NVME_RESET_CONTROLLER)` 同样不动 cmbmsc**（源码核对，interop 参考）。原"reset 清 CMBMSC"标注 `spec §3.1.24 + architect 复核 #4`——又一个 self-consistent trap（spec 误读 + architect review 都错，真驱动才暴露）。

### 修复（两处，spec + interop 对齐）
1. `regs.rs::cmbsz`：SQS→bit0…WDS→bit4、SZU→bits11:8；修 `cmbsz_field_offsets_match_spec` 测试断言正确布局 + 整字回归锚 `0x0020_001f`。
2. CMB 生命周期：`disable()`（Controller Reset CC.EN→0）**保留** CMBMSC；只 `reset()`（FLR/PCIe = Controller Level Reset）才清。改 `controller_reset_clears_cmb_enable_state`→`controller_reset_preserves_cmb_enable_state`。

### L4 真机结果：**trap + map 双模全 PASS**
```
=== PASS (L4,trap)：真 nvme 驱动真用 CMB + 真 IO，三 oracle 一致 ===
=== PASS (L4,map) ：真 nvme 驱动真用 CMB + 真 IO，三 oracle 一致 ===
oracle: guest IO PASS=True ; host backing marker=True ; CMB-USED=True
```
真 Linux nvme：SQS 识别 → cmb_use_sqes=true → CMBMSC 启用且跨 reset 持久 → SQ 落 CMB(0xfe000000) → firmware 从 CMB backing 取 SQE（CMB-RESIDENT-ACCESS fired）→ 真 write/read/flush 全程 PASS。

### 对 fork 问题的最终答案（推翻 S6/S7）
- **SQ-in-CMB 完全是 firmware 自身能力**：firmware（= NVMe controller）从**自己的** CMB backing 读 SQE，**不经** host DMA/IOMMU/dma-buf。S6/S7 的"vfio-user dma-buf 缺口 → 需 fork QEMU"是**追错了下游症状**——guest 根本没走到 dma-buf 那层（卡在更前的 CMBSZ.SQS 判定）。
- **不需要 fork QEMU，不需要 dma-buf**。两个 firmware 寄存器 bug 修完即真机打通。
- dma-buf 那条路（S7 三层缺口分析）**仅对"host DMA 引擎/别的设备直接 DMA 到 CMB BAR"（如 data-in-CMB 的 P2P）才相关**，与 SQ/CQ/PRP-in-CMB（firmware 自读自写 backing）正交。留作未来若做真 P2P-data 才需。

### 通用教训
- **self-consistent trap 连发两例**：wire 寄存器布局 + 寄存器生命周期，两端（firmware 编码/解码、firmware 测试、误读 spec、architect review）自洽 → 全绿，唯有真 host 驱动作独立 oracle 才暴露。凡"firmware 自定义 + 自测 + 自证 spec"的 wire 契约，必须有真对端或独立 oracle。
- **oracle 选择**：`map_addr_cmb`（误）→ `create_sq addr`（对）；`CMB-RESIDENT-ACCESS 缺失`（症状）→ guest-ftrace `pci_alloc_p2pmem 命中数`（根因）。症状层 oracle 会把人导向错误的上游归因（dma-buf/fork）。
- **追下游症状 vs 定位根因**：S2–S7 五轮在 dma-buf/fork 上深挖，根因却在最前一环（CMBSZ.SQS）。遇"驱动不按预期用某能力"，先 ftrace 驱动**第一个**决策分支，别从最深的失败处反推。
