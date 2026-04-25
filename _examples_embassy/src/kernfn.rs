use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use log::*;

/// KernelFunc implementation for x86_64 with bootloader offset-mapped memory.
pub struct Kernfn {
    /// Offset to convert kernel virtual addresses to physical:
    ///   paddr = kernel_vaddr - kernel_offset
    pub kernel_offset: u64,
    /// Bootloader's physical memory offset:
    ///   phys_mem_vaddr = paddr + phys_offset
    /// We return vaddr through this mapping so the driver's DMA memory
    /// accesses go through the same TLB path as QEMU's DMA writes.
    pub phys_offset: u64,
}

impl e1000_driver::e1000::KernelFunc for Kernfn {
    const PAGE_SIZE: usize = 4096;

    fn dma_alloc_coherent(&mut self, pages: usize) -> (usize, usize) {
        let size = pages * Self::PAGE_SIZE;
        let layout = Layout::from_size_align(size, Self::PAGE_SIZE).expect("invalid DMA layout");
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "DMA allocation of {} pages failed", pages);
        let heap_vaddr = ptr as usize;
        let paddr = heap_vaddr - self.kernel_offset as usize;
        // Return vaddr through the phys_offset mapping so CPU reads are
        // coherent with DMA writes in QEMU TCG mode.
        let vaddr = paddr + self.phys_offset as usize;
        info!(
            "DMA alloc: {} pages, paddr={:#x}, vaddr={:#x}",
            pages, paddr, vaddr
        );
        (vaddr, paddr)
    }

    fn dma_free_coherent(&mut self, vaddr: usize, pages: usize) {
        // Convert phys_offset vaddr back to kernel vaddr for dealloc
        let paddr = vaddr - self.phys_offset as usize;
        let heap_vaddr = paddr + self.kernel_offset as usize;
        let size = pages * Self::PAGE_SIZE;
        let layout = Layout::from_size_align(size, Self::PAGE_SIZE).unwrap();
        unsafe { dealloc(heap_vaddr as *mut u8, layout) };
    }
}
