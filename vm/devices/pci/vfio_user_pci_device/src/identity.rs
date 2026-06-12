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

/// **CMB-P3b** — usnvmemu CMB 数据 BAR 的默认 BAR index（CMBLOC.BIR）。
///
/// 校准源（设计 §3 / firmware `regs.rs` `cmbloc` 默认 BIR=2）：firmware 的
/// CMB CLI `--cmb-bir` 默认用**独立 BAR2**（避开 BAR0 的 doorbell/MSI-X 副作用区）。
/// client 侧用同一默认作为「declared CMB BAR 槽位」上界——若 firmware 实报的
/// CMB BAR index 与此不一致，discovery 仍按实报 index 暴露（见
/// [`discover_cmb_geometry`]），此常量只决定**装配期**预留哪个 BAR 槽。
///
/// 注意：pci_core 把每个 BAR 都呈现为 64-bit（占两 slot），故 BAR0→slot0/1、
/// CMB(BIR=2)→slot2/3、MSI-X(BAR4)→slot4/5，三者天然不重叠。
///
/// 这是**配置了 CMB 的设备**默认采用的 BIR（见 [`DeclaredGeometry::with_cmb`]），
/// 也是装配期当前唯一支持的 BIR 槽（见 [`CmbBarDecision`]）。`cmb_bar_decision`
/// 与 `resolver.rs::build_device_shim` 的「受支持 BIR」判断都引用本常量——**单一
/// 真相源**，不再有独立的 `CMB_SUPPORTED_BIR=2` 字面量（**LOW-2** 消漂移）。
pub const DEFAULT_CMB_BIR: u8 = 2;
/// **CMB-P3b** — declared CMB BAR 窗口字节大小上界（identity 校验上界）。
///
/// 设计 §3 `--cmb-size` 默认 2 MiB。配置了 CMB 的设备据此**预留** CMB BAR 窗口
/// （assemble-always 期 backend 可能未连上、无法 discover，故用 declared 默认预留
/// 窗口，与 BAR0 同纪律）；reconnect 真连上后 discover 实报 size，校验
/// `actual ≤ declared`（决策 b）。
pub const DEFAULT_CMB_SIZE: u64 = 2 * 1024 * 1024;

/// **CMB-P3b** — 一个设备**声明**的 CMB 数据 BAR（仅当设备启用了 CMB 才存在）。
///
/// 装配期据此预留 BAR 槽（index=`bir`、window=`size`）；reconnect discover 实报后
/// 校验 `actual ≤ declared`（[`validate_cmb`]）。默认 NVMe 设备 / Layer C 热插拔
/// **不**带 CMB（`DeclaredGeometry::cmb == None`），故不会凭空多一个 phantom BAR。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CmbBarDecl {
    /// CMB 所在 BAR index（CMBLOC.BIR）。装配期据此选 `DeviceBars` 的 barN 槽。
    pub bir: u8,
    /// CMB BAR 窗口字节大小（identity 校验上界）。
    pub size: u64,
}

/// underhill 声明给 guest 的几何（默认 + 可被 CLI override）。
#[derive(Clone, Copy, Debug)]
pub struct DeclaredGeometry {
    /// 声明给 guest 的 BAR0 窗口字节大小。
    pub bar0_size: u64,
    /// 声明给 guest 的 MSI-X 向量数。
    pub msix_count: u16,
    /// **CMB-P3b** — 声明的 CMB 数据 BAR（`None` = 设备未配置 CMB → 装配期**不**预留
    /// CMB BAR，无 phantom BAR）。`Some` 时装配期据 [`CmbBarDecl`] 预留 BAR 槽，reconnect
    /// discover 校验实报 `≤ declared`。
    pub cmb: Option<CmbBarDecl>,
}

impl DeclaredGeometry {
    /// 用 CLI override（None=用默认）构造。**默认无 CMB BAR**（`cmb: None`）：
    /// 所有现有调用点（含 Layer C 热插拔、firmware 未启用 CMB 的默认 NVMe）行为
    /// 完全不变、guest 拓扑无 phantom BAR2（**MEDIUM-2 消回归**）。配置了 CMB 的
    /// 设备走 [`with_cmb`](Self::with_cmb) / [`new_with_cmb`](Self::new_with_cmb)。
    pub fn new(bar0_override: Option<u64>, msix_override: Option<u16>) -> Self {
        Self {
            bar0_size: bar0_override.unwrap_or(DEFAULT_BAR0_SIZE),
            msix_count: msix_override.unwrap_or(DEFAULT_MSIX_COUNT),
            cmb: None,
        }
    }

    /// **CMB-P3b** — builder：给设备声明一个 CMB 数据 BAR（`bir` 槽、`size` 窗口）。
    /// 配置了 CMB 的设备装配期据此预留 BAR 槽。当前装配仅支持 BIR=2
    /// （[`DEFAULT_CMB_BIR`]，见 [`CmbBarDecision`] / TODO(CMB-P5)）。
    pub fn with_cmb(mut self, bir: u8, size: u64) -> Self {
        self.cmb = Some(CmbBarDecl { bir, size });
        self
    }

    /// **CMB-P3b** — 便捷构造：带 CMB BAR 的设备（= `new(...).with_cmb(...)`）。
    /// P3b 阶段主要给**测试**用（显式装 CMB 设备）；P5 接 CLI/config（`--cmb-bir` /
    /// `--cmb-size`）。
    pub fn new_with_cmb(
        bar0_override: Option<u64>,
        msix_override: Option<u16>,
        bir: u8,
        size: u64,
    ) -> Self {
        Self::new(bar0_override, msix_override).with_cmb(bir, size)
    }

    /// **CMB-P3b** — 装配期对「是否 / 如何预留 CMB BAR 槽」的决策（纯函数，可单测）。
    ///
    /// `build_device_shim` 据此决定装不装 CMB BAR——把决策从带 IO 副作用的装配代码里
    /// 抽出来，使「非-CMB 设备无 BAR、CMB 设备有 BAR、不支持的 BIR 优雅拒绝」三条
    /// 行为可在不起 async 装配基建的前提下直接断言。
    pub fn cmb_bar_decision(&self) -> CmbBarDecision {
        match self.cmb {
            None => CmbBarDecision::None,
            // **MEDIUM-1 / TODO(CMB-P5)**：当前装配仅支持 BIR=2（`DeviceBars::bar2`）。
            // `DeviceBars` 只暴露 bar0/bar2/bar4 builder，且 BAR4 留给 MSI-X，故 BIR=2
            // 是唯一可用的 CMB 数据 BAR 槽。firmware `--cmb-bir` 可配 1/3/5，但要支持
            // 这些需先扩 `DeviceBars` 的 barN builder + P5 config 把 BIR 真正传进来。
            // 在此之前，BIR != 2 → `Unsupported`：调用方 warn + 不暴露（绝不 panic）。
            Some(CmbBarDecl { bir, .. }) if bir != DEFAULT_CMB_BIR => {
                CmbBarDecision::Unsupported { bir }
            }
            Some(CmbBarDecl { bir, size }) => CmbBarDecision::Reserve { bir, size },
        }
    }
}

/// **CMB-P3b** — 装配期 CMB BAR 决策（[`DeclaredGeometry::cmb_bar_decision`] 产出）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmbBarDecision {
    /// 设备未配置 CMB（`cmb == None`）→ **不**预留任何 CMB BAR（无 phantom BAR）。
    None,
    /// 配置了 CMB 且 BIR 受支持（=2）→ 在 `bir` 槽预留 `size` 字节窗口。
    Reserve {
        /// CMB BAR index（受支持时恒为 [`DEFAULT_CMB_BIR`]）。
        bir: u8,
        /// CMB BAR 窗口字节大小。
        size: u64,
    },
    /// 配置了 CMB 但 BIR 当前装配不支持（≠2）→ 优雅拒绝（调用方 warn + 不暴露）。
    /// **TODO(CMB-P5)**：扩 `DeviceBars` barN builder + P5 config 后支持 BIR=1/3/5。
    Unsupported {
        /// 不受支持的 BIR（来自 `--cmb-bir` 配置）。
        bir: u8,
    },
}

/// usnvmemu 真报的几何（连接器读 region/irq info 得到）。
#[derive(Clone, Copy, Debug)]
pub struct ActualGeometry {
    /// usnvmemu region info 报的真 BAR0 窗口字节大小。
    pub bar0_size: u64,
    /// usnvmemu irq info 报的真 MSI-X 向量数。
    pub msix_count: u16,
}

/// **CMB-P3b** — discover 出的 CMB 数据 BAR 几何（firmware 经 `describe()` →
/// server `GET_REGION_INFO` 实报）。`None` = firmware 未启用 CMB（无任何数据 BAR）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CmbGeometry {
    /// CMB 所在 BAR index（= firmware CMBLOC.BIR）。worker 把 guest 对该 BAR 的
    /// MMIO 经 `REGION_READ/WRITE(region=bir, ...)` 透传给 server（**非硬编码 bar0**）。
    pub bir: u8,
    /// CMB BAR 窗口字节大小（server 实报）。
    pub size: u64,
    /// server 是否在 region info 置了 `FLAG_MMAP`（map 模式，附 memfd）。
    ///
    /// **OpenHCL client 无 `MemoryMapper`**（underhill VTL2 设计性限制，设计 §1）：
    /// 即便 server 想走 map 模式（零拷贝），client 也**无法 mmap** 那个 region fd。
    /// 故 `wants_mmap == true` 时 client **优雅降级**为 trap（Intercept + REGION_RW）
    /// 并**日志告警**（见 `reconnect.rs`，对齐 silent-failure 纪律）；**绝不**尝试 mmap。
    pub wants_mmap: bool,
}

/// **CMB-P3b** — 从「逐 region 的 (index, flags, size) 实报」中识别 CMB 数据 BAR。
///
/// **发现方案及理由**（挑了「server-queried 几何发现」而非 resolver 配置参数）：
/// firmware 才是 CMB 启用/大小/BIR 的**单一真相源**（它经 `describe()` 把 CMB BAR
/// 报进 server region info）。client 若另设配置参数会与 firmware 形成两个真相源、易漂移
/// （W6b 架构本就**从 server discover** BAR0 size 而非硬编码，CMB 沿用同纪律）。
///
/// 识别规则：在候选 index 中，跳过 BAR0（NVMe 寄存器窗口）与 MSI-X BAR（本地 table/PBA，
/// 不转发 firmware），取**第一条** `size > 0` 且 `READ|WRITE` 的 region 作为 CMB BAR。
/// 教学版 firmware 只暴露一条数据 BAR（CMB），故「第一条」即唯一一条。
///
/// 入参 `regions`：`(index, flags, size)` 三元组迭代器（flags 用
/// `vfio_user_wire::proto::region_flags` 位）。`bar0_index` / `msix_bar_index` 是要跳过的
/// 两个保留 BAR index。返回 `None` = 没有这样的数据 BAR（CMB 未启用）。
pub fn discover_cmb_geometry(
    regions: impl IntoIterator<Item = (u8, u32, u64)>,
    bar0_index: u8,
    msix_bar_index: u8,
) -> Option<CmbGeometry> {
    use vfio_user_wire::proto::region_flags;

    for (index, flags, size) in regions {
        if index == bar0_index || index == msix_bar_index {
            continue;
        }
        if size == 0 {
            continue;
        }
        // CMB 数据 BAR 必须 R/W（firmware trap/map 两模都置 READ|WRITE）。
        let rw = region_flags::READ | region_flags::WRITE;
        if flags & rw != rw {
            continue;
        }
        return Some(CmbGeometry {
            bir: index,
            size,
            wants_mmap: flags & region_flags::MMAP != 0,
        });
    }
    None
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

/// **CMB-P3b** — 校验 discover 的 CMB 几何是否与 declared 预留的 BAR 槽/窗口相容。
///
/// 与 [`validate_identity`] 同纪律（决策 b）：配置了 CMB 的设备装配期按 declared
/// 预留 CMB BAR 槽（index=`bir`、window=`size`）。reconnect 真连上后：
/// - actual BIR 与 declared 预留槽**不一致** → 拒绝（guest 看到的 CMB BAR 在错误 slot，
///   CMBLOC.BIR 会指向没映射的 BAR）；
/// - actual size **超** declared 窗口 → 拒绝（半映射 CMB）。
///
/// **declared 无 CMB（`declared.cmb == None`）但 server discover 到 CMB**（actual=`Some`）：
/// **不报错**——本设备根本没预留 CMB BAR 槽（assemble 期已定死），故 server 报的 CMB
/// 无处暴露。返回 `Ok` 让设备照常到 Live；由调用方（reconnect.rs）记一行 **info** 日志
/// 「server 报告 CMB 但本设备未配置 CMB BAR，忽略」（对齐 silent-failure 纪律，不静默）。
///
/// `None` actual（firmware 未启用 CMB）→ 恒 `Ok`（无 CMB 要校验）。
pub fn validate_cmb(declared: &DeclaredGeometry, actual: Option<&CmbGeometry>) -> IdentityCheck {
    let Some(cmb) = actual else {
        return IdentityCheck::Ok;
    };
    // declared 未配置 CMB BAR：server 报的 CMB 无处暴露（BAR 槽未预留），不拒绝、不卡
    // Live；忽略由调用方记 info 日志。
    let Some(decl) = declared.cmb else {
        return IdentityCheck::Ok;
    };
    if cmb.bir != decl.bir {
        return IdentityCheck::Exceeds("CMB BIR actual != declared reserved slot");
    }
    if cmb.size > decl.size {
        return IdentityCheck::Exceeds("CMB size actual > declared");
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

    // ───────────────────────── CMB-P3b discovery ─────────────────────────

    use vfio_user_wire::proto::region_flags;

    const RW: u32 = region_flags::READ | region_flags::WRITE;

    /// **MEDIUM-2** — 默认 `new()` 设备**不**配置 CMB BAR（`cmb == None`）。这是消
    /// phantom-BAR2 回归的核心：所有现有调用点（Layer C 热插拔、默认 NVMe）走 `new()`，
    /// 故默认无 CMB BAR、guest 拓扑无凭空多出的 BAR2。
    #[test]
    fn new_has_no_cmb_by_default() {
        let d = DeclaredGeometry::new(None, None);
        assert_eq!(
            d.cmb, None,
            "默认设备不应声明 CMB BAR（消 phantom BAR2 回归）"
        );
        assert_eq!(
            d.cmb_bar_decision(),
            CmbBarDecision::None,
            "默认设备装配决策应为 None（不预留 CMB BAR）"
        );
    }

    /// **CMB-P3b** — `with_cmb` / `new_with_cmb` 给设备装上 BIR=2 的 CMB BAR →
    /// 装配决策 `Reserve{bir:2}`（设备会暴露 BAR2）。
    #[test]
    fn with_cmb_reserves_bar2() {
        let d = DeclaredGeometry::new(None, None).with_cmb(DEFAULT_CMB_BIR, DEFAULT_CMB_SIZE);
        assert_eq!(
            d.cmb,
            Some(CmbBarDecl {
                bir: DEFAULT_CMB_BIR,
                size: DEFAULT_CMB_SIZE
            })
        );
        assert_eq!(
            d.cmb_bar_decision(),
            CmbBarDecision::Reserve {
                bir: 2,
                size: DEFAULT_CMB_SIZE
            },
            "配置了 BIR=2 的 CMB → 预留 BAR2"
        );
        // new_with_cmb 等价。
        let d2 = DeclaredGeometry::new_with_cmb(None, None, DEFAULT_CMB_BIR, DEFAULT_CMB_SIZE);
        assert_eq!(d2.cmb, d.cmb);
    }

    /// **MEDIUM-1 / TODO(CMB-P5)** — BIR != 2 当前装配不支持 → `Unsupported`
    /// （调用方 warn + 不暴露，绝不 panic）。锁定 BIR=1/3/5 的优雅拒绝行为。
    #[test]
    fn cmb_unsupported_bir_is_rejected_gracefully() {
        for bir in [1u8, 3, 5] {
            let d = DeclaredGeometry::new(None, None).with_cmb(bir, DEFAULT_CMB_SIZE);
            assert_eq!(
                d.cmb_bar_decision(),
                CmbBarDecision::Unsupported { bir },
                "BIR={bir} 当前无对应 DeviceBars barN 槽 → Unsupported（不 panic）"
            );
        }
    }

    /// discover：trap 模式 CMB BAR（BIR=2, R/W, 无 MMAP）→ Some(bir=2, !wants_mmap)。
    /// BAR0(index0) 与 MSI-X(index4) 即便 size>0 也被跳过。
    #[test]
    fn discover_cmb_trap_mode() {
        let regions = vec![
            (0u8, RW, 16 * 1024u64), // BAR0：跳过
            (2u8, RW, 4096u64),      // CMB BAR：命中
            (4u8, RW, 4096u64),      // MSI-X BAR：跳过
        ];
        let cmb = discover_cmb_geometry(regions, 0, 4).expect("应发现 CMB BAR");
        assert_eq!(cmb.bir, 2);
        assert_eq!(cmb.size, 4096);
        assert!(!cmb.wants_mmap, "trap 模式无 MMAP flag");
    }

    /// discover：map 模式 CMB BAR（置 MMAP flag）→ wants_mmap = true（client 据此降级 + 告警）。
    #[test]
    fn discover_cmb_map_mode_sets_wants_mmap() {
        let regions = vec![
            (0u8, RW, 16 * 1024u64),
            (2u8, RW | region_flags::MMAP, 8192u64),
        ];
        let cmb = discover_cmb_geometry(regions, 0, 4).expect("应发现 CMB BAR");
        assert_eq!(cmb.bir, 2);
        assert!(
            cmb.wants_mmap,
            "map 模式应置 wants_mmap（client 降级 trap + 告警）"
        );
    }

    /// discover：firmware 未启用 CMB（只有 BAR0 + MSI-X，无数据 BAR）→ None。
    #[test]
    fn discover_cmb_absent_when_no_data_bar() {
        let regions = vec![(0u8, RW, 16 * 1024u64), (4u8, RW, 4096u64)];
        assert_eq!(discover_cmb_geometry(regions, 0, 4), None);
    }

    /// discover：非 R/W 的 region（如只读）不算 CMB 数据 BAR → 跳过。
    #[test]
    fn discover_cmb_skips_non_rw_region() {
        let regions = vec![
            (0u8, RW, 16 * 1024u64),
            (2u8, region_flags::READ, 4096u64), // 只读：不是 CMB
        ];
        assert_eq!(discover_cmb_geometry(regions, 0, 4), None);
    }

    /// discover：size==0 的 region（describe 未产出的 BAR slot）不算 → 跳过。
    #[test]
    fn discover_cmb_skips_zero_size() {
        let regions = vec![(0u8, RW, 16 * 1024u64), (2u8, RW, 0u64)];
        assert_eq!(discover_cmb_geometry(regions, 0, 4), None);
    }

    // ───────────────────────── CMB-P3b validate ─────────────────────────

    /// CMB 未启用（actual=None）→ 恒 Ok（无 CMB 要校验）。
    #[test]
    fn validate_cmb_none_is_ok() {
        let d = DeclaredGeometry::new(None, None);
        assert_eq!(validate_cmb(&d, None), IdentityCheck::Ok);
    }

    /// **MEDIUM-2** — declared 无 CMB（默认设备）但 server discover 到 CMB →
    /// **不拒绝**（返 Ok）：本设备未预留 CMB BAR 槽，server 报的 CMB 无处暴露，
    /// 忽略即可（调用方记 info）；绝不卡 Live。
    #[test]
    fn validate_cmb_declared_none_but_server_has_cmb_is_ok() {
        let d = DeclaredGeometry::new(None, None);
        let cmb = CmbGeometry {
            bir: 2,
            size: 4096,
            wants_mmap: false,
        };
        assert_eq!(
            validate_cmb(&d, Some(&cmb)),
            IdentityCheck::Ok,
            "未配置 CMB 的设备遇 server 报 CMB 应忽略（Ok），不卡 Live"
        );
    }

    /// actual CMB ≤ declared 槽/窗口 → Ok。
    #[test]
    fn validate_cmb_within_declared_ok() {
        let d = DeclaredGeometry::new_with_cmb(None, None, DEFAULT_CMB_BIR, DEFAULT_CMB_SIZE);
        let decl = d.cmb.expect("配置了 CMB");
        let cmb = CmbGeometry {
            bir: decl.bir,
            size: decl.size,
            wants_mmap: false,
        };
        assert_eq!(validate_cmb(&d, Some(&cmb)), IdentityCheck::Ok);
        let smaller = CmbGeometry {
            bir: decl.bir,
            size: 4096,
            wants_mmap: false,
        };
        assert_eq!(validate_cmb(&d, Some(&smaller)), IdentityCheck::Ok);
    }

    /// actual CMB BIR 与 declared 预留槽不一致 → 拒绝（CMBLOC.BIR 会指向空 slot）。
    #[test]
    fn validate_cmb_wrong_bir_rejected() {
        let d = DeclaredGeometry::new_with_cmb(None, None, DEFAULT_CMB_BIR, DEFAULT_CMB_SIZE);
        let decl = d.cmb.expect("配置了 CMB");
        let cmb = CmbGeometry {
            bir: decl.bir + 1,
            size: decl.size,
            wants_mmap: false,
        };
        assert!(matches!(
            validate_cmb(&d, Some(&cmb)),
            IdentityCheck::Exceeds(_)
        ));
    }

    /// actual CMB size 超 declared 窗口 → 拒绝（半映射 CMB）。
    #[test]
    fn validate_cmb_size_exceeds_rejected() {
        let d = DeclaredGeometry::new_with_cmb(None, None, DEFAULT_CMB_BIR, DEFAULT_CMB_SIZE);
        let decl = d.cmb.expect("配置了 CMB");
        let cmb = CmbGeometry {
            bir: decl.bir,
            size: decl.size * 2,
            wants_mmap: false,
        };
        assert!(matches!(
            validate_cmb(&d, Some(&cmb)),
            IdentityCheck::Exceeds(_)
        ));
    }
}
