//! DEC 21140/21143 Tulip PCI driver wrapper.

use crate::driver::{PciDriver, ProbeCtx};
use crate::error::ProbeError;
use crate::nic::EmbcloxNic;
use alloc::boxed::Box;
use core::sync::atomic::Ordering;
use core::task::{Context, Waker};
use embassy_net_driver::{Capabilities, HardwareAddress, LinkState};
use embclox_core::dma_alloc::BootDmaAllocator;
use embclox_core::tulip_embassy::TULIP_WAKER;
use embclox_hal_x86::pci::PciDevice;
use embclox_hal_x86::runtime;
use embclox_tulip::csr;
use embclox_tulip::TulipDevice;
use x86_64::structures::idt::InterruptStackFrame;

const VENDOR_DEC: u16 = 0x1011;
const TULIP_DEVICES: &[u16] = &[0x0009, 0x0019];

pub struct TulipDriver;

impl PciDriver for TulipDriver {
    fn name(&self) -> &'static str {
        "tulip"
    }
    fn priority(&self) -> u8 {
        30
    }
    fn matches(&self, dev: &PciDevice) -> bool {
        dev.vendor == VENDOR_DEC && TULIP_DEVICES.contains(&dev.device)
    }
    fn probe(
        &self,
        dev: PciDevice,
        ctx: &mut ProbeCtx<'_>,
    ) -> Result<Box<dyn EmbcloxNic>, ProbeError> {
        ctx.pci.enable_bus_mastering(&dev);

        let bar0_raw = ctx.pci.read_config(&dev, 0x10);
        let is_io = (bar0_raw & 1) != 0;
        let (csr_access, mmio_holder) = if is_io {
            let io_base = (bar0_raw & !0x3) as u16;
            log::info!("tulip: I/O port {:#x}", io_base);
            (csr::CsrAccess::Io(io_base), None)
        } else {
            let bar0_phys = ctx.pci.read_bar(&dev, 0);
            let mmio = ctx.memory.map_mmio(bar0_phys, 0x1000);
            let base = mmio.vaddr();
            log::info!("tulip: MMIO {:#x}", base);
            (csr::CsrAccess::Mmio(base), Some(mmio))
        };

        // Publish CSR for the static ISR before wiring the interrupt.
        #[allow(clippy::deref_addrof)]
        unsafe {
            *(&raw mut CSR_FOR_ISR) = Some(csr_access);
        }
        // Memory barrier so the ISR sees the publication.
        core::sync::atomic::fence(Ordering::Release);

        let dma = ctx.dma.clone();
        let device = TulipDevice::new(csr_access, dma);
        let mac = device.mac();

        let line = (ctx.pci.read_config(&dev, 0x3C) & 0xFF) as u8;
        let isr = ctx.install_pci_isr(line, tulip_handler)?;
        log::info!(
            "tulip: PCI IRQ line {} -> vector {} (cpu {:?})",
            line,
            isr.vector,
            isr.cpu_id
        );

        device.enable_interrupts();

        Ok(Box::new(NicTulip {
            device,
            mac,
            _mmio: mmio_holder,
        }))
    }
}

// ---- EmbcloxNic wrapper ------------------------------------------------

struct NicTulip {
    device: TulipDevice<BootDmaAllocator>,
    mac: [u8; 6],
    /// MMIO mapping kept alive for the program lifetime when BAR0 is
    /// memory-mapped. Plain handle, no `Drop`; held here so ownership
    /// is explicit even though the underlying page-table entries are
    /// never torn down. `None` for I/O-port BAR0s.
    _mmio: Option<embclox_hal_x86::memory::MmioMapping>,
}

unsafe impl Send for NicTulip {}

impl EmbcloxNic for NicTulip {
    fn rx_ready(&mut self) -> bool {
        self.device.has_rx_packet()
    }
    fn tx_ready(&mut self) -> bool {
        self.device.has_tx_space()
    }
    fn register_waker(&mut self, waker: &Waker) {
        TULIP_WAKER.register(waker);
    }
    fn recv_with(&mut self, f: &mut dyn FnMut(&mut [u8])) {
        self.device
            .recv_with(|buf| f(buf))
            .expect("tulip: recv_with called without ready packet");
    }
    fn transmit_with(&mut self, len: usize, f: &mut dyn FnMut(&mut [u8])) {
        self.device
            .transmit_with(len, |buf| f(buf))
            .expect("tulip: transmit_with called without TX space");
    }
    fn link_state(&mut self, _cx: &mut Context<'_>) -> LinkState {
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

static mut CSR_FOR_ISR: Option<csr::CsrAccess> = None;

extern "x86-interrupt" fn tulip_handler(_frame: InterruptStackFrame) {
    // Read-clear status, ack the IOAPIC, then wake the runner.
    unsafe {
        let csr_ptr = &raw const CSR_FOR_ISR;
        if let Some(c) = &*csr_ptr {
            c.write(csr::CSR7, 0);
            let status = c.read(csr::CSR5);
            c.write(csr::CSR5, status);
            c.write(
                csr::CSR7,
                csr::CSR7_TIE | csr::CSR7_RIE | csr::CSR7_NIE | csr::CSR7_AIE,
            );
        }
    }
    TULIP_WAKER.wake();
    runtime::lapic_eoi();
}
