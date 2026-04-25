use x86_64::structures::paging::{
    Mapper, OffsetPageTable, Page, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

/// Frame allocator that allocates from the heap.
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

/// Map MMIO physical region to a new virtual address range with Uncacheable (PCD) flags.
/// Returns the virtual address of the mapped region.
///
/// Instead of modifying existing page table entries (which is fragile with huge pages),
/// we create a fresh mapping at a new virtual address with proper UC flags using the
/// `x86_64` crate's `OffsetPageTable` mapper.
pub fn map_mmio(phys_offset: u64, kernel_offset: u64, phys_base: u64, size: u64) -> usize {
    let cr3_phys = x86_64::registers::control::Cr3::read()
        .0
        .start_address()
        .as_u64();
    let l4_table =
        unsafe { &mut *((cr3_phys + phys_offset) as *mut x86_64::structures::paging::PageTable) };
    let mut mapper = unsafe { OffsetPageTable::new(l4_table, VirtAddr::new(phys_offset)) };
    let mut allocator = HeapFrameAllocator { kernel_offset };

    // Use a virtual address range above the kernel and phys_offset mappings
    let virt_base = 0x4000_0000_0000u64; // 64 TiB — unused region
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

    log::info!(
        "MMIO: mapped phys {:#x}..{:#x} to virt {:#x} with NO_CACHE ({} pages)",
        phys_base,
        phys_base + size,
        virt_base,
        num_pages
    );

    virt_base as usize
}
