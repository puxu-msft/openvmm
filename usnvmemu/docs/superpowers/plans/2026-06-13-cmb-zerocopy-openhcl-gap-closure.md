# 零拷贝 CMB-on-OpenHCL：补全缺口①②（POC-first 计划）

**日期**：2026-06-13
**目标**：把"firmware-in-VTL2 给 VTL0 guest 暴露零拷贝 CMB BAR"从两缺口未证路径，推进到真机打通（或坐实哪一步真不可行）。
**方法论**：plan → architect review → subagent-driven；**承重假设先 POC**（缺口②是承重，gate 缺口①）。
**前置事实**（已 file:line 核实，见 `experiments/2026-06-12-vtl-memory-direction-feasibility/` 与 CMB 设计 §5）：
- 缺口①：underhill 0 个 `MemoryMapper` 实现；VPCI relay 硬编码 `shared_mem_mapper: None`（`underhill_core/src/worker.rs:3540`）；`vfio_user_pci_device` BAR 是 `BarMemoryKind::Dummy`（纯 trap）。通用真-back 路径 `SharedMem`+`MemoryMapper` 对软件设备开放（virtio-pci 在用）但实现全在 host 侧。
- 缺口②：`mshv_vtl_low` 内核只覆盖 boot-time VTL0 RAM PFN（`MSHV_VTL_ADD_VTL0_MEMORY`←`mem_layout.ram()`）。

## 关键洞察（2026-06-13 源码自验 + architect 对抗复核后收窄）
GET `create_ram_gpa_range(slot, gpa_start, gpa_count, gpa_offset, flags)` 的语义是 **别名（alias）**，不是分配：
- 自验 `i440bx_host_pci_bridge.rs:202-207` + `:123/131/144/170`：`gpa_offset` 恒指向**已存在的 guest GPA**；flags 只有 `rom_mb`（写丢弃），**无 allocate 标志**。⇒ backing = gpa_offset 处已有内存。
- **architect 复核纠正"由构造成立"的过度自信**：i440bx 全部用例是 `gpa_start == gpa_offset`（或 ROM-shadow 分支 gpa_offset=start+rom_bios_offset，二者仍都落**低位已有 RAM**），**从未覆盖"gpa_start 在 BAR 空洞 + gpa_start≠gpa_offset"**。且开源唯一 host 实现（GED `guest_emulation_device/src/lib.rs:1038-1040`）对 create_ram_gpa_range **硬编码 FAILED**——别名语义对**我们需要的空洞异址用法无任何源码托底**，是真机第一手数据。
- 收窄后的准确表述：**gpa_offset 侧 backing 归属由生产用例锚定（点向 VTL0 RAM 即 firmware 可达）；但 gpa_start-in-hole 的别名是 Phase 1 才有第一手证据的真机维度。**

## Phase 0（设计期纯源码 gate — 已做，2026-06-13）
不需真机的承重假设先验，结果：
- **G1 trait 契约（architect 重点 4，最该先验）**：`MappedMemoryRegion::map(offset, section: &dyn AsMappableRef, …)` 收的是 **fd/section 对象**（`guestmem/src/lib.rs:2576-2599`，真实消费 `device_memory.rs:88-96`、`virtio_pmem:97-99`）；`create_ram_gpa_range` 收的是 **gpa_offset**，二者**不同构**。⇒ **缺口①不走 `MemoryMapper` 抽象**：CMB 的 backing 不是设备 fd，而是 firmware 经 mshv_vtl_low 映的 VTL0 RAM。正确形态 = 在 vfio_user device 的 **CMB BAR 构造处直接调 `create_ram_gpa_range`**（per-BAR），**不**把一个特化 mapper 接进 VPCI relay 的 per-device `shared_mem_mapper`（避免 blast radius：`worker.rs:3524-3540` 的 mapper 作用于该 relay 所有 VPCI 设备，`device_builder.rs:38` 是 per-device 非 per-BAR）。
- **G2 mshv_vtl_low 可写性（architect 重点 5#3）**：✅ **已证可写**。`poc3_mshv_vtl_low_probe.rs:63` + `openhcl-vtl2-deploy/client:77` 均 `PROT_READ|PROT_WRITE`；**生产** DMA 路径 `vfio_user_transport/src/dma.rs:64-65` 用 Rw region（PROT_READ|WRITE）写 guest buffer（NVMe read 命令）。W6c data plane 本就写 guest RAM。⇒ **CQ-in-CMB / data-write-in-CMB 的 firmware 写方向不被 mshv_vtl_low 阻**（peek POC 用 PROT_READ 只因它是只读探针，非机制限制）。

## Phase 1（缺口② POC — 承重 gate，architect 修正后 execution-ready）

**命题**：custom underhill 调 `create_ram_gpa_range`，验"空洞异址别名 + firmware 反手够到"。**architect 修正：递进三探针 + 动态双向 oracle + 先 dump memory map，否则单点失败无法归因。**

**步骤 0（选址）**：`ohcldiag-dev inspect vm/vtl0_memory_map` dump 当前内存图，选一段**确认为空的高 MMIO 洞**作 gpa_start 候选（实测该类 VM 主 RAM = `0x0-0xf8000000` + `0x1_0000_0000-0x1_0800_0000`，候选落两段之外）；gpa_start/gpa_offset/gpa_count 均 4K 对齐。

**递进三探针（逐级隔离失败维度）**：
- **探针 A（管路自检）**：复刻 i440bx 已知可行用法——`gpa_start == gpa_offset`、都在**低位已有 RAM**。SUCCESS ⇒ 探针管路 + GET client 通。
- **探针 B（高位等址）**：`gpa_start == gpa_offset`，移到**高位主 RAM**。隔离"是否只低位可用"。
- **探针 C（目标用法）**：`gpa_start` 在**步骤 0 选的空洞**、`gpa_offset` 指向 **gpa_allocator(`locked_memory_lower_vtl`/vtl0) 真分配的一段 VTL0 RAM**（**非**裸挑主 RAM——否则撞 guest 内存致假阴性，且选法对 Phase 2 无迁移价值）。

**oracle（动态双向，非静态预置 marker）**：
- ① 各探针 `CreateRamGpaRangeStatus::SUCCESS`（host 接受该 gpa_start/gpa_offset 组合；FAILED 时由 A/B/C 哪级失败决定性归因）。
- ② **别名生效 + 反手够到**：firmware 经 mshv_vtl_low 在 gpa_offset 写一个**运行时随机 token** → guest 立即在 gpa_start 读到**同一 token**（动态写后读，排除"host 拷贝一份"的假阳）。
- ③ **反向**：guest 写 gpa_start → firmware 在 gpa_offset 读到。
- ④ **可写性双向确认**（G2 已源码证，真机再钉）：firmware 写 → guest 读 **且** guest 写 → firmware 读，两向都过（CMB 需 SQ 读 + CQ 写双向）。

**Gate**：探针 C 的 ①②③④ 全 ✅ → 缺口② PROVEN → 进 Phase 2。C 在某 oracle 失败 + A/B 的对照 → 决定性定位是 host 拒空洞 gpa_start / 拒异址别名 / 别名不生效 / 不可写，按结论定 Phase 2 或坐实死路。

**环境**：WSL `cargo xflowey build-igvm x64` → Hyper-V VM（复用 pcie-remote-exp 或新建 OpenHCL VM，见 memory `hyperv-openhcl-vm-creation`）。

## Phase 2（缺口① — 条件于 Phase 1=✅，先 roadmap 后详化）

**详化 gate**：Phase 1 三探针确切返回值/约束出来后再详化。**G1 已定形态 = 直接别名，非 MemoryMapper**：
1. **CMB BAR 直接别名**：vfio_user device 在 CMB BAR 构造处（`cfg_space_emu` BAR kind），从 gpa_allocator 切 VTL0 RAM → 经 GET `create_ram_gpa_range`（gpa_start=guest 分配的 BAR GPA、gpa_offset=该 VTL0 RAM）别名进 guest。**只 CMB 那个 BAR/子区**；MSI-X、寄存器 BAR 仍 trap（`Dummy`）。**不接 VPCI relay 的 `shared_mem_mapper`**（避 blast radius）。
2. **slot 生命周期**：管一个独立 slot 分配器，与 underhill 其它 create_ram_gpa_range 用户（grep 确认 Gen2 无 i440bx，但需查 framebuffer 等）不冲突；BAR relocate/reset 时 `reset_ram_gpa_range`。
3. **并发前提**（adapt-proven-component 教训）：`create_ram_gpa_range` 是 async，BAR 构造/写触发路径是同步——需 `block_on`，**必须确认触发路径不在 GET 线程上**（i440bx `:230-235` 明示"绝不从 GET 线程调"），否则死锁。
4. **firmware backing 对齐**：firmware 侧 CMB backing 指向同一段 VTL0 RAM（经 mshv_vtl_low Rw 映射，G2 已证可写）。
5. **端到端**：真 Hyper-V guest → 真 Linux nvme 把 SQ 放进零拷贝 CMB → firmware 从 backing 取 SQE **无 VM-exit**。oracle：L4 三 oracle + 无 REGION_READ/WRITE 流量。

## 承重假设清单（架构复核后更新）
| # | 假设 | 状态 | 何时验 |
|---|---|---|---|
| **A** | host 接受 BAR-hole gpa_start + 异址 gpa_offset 别名 | ❌ **未证，无源码托底（GED stub FAILED）** | Phase 1 探针 C（**最可能翻车**）|
| **B** | gpa_allocator 切的 VTL0 RAM 在 mshv_vtl_low 注册段内 | ⚠️ 主 RAM 段实测在，allocator 切的具体段待确认 | Phase 1 顺带 |
| **C** | firmware 经 mshv_vtl_low 可**写** guest RAM | ✅ **已证**（poc3/vtl2-deploy PROT_WRITE + 生产 dma.rs Rw）| Phase 0 完成 |
| **D** | 缺口①走直接别名（非 MemoryMapper）| ✅ **已定**（G1 trait 不同构）| Phase 0 完成 |
| **E** | slot 资源不与 underhill 其它 GET-range 用户冲突 | ⚠️ 待 grep | Phase 2 |
| **F** | async create_ram_gpa_range 的 block_on 不在 GET 线程死锁 | ⚠️ 待验 | Phase 2 |
| **G** | 写序/缓存一致性（guest 写 SQE→firmware 立即按序见）| ⚠️ 待验 | Phase 1 oracle②/③ 顺带 |

## 非目标 / 边界
- 不碰 VTL2 私有内存暴露（已证死路）。
- 不改 trap 模式（已 L4 PASS，作为 fallback 永远保留）。
- CMB backing 用 host/VTL0 RAM 而非设备 SoC RAM，是相对真 NVMe CMB 的语义偏移（教学文档已注，CMB 设计 §10）。
