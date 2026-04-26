#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod critical_section_impl;
mod time;
mod tulip_embassy;

use core::arch::asm;
use core::fmt::Write;
use embassy_net::{Stack, StackResources};
use embclox_dma::{DmaAllocator, DmaRegion};
use embedded_io_async::Write as AsyncWrite;
use limine::BaseRevision;
use limine::request::ExecutableAddressRequest;
use limine::request::{
    FramebufferRequest, HhdmRequest, MemoryMapRequest, RequestsEndMarker, RequestsStartMarker,
    StackSizeRequest,
};
use static_cell::StaticCell;
use x86_64::structures::idt::InterruptStackFrame;

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

#[used]
#[unsafe(link_section = ".requests")]
static KERNEL_ADDR_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();

// Port I/O helpers

fn outb(port: u16, value: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) };
}

fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack)) };
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
        // Wait for TX holding register empty (LSR bit 5).
        // Limit spin to avoid hanging on Hyper-V virtual UART which
        // may not emulate LSR faithfully.
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

// Minimal bump allocator for heap
mod heap {
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicUsize, Ordering};

    const HEAP_SIZE: usize = 4 * 1024 * 1024; // 4 MiB

    struct HeapInner(UnsafeCell<[u8; HEAP_SIZE]>);
    unsafe impl Sync for HeapInner {}
    static HEAP: HeapInner = HeapInner(UnsafeCell::new([0; HEAP_SIZE]));
    static OFFSET: AtomicUsize = AtomicUsize::new(0);

    pub struct BumpAlloc;

    unsafe impl GlobalAlloc for BumpAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let heap_base = unsafe { (*HEAP.0.get()).as_ptr() as usize };
            loop {
                let off = OFFSET.load(Ordering::Relaxed);
                // Align the actual address, not just the offset
                let addr = heap_base + off;
                let aligned_addr = (addr + layout.align() - 1) & !(layout.align() - 1);
                let aligned_off = aligned_addr - heap_base;
                let new_off = aligned_off + layout.size();
                if new_off > HEAP_SIZE {
                    return core::ptr::null_mut();
                }
                if OFFSET
                    .compare_exchange_weak(off, new_off, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok()
                {
                    return aligned_addr as *mut u8;
                }
            }
        }
        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
    }
}

#[global_allocator]
static ALLOC: heap::BumpAlloc = heap::BumpAlloc;

// Minimal PCI config space access via port I/O
fn pci_config_read(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    let addr: u32 = (1 << 31)
        | ((bus as u32) << 16)
        | ((slot as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC);
    outl(0xCF8, addr);
    inl(0xCFC)
}

fn pci_config_write(bus: u8, slot: u8, func: u8, offset: u8, value: u32) {
    let addr: u32 = (1 << 31)
        | ((bus as u32) << 16)
        | ((slot as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC);
    outl(0xCF8, addr);
    outl(0xCFC, value);
}

fn outl(port: u16, value: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack)) };
}

fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe { asm!("in eax, dx", in("dx") port, out("eax") value, options(nomem, nostack)) };
    value
}

/// Find a Tulip NIC on PCI bus 0.
fn find_tulip() -> Option<(u8, u8)> {
    for slot in 0..32u8 {
        let id = pci_config_read(0, slot, 0, 0);
        let vendor = id & 0xFFFF;
        let device = (id >> 16) & 0xFFFF;
        // DEC 21140 (0x0009) or DEC 21143 (0x0019)
        if vendor == 0x1011 && (device == 0x0009 || device == 0x0019) {
            return Some((slot, device as u8));
        }
    }
    None
}

/// Enable PCI bus mastering for a device.
fn pci_enable_bus_mastering(slot: u8) {
    let cmd = pci_config_read(0, slot, 0, 0x04);
    pci_config_write(0, slot, 0, 0x04, cmd | (1 << 2)); // Set bit 2
}

/// Read PCI BAR0.
fn pci_read_bar0(slot: u8) -> u64 {
    let bar0 = pci_config_read(0, slot, 0, 0x10);
    let is_mmio = (bar0 & 1) == 0;
    if !is_mmio {
        return (bar0 & !0xF) as u64; // I/O BAR
    }
    let bar_type = (bar0 >> 1) & 0x3;
    let base = (bar0 & !0xF) as u64;
    if bar_type == 0x2 {
        // 64-bit BAR
        let bar1 = pci_config_read(0, slot, 0, 0x14);
        base | ((bar1 as u64) << 32)
    } else {
        base
    }
}

/// DMA allocator that allocates from HHDM-mapped physical memory.
/// Uses Limine's memory map to find usable physical pages, then
/// accesses them via the HHDM identity map (paddr + hhdm_offset = vaddr).
struct LimineDmaAllocator {
    hhdm_offset: u64,
}

/// Simple physical page allocator from Limine usable memory.
/// Tracks allocation with an atomic bump pointer into usable memory.
static DMA_PHYS_NEXT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static DMA_PHYS_END: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Initialize the DMA physical memory pool from the Limine memory map.
/// Finds the largest usable region and reserves it for DMA.
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
                // Zero the memory via HHDM
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

    writeln!(serial, "embclox Tulip example booting via Limine UEFI...").ok();

    assert!(BASE_REVISION.is_supported());
    writeln!(serial, "Limine base revision: supported").ok();

    // Get HHDM offset
    let hhdm_offset = HHDM_REQUEST.get_response().map(|r| r.offset()).unwrap_or(0);
    writeln!(serial, "HHDM offset: {:#x}", hhdm_offset).ok();

    // Get kernel physical/virtual base for DMA offset calculation
    let kernel_offset = if let Some(kaddr) = KERNEL_ADDR_REQUEST.get_response() {
        let vbase = kaddr.virtual_base();
        let pbase = kaddr.physical_base();
        writeln!(serial, "Kernel: virt={:#x} phys={:#x}", vbase, pbase).ok();
        vbase - pbase
    } else {
        writeln!(serial, "WARNING: KernelAddress request failed").ok();
        0
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

    // Calibrate TSC for embassy time driver
    time::calibrate_tsc();
    writeln!(serial, "TSC calibrated").ok();

    // Scan PCI for Tulip NIC
    writeln!(serial, "Scanning PCI bus for Tulip NIC...").ok();
    // Dump all PCI devices for diagnostics
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
    let (slot, dev_id) = find_tulip().expect("No Tulip NIC found on PCI bus");
    writeln!(
        serial,
        "Found Tulip: slot={}, device=0x{:04x}",
        slot, dev_id
    )
    .ok();

    pci_enable_bus_mastering(slot);
    let bar0_raw = pci_config_read(0, slot, 0, 0x10);
    let is_io = (bar0_raw & 1) != 0;

    let csr_access = if is_io {
        let io_base = (bar0_raw & !0x3) as u16;
        writeln!(serial, "Tulip: I/O port {:#x}", io_base).ok();
        embclox_tulip::csr::CsrAccess::Io(io_base)
    } else {
        let bar0 = pci_read_bar0(slot);
        let mmio_base = bar0 as usize + hhdm_offset as usize;
        writeln!(serial, "Tulip: MMIO {:#x}", mmio_base).ok();
        embclox_tulip::csr::CsrAccess::Mmio(mmio_base)
    };

    // Store CSR access for ISR
    #[allow(clippy::deref_addrof)]
    unsafe {
        *&raw mut CSR_FOR_ISR = Some(csr_access);
    }

    let dma = LimineDmaAllocator { hhdm_offset };
    let mut device = embclox_tulip::TulipDevice::new(csr_access, dma);
    let mac = device.mac();
    writeln!(
        serial,
        "Tulip MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
    .ok();

    // Send gratuitous ARP for QEMU slirp
    // Send ARP request padded to 60 bytes (minimum Ethernet frame without FCS)
    let mut arp = [0u8; 60];
    arp[0..6].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]); // dst
    arp[6..12].copy_from_slice(&mac); // src
    arp[12..14].copy_from_slice(&[0x08, 0x06]); // ARP
    arp[14..16].copy_from_slice(&[0x00, 0x01]); // HW type
    arp[16..18].copy_from_slice(&[0x08, 0x00]); // proto
    arp[18] = 0x06;
    arp[19] = 0x04; // hw/proto len
    arp[20..22].copy_from_slice(&[0x00, 0x01]); // ARP request
    arp[22..28].copy_from_slice(&mac); // sender MAC
    arp[28..32].copy_from_slice(&[10, 0, 2, 15]); // sender IP
    // target MAC already zeroed
    arp[38..42].copy_from_slice(&[10, 0, 2, 2]); // target IP
    device.transmit_with(60, |buf| buf.copy_from_slice(&arp));
    writeln!(serial, "Sent gratuitous ARP").ok();

    writeln!(serial, "TULIP INIT PASSED").ok();

    // --- Interrupt setup ---
    // Set up a minimal IDT for the Tulip interrupt
    setup_idt();

    // Read Tulip PCI interrupt line
    let tulip_irq = (pci_config_read(0, slot, 0, 0x3C) & 0xFF) as u8;
    writeln!(serial, "Tulip PCI IRQ line: {}", tulip_irq).ok();

    // Enable device interrupts
    device.enable_interrupts();

    // --- Embassy networking ---
    let driver = crate::tulip_embassy::TulipEmbassy::new(device, mac);

    let config = embassy_net::Config::dhcpv4(Default::default());

    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let resources = RESOURCES.init(StackResources::new());

    let (stack, runner) = embassy_net::new(driver, config, resources, 0x1234_5678_9ABC_DEF0u64);
    static STACK: StaticCell<Stack> = StaticCell::new();
    let stack = &*STACK.init(stack);

    // Embassy executor with hlt-on-idle
    static EXECUTOR: StaticCell<embassy_executor::raw::Executor> = StaticCell::new();
    let executor = EXECUTOR.init(embassy_executor::raw::Executor::new(core::ptr::null_mut()));

    let spawner = executor.spawner();
    spawner.spawn(net_task(runner).expect("spawn net_task"));
    spawner.spawn(echo_task(stack).expect("spawn echo_task"));

    writeln!(serial, "Starting Embassy executor...").ok();
    x86_64::instructions::interrupts::enable();
    loop {
        unsafe { executor.poll() };
        time::check_alarms();
        crate::tulip_embassy::TULIP_WAKER.wake();
        core::hint::spin_loop();
    }
}

// --- Global state for ISR ---
static mut CSR_FOR_ISR: Option<embclox_tulip::csr::CsrAccess> = None;

// --- Minimal IDT ---
fn setup_idt() {
    use x86_64::structures::idt::InterruptDescriptorTable;
    static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();
    unsafe {
        let idt = &raw mut IDT;
        (&mut *idt)[33].set_handler_fn(tulip_handler);
        (&*idt).load();
    }
}

extern "x86-interrupt" fn tulip_handler(_frame: InterruptStackFrame) {
    unsafe {
        let csr_ptr = &raw const CSR_FOR_ISR;
        if let Some(csr) = &*csr_ptr {
            csr.write(embclox_tulip::csr::CSR7, 0);
            let status = csr.read(embclox_tulip::csr::CSR5);
            csr.write(embclox_tulip::csr::CSR5, status);
            csr.write(
                embclox_tulip::csr::CSR7,
                embclox_tulip::csr::CSR7_TIE
                    | embclox_tulip::csr::CSR7_RIE
                    | embclox_tulip::csr::CSR7_NIE
                    | embclox_tulip::csr::CSR7_AIE,
            );
        }
    }
    crate::tulip_embassy::TULIP_WAKER.wake();
}

// --- Embassy tasks ---
#[embassy_executor::task]
async fn net_task(
    mut runner: embassy_net::Runner<
        'static,
        crate::tulip_embassy::TulipEmbassy<LimineDmaAllocator>,
    >,
) {
    runner.run().await
}

#[embassy_executor::task]
async fn echo_task(stack: &'static Stack<'static>) {
    // Wait for DHCP to assign an IP
    loop {
        if let Some(config) = stack.config_v4() {
            let mut serial = SerialPort::new(0x3F8);
            let addr = config.address;
            let _ = writeln!(serial, "DHCP assigned: {}", addr);
            break;
        }
        embassy_time::Timer::after_millis(100).await;
    }

    let mut buf = [0u8; 1024];
    loop {
        let mut tx_buf = [0u8; 1024];
        let mut socket = embassy_net::tcp::TcpSocket::new(*stack, &mut buf, &mut tx_buf);
        socket.set_timeout(None);
        if socket.accept(1234).await.is_err() {
            continue;
        }
        loop {
            let mut data = [0u8; 256];
            match socket.read(&mut data).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if socket.write_all(&data[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
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
