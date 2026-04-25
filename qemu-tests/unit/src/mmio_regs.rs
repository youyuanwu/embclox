use embclox_e1000::RegisterAccess;

/// MMIO register access via UC-mapped volatile pointer.
#[derive(Clone, Copy)]
pub struct MmioRegs {
    base: usize,
}

impl MmioRegs {
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
