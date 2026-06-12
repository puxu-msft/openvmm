// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **CMB（Controller Memory Buffer）backing 抽象** — `SharedRamRegion` trait。
//!
//! NVMe CMB 的本质是"设备把一段自己拥有的 RAM 经 BAR 暴露给 driver 直访"。本
//! domain core 只定义**这段 RAM 长什么样**（字节切片 + 长度），不定义它**怎么被
//! 暴露给 guest**（trap 转发 vs mmap 零拷贝）—— 那是各 transport adapter 的事。
//!
//! **设计裁定（architect 复核修订 #1）**：trait **不含 `gpa_base()`**。CMB 的
//! Controller Base Address（CBA）是 guest 经 CMBMSC 寄存器**编程进 firmware** 的
//! 状态，不是 backing 的固有属性。若把它放进 backing 抽象，map 模式下 client 映射
//! 的 region 与 firmware 记的 gpa_base 会成两个真相源。故 CBA↔offset 映射留在
//! firmware 的 `CmbState`，本 trait 只暴露纯粹的"一段可读写内存"。
//!
//! 本 crate 是 `#![forbid(unsafe_code)]`，故 trait 只定义接口；各 transport 的
//! 真实 backing（memfd / 匿名 mmap）在各自 crate 内实现。这里提供一个 `Vec<u8>`
//! backed 的中立实现 [`VecRamRegion`]，供测试与无 transport 场景使用。

/// 一段可被 firmware 与（map 模式下）guest 共享读写的 RAM 区域。
///
/// 实现者保证 [`as_bytes`](SharedRamRegion::as_bytes) /
/// [`as_bytes_mut`](SharedRamRegion::as_bytes_mut) 返回的切片长度恒等于
/// [`len`](SharedRamRegion::len)，且在 region 生命周期内地址稳定（map 模式依赖
/// 这一点：client 的 mmap 与 firmware 的本地访问指向同一物理页）。
pub trait SharedRamRegion {
    /// 只读访问整段 backing。切片长度 == [`len`](Self::len)。
    fn as_bytes(&self) -> &[u8];

    /// 可写访问整段 backing。切片长度 == [`len`](Self::len)。
    fn as_bytes_mut(&mut self) -> &mut [u8];

    /// backing 字节数。
    fn len(&self) -> usize;

    /// backing 是否为空（`len() == 0`）。clippy 要求与 `len` 成对出现。
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// **CMB-P4 (map 模式)** — 若本 backing 由一个**可被对端 mmap 的 fd** 支撑
    /// （如 memfd），返回它的 [`BorrowedFd`]；否则 `None`。
    ///
    /// 这是 map-based CMB 零拷贝的唯一承重点：transport 在 map 模式下把该 fd 经
    /// SCM_RIGHTS 附在 `GET_REGION_INFO` reply 里，client `mmap` 后 guest 零拷贝直访
    /// 同一物理页（server 经自己的 [`as_bytes`](Self::as_bytes) 看到同一内存）。
    ///
    /// **default `None`**：`#![forbid(unsafe_code)]` 的本 domain core 只定义这个**安全**
    /// 接口；纯内存 backing（如 [`VecRamRegion`]）无可共享 fd → 返 `None` → transport
    /// **降级 trap 模式**（不置 FLAG_MMAP、不附 fd），功能仍正确、只是非零拷贝。真正
    /// 能 mmap 的 backing（memfd）由各 transport adapter 在自己（允许 unsafe 的）crate
    /// 里实现并 override 本方法。
    fn as_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        None
    }
}

/// `Vec<u8>` backed 的中立 [`SharedRamRegion`]：测试 / 无 transport 时用。
///
/// 真 transport（vfio-user）会用 memfd-backed 实现替代它（既可被 firmware 本地
/// 读写，又可经 fd 暴露给 client mmap）；本实现仅提供进程内的等价语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VecRamRegion {
    bytes: Vec<u8>,
}

impl VecRamRegion {
    /// 分配一段 `len` 字节、全零的 backing。
    pub fn new(len: usize) -> Self {
        Self {
            bytes: vec![0u8; len],
        }
    }
}

impl SharedRamRegion for VecRamRegion {
    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_region_reports_len_and_zero_init() {
        let r = VecRamRegion::new(4096);
        assert_eq!(r.len(), 4096);
        assert!(!r.is_empty());
        assert!(r.as_bytes().iter().all(|&b| b == 0), "新 backing 应全零");
    }

    #[test]
    fn empty_region_is_empty() {
        let r = VecRamRegion::new(0);
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
    }

    #[test]
    fn mutation_through_as_bytes_mut_is_visible() {
        let mut r = VecRamRegion::new(8);
        r.as_bytes_mut()[3] = 0xAB;
        assert_eq!(r.as_bytes()[3], 0xAB, "写过的字节应可读回");
        // slice 长度恒等于 len()
        assert_eq!(r.as_bytes().len(), r.len());
        assert_eq!(r.as_bytes_mut().len(), 8);
    }

    /// trait object 安全性：CmbState 持 `Box<dyn SharedRamRegion>`，必须 object-safe。
    #[test]
    fn shared_ram_region_is_object_safe() {
        let mut boxed: Box<dyn SharedRamRegion> = Box::new(VecRamRegion::new(16));
        boxed.as_bytes_mut()[0] = 0x5A;
        assert_eq!(boxed.as_bytes()[0], 0x5A);
        assert_eq!(boxed.len(), 16);
    }

    /// **CMB-P4** — Vec backing（纯内存，无可共享 fd）的 `as_fd` 默认返 `None`，
    /// 故 transport 见 `None` 时降级 trap 模式（不 mmap）。这是降级路径的判据。
    #[test]
    fn vec_region_as_fd_is_none() {
        let r = VecRamRegion::new(4096);
        assert!(
            r.as_fd().is_none(),
            "Vec backing 无可 mmap 的 fd → None → 降级 trap"
        );
        let boxed: Box<dyn SharedRamRegion> = Box::new(VecRamRegion::new(8));
        assert!(boxed.as_fd().is_none(), "trait object 路径同样返 None");
    }
}
