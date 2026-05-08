use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use embclox_dma::{DmaAllocator, DmaRegion};
use log::*;

/// DmaAllocator implementation for x86_64 with Limine HHDM-mapped memory.
///
/// Translates kernel heap addresses to physical addresses for DMA, and
/// returns virtual addresses through Limine's higher-half direct map so
/// CPU reads are coherent with DMA writes in QEMU TCG mode.
#[derive(Clone)]
pub struct BootDmaAllocator {
    /// Offset to convert kernel virtual addresses to physical:
    ///   paddr = kernel_vaddr - kernel_offset
    pub kernel_offset: u64,
    /// Bootloader's physical memory offset:
    ///   phys_mem_vaddr = paddr + phys_offset
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

    unsafe fn free_coherent(&self, region: &DmaRegion) {
        let paddr = region.vaddr - self.phys_offset as usize;
        let heap_vaddr = paddr + self.kernel_offset as usize;
        let layout = Layout::from_size_align(region.size, 4096).unwrap();
        unsafe { dealloc(heap_vaddr as *mut u8, layout) };
    }
}
