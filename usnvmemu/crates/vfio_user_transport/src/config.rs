// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase W1** — host-side PCI configuration space 状态机。
//!
//! vfio-user 的 CONFIG region 读写必须由 host（本 server）服务：guest 驱动据此
//! enumerate 设备 identity / class / capability，并 probe BAR size。初始字节由中立
//! [`DeviceDescribe::config_space`] 合成；本状态机叠加 PCI config space 的 RW 语义：
//!
//! - **Command (0x04)**：RW（guest 写使能 memory / bus-master）。
//! - **BAR (0x10..0x28)**：base 地址位可写；写全 1 回读 size mask（size-probe）；
//!   低 4 位（type/prefetch）只读。
//! - **其余 header（vendor/device/class/subsystem/capability）**：只读，写忽略。
//!
//! 设计动机（ADR-010 / Phase W）：修复 vfio-user "cfg-space 未真路由" 硬 gap ——
//! 此前 CONFIG region 读写被误当 BAR MMIO 转给 `mmio_read`，guest 拿不到正确
//! vendor/device ID。

use pcie_device_sdk::BarKind;
use pcie_device_sdk::DeviceDescribe;
use pcie_device_sdk::describe::cfg_offset as o;

/// 单个 BAR dword slot 的写语义。
#[derive(Clone, Copy)]
struct BarSlot {
    /// 可写位 mask（base 地址位 = `~(size-1)`，低 4 位清零）。
    writable_mask: u32,
    /// 只读低位（type/prefetch），写后须强制保留。
    ro_low: u32,
}

/// host-side PCI config space + BAR 写语义状态机。
pub struct ConfigSpace {
    bytes: Vec<u8>,
    /// BAR0..5 各 dword 的写语义；`None` = 该 slot 非 BAR base（未用 / RO）。
    bar_slots: [Option<BarSlot>; 6],
}

impl ConfigSpace {
    /// config space 字节数（PCIe extended，4 KiB）。
    pub const SIZE: usize = o::SIZE;

    /// 从中立 [`DeviceDescribe`] 合成初始 config space + 推导各 BAR 写语义。
    pub fn new(d: &DeviceDescribe) -> Self {
        let bytes = d.config_space();
        let mut bar_slots: [Option<BarSlot>; 6] = [None; 6];
        for bar in &d.bars {
            let idx = bar.index as usize;
            if idx >= 6 {
                continue;
            }
            let size = bar.size.max(1);
            // PCI 约束：BAR size 须为 2 的幂；Mmio32 BAR 须 ≤ 4 GiB（否则 32 位
            // mask 截断无意义）。教学设备不应违反 —— debug 构建断言以早暴露 bug。
            debug_assert!(size.is_power_of_two(), "BAR size 须为 2 的幂");
            debug_assert!(
                !matches!(bar.kind, BarKind::Mmio32) || size <= (1u64 << 32),
                "32 位 BAR size 须 ≤ 4 GiB（否则 mask 截断）"
            );
            // low dword 可写 = base 位 ~(size-1)，低 4 位（type/prefetch）只读。
            let low_mask = (!(size.wrapping_sub(1)) as u32) & 0xFFFF_FFF0;
            let bar_off = o::BAR0 + idx * 4;
            let ro_low = u32::from_le_bytes(bytes[bar_off..bar_off + 4].try_into().unwrap()) & 0xF;
            bar_slots[idx] = Some(BarSlot {
                writable_mask: low_mask,
                ro_low,
            });
            // 64 位 BAR 占下一 slot：high dword 全 base 位可写（size ≤ 4 GiB 时
            // ~(size-1) 高 32 位全 1）。
            if matches!(bar.kind, BarKind::Mmio64) && idx + 1 < 6 {
                let high_mask = (!(size.wrapping_sub(1)) >> 32) as u32;
                bar_slots[idx + 1] = Some(BarSlot {
                    writable_mask: high_mask,
                    ro_low: 0,
                });
            }
        }
        Self { bytes, bar_slots }
    }

    /// 读 `size`（1/2/4/8）字节，小端组成 u64（越界字节读 0）。
    pub fn read(&self, offset: u64, size: u32) -> u64 {
        let off = offset as usize;
        let mut buf = [0u8; 8];
        for (i, b) in buf.iter_mut().enumerate().take(size as usize) {
            *b = self.bytes.get(off + i).copied().unwrap_or(0);
        }
        u64::from_le_bytes(buf)
    }

    /// 写 `size` 字节，按 PCI RW 语义过滤（BAR mask / Command RW / 其余 RO）。
    pub fn write(&mut self, offset: u64, size: u32, value: u64) {
        let off = offset as usize;
        let n = size as usize;
        if off + n > self.bytes.len() {
            return;
        }
        // BAR dword（base 地址 + size-probe）：仅接受 dword 对齐的 4 字节写。
        // **教学简化**：真实 HW 支持 byte-enable 的子 dword BAR 写；本教学版只处理
        // 4 字节写（guest 驱动 probe / 编程 BAR 的标准路径），其它粒度静默忽略。
        if (o::BAR0..o::BAR0 + 24).contains(&off) && off.is_multiple_of(4) && n == 4 {
            let slot = (off - o::BAR0) / 4;
            if let Some(bs) = self.bar_slots[slot] {
                let masked = ((value as u32) & bs.writable_mask) | bs.ro_low;
                self.bytes[off..off + 4].copy_from_slice(&masked.to_le_bytes());
            }
            // slot=None（未用 BAR）→ 忽略。
            return;
        }
        // Command (0x04, u16) RW；若 4 字节写含 Status 高半，Status 只读不存。
        if off == o::COMMAND && (n == 2 || n == 4) {
            self.bytes[o::COMMAND] = (value & 0xFF) as u8;
            self.bytes[o::COMMAND + 1] = ((value >> 8) & 0xFF) as u8;
        }
        // 其余字段只读：PCI 硬件对 RO 字段的写无效，静默忽略。
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcie_device_sdk::BarLayout;

    fn desc() -> DeviceDescribe {
        DeviceDescribe {
            vendor_id: 0x1414,
            device_id: 0xc0de,
            class_code: 0x01_08_02,
            revision: 1,
            subsystem_vendor: 0x1414,
            subsystem_device: 0,
            bars: vec![BarLayout {
                index: 0,
                size: 8192, // 0x2000
                kind: BarKind::Mmio32,
                prefetchable: false,
            }],
            msix_count: 4,
            capabilities: vec![],
            cfg_write_side_effect_offsets: vec![],
        }
    }

    /// guest 读 CONFIG 拿到正确 vendor/device ID（W1 acceptance 核心）。
    #[test]
    fn read_identity() {
        let cs = ConfigSpace::new(&desc());
        assert_eq!(cs.read(0x00, 2), 0x1414);
        assert_eq!(cs.read(0x02, 2), 0xc0de);
        // 一次 4 字节读 vendor+device 合并。
        assert_eq!(cs.read(0x00, 4), 0xc0de_1414);
    }

    /// Command 寄存器可写可回读；vendor ID（RO）写被忽略。
    #[test]
    fn command_rw_identity_ro() {
        let mut cs = ConfigSpace::new(&desc());
        cs.write(0x04, 2, 0x0006); // mem space + bus master
        assert_eq!(cs.read(0x04, 2), 0x0006);
        // 尝试改 vendor ID → 应被忽略（RO）。
        cs.write(0x00, 2, 0xDEAD);
        assert_eq!(cs.read(0x00, 2), 0x1414);
    }

    /// BAR0 size-probe：写全 1 回读 size mask（8 KiB → 0xFFFFE000）。
    #[test]
    fn bar0_size_probe() {
        let mut cs = ConfigSpace::new(&desc());
        // 初始 BAR0 = 0（base 未编程，32 位 non-prefetch 低位 0）。
        assert_eq!(cs.read(0x10, 4), 0x0);
        // 写全 1 → 回读 = ~(size-1) | type_low = 0xFFFFE000。
        cs.write(0x10, 4, 0xFFFF_FFFF);
        assert_eq!(cs.read(0x10, 4) as u32, 0xFFFF_E000);
        // guest 编程一个真实 base（对齐）→ 可回读（低位仍 RO=0）。
        cs.write(0x10, 4, 0xF000_0000);
        assert_eq!(cs.read(0x10, 4) as u32, 0xF000_0000);
    }

    /// 64 位 BAR：high dword 全可写（size-probe 回 0xFFFFFFFF）。
    #[test]
    fn bar_64bit_high_dword_probe() {
        let mut d = desc();
        d.bars = vec![BarLayout {
            index: 0,
            size: 65536, // 64 KiB
            kind: BarKind::Mmio64,
            prefetchable: true,
        }];
        let mut cs = ConfigSpace::new(&d);
        // low dword 低位 = 64bit(0x4) | prefetch(0x8) = 0xC
        assert_eq!(cs.read(0x10, 4) as u32 & 0xF, 0xC);
        cs.write(0x10, 4, 0xFFFF_FFFF); // low probe
        cs.write(0x14, 4, 0xFFFF_FFFF); // high probe
        // low = ~(0xFFFF) & ~0xF | 0xC = 0xFFFF0000 | 0xC
        assert_eq!(cs.read(0x10, 4) as u32, 0xFFFF_000C);
        // high = 全可写 → 0xFFFFFFFF
        assert_eq!(cs.read(0x14, 4) as u32, 0xFFFF_FFFF);
    }
}
