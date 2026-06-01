# Phase R4 — Resizable BAR (RBAR) ADR

## 当前状态：未实现

`pcie_remote_nvme_userspace` 不通过 PCIe Resizable BAR Capability
(PCIe Spec Rev 4.0 § 7.8.5) 协商 BAR 大小。BAR0 固定 8 KiB（足够装
NVMe controller regs 0..0xFFF + doorbell strip 0x1000..0x1100，
IO_QUEUE_CAP=4 → 4×2×4=32 byte doorbell 用量）。

## 为什么没做

RBAR 是 **PCIe transport** 层特性，不是 NVMe controller 自身能决定。
完整实现需要四方协作：

### 1. `pcie_remote_protocol` 协议扩展

`CapabilityBlob` 需新增 RBAR cap (Cap ID 0x15) 字段，至少含：
- Number of Resizable BARs (1..6)
- per-BAR supported sizes bitmap (size 1 MiB..128 TiB, power-of-2)
- per-BAR current size

### 2. `pcie_remote_device` (VTL2) 协商

driver 写 PCI config space RBAR Control Register (cap+0x08) 时：
1. VTL2 解码 BAR Sizes 位字段
2. 检查目标 size 是否在 supported bitmap 内
3. 通过 vsock 通知 host 端 device "BAR 已 resize 到 X MiB"
4. host 端重新设置 BAR window

### 3. SDK `device.rs` 新 callback

`PcieDevice` trait 加 `fn on_bar_resize(&mut self, bar: u32, new_size: u64)`，
让 device 知道 BAR 重 size 后要重新 expose 不同 MMIO 窗口（如 CMB 暴露
的内存区扩大）。

### 4. NVMe controller 状态适配

`Namespace.mmap` 可能要重新 layout；CMB / PMR backing 需要重新分配。

## 何时该真做

RBAR 对 NVMe 教学示例的价值依赖于这些 future features：

| Feature | RBAR 必需？ | 说明 |
|---|---|---|
| **CMB (Controller Memory Buffer)** | 是 | 暴露 host RAM 给 driver 直接做 SQ/CQ/PRP 列表存储，避免 host↔guest DMA |
| **PMR (Persistent Memory Region)** | 是 | 暴露持久化内存区，driver 读写后保证 power-loss durable |
| **Boot Partition Memory Buffer Location (BPMBL)** | 部分 | 当前 BPMBL register 是 driver 提供的 guest GPA，没用 RBAR；但真做 BP image service 时若想暴露内置 boot image 则需 |

教学示例当前**未实现** CMB/PMR/真 BP，所以 RBAR 不在关键路径。一旦 P 系列
扩展到 CMB/PMR 真做 (`Phase S1+`)，本 ADR 升级为 implementation task。

## 教学价值的权衡

读者从本项目学到：
- ✅ NVMe 2.0 控制器协议（A→R 全覆盖）
- ✅ 用户态 PCIe device 实现模式 (vsock + DMA token + MMIO callback)
- ❌ PCIe transport capability negotiation (RBAR 是其中一个)

把 RBAR 留到将来 CMB/PMR 实现一起做，对教学清晰度更友好（一个 ADR
两个能力一起讲），而不是现在为 RBAR 单独 dive into PCIe spec § 7.8.5
增加心智负担。

## 决策

**Defer**：暂不实现 RBAR。如果将来加 CMB/PMR/真 BP image service，
那时同 PR 引入 RBAR cap 协商。
