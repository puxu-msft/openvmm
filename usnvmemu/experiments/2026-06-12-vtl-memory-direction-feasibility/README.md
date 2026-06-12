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

## 对 firmware 的净意义

- firmware-in-VTL2 **暴露自有内存给 guest 零拷贝直访 = 死路**，勿在此设计任何特性。
- zero-copy 共享只能经 **VTL0 侧的页**，由 VTL2 反手够下去 —— 这对 NVMe data plane 已够用且
  是唯一能用的（正是 W6c DMA 零拷贝的形态）。
- 若将来要"共享内存设备区"，按 framebuffer 模式落地（host/GET 映 VTL0-RAM BAR），不要赌
  VTL2→VTL0 暴露。
