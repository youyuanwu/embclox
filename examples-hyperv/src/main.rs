#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

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

/// Build a minimal DHCP DISCOVER packet (Ethernet + IPv4 + UDP + DHCP).
/// Total size is exactly 300 bytes (well above the 60-byte Ethernet minimum).
fn build_dhcp_discover(mac: [u8; 6], xid: u32) -> [u8; 300] {
    let mut frame = [0u8; 300];

    // Ethernet header: dst=broadcast, src=our MAC, type=IPv4
    frame[0..6].copy_from_slice(&[0xff; 6]);
    frame[6..12].copy_from_slice(&mac);
    frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());

    // IPv4 header (20 bytes): src=0.0.0.0, dst=255.255.255.255, proto=UDP
    let ip_total = (300u16 - 14).to_be_bytes(); // 286
    frame[14] = 0x45; // version=4, IHL=5
    frame[15] = 0x00; // DSCP/ECN
    frame[16..18].copy_from_slice(&ip_total);
    frame[18..20].copy_from_slice(&0u16.to_be_bytes()); // ID
    frame[20..22].copy_from_slice(&0x4000u16.to_be_bytes()); // flags=DF, frag=0
    frame[22] = 64; // TTL
    frame[23] = 17; // proto=UDP
    // checksum at 24..26 (filled below)
    frame[26..30].copy_from_slice(&[0, 0, 0, 0]); // src IP
    frame[30..34].copy_from_slice(&[255, 255, 255, 255]); // dst IP
    let ip_csum = ipv4_checksum(&frame[14..34]);
    frame[24..26].copy_from_slice(&ip_csum.to_be_bytes());

    // UDP header (8 bytes): src=68 (client), dst=67 (server)
    let udp_total = (300u16 - 14 - 20).to_be_bytes(); // 266
    frame[34..36].copy_from_slice(&68u16.to_be_bytes());
    frame[36..38].copy_from_slice(&67u16.to_be_bytes());
    frame[38..40].copy_from_slice(&udp_total);
    // UDP checksum (38..40 ... wait, that's length). UDP checksum is at 40..42.
    frame[40..42].copy_from_slice(&0u16.to_be_bytes()); // checksum=0 (optional for IPv4)

    // DHCP message (BOOTREQUEST). DHCP body starts at frame offset 42
    // (14 Ethernet + 20 IPv4 + 8 UDP). The fixed BOOTP header is 236 bytes,
    // so the magic cookie sits at frame offset 42 + 236 = 278.
    frame[42] = 1; // op=BOOTREQUEST
    frame[43] = 1; // htype=Ethernet
    frame[44] = 6; // hlen
    frame[45] = 0; // hops
    frame[46..50].copy_from_slice(&xid.to_be_bytes()); // xid
    frame[50..52].copy_from_slice(&0u16.to_be_bytes()); // secs
    frame[52..54].copy_from_slice(&0x8000u16.to_be_bytes()); // flags=BROADCAST
    // ciaddr (54..58), yiaddr (58..62), siaddr (62..66), giaddr (66..70) = 0
    // chaddr at frame 70..86 (16 bytes; first 6 = our MAC)
    frame[70..76].copy_from_slice(&mac);
    // sname (86..150) + file (150..278) zeroed

    // Magic cookie at frame[278..282]
    frame[278..282].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
    // DHCP options (start at frame[282])
    let opt = 282;
    // Option 53: DHCP Message Type = 1 (DISCOVER)
    frame[opt] = 53;
    frame[opt + 1] = 1;
    frame[opt + 2] = 1; // DISCOVER
    // Option 55: Parameter Request List (subnet mask, router, DNS)
    frame[opt + 3] = 55;
    frame[opt + 4] = 3;
    frame[opt + 5] = 1; // subnet mask
    frame[opt + 6] = 3; // router
    frame[opt + 7] = 6; // DNS
    // End option
    frame[opt + 8] = 0xff;

    frame
}

/// Compute IPv4 header checksum (one's complement sum of 16-bit words).
fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < header.len() {
        sum += u16::from_be_bytes([header[i], header[i + 1]]) as u32;
        i += 2;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// IDT for the Hyper-V example. We only install one entry — the SynIC SINT2
/// vector that VMBus uses for synthetic interrupts (see
/// `embclox_hyperv::msr::VMBUS_VECTOR`).
fn setup_idt() {
    use x86_64::structures::idt::InterruptDescriptorTable;
    static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();
    unsafe {
        let idt = &raw mut IDT;
        (&mut *idt)[embclox_hyperv::msr::VMBUS_VECTOR].set_handler_fn(vmbus_isr);
        (&*idt).load();
    }
}

/// Counter incremented every time the SynIC SINT2 ISR fires. Useful for
/// verifying that interrupts are actually being delivered.
static VMBUS_IRQ_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// SynIC SINT2 → VMBus IDT vector handler.
///
/// We only wake the netvsc waker — the SINT MSR is configured with auto-EOI
/// (bit 17), so no explicit LAPIC EOI write is required.
extern "x86-interrupt" fn vmbus_isr(_frame: x86_64::structures::idt::InterruptStackFrame) {
    VMBUS_IRQ_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    embclox_hyperv::netvsc::NETVSC_WAKER.wake();
}

/// Phase 3 NetVSC smoke test: send a gratuitous ARP and dump any received
/// Ethernet frames. Verifies the RNDIS_PACKET_MSG TX and RX paths end-to-end
/// against the host vSwitch.
fn phase3_smoke_test(
    serial: &mut SerialPort,
    netvsc: &mut embclox_hyperv::netvsc::NetvscDevice,
    mac: [u8; 6],
) {
    // Crank up logging while we exercise the data path so we can see every
    // VMBus packet pass through pump_channel/try_receive.
    log::set_max_level(log::LevelFilter::Trace);
    // 1) Gratuitous ARP for 169.254.42.42
    writeln!(serial, "PHASE3: building gratuitous ARP for 169.254.42.42").ok();
    // Pad to Ethernet minimum (60 bytes) — some switches drop runts.
    let mut arp_frame = [0u8; 60];

    arp_frame[0..6].copy_from_slice(&[0xff; 6]);
    arp_frame[6..12].copy_from_slice(&mac);
    arp_frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes());

    let our_ip: [u8; 4] = [169, 254, 42, 42];
    arp_frame[14..16].copy_from_slice(&1u16.to_be_bytes()); // HTYPE=Ethernet
    arp_frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes()); // PTYPE=IPv4
    arp_frame[18] = 6; // HLEN
    arp_frame[19] = 4; // PLEN
    arp_frame[20..22].copy_from_slice(&1u16.to_be_bytes()); // OPER=request
    arp_frame[22..28].copy_from_slice(&mac);
    arp_frame[28..32].copy_from_slice(&our_ip);
    arp_frame[32..38].copy_from_slice(&[0; 6]);
    arp_frame[38..42].copy_from_slice(&our_ip);
    // bytes 42..60 stay zero (Ethernet pad)

    // Send the gratuitous ARP via the new closure-based API to exercise
    // `transmit_with` end-to-end.
    let arp_len = arp_frame.len();
    if let Err(e) = netvsc.transmit_with(arp_len, |buf| buf.copy_from_slice(&arp_frame)) {
        writeln!(serial, "PHASE3: ARP transmit_with failed: {}", e).ok();
        return;
    }
    writeln!(
        serial,
        "PHASE3: gratuitous ARP sent via transmit_with ({} bytes)",
        arp_len
    )
    .ok();

    // 2) DHCP DISCOVER — Default Switch runs a DHCP server, this should
    //    elicit an OFFER. Wait for TX space first to demonstrate the
    //    has_tx_space + transmit_with split.
    let xid: u32 = 0xdeadbeef;
    let dhcp_frame = build_dhcp_discover(mac, xid);

    // Spin briefly until the previous TX completion lands so we can reuse
    // the send-buffer slot. In Phase 4b an embassy task would simply yield
    // here and be re-polled when the SINT2 ISR wakes the waker.
    for _ in 0..1_000_000u64 {
        if netvsc.has_tx_space() {
            break;
        }
        core::hint::spin_loop();
    }
    if !netvsc.has_tx_space() {
        writeln!(serial, "PHASE3: timed out waiting for TX space after ARP").ok();
        return;
    }

    if let Err(e) = netvsc.transmit_with(dhcp_frame.len(), |buf| buf.copy_from_slice(&dhcp_frame)) {
        writeln!(serial, "PHASE3: DHCP transmit_with failed: {}", e).ok();
        return;
    }
    writeln!(
        serial,
        "PHASE3: DHCP DISCOVER sent via transmit_with ({} bytes, xid={:#x})",
        dhcp_frame.len(),
        xid
    )
    .ok();

    // 3) Drain RX via the new has_rx_packet + recv_with API. The poll loop
    //    is bounded; in Phase 4b an embassy task would `await` instead.
    let mut frames_seen = 0u32;
    let mut dhcp_offer_seen = false;
    for i in 0..6_000_000u64 {
        if netvsc.has_rx_packet() {
            let result = netvsc.recv_with(|frame| {
                frames_seen += 1;
                let n = frame.len();
                let dump_len = n.min(48);
                writeln!(
                    serial,
                    "PHASE3: rx frame #{} len={} bytes={:02x?}",
                    frames_seen,
                    n,
                    &frame[..dump_len]
                )
                .ok();
                n >= 42
                    && frame[12..14] == [0x08, 0x00]
                    && frame[23] == 17
                    && frame[34..36] == [0x00, 0x43]
            });
            if result == Some(true) {
                dhcp_offer_seen = true;
                writeln!(serial, "PHASE3: DHCP reply detected").ok();
            }
            if frames_seen >= 8 {
                break;
            }
        } else {
            for _ in 0..100 {
                core::hint::spin_loop();
            }
        }
        if i > 0 && i.is_multiple_of(2_000_000) {
            writeln!(
                serial,
                "PHASE3: still polling (iter {}M, rx={}, irq_count={})",
                i / 1_000_000,
                frames_seen,
                VMBUS_IRQ_COUNT.load(core::sync::atomic::Ordering::Relaxed)
            )
            .ok();
        }
    }

    if frames_seen == 0 {
        writeln!(serial, "PHASE3: no frames received").ok();
    }
    writeln!(
        serial,
        "PHASE3 SMOKE TEST DONE: rx={} dhcp_reply={} vmbus_irq_count={}",
        frames_seen,
        dhcp_offer_seen,
        VMBUS_IRQ_COUNT.load(core::sync::atomic::Ordering::Relaxed)
    )
    .ok();
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
                        Ok(mut netvsc) => {
                            let mac = netvsc.mac();
                            writeln!(
                                serial,
                                "NETVSC INIT PASSED: MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} MTU={}",
                                mac[0], mac[1], mac[2],
                                mac[3], mac[4], mac[5],
                                netvsc.mtu(),
                            ).ok();

                            // --- Phase 4a: install IDT + enable SynIC SINT2 ISR ---
                            setup_idt();
                            x86_64::instructions::interrupts::enable();
                            writeln!(
                                serial,
                                "IDT installed, interrupts enabled (SINT2 vector {})",
                                embclox_hyperv::msr::VMBUS_VECTOR
                            )
                            .ok();

                            // --- Phase 3: TX/RX smoke test ---
                            phase3_smoke_test(&mut serial, &mut netvsc, mac);
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
