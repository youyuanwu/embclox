use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use embclox_e1000::dma::{DmaAllocator, DmaRegion};
use log::*;

/// DmaAllocator for x86_64 with bootloader offset-mapped memory.
pub struct BootDmaAllocator {
    pub kernel_offset: u64,
    pub phys_offset: u64,
}

impl DmaAllocator for BootDmaAllocator {
    fn alloc_coherent(&self, size: usize, align: usize) -> DmaRegion {
        let layout = Layout::from_size_align(size, align).expect("invalid DMA layout");
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "DMA allocation of {} bytes failed", size);
        let heap_vaddr = ptr as usize;
        let paddr = heap_vaddr - self.kernel_offset as usize;
        let vaddr = paddr + self.phys_offset as usize;
        info!(
            "DMA alloc: {} bytes, paddr={:#x}, vaddr={:#x}",
            size, paddr, vaddr
        );
        DmaRegion { vaddr, paddr, size }
    }

    fn free_coherent(&self, region: &DmaRegion) {
        let paddr = region.vaddr - self.phys_offset as usize;
        let heap_vaddr = paddr + self.kernel_offset as usize;
        let layout = Layout::from_size_align(region.size, 4096).unwrap();
        unsafe { dealloc(heap_vaddr as *mut u8, layout) };
    }
}
