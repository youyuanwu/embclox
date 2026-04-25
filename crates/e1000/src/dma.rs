/// A region of DMA-coherent memory.
pub struct DmaRegion {
    pub vaddr: usize,
    pub paddr: usize,
    pub size: usize,
}

/// Allocator for DMA-coherent memory regions.
///
/// Uses `&self` — implementations use interior mutability if needed.
/// `alloc_coherent` panics on failure (boot-time, fixed system).
pub trait DmaAllocator {
    /// Allocate DMA-coherent memory with the given size and alignment.
    /// Panics on failure.
    fn alloc_coherent(&self, size: usize, align: usize) -> DmaRegion;

    /// Free a previously allocated DMA region.
    fn free_coherent(&self, region: &DmaRegion);
}
