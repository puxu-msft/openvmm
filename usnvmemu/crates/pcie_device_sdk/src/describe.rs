// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase W1** — transport-neutral 设备描述模型。
//!
//! 这是 SDK 自有的中立类型，**零 wire 依赖**（不 import `pcie_remote_protocol`）。
//! 各 transport adapter 负责把它转成自家 wire 形态：
//! - openhcl：[`crate::run`] handshake 处转 `pcie_remote_protocol::DeviceDescribe`
//!   （HelloAck 内发给 VTL2，VTL2 据此为 guest 合成 PCI config space）。
//! - vfio-user：`vfio_user_transport` 据此回 `GET_REGION_INFO` / `GET_IRQ_INFO`，
//!   并用 [`DeviceDescribe::config_space`] 合成 PCI Type 0 config header 服务
//!   CONFIG region 读（让 guest enumerate 到正确 vendor/device ID）。
//!
//! **设计动机**（ADR-010 / Phase W）：Phase T 让 `PcieDevice::describe()` 直接返
//! `pcie_remote_protocol::DeviceDescribe`（一个 OpenHCL wire 类型），导致 vfio-user
//! 等 adapter 被迫 import wire crate、且另起 `Regions` trait 重复描述同一份 BAR/MSI-X
//! 信息（"描述模型分叉"）。W1 用本中立类型作为**唯一真相源**，消除分叉。

/// PCI BAR（Base Address Register）类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BarKind {
    /// 32 位 MMIO BAR（占 1 个 BAR slot）。
    #[default]
    Mmio32,
    /// 64 位 MMIO BAR（占 2 个 BAR slot：本 index + 下一个）。
    Mmio64,
}

/// 单个 BAR 的布局描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarLayout {
    /// BAR 索引 0..=5。
    pub index: u8,
    /// BAR 字节数（须为 2 的幂；PCI 要求 power-of-two 对齐）。
    pub size: u64,
    /// 32 / 64 位。
    pub kind: BarKind,
    /// prefetchable 位（cfg space BAR 低位 bit3）。
    pub prefetchable: bool,
}

/// 一条 PCI capability（出现在 config space capability list 链表中）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    /// Capability ID（如 MSI-X = 0x11）。出现在 cap 结构第 0 字节。
    pub cap_id: u8,
    /// Cap 结构在 cap_id + next_ptr 两字节**之后**的原始字节（adapter
    /// 负责拼 cap_id / next_ptr 链表指针，device 只提供 body）。
    pub raw: Vec<u8>,
}

/// 设备的静态描述：PCI identity + BAR 布局 + MSI-X + capability list。
///
/// 这是 [`crate::PcieDevice::describe`] 的返回类型，是所有 transport adapter
/// 描述设备的**唯一真相源**。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceDescribe {
    /// PCI Vendor ID（config space offset 0x00，16 位）。
    pub vendor_id: u16,
    /// PCI Device ID（offset 0x02，16 位）。
    pub device_id: u16,
    /// Class code（offset 0x09..0x0C，24 位：base/sub/prog-if）。
    pub class_code: u32,
    /// Revision ID（offset 0x08，8 位）。
    pub revision: u8,
    /// Subsystem Vendor ID（offset 0x2C，16 位）。
    pub subsystem_vendor: u16,
    /// Subsystem ID（offset 0x2E，16 位）。
    pub subsystem_device: u16,
    /// BAR 布局（最多 6 个，64 位 BAR 占 2 slot）。
    pub bars: Vec<BarLayout>,
    /// MSI-X 向量数（0 = 无 MSI-X）。
    pub msix_count: u32,
    /// Capability list（adapter 据此拼 config space cap 链表）。
    pub capabilities: Vec<Capability>,
    /// guest 写这些 cfg offset 时 adapter 应回调 `cfg_write_side_effect`。
    pub cfg_write_side_effect_offsets: Vec<u32>,
}

/// PCI Type 0 configuration space header 字段偏移（spec PCI 3.0 § 6.1）。
/// 公开常量让 adapter（vfio-user）与单测都锚定同一 layout，杜绝手算漂移。
pub mod cfg_offset {
    /// Vendor ID（u16）。
    pub const VENDOR_ID: usize = 0x00;
    /// Device ID（u16）。
    pub const DEVICE_ID: usize = 0x02;
    /// Command（u16，RW，guest 写使能 mem/busmaster）。
    pub const COMMAND: usize = 0x04;
    /// Status（u16；bit4 = Capabilities List）。
    pub const STATUS: usize = 0x06;
    /// Revision ID（u8）。
    pub const REVISION: usize = 0x08;
    /// Class code 起始（u24：prog-if @0x09 / subclass @0x0A / base @0x0B）。
    pub const CLASS_CODE: usize = 0x09;
    /// Header Type（u8；0x00 = single-function Type 0）。
    pub const HEADER_TYPE: usize = 0x0E;
    /// BAR0 起始（6 × u32）。
    pub const BAR0: usize = 0x10;
    /// Subsystem Vendor ID（u16）。
    pub const SUBSYS_VENDOR: usize = 0x2C;
    /// Subsystem ID（u16）。
    pub const SUBSYS_ID: usize = 0x2E;
    /// Capabilities Pointer（u8；指向第一条 cap）。
    pub const CAP_PTR: usize = 0x34;
    /// Status register 的 Capabilities-List 位。
    pub const STATUS_CAP_LIST: u16 = 0x0010;
    /// 第一条 capability 放置偏移（standard header 之后，DWORD 对齐）。
    pub const FIRST_CAP: usize = 0x40;
    /// PCIe extended config space 大小（4 KiB）。
    pub const SIZE: usize = 4096;
}

impl DeviceDescribe {
    /// 合成 PCI Type 0 configuration space（4 KiB extended，未用字节补 0）。
    ///
    /// host-side adapter（vfio-user）用它服务 guest 的 CONFIG region 读，
    /// 让 guest 驱动 enumerate 到正确的 vendor/device/class/subsystem +
    /// capability 链表。openhcl 路径不调本函数（VTL2 shim 自行据 DeviceDescribe
    /// 各字段合成 cfg space）。
    ///
    /// **范围**：本函数产出 *初始* 静态 config bytes。BAR base 为 0（guest
    /// 编程前），BAR 低位编码 type/prefetch 供 guest 识别；BAR size-probe
    /// （写全 1 回读 size mask）是有状态行为，由 adapter 的 config-space
    /// 状态机处理，不在本静态合成内。
    pub fn config_space(&self) -> Vec<u8> {
        use cfg_offset as o;
        let mut cfg = vec![0u8; o::SIZE];

        cfg[o::VENDOR_ID..o::VENDOR_ID + 2].copy_from_slice(&self.vendor_id.to_le_bytes());
        cfg[o::DEVICE_ID..o::DEVICE_ID + 2].copy_from_slice(&self.device_id.to_le_bytes());
        cfg[o::REVISION] = self.revision;

        // Class code：24 位拆 prog-if / subclass / base（小端布于 0x09..0x0C）。
        let cc = self.class_code & 0x00FF_FFFF;
        cfg[o::CLASS_CODE] = (cc & 0xFF) as u8; // prog IF
        cfg[o::CLASS_CODE + 1] = ((cc >> 8) & 0xFF) as u8; // subclass
        cfg[o::CLASS_CODE + 2] = ((cc >> 16) & 0xFF) as u8; // base class

        cfg[o::HEADER_TYPE] = 0x00; // Type 0, single-function

        // BARs：base=0，低位编码 memory/type/prefetch（PCI spec § 6.2.5.1）。
        // bit0=0 memory；bits[2:1] type(00=32 / 10=64)；bit3 prefetchable。
        for bar in &self.bars {
            let off = o::BAR0 + (bar.index as usize) * 4;
            if off + 4 > o::SIZE {
                continue;
            }
            let type_field: u32 = match bar.kind {
                BarKind::Mmio32 => 0b00,
                BarKind::Mmio64 => 0b10,
            };
            let low = (type_field << 1) | if bar.prefetchable { 0b1000 } else { 0 };
            cfg[off..off + 4].copy_from_slice(&low.to_le_bytes());
            // 64 位 BAR 的高 dword（下一 slot）保持 0。
        }

        cfg[o::SUBSYS_VENDOR..o::SUBSYS_VENDOR + 2]
            .copy_from_slice(&self.subsystem_vendor.to_le_bytes());
        cfg[o::SUBSYS_ID..o::SUBSYS_ID + 2].copy_from_slice(&self.subsystem_device.to_le_bytes());

        // Capability list：置 Status cap 位 + cap pointer，链式写入各 cap。
        if !self.capabilities.is_empty() {
            let status =
                u16::from_le_bytes([cfg[o::STATUS], cfg[o::STATUS + 1]]) | o::STATUS_CAP_LIST;
            cfg[o::STATUS..o::STATUS + 2].copy_from_slice(&status.to_le_bytes());
            cfg[o::CAP_PTR] = o::FIRST_CAP as u8;

            let mut cap_off = o::FIRST_CAP;
            let n = self.capabilities.len();
            for (i, cap) in self.capabilities.iter().enumerate() {
                // 一条 cap = cap_id(1) + next_ptr(1) + raw(body)。
                let end = cap_off + 2 + cap.raw.len();
                if end > o::SIZE {
                    break; // 防越界：cap 太多/太大则截断（教学版不期望发生）。
                }
                cfg[cap_off] = cap.cap_id;
                let next = if i + 1 < n {
                    end.next_multiple_of(4) // 下一 cap DWORD 对齐
                } else {
                    0 // 链尾
                };
                cfg[cap_off + 1] = next as u8;
                cfg[cap_off + 2..end].copy_from_slice(&cap.raw);
                if next == 0 {
                    break;
                }
                cap_off = next;
            }
        }

        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 中立类型构造 + 字段保真（防 BarKind Default 漂移）。
    #[test]
    fn device_describe_construct_and_defaults() {
        let d = DeviceDescribe {
            vendor_id: 0x1414,
            device_id: 0xc0de,
            class_code: 0x01_08_02,
            revision: 1,
            subsystem_vendor: 0x1414,
            subsystem_device: 0,
            bars: vec![BarLayout {
                index: 0,
                size: 8192,
                kind: BarKind::Mmio32,
                prefetchable: false,
            }],
            msix_count: 4,
            capabilities: vec![],
            cfg_write_side_effect_offsets: vec![],
        };
        assert_eq!(d.vendor_id, 0x1414);
        assert_eq!(d.bars[0].kind, BarKind::Mmio32);
        // Default BarKind = Mmio32（cfg space 默认 32 位 BAR，省 slot）。
        assert_eq!(BarKind::default(), BarKind::Mmio32);
        // DeviceDescribe::default() 给 CaptureDevice 等 test fixture 用。
        assert_eq!(DeviceDescribe::default().msix_count, 0);
    }

    /// config_space：identity / class / subsystem 落在 PCI spec 偏移。
    #[test]
    fn config_space_identity_at_spec_offsets() {
        use cfg_offset as o;
        let d = DeviceDescribe {
            vendor_id: 0x1414,
            device_id: 0xc0de,
            class_code: 0x01_08_02, // base 01 / sub 08 / prog-if 02
            revision: 0x07,
            subsystem_vendor: 0xABCD,
            subsystem_device: 0x1234,
            ..Default::default()
        };
        let cfg = d.config_space();
        assert_eq!(cfg.len(), o::SIZE);
        assert_eq!(
            &cfg[o::VENDOR_ID..o::VENDOR_ID + 2],
            &0x1414u16.to_le_bytes()
        );
        assert_eq!(
            &cfg[o::DEVICE_ID..o::DEVICE_ID + 2],
            &0xc0deu16.to_le_bytes()
        );
        assert_eq!(cfg[o::REVISION], 0x07);
        // class code little-endian: prog-if @0x09, subclass @0x0A, base @0x0B
        assert_eq!(cfg[o::CLASS_CODE], 0x02);
        assert_eq!(cfg[o::CLASS_CODE + 1], 0x08);
        assert_eq!(cfg[o::CLASS_CODE + 2], 0x01);
        assert_eq!(cfg[o::HEADER_TYPE], 0x00);
        assert_eq!(
            &cfg[o::SUBSYS_VENDOR..o::SUBSYS_VENDOR + 2],
            &0xABCDu16.to_le_bytes()
        );
        assert_eq!(
            &cfg[o::SUBSYS_ID..o::SUBSYS_ID + 2],
            &0x1234u16.to_le_bytes()
        );
        // 无 cap → status cap 位清零，cap pointer = 0
        let status = u16::from_le_bytes([cfg[o::STATUS], cfg[o::STATUS + 1]]);
        assert_eq!(status & o::STATUS_CAP_LIST, 0);
        assert_eq!(cfg[o::CAP_PTR], 0);
    }

    /// config_space：BAR 低位按 32/64 + prefetch 编码（base=0）。
    #[test]
    fn config_space_bar_low_bits_encode_type() {
        use cfg_offset as o;
        let d = DeviceDescribe {
            bars: vec![
                BarLayout {
                    index: 0,
                    size: 8192,
                    kind: BarKind::Mmio32,
                    prefetchable: false,
                },
                BarLayout {
                    index: 2,
                    size: 65536,
                    kind: BarKind::Mmio64,
                    prefetchable: true,
                },
            ],
            ..Default::default()
        };
        let cfg = d.config_space();
        // BAR0：32 位 non-prefetch → 低 dword = 0
        let bar0 = u32::from_le_bytes(cfg[o::BAR0..o::BAR0 + 4].try_into().unwrap());
        assert_eq!(bar0, 0x0);
        // BAR2：64 位 (bits[2:1]=10 → 0x4) + prefetch (bit3 → 0x8) = 0xC
        let bar2 = u32::from_le_bytes(cfg[o::BAR0 + 8..o::BAR0 + 12].try_into().unwrap());
        assert_eq!(bar2, 0xC);
        // 64 位 BAR 的高 dword (BAR3 slot) = 0
        let bar3 = u32::from_le_bytes(cfg[o::BAR0 + 12..o::BAR0 + 16].try_into().unwrap());
        assert_eq!(bar3, 0x0);
    }

    /// config_space：capability 链表 — status 位 + cap ptr + cap_id/next 链。
    #[test]
    fn config_space_capability_chain() {
        use cfg_offset as o;
        let d = DeviceDescribe {
            // 两条 cap：MSI-X(0x11, body 10B) + PM(0x01, body 6B)
            capabilities: vec![
                Capability {
                    cap_id: 0x11,
                    raw: vec![0xAA; 10],
                },
                Capability {
                    cap_id: 0x01,
                    raw: vec![0xBB; 6],
                },
            ],
            ..Default::default()
        };
        let cfg = d.config_space();
        let status = u16::from_le_bytes([cfg[o::STATUS], cfg[o::STATUS + 1]]);
        assert_ne!(status & o::STATUS_CAP_LIST, 0, "Capabilities List 位应置");
        // cap pointer → 第一条 cap
        let cap1 = cfg[o::CAP_PTR] as usize;
        assert_eq!(cap1, o::FIRST_CAP);
        assert_eq!(cfg[cap1], 0x11); // cap_id MSI-X
        let next = cfg[cap1 + 1] as usize;
        assert_ne!(next, 0, "首 cap next 指针非 0");
        assert_eq!(next % 4, 0, "下一 cap DWORD 对齐");
        // body 透传
        assert_eq!(&cfg[cap1 + 2..cap1 + 12], &[0xAA; 10]);
        // 第二条 cap
        assert_eq!(cfg[next], 0x01); // PM cap_id
        assert_eq!(cfg[next + 1], 0, "末 cap next = 0 (链尾)");
        assert_eq!(&cfg[next + 2..next + 8], &[0xBB; 6]);
    }
}
