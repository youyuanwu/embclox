use x86_64::structures::paging::{
    Mapper, OffsetPageTable, Page, PageTableFlags, PhysFrame, Size4KiB, Translate,
};
use x86_64::{PhysAddr, VirtAddr};

const MMIO_VIRT_BASE: u64 = 0x4000_0000_0000;

/// Handle for a mapped MMIO region.
///
/// Plain data handle (vaddr, size) — not an owner. Mirrors `DmaRegion`.
/// The caller is responsible for calling `MemoryMapper::unmap_mmio`
/// when the region is no longer needed.
pub struct MmioMapping {
    vaddr: usize,
    size: u64,
}

impl MmioMapping {
    /// Virtual address of the mapped region.
    pub fn vaddr(&self) -> usize {
        self.vaddr
    }

    /// Size of the mapped region in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }
}

/// Physical memory mapper for MMIO and virtual/physical address translation.
///
/// # Ownership model
///
/// `map_mmio` returns an `MmioMapping` handle. The caller is responsible
/// for calling `unmap_mmio` when the mapping is no longer needed. This
/// mirrors the `DmaAllocator::alloc_coherent`/`free_coherent` pattern.
///
/// For long-lived mappings (e.g., the example's NIC), the mapping lives
/// for the program's lifetime and is never freed. For tests, explicit
/// unmap enables per-test setup/teardown.
pub struct MemoryMapper {
    phys_offset: u64,
    kernel_offset: u64,
    next_vaddr: u64,
}

impl MemoryMapper {
    /// Create a new memory mapper with bootloader-provided offsets.
    pub fn new(phys_offset: u64, kernel_offset: u64) -> Self {
        Self {
            phys_offset,
            kernel_offset,
            next_vaddr: MMIO_VIRT_BASE,
        }
    }

    /// Bootloader's physical memory mapping offset.
    pub fn phys_offset(&self) -> u64 {
        self.phys_offset
    }

    /// Kernel virtual-to-physical address offset.
    pub fn kernel_offset(&self) -> u64 {
        self.kernel_offset
    }

    /// Map an MMIO physical region with Uncacheable 4KB pages.
    /// Returns an `MmioMapping` handle. Call `unmap_mmio` to free.
    pub fn map_mmio(&mut self, phys_base: u64, size: u64) -> MmioMapping {
        let mut mapper = page_table_mapper(self.phys_offset);
        let mut allocator = HeapFrameAllocator {
            kernel_offset: self.kernel_offset,
        };

        let virt_base = self.next_vaddr;
        let num_pages = size.div_ceil(0x1000);
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE;

        for i in 0..num_pages {
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt_base + i * 0x1000));
            let frame = PhysFrame::containing_address(PhysAddr::new(phys_base + i * 0x1000));
            unsafe {
                mapper
                    .map_to(page, frame, flags, &mut allocator)
                    .expect("MMIO map_to failed")
                    .flush();
            }
        }

        self.next_vaddr = virt_base + (num_pages + 1) * 0x1000;

        log::info!(
            "MMIO: mapped phys {:#x}..{:#x} to virt {:#x} ({} pages)",
            phys_base,
            phys_base + size,
            virt_base,
            num_pages
        );

        MmioMapping {
            vaddr: virt_base as usize,
            size,
        }
    }

    /// Map a physical region as executable code pages (cached, no NO_EXECUTE).
    /// Unlike `map_mmio`, this does not set NO_CACHE — suitable for code pages
    /// like the Hyper-V hypercall page.
    pub fn map_code(&mut self, phys_base: u64, size: u64) -> MmioMapping {
        let mut mapper = page_table_mapper(self.phys_offset);
        let mut allocator = HeapFrameAllocator {
            kernel_offset: self.kernel_offset,
        };

        let virt_base = self.next_vaddr;
        let num_pages = size.div_ceil(0x1000);
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;

        for i in 0..num_pages {
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt_base + i * 0x1000));
            let frame = PhysFrame::containing_address(PhysAddr::new(phys_base + i * 0x1000));
            unsafe {
                mapper
                    .map_to(page, frame, flags, &mut allocator)
                    .expect("code map_to failed")
                    .flush();
            }
        }

        self.next_vaddr = virt_base + (num_pages + 1) * 0x1000;

        log::info!(
            "CODE: mapped phys {:#x} to virt {:#x} ({} pages)",
            phys_base,
            virt_base,
            num_pages
        );

        MmioMapping {
            vaddr: virt_base as usize,
            size,
        }
    }

    /// Translate a virtual address to its physical address via page table walk.
    pub fn translate_addr(&self, vaddr: u64) -> Option<u64> {
        let mapper = page_table_mapper(self.phys_offset);
        mapper
            .translate_addr(VirtAddr::new(vaddr))
            .map(|p| p.as_u64())
    }

    /// Unmap a previously mapped MMIO region.
    ///
    /// Removes page table entries and flushes the TLB.
    ///
    /// # Safety
    /// The caller must ensure no references to the mapped memory exist
    /// (e.g., `MmioRegs` pointing into this region). Using the virtual
    /// addresses after this call is undefined behavior.
    pub unsafe fn unmap_mmio(&self, mapping: &MmioMapping) {
        let mut mapper = page_table_mapper(self.phys_offset);
        let num_pages = mapping.size.div_ceil(0x1000);

        for i in 0..num_pages {
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(
                mapping.vaddr as u64 + i * 0x1000,
            ));
            let (_frame, flush) = mapper.unmap(page).expect("MMIO unmap failed");
            flush.flush();
        }

        log::info!(
            "MMIO: unmapped virt {:#x} ({} pages)",
            mapping.vaddr,
            num_pages
        );
    }
}

pub(crate) fn page_table_mapper(phys_offset: u64) -> OffsetPageTable<'static> {
    let cr3_phys = x86_64::registers::control::Cr3::read()
        .0
        .start_address()
        .as_u64();
    let l4_table =
        unsafe { &mut *((cr3_phys + phys_offset) as *mut x86_64::structures::paging::PageTable) };
    unsafe { OffsetPageTable::new(l4_table, VirtAddr::new(phys_offset)) }
}

struct HeapFrameAllocator {
    kernel_offset: u64,
}

unsafe impl x86_64::structures::paging::FrameAllocator<Size4KiB> for HeapFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        let layout = core::alloc::Layout::from_size_align(4096, 4096).ok()?;
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return None;
        }
        let paddr = ptr as u64 - self.kernel_offset;
        Some(PhysFrame::containing_address(PhysAddr::new(paddr)))
    }
}
