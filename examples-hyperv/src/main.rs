#![no_std]
#![no_main]

extern crate alloc;

use core::arch::asm;
use core::fmt::Write;
use embclox_dma::{DmaAllocator, DmaRegion};
use limine::BaseRevision;
use limine::request::{
    FramebufferRequest, HhdmRequest, MemoryMapRequest, RequestsEndMarker, RequestsStartMarker,
    StackSizeRequest,
};
use x86_64::VirtAddr;
use x86_64::structures::paging::Translate;

// Limine protocol markers and requests

#[used]
#[unsafe(link_section = ".requests_start_marker")]
static _START_MARKER: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".requests_end_marker")]
static _END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

#[used]
#[unsafe(link_section = ".requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[unsafe(link_section = ".requests")]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
static MEMMAP_REQUEST: MemoryMapRequest = MemoryMapRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

#[used]
#[unsafe(link_section = ".requests")]
static STACK_SIZE_REQUEST: StackSizeRequest = StackSizeRequest::new().with_size(64 * 1024);

// Port I/O helpers

fn outb(port: u16, value: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) };
}

fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack)) };
    value
}

fn outl(port: u16, value: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack)) };
}

fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe { asm!("in eax, dx", in("dx") port, out("eax") value, options(nomem, nostack)) };
    value
}

/// Minimal serial port writer for early boot output.
struct SerialPort {
    port: u16,
}

impl SerialPort {
    const fn new(port: u16) -> Self {
        Self { port }
    }

    fn init(&self) {
        outb(self.port + 1, 0x00); // Disable interrupts
        outb(self.port + 3, 0x80); // Enable DLAB
        outb(self.port, 0x01); // Baud rate 115200 (divisor = 1)
        outb(self.port + 1, 0x00);
        outb(self.port + 3, 0x03); // 8 bits, no parity, 1 stop bit
        outb(self.port + 2, 0xC7); // Enable FIFO
        outb(self.port + 4, 0x0B); // RTS/DSR set
    }

    fn write_byte(&self, byte: u8) {
        // Bounded spin — Hyper-V virtual UART may not emulate LSR faithfully
        for _ in 0..10000u32 {
            if inb(self.port + 5) & 0x20 != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        outb(self.port, byte);
    }
}

impl Write for SerialPort {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

// Uses embclox-hal-x86's global allocator (linked_list_allocator in heap.rs)

// PCI config space access via port I/O
fn pci_config_read(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    let addr: u32 = (1 << 31)
        | ((bus as u32) << 16)
        | ((slot as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC);
    outl(0xCF8, addr);
    inl(0xCFC)
}

/// DMA allocator using Limine HHDM-mapped physical memory pool.
struct LimineDmaAllocator {
    hhdm_offset: u64,
}

/// Physical page allocator from Limine usable memory (sub-4GB).
static DMA_PHYS_NEXT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static DMA_PHYS_END: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Initialize the DMA physical memory pool from the Limine memory map.
fn init_dma_pool() {
    use core::sync::atomic::Ordering;
    if let Some(memmap) = MEMMAP_REQUEST.get_response() {
        let mut best_base = 0u64;
        let mut best_len = 0u64;
        for entry in memmap.entries().iter() {
            if entry.entry_type == limine::memory_map::EntryType::USABLE
                && entry.length > best_len
                && entry.base + entry.length <= 0xFFFF_FFFF
            {
                best_base = entry.base;
                best_len = entry.length;
            }
        }
        assert!(best_len > 0, "No usable physical memory below 4GB for DMA");
        DMA_PHYS_NEXT.store(best_base as usize, Ordering::Relaxed);
        DMA_PHYS_END.store((best_base + best_len) as usize, Ordering::Relaxed);
    }
}

impl DmaAllocator for LimineDmaAllocator {
    fn alloc_coherent(&self, size: usize, align: usize) -> DmaRegion {
        use core::sync::atomic::Ordering;
        loop {
            let cur = DMA_PHYS_NEXT.load(Ordering::Relaxed);
            let aligned = (cur + align - 1) & !(align - 1);
            let next = aligned + size;
            let end = DMA_PHYS_END.load(Ordering::Relaxed);
            assert!(next <= end, "DMA pool exhausted");
            if DMA_PHYS_NEXT
                .compare_exchange_weak(cur, next, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                let paddr = aligned;
                let vaddr = paddr + self.hhdm_offset as usize;
                unsafe {
                    core::ptr::write_bytes(vaddr as *mut u8, 0, size);
                }
                return DmaRegion { vaddr, paddr, size };
            }
        }
    }

    unsafe fn free_coherent(&self, _region: &DmaRegion) {
        // Bump allocator doesn't free
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn kmain() -> ! {
    let mut serial = SerialPort::new(0x3F8);
    serial.init();

    writeln!(serial, "embclox Hyper-V example booting via Limine...").ok();

    // Set up HAL serial logger so log::info! works inside crate code
    let hal_serial = embclox_hal_x86::serial::Serial::new(0x3F8);
    embclox_hal_x86::serial::init_global(hal_serial);

    assert!(BASE_REVISION.is_supported());
    writeln!(serial, "Limine base revision: supported").ok();

    // Get HHDM offset
    let hhdm_offset = HHDM_REQUEST.get_response().map(|r| r.offset()).unwrap_or(0);
    writeln!(serial, "HHDM offset: {:#x}", hhdm_offset).ok();

    // Init HAL heap (uses embclox-hal-x86's linked_list_allocator)
    embclox_hal_x86::heap::init(4 * 1024 * 1024);

    // Compute kernel_offset by probing the heap's page table mapping
    let kernel_offset = {
        let mapper = embclox_hal_x86::memory::page_table_mapper(hhdm_offset);
        let probe_vaddr = VirtAddr::new(embclox_hal_x86::heap::heap_start() as u64);
        let probe_paddr = mapper
            .translate_addr(probe_vaddr)
            .expect("failed to translate heap address for kernel_offset");
        probe_vaddr.as_u64() - probe_paddr.as_u64()
    };
    writeln!(serial, "Kernel offset: {:#x}", kernel_offset).ok();

    // Print memory map summary
    if let Some(memmap) = MEMMAP_REQUEST.get_response() {
        writeln!(serial, "Memory map: {} entries", memmap.entries().len()).ok();
    }

    // Check framebuffer
    if let Some(fb_response) = FRAMEBUFFER_REQUEST.get_response()
        && let Some(fb) = fb_response.framebuffers().next()
    {
        writeln!(
            serial,
            "Framebuffer: {}x{} bpp={}",
            fb.width(),
            fb.height(),
            fb.bpp(),
        )
        .ok();
    }

    // Init DMA pool from Limine memory map (sub-4GB usable region)
    init_dma_pool();

    writeln!(serial, "HYPERV BOOT PASSED").ok();

    // Scan PCI bus
    writeln!(serial, "Scanning PCI bus...").ok();
    for slot in 0..32u8 {
        let id = pci_config_read(0, slot, 0, 0);
        let vendor = id & 0xFFFF;
        let device = (id >> 16) & 0xFFFF;
        if vendor != 0xFFFF {
            let class = pci_config_read(0, slot, 0, 0x08);
            writeln!(
                serial,
                "  PCI {:02}:00.0 {:04x}:{:04x} class={:08x}",
                slot, vendor, device, class
            )
            .ok();
        }
    }

    // --- Hyper-V VMBus initialization ---

    let dma = LimineDmaAllocator { hhdm_offset };

    match embclox_hyperv::detect::detect() {
        Some(features) => {
            writeln!(
                serial,
                "Hyper-V detected: synic={}, hypercall={}",
                features.has_synic, features.has_hypercall
            )
            .ok();

            // Disable debug crash stages
            embclox_hyperv::CRASH_AFTER_STAGE.store(0, core::sync::atomic::Ordering::Relaxed);

            // Construct MemoryMapper from Limine-provided offsets
            let mut memory = embclox_hal_x86::memory::MemoryMapper::new(hhdm_offset, kernel_offset);

            match embclox_hyperv::init(&dma, &mut memory) {
                Ok(mut vmbus) => {
                    writeln!(
                        serial,
                        "VMBus initialized: version={:#x}, {} channel offers",
                        vmbus.version(),
                        vmbus.offers().len()
                    )
                    .ok();

                    for offer in vmbus.offers() {
                        writeln!(
                            serial,
                            "  Channel {}: type={} instance={}",
                            offer.child_relid, offer.device_type, offer.instance_id
                        )
                        .ok();

                        // Identify well-known devices
                        if offer.device_type == embclox_hyperv::guid::SYNTHVID {
                            writeln!(serial, "    -> Synthvid (display)").ok();
                        } else if offer.device_type == embclox_hyperv::guid::NETVSC {
                            writeln!(serial, "    -> NetVSC (network)").ok();
                        }
                    }

                    writeln!(serial, "VMBUS INIT PASSED").ok();

                    // --- NetVSC init ---
                    writeln!(serial, "Starting NetVSC init...").ok();
                    match embclox_hyperv::netvsc::NetvscDevice::init(&mut vmbus, &dma, &memory) {
                        Ok(netvsc) => {
                            writeln!(
                                serial,
                                "NETVSC INIT PASSED: MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} MTU={}",
                                netvsc.mac()[0], netvsc.mac()[1], netvsc.mac()[2],
                                netvsc.mac()[3], netvsc.mac()[4], netvsc.mac()[5],
                                netvsc.mtu(),
                            ).ok();
                        }
                        Err(e) => {
                            writeln!(serial, "NetVSC init failed: {}", e).ok();
                        }
                    }
                }
                Err(e) => {
                    writeln!(serial, "VMBus init failed: {}", e).ok();
                }
            }
        }
        None => {
            writeln!(serial, "Not running on Hyper-V (QEMU or bare metal)").ok();
        }
    }

    writeln!(serial, "Halting.").ok();
    hcf()
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let mut serial = SerialPort::new(0x3F8);
    let _ = writeln!(serial, "PANIC: {}", info);
    hcf()
}

fn hcf() -> ! {
    loop {
        unsafe { asm!("hlt") };
    }
}
