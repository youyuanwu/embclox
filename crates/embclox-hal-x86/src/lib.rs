#![no_std]
#![feature(abi_x86_interrupt)]

extern crate alloc;

pub mod apic;
pub mod critical_section_impl;
pub mod heap;
pub mod idt;
pub mod ioapic;
pub mod memory;
pub mod pci;
pub mod pic;
pub mod pit;
pub mod serial;
pub mod time;

use bootloader_api::BootInfo;
use core::sync::atomic::{AtomicBool, Ordering};
use x86_64::structures::paging::Translate;
use x86_64::VirtAddr;

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
            heap_size: 4 * 1024 * 1024, // 4 MiB
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
/// Initializes serial, heap, and memory mapper in order.
/// `kernel_offset` and `phys_offset` are computed from `BootInfo`.
pub fn init(boot_info: &'static mut BootInfo, config: Config) -> Peripherals {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        panic!("embclox_hal_x86::init() called more than once");
    }

    let serial = serial::Serial::new(config.serial_port);
    serial::init_global(serial.clone());

    heap::init(config.heap_size);
    log::info!("Heap initialized ({} KiB)", config.heap_size / 1024);

    let phys_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("physical_memory_offset not available");

    // Compute kernel_offset dynamically by probing the page tables.
    // kernel_offset is the linear shift between kernel virtual addresses and
    // physical addresses (i.e. kernel_vaddr - paddr). It is constant for all
    // kernel-mapped memory, so we only need to probe once with any known
    // kernel address. We use the same OffsetPageTable that MemoryMapper wraps
    // internally, but after bootstrap we store the offset as a plain u64 for
    // cheap arithmetic instead of repeating page-table walks.
    let kernel_offset: u64 = {
        let mapper = memory::page_table_mapper(phys_offset);
        let probe_vaddr = VirtAddr::new(&INITIALIZED as *const _ as u64);
        // Walk the page tables to resolve the physical address of the probe.
        let probe_paddr = mapper
            .translate_addr(probe_vaddr)
            .expect("failed to translate probe address for kernel_offset");
        probe_vaddr.as_u64() - probe_paddr.as_u64()
    };

    log::info!("Physical memory offset: {:#x}", phys_offset);
    log::info!("Kernel offset: {:#x}", kernel_offset);

    let memory = memory::MemoryMapper::new(phys_offset, kernel_offset);

    Peripherals {
        serial,
        pci: pci::PciBus,
        memory,
    }
}
