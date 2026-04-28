#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

use core::arch::asm;
use core::fmt::Write;
use embassy_net::{Stack, StackResources};
use embclox_dma::{DmaAllocator, DmaRegion};
use embclox_hyperv::netvsc_embassy::NetvscEmbassy;
use embedded_io_async::Write as AsyncWrite;
use limine::BaseRevision;
use limine::request::{
    FramebufferRequest, HhdmRequest, MemoryMapRequest, RequestsEndMarker, RequestsStartMarker,
    StackSizeRequest,
};
use static_cell::StaticCell;
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
                            writeln!(
                                serial,
                                "IDT installed (SINT2 vector {})",
                                embclox_hyperv::msr::VMBUS_VECTOR
                            )
                            .ok();

                            // --- Phase 4b: hand the device to embassy and run ---
                            //
                            // From this point on the kernel main thread is the
                            // embassy executor's hlt loop; it never returns.
                            run_embassy(netvsc);
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

// ── Phase 4b: embassy executor + embassy-net ────────────────────────────

/// Take ownership of an initialized [`embclox_hyperv::netvsc::NetvscDevice`]
/// and hand it to the embassy executor. Spawns the network runner and a
/// TCP echo server task on port 1234, then runs the executor forever.
///
/// The executor uses a `hlt` between polls so the CPU goes idle when no
/// task is ready; the SynIC SINT2 ISR (`vmbus_isr`) wakes it via
/// `NETVSC_WAKER`, and PIT-calibrated TSC alarms cover timer wakeups.
fn run_embassy(mut netvsc: embclox_hyperv::netvsc::NetvscDevice) -> ! {
    let mut serial = SerialPort::new(0x3F8);

    // Calibrate the TSC. Prefer the Hyper-V TSC frequency MSR (exact);
    // fall back to PIT calibration; final fallback is 2.4 GHz default.
    let tsc_per_us = read_hv_tsc_freq()
        .or_else(calibrate_tsc_via_pit)
        .unwrap_or(2400);
    embclox_hal_x86::time::set_tsc_per_us(tsc_per_us);
    writeln!(serial, "PHASE4B: TSC calibrated: {} cycles/us", tsc_per_us).ok();

    // Send a gratuitous ARP for our static IP so the host learns our MAC
    // and the vSwitch starts forwarding broadcasts (incl. ARP requests
    // from the host) to us. Standard practice for any host coming up
    // with a static address.
    //
    // NOTE: On Windows hosts where Default Switch has previously assigned
    // 172.19.192.50 to a different VM via its NAT/DHCP service, the host
    // ARP table will contain a *Permanent* entry for that IP→old MAC.
    // Gratuitous ARP cannot override Permanent entries; the operator must
    // clear it (`Remove-NetNeighbor -IPAddress 172.19.192.50`, elevated)
    // or boot the VM on a clean Default Switch state.
    let mac = netvsc.mac();
    let our_ip: [u8; 4] = [172, 19, 192, 50];
    send_gratuitous_arp(&mut netvsc, mac, our_ip);
    writeln!(
        serial,
        "PHASE4B: gratuitous ARP for {}.{}.{}.{} sent",
        our_ip[0], our_ip[1], our_ip[2], our_ip[3]
    )
    .ok();

    // Wrap the synthetic NIC in the embassy Driver adapter.
    let driver = NetvscEmbassy::new(netvsc);

    // embassy-net stack with a static IPv4 address.
    //
    // We initially used `Config::dhcpv4(Default::default())`, but Hyper-V's
    // Default Switch DHCP server never replies to smoltcp's DISCOVER (the
    // embedded DHCP socket hardcodes BOOTP `broadcast: false` and the
    // Default Switch DHCP server seems to require the broadcast flag, or
    // expects KVP integration services that we don't implement). Static
    // configuration is sufficient to validate the embassy/embassy-net
    // stack end-to-end via TCP echo.
    //
    // The address must be on the same /20 as the Default Switch's
    // dynamically-chosen subnet (verified during Phase 3: 172.19.192.0/20,
    // gateway 172.19.192.1). Different hosts may pick a different range;
    // adjust if your Default Switch differs.
    let config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(embassy_net::Ipv4Address::new(172, 19, 192, 50), 20),
        gateway: Some(embassy_net::Ipv4Address::new(172, 19, 192, 1)),
        dns_servers: heapless::Vec::new(),
    });
    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let resources = RESOURCES.init(StackResources::new());

    let (stack, runner) = embassy_net::new(driver, config, resources, 0xc0fe_face_dead_beefu64);
    static STACK: StaticCell<Stack> = StaticCell::new();
    let stack = &*STACK.init(stack);

    // Embassy executor — single-threaded, hlt-on-idle.
    static EXECUTOR: StaticCell<embassy_executor::raw::Executor> = StaticCell::new();
    let executor = EXECUTOR.init(embassy_executor::raw::Executor::new(core::ptr::null_mut()));

    let spawner = executor.spawner();
    spawner.spawn(net_task(runner).expect("net_task SpawnToken"));
    spawner.spawn(echo_task(stack).expect("echo_task SpawnToken"));

    writeln!(serial, "PHASE4B: starting embassy executor").ok();
    x86_64::instructions::interrupts::enable();

    loop {
        unsafe { executor.poll() };
        // Fire any expired timer alarms — without an APIC timer interrupt
        // wired up, this poll-loop call is what advances embassy-time.
        embclox_hal_x86::time::on_timer_tick();
        // Belt-and-braces wake — the SINT2 ISR also signals the waker, but
        // re-arming each loop ensures we never sleep through a freshly
        // delivered packet.
        embclox_hyperv::netvsc::NETVSC_WAKER.wake();
        // Brief pause; we cannot `hlt` blindly because we still need the
        // poll loop to advance timer alarms (no APIC timer ISR yet).
        for _ in 0..1000 {
            core::hint::spin_loop();
        }
    }
}

/// Send a gratuitous ARP for `our_ip` claiming `mac` as the MAC address.
/// Pads to the 60-byte Ethernet minimum.
fn send_gratuitous_arp(
    netvsc: &mut embclox_hyperv::netvsc::NetvscDevice,
    mac: [u8; 6],
    our_ip: [u8; 4],
) {
    let mut frame = [0u8; 60];
    frame[0..6].copy_from_slice(&[0xff; 6]);
    frame[6..12].copy_from_slice(&mac);
    frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
    frame[14..16].copy_from_slice(&1u16.to_be_bytes()); // HTYPE=Ethernet
    frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes()); // PTYPE=IPv4
    frame[18] = 6; // HLEN
    frame[19] = 4; // PLEN
    frame[20..22].copy_from_slice(&1u16.to_be_bytes()); // OPER=request
    frame[22..28].copy_from_slice(&mac);
    frame[28..32].copy_from_slice(&our_ip);
    frame[32..38].copy_from_slice(&[0; 6]);
    frame[38..42].copy_from_slice(&our_ip);
    let _ = netvsc.transmit_with(60, |buf| buf.copy_from_slice(&frame));
}

/// Read the Hyper-V TSC frequency MSR (cycles per second) and convert to
/// cycles per microsecond. Returns None if the MSR isn't readable.
fn read_hv_tsc_freq() -> Option<u64> {
    let hz = unsafe { embclox_hyperv::msr::rdmsr(embclox_hyperv::msr::TSC_FREQUENCY) };
    if hz == 0 { None } else { Some(hz / 1_000_000) }
}

/// Calibrate TSC frequency using PIT channel 2 (~50ms gate).
/// Returns TSC ticks per microsecond, or None if the PIT isn't responsive.
fn calibrate_tsc_via_pit() -> Option<u64> {
    // PIT channel 2: gate on, mode 0 (one-shot), ~50ms
    let count: u16 = 59659; // 1193182 Hz / 20 = ~50ms

    outb(0x61, (inb(0x61) & 0x0C) | 0x01); // Gate on, speaker off
    outb(0x43, 0xB0); // Channel 2, lobyte/hibyte, mode 0
    outb(0x42, (count & 0xFF) as u8);
    outb(0x42, (count >> 8) as u8);

    // Reset gate to start counting
    let gate = inb(0x61);
    outb(0x61, gate & !0x01);
    outb(0x61, gate | 0x01);

    let start = unsafe { core::arch::x86_64::_rdtsc() };
    // Wait for PIT output bit (bit 5 of port 0x61), bounded so we don't
    // loop forever if the PIT doesn't respond.
    let mut bounded = 0u64;
    while inb(0x61) & 0x20 == 0 {
        bounded += 1;
        if bounded > 100_000_000 {
            return None;
        }
        core::hint::spin_loop();
    }
    let end = unsafe { core::arch::x86_64::_rdtsc() };

    let tsc_per_50ms = end - start;
    let tsc_per_us = tsc_per_50ms / 50_000;
    if tsc_per_us > 0 {
        Some(tsc_per_us)
    } else {
        None
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, NetvscEmbassy>) {
    runner.run().await
}

#[embassy_executor::task]
async fn echo_task(stack: &'static Stack<'static>) {
    // With the static config the stack is ready immediately; just print
    // it once for confirmation.
    let mut serial = SerialPort::new(0x3F8);
    if let Some(config) = stack.config_v4() {
        let _ = writeln!(serial, "PHASE4B: IPv4 configured: {}", config.address);
        if let Some(gw) = config.gateway {
            let _ = writeln!(serial, "PHASE4B: gateway: {}", gw);
        }
    }
    let _ = writeln!(serial, "PHASE4B ECHO READY: TCP port 1234");

    let mut rx = [0u8; 1024];
    loop {
        let mut tx = [0u8; 1024];
        let mut socket = embassy_net::tcp::TcpSocket::new(*stack, &mut rx, &mut tx);
        socket.set_timeout(None);
        if socket.accept(1234).await.is_err() {
            continue;
        }
        let _ = writeln!(serial, "PHASE4B: tcp client connected");
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
        let _ = writeln!(serial, "PHASE4B: tcp client disconnected");
    }
}
