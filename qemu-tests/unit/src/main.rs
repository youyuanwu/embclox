#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;
extern crate embclox_hal_x86;

mod harness;
mod suites;

use core::panic::PanicInfo;
use embclox_core::mmio_regs::MmioRegs;
use log::*;

embclox_hal_x86::limine_boot_requests!(limine_boot);

#[unsafe(no_mangle)]
unsafe extern "C" fn kmain() -> ! {
    let boot_info = limine_boot::collect();
    let mut p = embclox_hal_x86::init(boot_info, embclox_hal_x86::Config::default());
    info!("=== embclox test runner ===");

    let mut total = 0usize;

    // --- HAL tests (no device setup needed) ---

    // hal_pci: PciBus is zero-sized, no init needed
    let (name, tests) = suites::hal_pci::suite();
    total += harness::run_suite(name, tests);

    // hal_memory: init with a MemoryMapper, run before e1000 maps BAR0
    // (tests map/unmap cleanly, leaving pages free for later use)
    unsafe {
        suites::hal_memory::init(p.memory.phys_offset(), p.memory.kernel_offset());
    }
    let (name, tests) = suites::hal_memory::suite();
    total += harness::run_suite(name, tests);

    // --- e1000 tests (need PCI scan, BAR0 map, device reset) ---

    let pci_dev = p
        .pci
        .find_device_any(0x8086, &[0x100E, 0x100F, 0x10D3])
        .expect("e1000 device not found on PCI bus");
    let bar0_phys = p.pci.read_bar(&pci_dev, 0);
    let e1000_mmio = p.memory.map_mmio(bar0_phys, 0x20000);
    let regs = MmioRegs::new(e1000_mmio.vaddr());
    embclox_core::e1000_helpers::reset_device(&regs);
    p.pci.enable_bus_mastering(&pci_dev);

    let dma = embclox_core::dma_alloc::BootDmaAllocator {
        kernel_offset: p.memory.kernel_offset(),
        phys_offset: p.memory.phys_offset(),
    };

    unsafe {
        suites::e1000_smoke::init(regs, dma.clone());
        suites::e1000_driver::init(regs, dma.clone());
        suites::e1000_embassy::init(regs, dma);
    }

    let (name, tests) = suites::e1000_smoke::suite();
    total += harness::run_suite(name, tests);

    let (name, tests) = suites::e1000_driver::suite();
    total += harness::run_suite(name, tests);

    let (name, tests) = suites::e1000_embassy::suite();
    total += harness::run_suite(name, tests);

    // Clean up MMIO mapping
    // Safety: all e1000 tests are done, no references to mapped memory remain.
    unsafe { p.memory.unmap_mmio(&e1000_mmio) };

    info!("=== {} passed ===", total);
    harness::qemu_exit(0);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    error!("PANIC: {}", info);
    harness::qemu_exit(1);
}
