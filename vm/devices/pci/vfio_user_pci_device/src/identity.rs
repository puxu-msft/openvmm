// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! 声明 geometry（usnvmemu 实际固定几何）+ identity 校验（决策 b）。
//!
//! usnvmemu identity 实际固定，故声明默认 hardcode；CLI 可 override（决策 c）。
//! 连接器每次连上读真 identity 比对：vendor/class drift → log+继续 Live；
//! **actual BAR0/MSI-X > declared → 拒绝（stay-Lost）**，防 guest 拿到半映射控制器。

use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;

/// usnvmemu 的固定 PCI 几何默认值（与 firmware 一起维护）。
pub const DEFAULT_VENDOR_ID: u16 = 0x1414;
/// NVMe class：base 0x01 / sub 0x08 / prog_if 0x02。
pub const DEFAULT_CLASS_BASE: u8 = 0x01;
/// NVMe sub-class（Mass Storage / NVM）。
pub const DEFAULT_CLASS_SUB: u8 = 0x08;
/// NVMe programming interface（NVM Express）。
pub const DEFAULT_CLASS_PROGIF: u8 = 0x02;
/// usnvmemu 实际 BAR0 NVMe 寄存器窗口。
///
/// 校准源（2026-06-12，A1.3）：`usnvmemu/crates/nvme_firmware/src/regs.rs`
/// `pub const BAR0_SIZE: u64 = 16 * 1024;`——16 KiB 容纳 controller regs
/// (0..0x1000) + doorbell strip (0x1000..0x2000) + MSI-X table (0x2000) +
/// MSI-X PBA (0x3000)。firmware `describe()`（controller/mod.rs）把
/// `bars[0].size = BAR0_SIZE` 报给 vfio-user region info，连接器读到的就是此值。
/// （早期 MockDev 测试用 8192 是过时 fixture，非真服务值。）
pub const DEFAULT_BAR0_SIZE: u64 = 16 * 1024;
/// usnvmemu 实际 MSI-X 向量数。
///
/// 校准源（2026-06-12，A1.3）：`NvmeController::open()`（controller/mod.rs）
/// 固定 `msix_count: 4`（admin vec 0 + IO vec 1 + 2 spare）；`--vfio-user-sock`
/// 服务路径（nvme_firmware/src/main.rs `run_vfio_user`）只调 `open()` + 几个
/// 不触碰 msix_count 的 setter，故真服务值就是 4。`describe().msix_count`
/// 经 vfio-user `GET_IRQ_INFO` 报给连接器。
/// （早期 MockDev 测试用 8 是过时 fixture，非真服务值。）
pub const DEFAULT_MSIX_COUNT: u16 = 4;

/// underhill 声明给 guest 的几何（默认 + 可被 CLI override）。
#[derive(Clone, Copy, Debug)]
pub struct DeclaredGeometry {
    /// 声明给 guest 的 BAR0 窗口字节大小。
    pub bar0_size: u64,
    /// 声明给 guest 的 MSI-X 向量数。
    pub msix_count: u16,
}

impl DeclaredGeometry {
    /// 用 CLI override（None=用默认）构造。
    pub fn new(bar0_override: Option<u64>, msix_override: Option<u16>) -> Self {
        Self {
            bar0_size: bar0_override.unwrap_or(DEFAULT_BAR0_SIZE),
            msix_count: msix_override.unwrap_or(DEFAULT_MSIX_COUNT),
        }
    }
}

/// usnvmemu 真报的几何（连接器读 region/irq info 得到）。
#[derive(Clone, Copy, Debug)]
pub struct ActualGeometry {
    /// usnvmemu region info 报的真 BAR0 窗口字节大小。
    pub bar0_size: u64,
    /// usnvmemu irq info 报的真 MSI-X 向量数。
    pub msix_count: u16,
}

/// 校验结果。
#[derive(Debug, PartialEq, Eq)]
pub enum IdentityCheck {
    /// 可转 Live（actual ≤ declared）。
    Ok,
    /// actual 超 declared → 拒绝，stay-Lost（决策 b）。
    Exceeds(&'static str),
}

/// 决策 b：actual BAR0 size 或 MSI-X count 超 declared → Exceeds（拒绝）。actual ≤ declared → Ok。
pub fn validate_identity(declared: &DeclaredGeometry, actual: &ActualGeometry) -> IdentityCheck {
    if actual.bar0_size > declared.bar0_size {
        return IdentityCheck::Exceeds("BAR0 size actual > declared");
    }
    if actual.msix_count > declared.msix_count {
        return IdentityCheck::Exceeds("MSI-X count actual > declared");
    }
    IdentityCheck::Ok
}

/// 用声明默认值构造 guest-facing HardwareIds（resolver 建 cfg_space 用）。
pub fn declared_hardware_ids() -> HardwareIds {
    HardwareIds {
        vendor_id: DEFAULT_VENDOR_ID,
        device_id: 0x00a9,
        revision_id: 1,
        prog_if: ProgrammingInterface::from(DEFAULT_CLASS_PROGIF),
        sub_class: Subclass::from(DEFAULT_CLASS_SUB),
        base_class: ClassCode::from(DEFAULT_CLASS_BASE),
        type0_sub_vendor_id: 0,
        type0_sub_system_id: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_declared_ok() {
        let d = DeclaredGeometry::new(None, None);
        let a = ActualGeometry {
            bar0_size: d.bar0_size,
            msix_count: d.msix_count,
        };
        assert_eq!(validate_identity(&d, &a), IdentityCheck::Ok);
        let a2 = ActualGeometry {
            bar0_size: d.bar0_size - 1,
            msix_count: d.msix_count - 1,
        };
        assert_eq!(validate_identity(&d, &a2), IdentityCheck::Ok);
    }

    #[test]
    fn bar0_exceeds_rejected() {
        // declared = usnvmemu 真几何（16 KiB / 4 vec）；actual 超 BAR0 → 拒绝。
        let d = DeclaredGeometry::new(Some(DEFAULT_BAR0_SIZE), Some(DEFAULT_MSIX_COUNT));
        let a = ActualGeometry {
            bar0_size: DEFAULT_BAR0_SIZE * 2,
            msix_count: DEFAULT_MSIX_COUNT,
        };
        assert!(matches!(
            validate_identity(&d, &a),
            IdentityCheck::Exceeds(_)
        ));
    }

    #[test]
    fn msix_exceeds_rejected() {
        // declared = usnvmemu 真几何；actual 超 MSI-X count → 拒绝。
        let d = DeclaredGeometry::new(Some(DEFAULT_BAR0_SIZE), Some(DEFAULT_MSIX_COUNT));
        let a = ActualGeometry {
            bar0_size: DEFAULT_BAR0_SIZE,
            msix_count: DEFAULT_MSIX_COUNT + 1,
        };
        assert!(matches!(
            validate_identity(&d, &a),
            IdentityCheck::Exceeds(_)
        ));
    }

    #[test]
    fn cli_override_applies() {
        let d = DeclaredGeometry::new(Some(16384), Some(4));
        assert_eq!(d.bar0_size, 16384);
        assert_eq!(d.msix_count, 4);
    }
}
