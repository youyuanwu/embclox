//! Error returned by `ProbeCtx` and `Driver::probe`.

/// All ways probing can fail.
///
/// Returned by [`crate::ProbeCtx::install_pci_isr`] and the
/// `probe()` method on [`crate::PciDriver`] / [`crate::VmBusDriver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeError {
    /// PCI config 0x3C reported 0xFF (no IRQ wired) or a pin index
    /// outside the IOAPIC's redirection table.
    InvalidIrqLine(u8),
    /// `VectorAllocator` ran out of free IDT vectors.
    NoFreeVector,
    /// `MemoryMapper::map_mmio` failed (out of HHDM space, etc.).
    MmioMap,
    /// `BootDmaAllocator` allocation failed (heap exhausted).
    DmaAlloc,
    /// Driver-specific failure (link timeout, EEPROM CRC, VMBus
    /// channel-open NAK, ...). Carries a `&'static str` so each
    /// driver can give a one-line reason without dragging in
    /// `alloc::string::String` here.
    Driver(&'static str),
}

impl core::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProbeError::InvalidIrqLine(l) => write!(f, "invalid PCI IRQ line {l:#x}"),
            ProbeError::NoFreeVector => write!(f, "no free IDT vector"),
            ProbeError::MmioMap => write!(f, "MMIO mapping failed"),
            ProbeError::DmaAlloc => write!(f, "DMA allocation failed"),
            ProbeError::Driver(msg) => write!(f, "driver error: {msg}"),
        }
    }
}
