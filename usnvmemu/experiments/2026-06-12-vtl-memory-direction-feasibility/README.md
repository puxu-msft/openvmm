# VTL 内存共享方向可行性 —— guest 能否零拷贝直访 VTL2/host 内存

**日期**：2026-06-12
**类型**：可行性调查（代码研究 + 对抗性 architect review + 真机接口经验证实，**无须写探针**）
**触发**：探索"mmap 效果（零拷贝共享内存）在 host / VTL2 / VTL0 三方之间，哪些方向可行"。
先前已证**正向**（firmware-in-VTL2 经 `/dev/mshv_vtl_low` 零拷贝读写 guest RAM，W6c）；本调查
补完**反向**：让 VTL0 guest 零拷贝**直访一段 VTL2/firmware 自有的内存**（内存定义在 VTL2 侧、
guest 直接 load/store）是否可行。

## 裁定

**架构性不可行（对 VTL2 发起的反向）。** 三层独立证据互相印证，结论锁死。

> **VSM 内存共享是单向的，方向由特权所有权决定，不可对称**：
> - **VTL2 → VTL0 RAM**（高权方把低权方的页拉进自己空间）：✅ 有 ioctl + 已 PROVEN。
> - **VTL0 → VTL2 私有内存**（低权方直访高权方内存）：❌ **没有任何接口/原语**。
> - 谁拥有 VTL0 的 GPA→SPA 二级翻译表（SLAT），谁才能给 VTL0 加内存 backing —— 那是
>   **host / root partition**，**不是** VTL2、不是 firmware。

这与本项目第一问"host 够不到运行中 guest 的 RAM"（见
`../2026-06-11-openhcl-vfio-user-client-poc/`）是**同一堵特权墙的两面**：谁都越不过 VSM 边界，
唯一通的是"高权方 VTL2 反手读低权方 VTL0 的页"。

## 三层证据链

### 层 1 — 代码研究

| 断言 | 证据（file:line） |
|---|---|
| 全 hypercall 集**无生成性（加 backing）原语** | `vm/hv1/hvdef/src/lib.rs:706-755`：内存类只有 `HvCallMemoryMappedIoRead/Write`(0x0106/0x0107，转发)、`HvCallInstallIntercept`(0x004d，**装陷阱**，与映射相反)；无 `HvCallMapGpaPages` 等价物 |
| underhill 被授予的内存 hypercall 全是**限制性 / CVM 态** | `openhcl/underhill_mem/src/lib.rs:226-229`、`init.rs:638`：`ModifyVtlProtectionMask`（只能减权）/ `AcceptGpaPages` / `ModifySparseGpaPageHostVisibility` |
| `MemoryMapper::map_to_guest`（把 fd/内存零拷贝映进 guest GPA 的机制）实现**全在 host 后端** | trait：`vm/vmcore/guestmem/src/lib.rs:2565,2590`；真实后端：`openvmm/membacking/src/memory_manager/device_memory.rs:146`；impl 分布：`vmm_core/virt_whp`、`virt_kvm`、`virt_mshv`、`virt_hvf` |
| `openhcl/` 树里 `MemoryMapper` / `MappableGuestMemory` **0 实现** | grep 全 `openhcl/` 无匹配 |
| `vfio_user_pci_device` 纯 trap-and-forward（**佐证**，非独立证据） | `vm/devices/pci/vfio_user_pci_device/src/worker.rs:15`：guest_memory/ReadGpa/WriteGpa 已从路径删除 |

### 层 2 — architect 对抗性 review（5 个攻击向量逐一尝试推翻，全失败）

| 攻击向量 | 结果 | 关键证据 |
|---|---|---|
| A. `VtlMemoryMapper`（名字最像反例） | 误报 | `vmm_core/virt_whp/src/memory/vtl2_mapper.rs:1-9` 模块 doc 自述 "limited to **WHP** / remote mapping = Windows process" —— 是 **OpenVMM-on-WHP 模拟 VSM** 的 host 后端，非 real-Hyper-V/underhill |
| B. CVM host-visibility 把 VTL2 页变 VTL0 可见 | 失败，语义相反 | `hvdef/src/lib.rs:2123-2133`：host-visibility 只对 **host** 暴露，作用于 CVM guest 自己的页相对 host，与 VTL0 无关 |
| C. 别处授予了更多 hypercall / 漏看 `HvCallMapGpaPages` | 失败（反而更强） | 主白名单 `openhcl/virt_mshv_vtl/src/lib.rs:1663-1692`（最全 13+3+2 项）逐项核对仍无加-backing 原语；该原语在 hvdef 全集压根不存在 |
| D. vmbus GPADL / GET 中介达成"VTL2 定义 + VTL0 直访" | 失败，恰证 framebuffer 本质 | framebuffer 在 OpenHCL **不调 `map_to_guest`**：走 `emuplat/framebuffer.rs:22` → host `handle_map_framebuffer`(`vm/devices/get/guest_emulation_device/src/lib.rs:982`)由 **host 完成映射**；VTL2 仅 `/dev/mshv_vtl_low` 开**只读**第二映射(`openhcl/underhill_core/src/lib.rs:85-101`) |
| E. VSM 语义"高权下放内存给低权直访"反例 | 失败 | VTL2 对 VTL0 的内存操作只有 `ModifyVtlProtectionMask`（减权）+ intercept；**根因 = VTL0 的 SLAT 归 host 拥有，VTL2 结构上无从写它**（所有权问题，不是策略授予，更不可绕过） |

### 层 3 — 真机接口经验证实（活 VM `pcie-remote-exp`，Running）

VTL2 的 mshv 设备面（实测）：
```
/dev/mshv  /dev/mshv_hvcall  /dev/mshv_sint  /dev/mshv_vtl_low  /dev/mshv_vtl_sidecar0
```
唯一的内存映射设备是 `mshv_vtl_low`（**正向**：VTL0 RAM 映进 VTL2）。

`/dev/mshv_vtl` 全 ioctl 子码表（`openhcl/hcl/src/ioctl.rs:353-378`）逐项核对：VP 寄存器、
RETURN_TO_LOWER_VTL、HVCALL（受白名单约束）、CVM 页态（PVALIDATE/RMPADJUST/RMPQUERY/TDCALL）、
TLB、中断 —— **无任何"把 VTL2 页注入 VTL0 GPA"的命令**。

**决定性一刀**：名字最像"建立映射"的 `MSHV_VTL_ADD_VTL0_MEMORY`(0x21)，doc 写死
（`openhcl/hcl/src/ioctl.rs:493-500`）：
> *"Adds the VTL0 memory as a ZONE_DEVICE memory (I/O) to support **DMA from the guest**"*

它把 **VTL0 的页拉进 VTL2**（正向 alias，`mshv_vtl_low` 的底座，调用方 `underhill_mem/src/lib.rs:78`），
**不是**把 VTL2 的页推进 VTL0。最后一个名字可疑的反例被排除。

## 唯一可行的形态 —— framebuffer 模式（构造性补充）

要"guest 零拷贝直访一段共享内存设备区"（CMB/PMR/framebuffer 式）唯一现实路径：

1. **backing 是 host 分配的 VTL0 侧 RAM**（不是 VTL2 私有内存）；
2. **由 host（经 GET）把它映进 VTL0 GPA**（framebuffer / assigned-BAR / virtio-shared-mem 都走这条）；
3. firmware（VTL2）再经 `/dev/mshv_vtl_low` 反手够到同一页 → 双方零拷贝共享。

即"设备共享内存 BAR"能做，但**内存本质落在 VTL0 侧、host 发起**，承重点是"host/GET 为 vfio-user
设备做 BAR 映射"（工程问题），**不在 VTL2 暴露私有内存**（架构禁区）。先例：
- `vm/devices/framebuffer/src/lib.rs:286`（VRAM 映进 guest，guest 直写像素）
- `vm/devices/pci/vfio_assigned_device/src/lib.rs:10,58,655`（assigned BAR 直映 guest GPA；`map_to_guest` 在 :655）

### 关键：framebuffer 不是孤例，`CreateRamGpaRange` 是通用原语（已生产在用）

GET 有一个**通用** HostRequest `CREATE_RAM_GPA_RANGE`(=28) / `RESET_RAM_GPA_RANGE`(=29)
（`vm/devices/get/get_protocol/src/lib.rs:159-160`），映射到 host 侧
`IVmGuestMemoryAccess::CreateRamGpaRange`（`guest_emulation_transport/src/process_loop.rs:272`；
client API `client.rs:717`）。语义 = **VTL2 经 GET 请求 host 在 guest GPA 里创建一段
host-backed、RAM-backed 的区**，guest 零拷贝直访。

- **不是 framebuffer 专用**：生产代码 `openhcl/underhill_core/src/emuplat/i440bx_host_pci_bridge.rs:202,207`
  —— **PCI host bridge** 用它给 option ROM / PCI RAM 区做 backing（`CreateRamGpaRangeFlags::with_rom_mb`，
  flags 见 `get_protocol/src/lib.rs:1788`）。"host 为 PCI 设备建 guest-可见 RAM 区"**已在跑**。
- **与单向定律一致**：bytes 由 host 分配、落在 guest GPA（非 VTL2 私有），guest 直访，firmware
  经 `mshv_vtl_low` 反手共享 → 双向零拷贝。是 framebuffer 模式的泛化，不违反定律。
- **诚实警示**：OpenVMM 自带的 `guest_emulation_device`（OpenVMM-hosted 测试用模拟 host）把
  `handle_create_ram_gpa_range` **硬编码返回 FAILED**（`vm/devices/get/guest_emulation_device/src/lib.rs`
  的 handler 体）；**真 Hyper-V** 的 GET 对端实现 `IVmGuestMemoryAccess`（故 client/protocol/i440bx
  生产用例都在）。即真机支持、OpenVMM 测试桩未实现。

### 真机确认（活 VM `pcie-remote-exp`，2026-06-12，ohcldiag-dev inspect）

- ✅ **VTL0-backed 内存池机制活着**：`inspect vm/get/gpa_allocator` 显示
  `backing.type="locked_memory_lower_vtl"`、`params.lower_vtl_policy="vtl0"` —— underhill 实际在用
  VTL0 侧内存池（"VTL2 取用 host/VTL0 侧内存"这半截**实测在跑**）。GET 通道活、版本 `NICKEL_REV2`
  （`CreateRamGpaRange`=28 是早立请求，远在该版本支持内）。
- ❌ **`CreateRamGpaRange` 本身在本 VM 未被触发**：`inspect vm/vtl0_memory_map` 只有两段裸 guest RAM
  （0x0-0xf8000000、0x1_0000_0000-0x1_0800_0000），无 host-created RAM GPA 区；chipset 无 i440bx host
  bridge（Gen2 OpenHCL 不用 PIIX4），无 framebuffer → 无任何用户触发它。故无法被动观测。
- **结论**：真机证实了**支撑半截**（VTL0 侧内存池 + GET 通道）；`CreateRamGpaRange` 本身的 SUCCESS
  需 **underhill 侧探针**主动调 GET client 才能实测（重活：自定义 underhill 构建/注入）。**但其可行性
  已被生产代码强锚定**——shipping OpenHCL 的 `i440bx_host_pci_bridge` 真调它并按 success 处理，若真
  Hyper-V 不实现 `IVmGuestMemoryAccess::CreateRamGpaRange`，该生产路径会坏。即"推断"实为"生产代码依赖"。

**离今天 vfio-user 的差距（2026-06-13 深挖纠正——原"只缺 wiring"判断偏乐观）**：`vfio_user_pci_device`
BAR 现为 trap-and-forward（`BarMemoryKind::Dummy`，注释明写"仅测试用、不背内存"，每访问 = 往返
firmware message）。要拿零拷贝共享区**不是"只缺 wiring"，而是两个比接线更深的缺口**：

- **缺口①（造一个不存在的新组件）**：通用真-back 路径 `BarMemoryKind::SharedMem` + `MemoryMapper`
  **对软件设备开放**（virtio-pci 在用），但 `MemoryMapper` 实现**全在 host 侧**；`openhcl/` 树 0 个
  实现，且 underhill VPCI relay 硬编码 `shared_mem_mapper: None`（`underhill_core/src/worker.rs:3540`，
  正因此 assigned 设备在该路 `bail!("memory mapper is required")`）。`create_ram_gpa_range` **从未被
  包成 `MemoryMapper`**（只服务 i440bx framebuffer）。→ 需**新写**一个 "GET-backed `MemoryMapper`"
  （把 `new_region`/`map_to_guest` 翻成 `create_ram_gpa_range`/`reset_ram_gpa_range` GET 调用）+ 把
  CMB BAR 从 `Dummy` 改 `SharedMem`。是造组件，非接现成线。
- **缺口②（源码定不了的闭源反手够到，默认 ❌）**：`mshv_vtl_low` 内核覆盖 = **boot-time VTL0 RAM
  PFN 段**（`MSHV_VTL_ADD_VTL0_MEMORY`，喂 `mem_layout.ram()`）；`create_ram_gpa_range`（运行时 slot
  28）**不重新注册**进去。生产唯一用例 i440bx 是**别名到已有 VTL0 RAM**（gpa_offset → 主 RAM 顶端
  ROM 镜像），**非新分配**——证了别名可达，却**揭穿** CMB 要的"host 新建 backing"前提。host 闭源的
  `IVmGuestMemoryAccess::CreateRamGpaRange` 给新区落 VTL0 池页（firmware 够得到 ✅）还是 host 私有页
  （够不到 ❌ → 退化纯 framebuffer，firmware 取不到 SQE、对 CMB 无用），**源码层无法判定，需真机探针**。

**修正裁定**：CMB/shared-BAR 有现成 host 原语（`create_ram_gpa_range`）支撑、不是凭空发明（早前"CMB
无意义"判断的一半仍纠正）；但**零拷贝 CMB-on-OpenHCL 是两缺口的未证路径，缺口②是承重假设、源码定不了、
真机探针前应按 ❌ 默认**——不是"工程量、非禁区"那么轻。

## 对 firmware 的净意义

- firmware-in-VTL2 **暴露自有内存给 guest 零拷贝直访 = 死路**，勿在此设计任何特性。
- zero-copy 共享只能经 **VTL0 侧的页**，由 VTL2 反手够下去 —— 这对 NVMe data plane 已够用且
  是唯一能用的（正是 W6c DMA 零拷贝的形态）。
- 若将来要"共享内存设备区"，按 framebuffer 模式落地（host/GET 映 VTL0-RAM BAR），**但先真机探针定
  缺口②（host 给 create_ram_gpa_range 落的页 firmware 经 mshv_vtl_low 够不够得到）**，再投缺口①的组件；
  不要赌 VTL2→VTL0 暴露，也不要把零拷贝 CMB 当"只缺 wiring"。
