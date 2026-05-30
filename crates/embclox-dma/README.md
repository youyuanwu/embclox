# embclox-dma

Minimal `no_std` DMA allocation traits used by the device-driver
crates in this workspace.

```rust
pub trait DmaAllocator {
    fn alloc_coherent(&self, size: usize, align: usize) -> DmaRegion;
    unsafe fn free_coherent(&self, region: &DmaRegion);
}

pub struct DmaRegion {
    pub vaddr: usize,
    pub paddr: usize,
    pub size: usize,
}
```

Driver crates (`embclox-e1000`, `embclox-tulip`, `embclox-hyperv`)
take an `&impl DmaAllocator` so each consumer can supply its own
implementation:

- **`examples-kernel` and `qemu-tests/unit`** use
  `BootDmaAllocator` (heap-backed, translates kernel virt→phys via
  the offsets that `embclox_hal_x86::init` reads from Limine).
  Both the e1000 and tulip code paths inside `examples-kernel`
  share this allocator. (The retired standalone `examples-tulip`
  binary previously had its own bump allocator over the Limine
  HHDM-mapped sub-4GB pool; that crate was retired with Phase 3d
  and the `BootDmaAllocator` is now the only in-tree DmaAllocator
  implementation.)

Keeping the allocator out of the driver crates lets each consumer
pick the strategy that matches its DMA footprint.
