use x86_64::structures::paging::{
    Mapper, OffsetPageTable, Page, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

const MMIO_VIRT_BASE: u64 = 0x4000_0000_0000;

pub struct MemoryMapper {
    phys_offset: u64,
    kernel_offset: u64,
    next_vaddr: u64,
}

impl MemoryMapper {
    pub fn new(phys_offset: u64, kernel_offset: u64) -> Self {
        Self {
            phys_offset,
            kernel_offset,
            next_vaddr: MMIO_VIRT_BASE,
        }
    }

    pub fn phys_offset(&self) -> u64 {
        self.phys_offset
    }

    pub fn kernel_offset(&self) -> u64 {
        self.kernel_offset
    }

    /// Map an MMIO physical region with Uncacheable 4KB pages.
    /// Returns the virtual address. Each call maps at a new address.
    pub fn map_mmio(&mut self, phys_base: u64, size: u64) -> usize {
        let cr3_phys = x86_64::registers::control::Cr3::read()
            .0
            .start_address()
            .as_u64();
        let l4_table = unsafe {
            &mut *((cr3_phys + self.phys_offset) as *mut x86_64::structures::paging::PageTable)
        };
        let mut mapper = unsafe { OffsetPageTable::new(l4_table, VirtAddr::new(self.phys_offset)) };
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

        // Advance cursor past mapped region + 1 guard page
        self.next_vaddr = virt_base + (num_pages + 1) * 0x1000;

        log::info!(
            "MMIO: mapped phys {:#x}..{:#x} to virt {:#x} ({} pages)",
            phys_base,
            phys_base + size,
            virt_base,
            num_pages
        );

        virt_base as usize
    }
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
