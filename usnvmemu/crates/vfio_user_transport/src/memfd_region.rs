// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **CMB-P4 (map 模式)** — memfd-backed [`SharedRamRegion`]。
//!
//! map-based CMB 的 backing：一段由 **memfd** 支撑的共享内存，同时满足两个角色：
//! - **firmware 本地读写**：经本进程内 `mmap(MAP_SHARED)` 视图（[`as_bytes`] /
//!   [`as_bytes_mut`]）——firmware core 把它当普通 `SharedRamRegion` 用；
//! - **client 零拷贝直访**：经 [`as_fd`] 暴露 memfd，transport 在 map 模式把它经
//!   SCM_RIGHTS 附在 `GET_REGION_INFO` reply 里，client `mmap` 同一 memfd → guest
//!   与 firmware 共享同一物理页（零拷贝，无 REGION_RW 往返）。
//!
//! **信任方向**（设计 §2）：memfd 由 **server（firmware 侧）自持**并经 fd 暴露给
//! client，与 W3 的 DMA_MAP（client→server 暴露 guest RAM）方向相反。server 自持
//! → 生命周期由 server 控制（≥ client 映射，设计 §9#4）。
//!
//! 本类型放在 `vfio_user_transport`（允许 `unsafe`），而非 `#![forbid(unsafe_code)]`
//! 的 `pcie_device_core`——后者只定义 `SharedRamRegion` 安全接口。承重 spike
//! （`tests/cmb_map_fd_pass_spike.rs`）已证 fd-pass + 共享 mmap 双向可见。
//!
//! [`as_bytes`]: pcie_device_core::SharedRamRegion::as_bytes
//! [`as_bytes_mut`]: pcie_device_core::SharedRamRegion::as_bytes_mut
//! [`as_fd`]: pcie_device_core::SharedRamRegion::as_fd

use pcie_device_core::SharedRamRegion;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::OwnedFd;

/// memfd-backed 的 [`SharedRamRegion`]：firmware 本地经 mmap 视图读写，client 经
/// [`SharedRamRegion::as_fd`] 拿到的 memfd `mmap` 后零拷贝共享同一物理页。
///
/// **不 derive Clone**：持有 `OwnedFd` + `MmapMut` 独占资源，复制无意义且不安全。
pub struct MemfdRamRegion {
    /// memfd 句柄（server 自持，经 SCM_RIGHTS 暴露给 client）。映射建立后本 fd 仍
    /// 须存活——client 需要它来 mmap，且 [`as_fd`](SharedRamRegion::as_fd) 借出它。
    fd: OwnedFd,
    /// firmware 本地的读写映射（`MAP_SHARED` → 与 client 的 mmap 指向同一物理页）。
    mmap: memmap2::MmapMut,
    /// backing 字节数（== mmap 长度 == memfd 真实大小）。
    len: usize,
}

impl MemfdRamRegion {
    /// 建一个 `len` 字节、全零的 memfd-backed region。
    ///
    /// 步骤：`memfd_create` → `ftruncate(len)`（赋予真实页 backing，防 mmap 后 SIGBUS）
    /// → `mmap(MAP_SHARED, PROT_READ|WRITE)` 建本地视图。`len` 应为页对齐倍数（CMB
    /// 由 CMBSZ 粒度对齐保证，设计 §10#8）；非页对齐时 `mmap` 仍按页向上取整，本地
    /// 视图长度以请求 `len` 为准（slice 边界以 `len` 把关）。
    pub fn new(len: usize) -> std::io::Result<Self> {
        let fd = nix::sys::memfd::memfd_create(c"usnvme-cmb", nix::sys::memfd::MFdFlags::empty())
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
        // ftruncate 赋予真实页 backing：mmap 区间全程有页，普通访问不 SIGBUS。
        nix::unistd::ftruncate(fd.as_fd(), len as libc::off_t)
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
        let mut opts = memmap2::MmapOptions::new();
        opts.len(len);
        let raw = fd.as_raw_fd();
        #[allow(unsafe_code)]
        // SAFETY: 本类型自建并自持 `fd`（memfd），刚 `ftruncate(len)` 故 `[0, len)` 全程
        // 有真实页 backing —— 普通访问不会 SIGBUS（不依赖任何外部 client 的声明，oracle
        // 是我们自己 ftruncate 的 len）。MAP_SHARED 使本地视图与 client 后续 mmap 同一
        // 物理页（map 模式零拷贝的根据，spike 已证）；映射独立于 fd 存续，但我们仍把 `fd`
        // 存进结构体以便 `as_fd` 借出给 transport 发 SCM_RIGHTS。munmap 随 `MmapMut` Drop，
        // close 随 `OwnedFd` Drop。本进程内 firmware 顺序访问 + guest 并发访问共享页是 DMA/
        // CMB 的固有语义：纯 u8 memcpy 无 typed 解释 → 无 Rust 内存模型 UB；逻辑层并发同步
        // （CMB 内 SQ/CQ 的 producer/consumer）由 doorbell（BAR0 仍 trap）作同步点保证，
        // 非本 backing 层职责（设计 §10#6）。
        //
        // **承重外部假设（reviewer M1，对齐 W3 `dma.rs` 不变量 #4 的诚实披露）**：本 fd 经
        // SCM_RIGHTS 暴露给 client 后，client 持 dup fd，理论上可 `ftruncate` **缩小** →
        // server 自己 `as_bytes_mut()` 访问超出新 size 的页会 SIGBUS（方向与 W3 相反：那里
        // client 缩小自己传入的 fd，这里 client 缩小我们暴露的 fd）。本教学版**信任 client
        // 不 shrink 已暴露的 CMB memfd**；**生产**应 `memfd_create` 加 `MFD_ALLOW_SEALING` +
        // 暴露前 `F_SEAL_SHRINK | F_SEAL_GROW` 封死尺寸。
        let mmap = unsafe { opts.map_mut(raw) }?;
        Ok(Self { fd, mmap, len })
    }
}

impl SharedRamRegion for MemfdRamRegion {
    fn as_bytes(&self) -> &[u8] {
        &self.mmap[..self.len]
    }

    fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.mmap[..self.len]
    }

    fn len(&self) -> usize {
        self.len
    }

    /// 借出 memfd：map 模式下 transport 经 SCM_RIGHTS 附在 `GET_REGION_INFO` reply，
    /// client `mmap` 它 → 与本地 [`as_bytes`](Self::as_bytes) 共享同一物理页（零拷贝）。
    fn as_fd(&self) -> Option<BorrowedFd<'_>> {
        Some(self.fd.as_fd())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// 基本：new 出 4 KiB 全零 backing，len 正确，as_fd 返 Some（非 Vec 的 None）。
    #[test]
    fn memfd_region_basic_shape() {
        let r = MemfdRamRegion::new(4096).expect("new memfd region");
        assert_eq!(r.len(), 4096);
        assert!(!r.is_empty());
        assert!(r.as_bytes().iter().all(|&b| b == 0), "新 backing 应全零");
        assert!(r.as_fd().is_some(), "memfd backing 应暴露可 mmap 的 fd");
        assert_eq!(r.as_bytes().len(), r.len());
    }

    /// firmware 本地写经 as_bytes_mut 可读回（mmap 视图自洽）。
    #[test]
    fn memfd_local_write_readback() {
        let mut r = MemfdRamRegion::new(64).expect("new");
        r.as_bytes_mut()[8..16].copy_from_slice(&0xCAFEBABEu32.to_le_bytes()[..].repeat(2)[..8]);
        let v = u32::from_le_bytes(r.as_bytes()[8..12].try_into().unwrap());
        assert_eq!(v, 0xCAFEBABE);
    }

    /// **map 模式核心** — firmware 经本地 mmap 写 → 经 as_fd 拿到的 fd 另开一个独立
    /// mmap(MAP_SHARED) 应读到同一字节（**同一物理页**，零拷贝共享的判据）。反向亦然。
    #[test]
    fn memfd_as_fd_shares_same_physical_page() {
        let mut r = MemfdRamRegion::new(4096).expect("new");
        // firmware 本地写 marker。
        r.as_bytes_mut()[0x100..0x108].copy_from_slice(&0xDEADBEEF_01020304u64.to_le_bytes());

        // 模拟 client：经 as_fd 拿 fd → 另开独立 mmap。
        let fd = r.as_fd().expect("as_fd Some");
        let mut opts = memmap2::MmapOptions::new();
        opts.len(4096);
        #[allow(unsafe_code)]
        // SAFETY: fd 是同一 memfd，已 ftruncate(4096)，[0,4096) 有页 backing；测试单线程
        // 顺序访问，纯 u8 memcpy 无 typed UB。
        let mut peer = unsafe { opts.map_mut(fd.as_raw_fd()) }.expect("peer mmap");
        let seen = u64::from_le_bytes(peer[0x100..0x108].try_into().unwrap());
        assert_eq!(
            seen, 0xDEADBEEF_01020304,
            "peer mmap 读到 firmware 写的 marker（同页）"
        );

        // 反向：peer 写 → firmware 本地视图看到。
        peer[0x200..0x208].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        peer.flush().ok();
        let back = u64::from_le_bytes(r.as_bytes()[0x200..0x208].try_into().unwrap());
        assert_eq!(
            back, 0x1122_3344_5566_7788,
            "firmware 本地视图看到 peer 写入（同页）"
        );
    }

    /// trait object 路径：装箱后 as_fd / as_bytes 仍工作（CmbState 持 Box<dyn>）。
    #[test]
    fn memfd_region_as_trait_object() {
        let mut boxed: Box<dyn SharedRamRegion> = Box::new(MemfdRamRegion::new(32).unwrap());
        boxed.as_bytes_mut()[0] = 0x7E;
        assert_eq!(boxed.as_bytes()[0], 0x7E);
        assert!(boxed.as_fd().is_some());
    }
}
