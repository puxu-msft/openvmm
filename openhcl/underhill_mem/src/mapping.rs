// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Implementing GuestMemoryAccess.
#![expect(unsafe_code)]

use crate::MshvVtlWithPolicy;
use crate::RegistrationError;
use crate::registrar::MemoryRegistrar;
use guestmem::GuestMemoryAccess;
use guestmem::GuestMemoryBackingError;
use guestmem::GuestMemorySharing;
use guestmem::PAGE_SIZE;
use guestmem::ProvideShareableRegions;
use guestmem::ShareableRegion;
use guestmem::ShareableRegionError;
use hcl::GuestVtl;
use hcl::ioctl::Mshv;
use hcl::ioctl::MshvVtlLow;
use hvdef::HvMapGpaFlags;
use inspect::Inspect;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use sparse_mmap::SparseMapping;
use std::ptr::NonNull;
use std::sync::Arc;
use thiserror::Error;
use virt_mshv_vtl::ProtectIsolatedMemory;
use vm_topology::memory::MemoryLayout;

pub struct GuestPartitionMemoryView<'a> {
    memory_layout: &'a MemoryLayout,
    valid_memory: Arc<GuestValidMemory>,
}

impl<'a> GuestPartitionMemoryView<'a> {
    /// A bitmap is created to track the accessibility state of each page in the
    /// lower VTL memory. The bitmap is initialized to valid_bitmap_state.
    ///
    /// This is used to support tracking the shared/encrypted state of each
    /// page.
    pub fn new(
        memory_layout: &'a MemoryLayout,
        memory_type: GuestValidMemoryType,
        valid_bitmap_state: bool,
    ) -> Result<Self, MappingError> {
        let valid_memory =
            GuestValidMemory::new(memory_layout, memory_type, valid_bitmap_state).map(Arc::new)?;
        Ok(Self {
            memory_layout,
            valid_memory,
        })
    }

    /// Returns the built partition-wide valid memory.
    pub fn partition_valid_memory(&self) -> Arc<GuestValidMemory> {
        self.valid_memory.clone()
    }

    /// Build a [`GuestMemoryMapping`], feeding in any related partition-wide
    /// state.
    fn build_guest_memory_mapping(
        &self,
        mshv_vtl_low: &MshvVtlLow,
        memory_mapping_builder: &mut GuestMemoryMappingBuilder,
    ) -> Result<GuestMemoryMapping, MappingError> {
        memory_mapping_builder
            .use_partition_valid_memory(Some(self.valid_memory.clone()))
            .build(mshv_vtl_low, self.memory_layout)
    }
}

#[derive(Debug, Inspect)]
pub enum GuestMemoryViewReadType {
    Read,
    KernelExecute,
    UserExecute,
}

#[derive(Inspect)]
pub struct GuestMemoryView {
    #[inspect(skip)]
    protector: Option<Arc<dyn ProtectIsolatedMemory>>,
    pub memory_mapping: Arc<GuestMemoryMapping>,
    pub view_type: GuestMemoryViewReadType,
    vtl: GuestVtl,
}

impl GuestMemoryView {
    pub fn new(
        protector: Option<Arc<dyn ProtectIsolatedMemory>>,
        memory_mapping: Arc<GuestMemoryMapping>,
        view_type: GuestMemoryViewReadType,
        vtl: GuestVtl,
    ) -> Self {
        Self {
            protector,
            memory_mapping,
            view_type,
            vtl,
        }
    }
}

#[derive(Error, Debug)]
#[error("the specified page is not mapped")]
struct NotMapped;

#[derive(Error, Debug)]
enum BitmapFailure {
    #[error("the specified page was accessed using the wrong visibility mapping")]
    IncorrectHostVisibilityAccess,
    #[error("the specified page access violates VTL 1 protections")]
    Vtl1ProtectionsViolation,
}

/// SAFETY: Implementing the `GuestMemoryAccess` contract, including the
/// size and lifetime of the mappings and bitmaps.
unsafe impl GuestMemoryAccess for GuestMemoryView {
    fn mapping(&self) -> Option<NonNull<u8>> {
        NonNull::new(self.memory_mapping.mapping.as_ptr().cast())
    }

    fn max_address(&self) -> u64 {
        self.memory_mapping.mapping.len() as u64
    }

    fn expose_va(&self, address: u64, len: u64) -> Result<(), GuestMemoryBackingError> {
        if let Some(registrar) = &self.memory_mapping.registrar {
            registrar
                .register(address, len)
                .map_err(|start| GuestMemoryBackingError::other(start, RegistrationError))
        } else {
            // TODO: fail this call once we have a way to avoid calling this for
            // user-mode-only accesses to locked memory (e.g., for vmbus ring
            // buffers). We can't fail this for now because TDX cannot register
            // encrypted memory.
            Ok(())
        }
    }

    fn base_iova(&self) -> Option<u64> {
        // When the alias map is configured for this mapping, VTL2-mapped
        // devices need to do DMA with the alias map bit set to avoid DMAing
        // into VTL1 memory.
        self.memory_mapping.iova_offset
    }

    fn access_bitmap(&self) -> Option<guestmem::BitmapInfo> {
        // When the permissions bitmaps are available, they take precedence and
        // therefore should be no more permissive than the access bitmap.
        //
        // TODO GUEST VSM: consider being able to dynamically update these
        // bitmaps. There are two scenarios where this would be useful:
        // 1. To reduce memory consumption in cases where the bitmaps aren't
        //    needed, i.e. the guest chooses not to enable guest vsm and VTL 1
        //    gets revoked.
        // 2. Because the related guest memory objects are initialized before
        // VTL 1 is, the code as it currently stands will always enforce vtl 1
        // protections even if VTL 1 hasn't explicitly enabled it. e.g. if VTL 1
        // never enables vtl protections via the vsm partition config, but it
        // still makes hypercalls to modify the vtl protection mask (this is a
        // valid scenario to help set up default protections), these protections
        // will still be enforced. In practice, a well-designed VTL 1 probably
        // would enable vtl protections before allowing VTL 0 to run again, but
        // technically the implementation here is not to spec.
        if let Some(bitmaps) = self.memory_mapping.permission_bitmaps.as_ref() {
            match self.view_type {
                GuestMemoryViewReadType::Read => Some(guestmem::BitmapInfo {
                    read_bitmap: NonNull::new(bitmaps.read_bitmap.as_ptr().cast()).unwrap(),
                    write_bitmap: NonNull::new(bitmaps.write_bitmap.as_ptr().cast()).unwrap(),
                    bit_offset: 0,
                }),
                GuestMemoryViewReadType::KernelExecute => Some(guestmem::BitmapInfo {
                    read_bitmap: NonNull::new(bitmaps.kernel_execute_bitmap.as_ptr().cast())
                        .unwrap(),
                    write_bitmap: NonNull::new(bitmaps.write_bitmap.as_ptr().cast()).unwrap(),
                    bit_offset: 0,
                }),
                GuestMemoryViewReadType::UserExecute => Some(guestmem::BitmapInfo {
                    read_bitmap: NonNull::new(bitmaps.user_execute_bitmap.as_ptr().cast()).unwrap(),
                    write_bitmap: NonNull::new(bitmaps.write_bitmap.as_ptr().cast()).unwrap(),
                    bit_offset: 0,
                }),
            }
        } else {
            self.memory_mapping
                .valid_memory
                .as_ref()
                .map(|bitmap| bitmap.access_bitmap())
        }
    }

    fn page_fault(
        &self,
        address: u64,
        len: usize,
        write: bool,
        bitmap_failure: bool,
    ) -> guestmem::PageFaultAction {
        let gpn = address / PAGE_SIZE as u64;
        if !bitmap_failure {
            guestmem::PageFaultAction::Fail(guestmem::PageFaultError::other(NotMapped {}))
        } else {
            let valid_memory = self
                .memory_mapping
                .valid_memory
                .as_ref()
                .expect("all backings with bitmaps should have a GuestValidMemory");
            if !valid_memory.check_valid(gpn) {
                match valid_memory.memory_type() {
                    GuestValidMemoryType::Shared => {
                        tracelimit::warn_ratelimited!(
                            ?address,
                            ?len,
                            ?write,
                            "tried to access private page using shared mapping"
                        );
                        guestmem::PageFaultAction::Fail(guestmem::PageFaultError::new(
                            guestmem::GuestMemoryErrorKind::NotShared,
                            BitmapFailure::IncorrectHostVisibilityAccess,
                        ))
                    }
                    GuestValidMemoryType::Encrypted => {
                        tracelimit::warn_ratelimited!(
                            ?address,
                            ?len,
                            ?write,
                            "tried to access shared page using private mapping"
                        );
                        guestmem::PageFaultAction::Fail(guestmem::PageFaultError::new(
                            guestmem::GuestMemoryErrorKind::NotPrivate,
                            BitmapFailure::IncorrectHostVisibilityAccess,
                        ))
                    }
                }
            } else {
                // Currently, only VTL 1 permissions are tracked, so any
                // invalid accesses here violate VTL 1 protections.
                if let Some(permission_bitmaps) = &self.memory_mapping.permission_bitmaps {
                    let check_bitmap = if write {
                        &permission_bitmaps.write_bitmap
                    } else {
                        match self.view_type {
                            GuestMemoryViewReadType::Read => &permission_bitmaps.read_bitmap,
                            GuestMemoryViewReadType::KernelExecute => {
                                &permission_bitmaps.kernel_execute_bitmap
                            }
                            GuestMemoryViewReadType::UserExecute => {
                                &permission_bitmaps.user_execute_bitmap
                            }
                        }
                    };

                    if !check_bitmap.page_state(gpn) {
                        tracelimit::warn_ratelimited!(?address, ?len, ?write, ?self.view_type, "VTL 1 permissions violation");

                        return guestmem::PageFaultAction::Fail(guestmem::PageFaultError::new(
                            guestmem::GuestMemoryErrorKind::VtlProtected,
                            BitmapFailure::Vtl1ProtectionsViolation,
                        ));
                    }
                }

                // Possible race condition where the bitmaps are in transition
                // and while the original check failed, the bitmaps now show
                // valid access to the page. Retry in that situation.
                guestmem::PageFaultAction::Retry
            }
        }
    }

    fn lock_gpns(&self, gpns: &[u64]) -> Result<bool, GuestMemoryBackingError> {
        if let Some(protector) = self.protector.as_ref() {
            protector.lock_gpns(self.vtl, gpns)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn unlock_gpns(&self, gpns: &[u64]) {
        if let Some(protector) = self.protector.as_ref() {
            protector.unlock_gpns(self.vtl, gpns)
        }
    }

    /// W6c finding-④：仅当底层 mapping 走 no-bitmap 路径（非隔离 VTL0/VTL1）时返回
    /// `Some`——把 guest RAM 经 fd-passing 零拷贝交给 VTL2 里的 usnvmemu server
    /// （vfio-user DMA_MAP）。
    ///
    /// 带 valid/permission bitmap 的 mapping（CVM / software-isolated）`shared_fd`
    /// 恒为 `None`，故此处自动返回 `None`，**结构性**杜绝绕过 bitmap 门控的直接 mmap
    /// （满足 `ShareableRegion` 的「fully committed / 无 bitmap 门控」契约）。
    fn sharing(&self) -> Option<GuestMemorySharing> {
        let fd = self.memory_mapping.shared_fd.clone()?;
        Some(GuestMemorySharing::new(UnderhillVtl0Sharing {
            fd,
            regions: self.memory_mapping.shareable_ranges.clone(),
        }))
    }
}

/// W6c finding-④：`ProvideShareableRegions` 实现——把 [`GuestMemoryMapping`] 收集的
/// 逐段 `(guest_address, size, file_offset)` 连同 dup'd `/dev/mshv_vtl_low` fd 暴露成
/// [`ShareableRegion`]，供 vfio-user DMA_MAP 经 SCM_RIGHTS 把每段 ship 给 VTL2 server。
///
/// 区间集合在 VM 生命周期内静态（与 guestmem 契约一致，暂不支持 hotplug）。
struct UnderhillVtl0Sharing {
    /// dup'd `/dev/mshv_vtl_low` fd（`Arc` 共享，避免每段 OS 级 `dup()`）。
    fd: Arc<sparse_mmap::Mappable>,
    /// 逐 ram() 段的 `(guest_address, size, file_offset)`。
    regions: Vec<(u64, u64, u64)>,
}

impl ProvideShareableRegions for UnderhillVtl0Sharing {
    async fn get_regions(&self) -> Result<Vec<ShareableRegion>, ShareableRegionError> {
        Ok(self
            .regions
            .iter()
            .map(|&(guest_address, size, file_offset)| ShareableRegion {
                guest_address,
                size,
                file: self.fd.clone(),
                file_offset,
            })
            .collect())
    }
}

#[derive(Debug, Copy, Clone)]
pub enum GuestValidMemoryType {
    Shared,
    Encrypted,
}

/// Partition-wide (cross-vtl) tracking of valid memory that can be used in
/// individual GuestMemoryMappings.
#[derive(Debug)]
pub struct GuestValidMemory {
    valid_bitmap: GuestMemoryBitmap,
    valid_bitmap_lock: Mutex<()>,
    memory_type: GuestValidMemoryType,
}

impl GuestValidMemory {
    fn new(
        memory_layout: &MemoryLayout,
        memory_type: GuestValidMemoryType,
        valid_bitmap_state: bool,
    ) -> Result<Self, MappingError> {
        let valid_bitmap = {
            let mut bitmap = {
                // Calculate the total size of the address space by looking at the ending region.
                let last_entry = memory_layout
                    .ram()
                    .last()
                    .expect("memory map must have at least 1 entry");
                let address_space_size = last_entry.range.end();
                GuestMemoryBitmap::new(address_space_size as usize)?
            };

            for entry in memory_layout.ram() {
                if entry.range.is_empty() {
                    continue;
                }

                bitmap.init(entry.range, valid_bitmap_state)?;
            }

            bitmap
        };

        Ok(GuestValidMemory {
            valid_bitmap,
            valid_bitmap_lock: Default::default(),
            memory_type,
        })
    }

    /// Update the bitmap to reflect the validity of the given range.
    pub fn update_valid(&self, range: MemoryRange, state: bool) {
        let _lock = self.valid_bitmap_lock.lock();
        self.valid_bitmap.update(range, state);
    }

    /// Check if the given page is valid.
    pub(crate) fn check_valid(&self, gpn: u64) -> bool {
        self.valid_bitmap.page_state(gpn)
    }

    /// Returns the type of memory tracked by the bitmap
    pub(crate) fn memory_type(&self) -> GuestValidMemoryType {
        self.memory_type
    }

    fn access_bitmap(&self) -> guestmem::BitmapInfo {
        let ptr = NonNull::new(self.valid_bitmap.as_ptr()).unwrap();
        guestmem::BitmapInfo {
            read_bitmap: ptr,
            write_bitmap: ptr,
            bit_offset: 0,
        }
    }
}

/// An implementation of a [`GuestMemoryAccess`] trait for Underhill VMs.
#[derive(Debug, Inspect)]
pub struct GuestMemoryMapping {
    #[inspect(skip)]
    mapping: SparseMapping,
    iova_offset: Option<u64>,
    #[inspect(with = "Option::is_some")]
    valid_memory: Option<Arc<GuestValidMemory>>,
    #[inspect(with = "Option::is_some")]
    permission_bitmaps: Option<PermissionBitmaps>,
    registrar: Option<MemoryRegistrar<MshvVtlWithPolicy>>,
    /// W6c finding-④：底层 `/dev/mshv_vtl_low` 的 dup'd fd，仅在**无 bitmap 门控**
    /// 的 mapping 上为 `Some`。`sharing()` 据此把 guest RAM 经 fd-passing
    /// （vfio-user DMA_MAP）零拷贝交给 VTL2 里的 usnvmemu server。
    ///
    /// **结构性安全约束**：任何带 valid/permission bitmap 的 mapping（CVM /
    /// software-isolated）此字段恒为 `None`，故 `sharing()` 自动返回 `None`，杜绝把
    /// 受 bitmap 门控的页直接暴露给外部 mmap（绕过门控会读到未提交/错误可见性的页）。
    #[inspect(with = "Option::is_some")]
    shared_fd: Option<Arc<sparse_mmap::Mappable>>,
    /// W6c finding-④：可共享区间表 `(guest_address, size, file_offset)`，逐
    /// `memory_layout.ram()` 段（guest 物理空间有洞，必须分段，不能整块 map）。
    /// `file_offset` 复用 `build()` 里现有的 `file_starting_offset + range.start()`
    /// 表达式（自动带上 alias-map bit）；IOVA 仍是裸 `guest_address`。仅在
    /// `shared_fd` 为 `Some` 时非空。
    #[inspect(skip)]
    shareable_ranges: Vec<(u64, u64, u64)>,
}

/// Bitmap implementation using sparse mapping that can be used to track page
/// states.
#[derive(Debug)]
struct PermissionBitmaps {
    permission_update_lock: Mutex<()>,
    read_bitmap: GuestMemoryBitmap,
    write_bitmap: GuestMemoryBitmap,
    kernel_execute_bitmap: GuestMemoryBitmap,
    user_execute_bitmap: GuestMemoryBitmap,
}

#[derive(Error, Debug)]
pub enum VtlPermissionsError {
    #[error("no vtl 1 permissions enforcement, bitmap is not present")]
    NoPermissionsTracked,
}

#[derive(Debug)]
struct GuestMemoryBitmap {
    bitmap: SparseMapping,
}

impl GuestMemoryBitmap {
    fn new(address_space_size: usize) -> Result<Self, MappingError> {
        let bitmap = SparseMapping::new((address_space_size / PAGE_SIZE).div_ceil(8))
            .map_err(MappingError::BitmapReserve)?;
        bitmap
            .map_zero(0, bitmap.len())
            .map_err(MappingError::BitmapMap)?;
        Ok(Self { bitmap })
    }

    fn init(&mut self, range: MemoryRange, state: bool) -> Result<(), MappingError> {
        if !range.start().is_multiple_of(PAGE_SIZE as u64 * 8)
            || !range.end().is_multiple_of(PAGE_SIZE as u64 * 8)
        {
            return Err(MappingError::BadAlignment(range));
        }

        let bitmap_start = range.start() as usize / PAGE_SIZE / 8;
        let bitmap_end = (range.end() - 1) as usize / PAGE_SIZE / 8;
        let bitmap_page_start = bitmap_start / PAGE_SIZE;
        let bitmap_page_end = bitmap_end / PAGE_SIZE;
        let page_count = bitmap_page_end + 1 - bitmap_page_start;

        // TODO CVM FUTURE: map some pre-reserved lower VTL memory into the
        // bitmap. Or just figure out how to hot add that memory to the
        // kernel. Or have the boot loader reserve it at boot time.
        //
        // Be careful, though--if we do that, we have to remember which
        // pages were already allocated and initialized; otherwise, we will
        // zero them again. To avoid tracking this as is, just update
        // the page protections on the existing zero-mapped pages.
        self.bitmap
            .set_writable(bitmap_page_start * PAGE_SIZE, page_count * PAGE_SIZE, true)
            .map_err(MappingError::BitmapProtect)?;

        // Set the initial bitmap state.
        if state {
            let start_gpn = range.start() / PAGE_SIZE as u64;
            let gpn_count = range.len() / PAGE_SIZE as u64;
            assert_eq!(range.start() % 8, 0);
            assert_eq!(gpn_count % 8, 0);
            self.bitmap
                .fill_at(start_gpn as usize / 8, 0xff, gpn_count as usize / 8)
                .unwrap();
        }

        Ok(())
    }

    /// Panics if the range is outside of guest RAM.
    fn update(&self, range: MemoryRange, state: bool) {
        for gpn in range.start() / PAGE_SIZE as u64..range.end() / PAGE_SIZE as u64 {
            // TODO: use `fill_at` for the aligned part of the range.
            let mut b = 0;
            self.bitmap
                .read_at(gpn as usize / 8, std::slice::from_mut(&mut b))
                .unwrap();
            if state {
                b |= 1 << (gpn % 8);
            } else {
                b &= !(1 << (gpn % 8));
            }
            self.bitmap
                .write_at(gpn as usize / 8, std::slice::from_ref(&b))
                .unwrap();
        }
    }

    /// Read the bitmap for `gpn`.
    /// Panics if the range is outside of guest RAM.
    fn page_state(&self, gpn: u64) -> bool {
        let mut b = 0;
        self.bitmap
            .read_at(gpn as usize / 8, std::slice::from_mut(&mut b))
            .unwrap();
        b & (1 << (gpn % 8)) != 0
    }

    fn as_ptr(&self) -> *mut u8 {
        self.bitmap.as_ptr().cast()
    }
}

/// Error constructing a [`GuestMemoryMapping`].
#[derive(Debug, Error)]
pub enum MappingError {
    #[error("failed to allocate VA space for guest memory")]
    Reserve(#[source] std::io::Error),
    #[error("failed to map guest memory pages")]
    Map(#[source] std::io::Error),
    #[error("failed to allocate VA space for bitmap")]
    BitmapReserve(#[source] std::io::Error),
    #[error("failed to map zero pages for bitmap")]
    BitmapMap(#[source] std::io::Error),
    #[error("failed to make pages writable for bitmap")]
    BitmapProtect(#[source] std::io::Error),
    #[error("memory map entry {0} has insufficient alignment to support a bitmap")]
    BadAlignment(MemoryRange),
    #[error("failed to open device")]
    OpenDevice(#[source] hcl::ioctl::Error),
}

/// A builder for [`GuestMemoryMapping`].
pub struct GuestMemoryMappingBuilder {
    physical_address_base: u64,
    valid_memory: Option<Arc<GuestValidMemory>>,
    permissions_bitmap_state: Option<bool>,
    shared: bool,
    for_kernel_access: bool,
    /// W6c finding-④：显式 opt-in 是否允许把本 mapping 的 guest RAM 经 fd 共享给
    /// 另一进程（vfio-user / vhost-user DMA_MAP）。**隔离安全不变量**：只能在
    /// 完全 commit、非隔离、无 bitmap 门控的构造点设 `true`（即非隔离 VTL0）；
    /// 绝不能为 CVM / 软隔离-VTL1-门控 / 惰性校验 的 mapping 设置。把"恰好没
    /// bitmap 所以安全"的隐式耦合，升级为构造点人工审过的显式标记（见 build()
    /// 里的 debug_assert 兜底）。
    shareable: bool,
    dma_base_address: Option<u64>,
    ignore_registration_failure: bool,
}

impl GuestMemoryMappingBuilder {
    fn use_partition_valid_memory(
        &mut self,
        valid_memory: Option<Arc<GuestValidMemory>>,
    ) -> &mut Self {
        self.valid_memory = valid_memory;
        self
    }

    /// Set whether to allocate tracking bitmaps for memory access permissions,
    /// and specify the initial state of the bitmaps.
    ///
    /// This is used to support tracking the read/write/kernel execute/user
    /// execute permissions of each page.
    pub fn use_permissions_bitmaps(&mut self, initial_state: Option<bool>) -> &mut Self {
        self.permissions_bitmap_state = initial_state;
        self
    }

    /// Set whether this is a mapping to access shared memory.
    pub fn shared(&mut self, is_shared: bool) -> &mut Self {
        self.shared = is_shared;
        self
    }

    /// Set whether this mapping's memory can be locked to pass to the kernel.
    ///
    /// If so, then the memory will be registered with the kernel as part of
    /// `expose_va`, which is called when memory is locked.
    pub fn for_kernel_access(&mut self, for_kernel_access: bool) -> &mut Self {
        self.for_kernel_access = for_kernel_access;
        self
    }

    /// W6c finding-④：opt-in 允许本 mapping 经 fd 共享给另一进程（vfio-user /
    /// vhost-user DMA_MAP）。**只在非隔离、无 bitmap 门控的构造点设 `true`**
    /// （隔离安全不变量，见字段注释与 `build()` 的 debug_assert）。即便误设，
    /// `build()` 仍要求 no-bitmap + alias-off 才真共享，故 CVM/隔离 mapping 不会泄露。
    pub fn shareable(&mut self, shareable: bool) -> &mut Self {
        self.shareable = shareable;
        self
    }

    /// Sets the base address to use for DMAs to this memory.
    ///
    /// This may be `None` if DMA is not supported.
    ///
    /// The address to use depends on the backing technology. For SNP VMs, it
    /// should be either zero or the VTOM address, since shared memory is mapped
    /// twice. For TDX VMs, shared memory is only mapped once, but the IOMMU
    /// expects the SHARED bit to be set in DMA transactions, so it should be
    /// set here. And for non-isolated/software-isolated VMs, it should be zero
    /// or the VTL0 alias address, depending on which VTL this memory mapping is
    /// for.
    pub fn dma_base_address(&mut self, dma_base_address: Option<u64>) -> &mut Self {
        self.dma_base_address = dma_base_address;
        self
    }

    /// Ignore registration failures when registering memory with the kernel.
    ///
    /// This should be used when user mode is restarted for servicing but the
    /// kernel is not. Since this is not currently a production scenario, this
    /// is a simple way to avoid needing to track the state of the kernel
    /// registration across user-mode restarts.
    ///
    /// It is not a good idea to enable this otherwise, since the kernel very
    /// noisily complains if memory is registered twice, so we don't want that
    /// leaking into production scenarios.
    ///
    /// FUTURE: fix the kernel to silently succeed duplication registrations.
    pub fn ignore_registration_failure(&mut self, ignore: bool) -> &mut Self {
        self.ignore_registration_failure = ignore;
        self
    }

    /// Mapping should leverage the bitmap used to track the accessibility state
    /// of each page in the lower VTL memory.
    pub fn build_with_bitmap(
        &mut self,
        mshv_vtl_low: &MshvVtlLow,
        partition_builder: &GuestPartitionMemoryView<'_>,
    ) -> Result<GuestMemoryMapping, MappingError> {
        partition_builder.build_guest_memory_mapping(mshv_vtl_low, self)
    }

    pub fn build_without_bitmap(
        &self,
        mshv_vtl_low: &MshvVtlLow,
        memory_layout: &MemoryLayout,
    ) -> Result<GuestMemoryMapping, MappingError> {
        self.build(mshv_vtl_low, memory_layout)
    }

    /// Map the lower VTL address space.
    ///
    /// If `is_shared`, then map the kernel mapping as shared memory.
    ///
    /// Add in `file_starting_offset` to construct the page offset for each
    /// memory range. This can be the high bit to specify decrypted/shared
    /// memory, or it can be the VTL0 alias map start for non-isolated VMs.
    ///
    /// When handing out IOVAs for device DMA, add `iova_offset`. This can be
    /// VTOM for SNP-isolated VMs, or it can be the VTL0 alias map start for
    /// non-isolated VMs.
    fn build(
        &self,
        mshv_vtl_low: &MshvVtlLow,
        memory_layout: &MemoryLayout,
    ) -> Result<GuestMemoryMapping, MappingError> {
        // Calculate the file offset within the `mshv_vtl_low` file.
        let file_starting_offset = self.physical_address_base
            | if self.shared {
                MshvVtlLow::SHARED_MEMORY_FLAG
            } else {
                0
            };

        // Calculate the total size of the address space by looking at the ending region.
        let last_entry = memory_layout
            .ram()
            .last()
            .expect("memory map must have at least 1 entry");
        let address_space_size = last_entry.range.end();
        let mapping =
            SparseMapping::new(address_space_size as usize).map_err(MappingError::Reserve)?;

        tracing::trace!(?mapping, "map_lower_vtl_memory mapping");

        let mut permission_bitmaps = if self.permissions_bitmap_state.is_some() {
            Some(PermissionBitmaps {
                permission_update_lock: Default::default(),
                read_bitmap: GuestMemoryBitmap::new(address_space_size as usize)?,
                write_bitmap: GuestMemoryBitmap::new(address_space_size as usize)?,
                kernel_execute_bitmap: GuestMemoryBitmap::new(address_space_size as usize)?,
                user_execute_bitmap: GuestMemoryBitmap::new(address_space_size as usize)?,
            })
        } else {
            None
        };

        // W6c finding-④：是否把本 mapping 的 guest RAM 经 vfio-user DMA_MAP 暴露给
        // VTL2 server（零拷贝），需三条件同时成立：
        // ① `shareable` 显式 opt-in——隔离安全不靠"恰好没 bitmap"的隐式耦合，而靠
        //    构造点（仅非隔离 VTL0）人工审过的显式标记；
        // ② no-bitmap 路径（无 valid/permission bitmap 门控）——结构性兜底：任何带
        //    bitmap 的 CVM/隔离 mapping 即便误设 shareable 也不真共享，sharing() 返 None；
        // ③ `file_starting_offset == 0`（alias-map off 且非 shared）——此时
        //    IOVA(裸 guest_address) == fd_offset，是 W6c 真机 POC 验证过的唯一情形。
        //    alias-on 时 IOVA≠fd_offset 的约定未经验证 → 降级为不共享（不赌未验证假设）。
        let no_bitmap_gating =
            self.valid_memory.is_none() && self.permissions_bitmap_state.is_none();
        if self.shareable {
            // 显式 opt-in 必须落在 no-bitmap 路径上；否则是构造点用错了——debug/CI 兜住，
            // 真共享仍被 `do_share` 的 `no_bitmap_gating` 项挡掉（运行期不泄露）。
            debug_assert!(
                no_bitmap_gating,
                "shareable mapping 必须无 valid/permission bitmap（隔离安全不变量）"
            );
        }
        let iova_equals_fd_offset = file_starting_offset == 0;
        if self.shareable && no_bitmap_gating && !iova_equals_fd_offset {
            tracing::warn!(
                file_starting_offset,
                "W6c：vfio-user DMA 零拷贝共享已禁用——VTL0 alias-map/shared offset 非零，\
                 IOVA≠fd_offset 的约定未经真机验证，降级为无零拷贝 DMA"
            );
        }
        let do_share = self.shareable && no_bitmap_gating && iova_equals_fd_offset;
        let mut shareable_ranges: Vec<(u64, u64, u64)> = Vec::new();

        // Loop through each of the memory map entries and create a mapping for it.
        for entry in memory_layout.ram() {
            if entry.range.is_empty() {
                continue;
            }
            let base_addr = entry.range.start();
            let file_offset = file_starting_offset.checked_add(base_addr).unwrap();

            tracing::trace!(base_addr, file_offset, "mapping lower ram");

            mapping
                .map_file(
                    base_addr as usize,
                    entry.range.len() as usize,
                    mshv_vtl_low.get(),
                    file_offset,
                    true,
                )
                .map_err(MappingError::Map)?;

            // W6c finding-④：逐段记录 (IOVA=裸 guest_address, size, fd_offset)。
            // fd_offset 用与上面 `map_file` 同一个 `file_offset`（alias-off 时 == base_addr，
            // 保证 IOVA==fd_offset；alias-on 已被 do_share 排除）。仅 do_share 时收集。
            if do_share {
                shareable_ranges.push((base_addr, entry.range.len(), file_offset));
            }

            if let Some((bitmaps, state)) = permission_bitmaps
                .as_mut()
                .zip(self.permissions_bitmap_state)
            {
                bitmaps.read_bitmap.init(entry.range, state)?;
                bitmaps.write_bitmap.init(entry.range, state)?;
                bitmaps.kernel_execute_bitmap.init(entry.range, state)?;
                bitmaps.user_execute_bitmap.init(entry.range, state)?;
            }

            tracing::trace!(?entry, "mapped memory map entry");
        }

        // W6c finding-④：仅 do_share（显式 opt-in + no-bitmap + alias-off）时 dup
        // `/dev/mshv_vtl_low` fd。`try_clone` 复制 fd（指向同一内核文件），`File` →
        // `OwnedFd`（= `sparse_mmap::Mappable` on Unix）。失败按 mapping 失败处理。
        // 非共享路径不 dup，避免无谓占用 fd。
        let shared_fd = if do_share {
            let cloned = mshv_vtl_low.get().try_clone().map_err(MappingError::Map)?;
            Some(Arc::new(std::os::fd::OwnedFd::from(cloned)))
        } else {
            None
        };

        let registrar = if self.for_kernel_access {
            let mshv = Mshv::new().map_err(MappingError::OpenDevice)?;
            let mshv_vtl = mshv.create_vtl().map_err(MappingError::OpenDevice)?;
            Some(MemoryRegistrar::new(
                memory_layout,
                self.physical_address_base,
                MshvVtlWithPolicy {
                    mshv_vtl,
                    ignore_registration_failure: self.ignore_registration_failure,
                    shared: self.shared,
                },
            ))
        } else {
            None
        };

        Ok(GuestMemoryMapping {
            mapping,
            iova_offset: self.dma_base_address,
            valid_memory: self.valid_memory.clone(),
            permission_bitmaps,
            registrar,
            shared_fd,
            shareable_ranges,
        })
    }
}

impl GuestMemoryMapping {
    /// Create a new builder for a guest memory mapping.
    ///
    /// Map all ranges with a physical address offset of
    /// `physical_address_base`. This can be zero, or the VTOM address for SNP,
    /// or the VTL0 alias address for non-isolated/software-isolated VMs.
    pub fn builder(physical_address_base: u64) -> GuestMemoryMappingBuilder {
        GuestMemoryMappingBuilder {
            physical_address_base,
            valid_memory: None,
            permissions_bitmap_state: None,
            shared: false,
            for_kernel_access: false,
            shareable: false,
            dma_base_address: None,
            ignore_registration_failure: false,
        }
    }

    /// Update the permission bitmaps to reflect the given flags.
    /// Panics if the range is outside of guest RAM.
    pub fn update_permission_bitmaps(&self, range: MemoryRange, flags: HvMapGpaFlags) {
        if let Some(bitmaps) = self.permission_bitmaps.as_ref() {
            let _lock = bitmaps.permission_update_lock.lock();
            bitmaps.read_bitmap.update(range, flags.readable());
            bitmaps.write_bitmap.update(range, flags.writable());
            bitmaps
                .kernel_execute_bitmap
                .update(range, flags.kernel_executable());
            bitmaps
                .user_execute_bitmap
                .update(range, flags.user_executable());
        }
    }

    /// Query the permissions for the given gpn.
    /// Panics if the range is outside of guest RAM.
    pub fn query_access_permission(&self, gpn: u64) -> Result<HvMapGpaFlags, VtlPermissionsError> {
        if let Some(bitmaps) = self.permission_bitmaps.as_ref() {
            Ok(HvMapGpaFlags::new()
                .with_readable(bitmaps.read_bitmap.page_state(gpn))
                .with_writable(bitmaps.write_bitmap.page_state(gpn))
                .with_kernel_executable(bitmaps.kernel_execute_bitmap.page_state(gpn))
                .with_user_executable(bitmaps.user_execute_bitmap.page_state(gpn)))
        } else {
            Err(VtlPermissionsError::NoPermissionsTracked)
        }
    }

    /// Zero the given range of memory.
    pub(crate) fn zero_range(
        &self,
        range: MemoryRange,
    ) -> Result<(), sparse_mmap::SparseMappingError> {
        self.mapping
            .fill_at(range.start() as usize, 0, range.len() as usize)
    }
}

#[cfg(test)]
mod tests {
    use crate::mapping::GuestValidMemory;
    use crate::mapping::GuestValidMemoryType;
    use memory_range::MemoryRange;
    use vm_topology::memory::MemoryLayout;
    use vm_topology::memory::MemoryRangeWithNode;

    // Ensure a bitmap initialized with ranges whose bitmap backings overlap
    // does not cause any issues.
    #[test]
    fn test_overlapping_bitmap() {
        let memory_ranges = [0..1, 1..2, 3..4].map(|r| MemoryRangeWithNode {
            range: MemoryRange::from_4k_gpn_range(r.start * 8..r.end * 8),
            vnode: 0,
        });
        let memory_layout = MemoryLayout::new_from_ranges(&memory_ranges, &[])
            .expect("Failed to create memory layout");
        let guest_valid_mem =
            GuestValidMemory::new(&memory_layout, GuestValidMemoryType::Encrypted, true).unwrap();
        for (i, &b) in [true, true, false, true, false].iter().enumerate() {
            assert_eq!(guest_valid_mem.check_valid(i as u64 * 8), b, "{i}");
        }
    }
}
