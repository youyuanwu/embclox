#![no_std]

extern crate alloc;

pub mod critical_section_impl;
pub mod heap;
pub mod memory;
pub mod pci;
pub mod serial;
pub mod time;

use bootloader_api::BootInfo;
use core::sync::atomic::{AtomicBool, Ordering};

static INITIALIZED: AtomicBool = AtomicBool::new(false);

pub struct Config {
    pub serial_port: u16,
    pub heap_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            serial_port: 0x3F8,
            heap_size: 4 * 1024 * 1024, // 4 MiB
        }
    }
}

pub struct Peripherals {
    pub serial: serial::Serial,
    pub pci: pci::PciBus,
    pub memory: memory::MemoryMapper,
}

/// Initialize the HAL. Can only be called once (panics on second call).
///
/// Initializes serial, heap, and memory mapper in order.
/// `kernel_offset` and `phys_offset` are computed from `BootInfo`.
pub fn init(boot_info: &'static mut BootInfo, config: Config) -> Peripherals {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        panic!("hal_x86::init() called more than once");
    }

    let serial = serial::Serial::new(config.serial_port);
    serial::init_global(serial.clone());

    heap::init(config.heap_size);
    log::info!("Heap initialized ({} KiB)", config.heap_size / 1024);

    let phys_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("physical_memory_offset not available");

    // kernel_offset: the difference between kernel virtual addresses and
    // physical addresses. Derived from the bootloader's physical_memory_offset
    // and a known virtual address. For bootloader v0.11 on QEMU this is
    // consistently 0xFFFF000000. A future improvement could compute this
    // dynamically from the memory map.
    let kernel_offset: u64 = 0xFFFF000000;

    log::info!("Physical memory offset: {:#x}", phys_offset);
    log::info!("Kernel offset: {:#x}", kernel_offset);

    let memory = memory::MemoryMapper::new(phys_offset, kernel_offset);

    Peripherals {
        serial,
        pci: pci::PciBus,
        memory,
    }
}
