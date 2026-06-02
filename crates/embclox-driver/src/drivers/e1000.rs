//! Intel e1000 PCI driver wrapper.

use crate::driver::{PciDriver, ProbeCtx};
use crate::error::ProbeError;
use crate::nic::EmbcloxNic;
use alloc::boxed::Box;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Waker};
use embassy_net_driver::{Capabilities, HardwareAddress, LinkState};
use embassy_sync::waitqueue::AtomicWaker;
use embclox_core::dma_alloc::BootDmaAllocator;
use embclox_core::mmio_regs::MmioRegs;
use embclox_e1000::E1000Device;
use embclox_hal_x86::pci::PciDevice;
use embclox_hal_x86::runtime;
use x86_64::structures::idt::InterruptStackFrame;

const VENDOR_INTEL: u16 = 0x8086;
const E1000_DEVICES: &[u16] = &[0x100E, 0x100F, 0x10D3];

/// Waker for the e1000 NIC. Signalled by [`e1000_handler`] (the ISR)
/// and registered by [`NicE1000::register_waker`]. Lives next to its
/// only producer + consumer rather than as a cross-crate `pub static`.
static NET_WAKER: AtomicWaker = AtomicWaker::new();

/// Marker registered with [`crate::DriverRegistry`].
pub struct E1000Driver;

impl PciDriver for E1000Driver {
    fn name(&self) -> &'static str {
        "e1000"
    }
    fn priority(&self) -> u8 {
        20
    }
    fn matches(&self, dev: &PciDevice) -> bool {
        dev.vendor == VENDOR_INTEL && E1000_DEVICES.contains(&dev.device)
    }
    fn probe(
        &self,
        dev: PciDevice,
        ctx: &mut ProbeCtx<'_>,
    ) -> Result<Box<dyn EmbcloxNic>, ProbeError> {
        let bar0_phys = ctx.pci.read_bar(&dev, 0);
        let mmio = ctx.memory.map_mmio(bar0_phys, 0x20000);
        let regs = MmioRegs::new(mmio.vaddr());

        embclox_core::e1000_helpers::reset_device(&regs);
        ctx.pci.enable_bus_mastering(&dev);

        let dma = ctx.dma.clone();
        let device = E1000Device::new(regs, dma);
        let mac = device.mac_address();

        // Publish MMIO base for the static ISR before wiring the
        // interrupt so the first edge is observable.
        E1000_REGS_BASE.store(mmio.vaddr(), Ordering::Release);

        let line = (ctx.pci.read_config(&dev, 0x3C) & 0xFF) as u8;
        let isr = ctx.install_pci_isr(line, e1000_handler)?;
        log::info!(
            "e1000: PCI IRQ line {} -> vector {} (cpu {:?})",
            line,
            isr.vector,
            isr.cpu_id
        );

        device.enable_interrupts();

        Ok(Box::new(NicE1000 {
            device,
            mac,
            _mmio: mmio,
        }))
    }
}

// ---- EmbcloxNic wrapper ------------------------------------------------

struct NicE1000 {
    device: E1000Device<MmioRegs, BootDmaAllocator>,
    mac: [u8; 6],
    /// MMIO mapping kept alive for the program lifetime. Plain handle,
    /// no Drop; held in this struct so ownership is explicit even
    /// though the underlying page-table entries never get torn down.
    _mmio: embclox_hal_x86::memory::MmioMapping,
}

// Safety: NicE1000 is owned by a single task. The ISR only touches
// NET_WAKER and E1000_REGS_BASE.
unsafe impl Send for NicE1000 {}

impl EmbcloxNic for NicE1000 {
    fn rx_ready(&mut self) -> bool {
        let (rx, _) = self.device.split();
        rx.has_rx_packet()
    }
    fn tx_ready(&mut self) -> bool {
        let (_, tx) = self.device.split();
        tx.has_tx_space()
    }
    fn register_waker(&mut self, waker: &Waker) {
        NET_WAKER.register(waker);
    }
    fn recv_with(&mut self, f: &mut dyn FnMut(&mut [u8])) {
        let (mut rx, _) = self.device.split();
        rx.recv_with(|buf| f(buf))
            .expect("e1000: recv_with called without ready packet");
    }
    fn transmit_with(&mut self, len: usize, f: &mut dyn FnMut(&mut [u8])) {
        let (_, mut tx) = self.device.split();
        tx.transmit_with(len, |buf| f(buf))
            .expect("e1000: transmit_with called without TX space");
    }
    fn link_state(&mut self, _cx: &mut Context<'_>) -> LinkState {
        // Real link tracking can be added when the driver exposes it.
        LinkState::Up
    }
    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::default();
        caps.max_transmission_unit = 1514;
        caps
    }
    fn hardware_address(&self) -> HardwareAddress {
        HardwareAddress::Ethernet(self.mac)
    }
}

// ---- static ISR --------------------------------------------------------

static E1000_REGS_BASE: AtomicUsize = AtomicUsize::new(0);

extern "x86-interrupt" fn e1000_handler(_frame: InterruptStackFrame) {
    // Ack the e1000 by reading ICR (read-clear).
    let base = E1000_REGS_BASE.load(Ordering::Acquire);
    if base != 0 {
        unsafe {
            // ICR is at byte offset 0xC0 = word index 0x30.
            core::ptr::read_volatile((base as *const u32).add(0x000C0 / 4));
        }
    }
    NET_WAKER.wake();
    runtime::lapic_eoi();
}
