//! Driver traits, `ProbeCtx`, and `ProbedNic`.

use crate::error::ProbeError;
use crate::nic::EmbcloxNic;
use alloc::boxed::Box;
use embclox_core::dma_alloc::BootDmaAllocator;
use embclox_hal_x86::idt;
use embclox_hal_x86::ioapic::IoApic;
use embclox_hal_x86::memory::MemoryMapper;
use embclox_hal_x86::pci::{PciBus, PciDevice};
use embclox_hal_x86::vector_alloc::{InstalledIsr, VectorAllocator};
use embclox_hyperv::{ChannelOffer, VmBus};
use x86_64::structures::idt::InterruptStackFrame;

/// Per-driver capabilities passed into `probe()`.
///
/// Holds `&mut` borrows of system singletons (IOAPIC, vector allocator,
/// memory mapper, optional VMBus), so probe is serialised on the BSP.
/// The borrow checker is what enforces this — no global state, no locks.
///
/// `dma` is concrete (`&BootDmaAllocator`) rather than `&dyn DmaAllocator`
/// so drivers that need an owned allocator (e1000, tulip) can `.clone()`
/// it; the type is `Clone` and 16 bytes.
///
/// `vmbus` is `Option<&'a mut VmBus>` because the host may not be
/// Hyper-V at all; PCI drivers never touch it.
pub struct ProbeCtx<'a> {
    pub dma: &'a BootDmaAllocator,
    pub memory: &'a mut MemoryMapper,
    pub ioapic: &'a mut IoApic,
    pub irq_alloc: &'a mut VectorAllocator,
    pub pci: &'a PciBus,
    pub vmbus: Option<&'a mut VmBus>,
}

impl ProbeCtx<'_> {
    /// Install an INTx-line ISR for a PCI device. Allocates an IDT
    /// vector, points the IDT entry at `handler`, and routes the
    /// IOAPIC pin to that vector.
    ///
    /// `line` is the PCI interrupt line from config space offset 0x3C.
    pub fn install_pci_isr(
        &mut self,
        line: u8,
        handler: extern "x86-interrupt" fn(InterruptStackFrame),
    ) -> Result<InstalledIsr, ProbeError> {
        if line == 0xFF {
            return Err(ProbeError::InvalidIrqLine(line));
        }
        if line as usize >= self.ioapic.max_entries() as usize {
            return Err(ProbeError::InvalidIrqLine(line));
        }
        let isr = self.irq_alloc.allocate().ok_or(ProbeError::NoFreeVector)?;
        unsafe { idt::set_handler(isr.vector, handler) };
        self.ioapic
            .enable_irq(line, isr.vector, isr.cpu_id.apic_id());
        Ok(isr)
    }
}

/// PCI driver. Implemented per-device-family (e1000, tulip, ...).
pub trait PciDriver: Send + Sync {
    fn name(&self) -> &'static str;
    /// Priority for multi-NIC selection; lower wins. See the design
    /// doc's "Multi-NIC" section.
    fn priority(&self) -> u8;
    fn matches(&self, dev: &PciDevice) -> bool;
    fn probe(
        &self,
        dev: PciDevice,
        ctx: &mut ProbeCtx<'_>,
    ) -> Result<Box<dyn EmbcloxNic>, ProbeError>;
}

/// VMBus driver. Implemented per-synthetic-device family (NetVSC, ...).
pub trait VmBusDriver: Send + Sync {
    fn name(&self) -> &'static str;
    fn priority(&self) -> u8;
    fn matches(&self, offer: &ChannelOffer) -> bool;
    fn probe(
        &self,
        offer: ChannelOffer,
        ctx: &mut ProbeCtx<'_>,
    ) -> Result<Box<dyn EmbcloxNic>, ProbeError>;
}

/// Wrapper around a successfully-probed NIC that preserves driver
/// identity past the `Box<dyn EmbcloxNic>` erasure.
pub struct ProbedNic {
    pub driver: Box<dyn EmbcloxNic>,
    pub priority: u8,
    pub name: &'static str,
}

impl ProbedNic {
    pub fn new(driver: Box<dyn EmbcloxNic>, priority: u8, name: &'static str) -> Self {
        Self {
            driver,
            priority,
            name,
        }
    }
}
