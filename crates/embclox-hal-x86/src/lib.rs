#![no_std]
#![feature(abi_x86_interrupt)]

extern crate alloc;

pub mod apic;
pub mod cmdline;
pub mod critical_section_impl;
pub mod heap;
pub mod idt;
pub mod ioapic;
pub mod limine_boot;
pub mod memory;
pub mod pci;
pub mod pic;
pub mod pit;
pub mod runtime;
pub mod serial;
pub mod time;

use core::sync::atomic::{AtomicBool, Ordering};

pub use limine_boot::LimineBootInfo;

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// HAL configuration.
pub struct Config {
    pub serial_port: u16,
    pub heap_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            serial_port: 0x3F8,
            heap_size: 8 * 1024 * 1024, // 8 MiB
        }
    }
}

/// Platform peripherals returned by [`init`].
pub struct Peripherals {
    pub serial: serial::Serial,
    pub pci: pci::PciBus,
    pub memory: memory::MemoryMapper,
}

/// Initialize the HAL. Can only be called once (panics on second call).
///
/// Initializes serial, heap, and memory mapper in order from a Limine
/// [`LimineBootInfo`]. The Limine `HhdmRequest` provides the physical
/// memory mapping offset; `ExecutableAddressRequest` provides the kernel
/// virtual/physical base offset.
///
/// Use [`crate::limine_boot_requests!`] in your `kmain` crate to declare
/// the standard request statics, then call `<mod>::collect()` to obtain
/// the `LimineBootInfo` to pass here.
pub fn init(boot_info: LimineBootInfo<'_>, config: Config) -> Peripherals {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        panic!("embclox_hal_x86::init() called more than once");
    }

    let serial = serial::Serial::new(config.serial_port);
    serial::init_global(serial.clone());

    heap::init(config.heap_size);
    log::info!("Heap initialized ({} KiB)", config.heap_size / 1024);

    log::info!("HHDM offset: {:#x}", boot_info.hhdm_offset);
    log::info!("Kernel offset: {:#x}", boot_info.kernel_offset);

    let memory = memory::MemoryMapper::new(boot_info.hhdm_offset, boot_info.kernel_offset);

    Peripherals {
        serial,
        pci: pci::PciBus,
        memory,
    }
}
