use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use e1000::dma::{DmaAllocator, DmaRegion};
use log::*;

/// DmaAllocator implementation for x86_64 with bootloader offset-mapped memory.
pub struct BootDmaAllocator {
    /// Offset to convert kernel virtual addresses to physical:
    ///   paddr = kernel_vaddr - kernel_offset
    pub kernel_offset: u64,
    /// Bootloader's physical memory offset:
    ///   phys_mem_vaddr = paddr + phys_offset
    /// We return vaddr through this mapping so the driver's DMA memory
    /// accesses go through the same TLB path as QEMU's DMA writes.
    pub phys_offset: u64,
}

impl DmaAllocator for BootDmaAllocator {
    fn alloc_coherent(&self, size: usize, align: usize) -> DmaRegion {
        let layout = Layout::from_size_align(size, align).expect("invalid DMA layout");
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "DMA allocation of {} bytes failed", size);
        let heap_vaddr = ptr as usize;
        let paddr = heap_vaddr - self.kernel_offset as usize;
        // Return vaddr through the phys_offset mapping so CPU reads are
        // coherent with DMA writes in QEMU TCG mode.
        let vaddr = paddr + self.phys_offset as usize;
        info!(
            "DMA alloc: {} bytes, paddr={:#x}, vaddr={:#x}",
            size, paddr, vaddr
        );
        DmaRegion { vaddr, paddr, size }
    }

    fn free_coherent(&self, region: &DmaRegion) {
        // Convert phys_offset vaddr back to kernel vaddr for dealloc
        let paddr = region.vaddr - self.phys_offset as usize;
        let heap_vaddr = paddr + self.kernel_offset as usize;
        let layout = Layout::from_size_align(region.size, 4096).unwrap();
        unsafe { dealloc(heap_vaddr as *mut u8, layout) };
    }
}
