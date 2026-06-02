//! MMIO register accessor for e1000 BAR0 (UC-mapped).
//!
//! Word-indexed 32-bit volatile reads/writes. The base address must
//! be a kernel-virtual pointer to a UC (uncached) mapping of the
//! device's BAR0 region — typically established by
//! [`embclox_hal_x86::memory::MemoryMapper::map_mmio`].

use crate::RegisterAccess;

/// MMIO register access via UC-mapped volatile pointer.
///
/// Wraps a base virtual address (must be UC-mapped) and implements
/// [`RegisterAccess`] using volatile reads/writes at word-index offsets.
#[derive(Clone, Copy)]
pub struct MmioRegs {
    base: usize,
}

impl MmioRegs {
    /// Create a new `MmioRegs` accessor for the given UC-mapped base address.
    pub fn new(base: usize) -> Self {
        Self { base }
    }
}

impl RegisterAccess for MmioRegs {
    fn read_reg(&self, offset: usize) -> u32 {
        let ptr = (self.base + offset * 4) as *const u32;
        unsafe { core::ptr::read_volatile(ptr) }
    }

    fn write_reg(&self, offset: usize, value: u32) {
        let ptr = (self.base + offset * 4) as *mut u32;
        unsafe { core::ptr::write_volatile(ptr, value) }
    }
}
