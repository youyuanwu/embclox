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
    ExecutableCmdlineRequest, FramebufferRequest, HhdmRequest, MemoryMapRequest, RequestsEndMarker,
    RequestsStartMarker, StackSizeRequest,
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

#[used]
#[unsafe(link_section = ".requests")]
static CMDLINE_REQUEST: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();

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

    // Initialize the IDT + disable the legacy PIC before any handler
    // registration. The runtime APIC timer (started inside run_embassy)
    // and the SynIC SINT2 handler (registered just before run_embassy)
    // both use the shared HAL IDT.
    embclox_hal_x86::idt::init();
    embclox_hal_x86::pic::disable();

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
                            // Register vmbus_isr at the SynIC SINT2 vector via the
                            // shared HAL IDT (the runtime module also installs the
                            // APIC timer ISR there during run_embassy).
                            unsafe {
                                embclox_hal_x86::idt::set_handler(
                                    embclox_hyperv::msr::VMBUS_VECTOR,
                                    vmbus_isr,
                                );
                            }
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
                            run_embassy(netvsc, memory);
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

/// Default static network configuration when the cmdline doesn't specify
/// `ip=`/`gw=`. Matches `scripts/hyperv-setup-vswitch.ps1`.
const NET_DEFAULTS: embclox_hal_x86::cmdline::StaticDefaults =
    embclox_hal_x86::cmdline::StaticDefaults {
        ip: [192, 168, 234, 50],
        prefix: 24,
        gw: [192, 168, 234, 1],
    };

/// Read the Limine-provided kernel command line as a UTF-8 string slice.
/// Returns "" when the bootloader didn't pass one.
fn cmdline_str() -> &'static str {
    if let Some(resp) = CMDLINE_REQUEST.get_response() {
        resp.cmdline().to_str().unwrap_or("")
    } else {
        ""
    }
}

/// Take ownership of an initialized [`embclox_hyperv::netvsc::NetvscDevice`]
/// and hand it to the embassy executor. Spawns the network runner and a
/// TCP echo server task on port 1234, then runs the executor forever.
///
/// Network mode (DHCP vs static) is selected by the Limine cmdline
/// `net=` parameter — see [`embclox_hal_x86::cmdline`] and `limine.conf`.
///
/// The executor uses a `hlt` between polls so the CPU goes idle when no
/// task is ready; the SynIC SINT2 ISR (`vmbus_isr`) wakes it via
/// `NETVSC_WAKER`, and the APIC periodic timer (installed by the shared
/// runtime module) covers timer wakeups.
fn run_embassy(
    mut netvsc: embclox_hyperv::netvsc::NetvscDevice,
    mut memory: embclox_hal_x86::memory::MemoryMapper,
) -> ! {
    let mut serial = SerialPort::new(0x3F8);

    // Enable Debug to see VMBus packet activity in Azure debugging.
    log::set_max_level(log::LevelFilter::Debug);

    // Calibrate the TSC. Prefer the Hyper-V TSC frequency MSR (exact);
    // fall back to PIT calibration; final fallback is 2.4 GHz default.
    let tsc_per_us = read_hv_tsc_freq()
        .or_else(embclox_hal_x86::pit::calibrate_tsc_mhz)
        .unwrap_or(2400);
    embclox_hal_x86::time::set_tsc_per_us(tsc_per_us);
    writeln!(serial, "PHASE4B: TSC calibrated: {} cycles/us", tsc_per_us).ok();

    // Map LAPIC MMIO via the page tables. The Limine HHDM only covers
    // as much physical RAM as the guest has, so the LAPIC MMIO page at
    // 0xFEE0_0000 (~4 GiB physical) is not addressable through it on
    // memory-constrained hosts. Use the MemoryMapper so we always get
    // a valid uncached virtual mapping. MmioMapping has no Drop impl,
    // so the page-table entry persists for the program lifetime.
    let lapic_vaddr = memory
        .map_mmio(embclox_hal_x86::apic::LAPIC_PHYS_BASE, 0x1000)
        .vaddr();
    let mut lapic = embclox_hal_x86::apic::LocalApic::new(lapic_vaddr);
    lapic.enable();
    embclox_hal_x86::runtime::start_apic_timer(lapic, tsc_per_us, 1_000);

    // Read the kernel cmdline that limine.conf passed (e.g. "net=dhcp"
    // for the DHCP boot menu entry; default = static).
    let cmdline = cmdline_str();
    writeln!(serial, "PHASE4B: cmdline = '{}'", cmdline).ok();
    let net_mode = embclox_hal_x86::cmdline::parse_net_mode(cmdline, NET_DEFAULTS);

    let mac = netvsc.mac();
    let driver;
    let config;
    match net_mode {
        embclox_hal_x86::cmdline::NetMode::Dhcp => {
            // No gratuitous ARP — the DHCP DISCOVER itself announces us.
            // Use this mode on QEMU SLIRP, Azure, External vSwitch, or
            // anywhere a real DHCP server is available.
            writeln!(serial, "PHASE4B: network mode = DHCPv4").ok();
            driver = NetvscEmbassy::new(netvsc);
            config = embassy_net::Config::dhcpv4(Default::default());
        }
        embclox_hal_x86::cmdline::NetMode::Static { ip, prefix, gw } => {
            // Send a gratuitous ARP so the host learns our MAC before
            // any TCP traffic flows.
            send_gratuitous_arp(&mut netvsc, mac, ip);
            writeln!(
                serial,
                "PHASE4B: network mode = static {}.{}.{}.{}/{} gw={}.{}.{}.{}",
                ip[0], ip[1], ip[2], ip[3], prefix, gw[0], gw[1], gw[2], gw[3],
            )
            .ok();
            driver = NetvscEmbassy::new(netvsc);
            config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
                address: embassy_net::Ipv4Cidr::new(
                    embassy_net::Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]),
                    prefix,
                ),
                gateway: Some(embassy_net::Ipv4Address::new(gw[0], gw[1], gw[2], gw[3])),
                dns_servers: heapless::Vec::new(),
            });
        }
    }
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
    embclox_hal_x86::runtime::run_executor(executor);
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

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, NetvscEmbassy>) {
    runner.run().await
}

#[embassy_executor::task]
async fn echo_task(stack: &'static Stack<'static>) {
    // Wait for an IPv4 address to be configured. With static config this
    // is immediate; with DHCP it can take 1-3 seconds for OFFER+ACK.
    let mut serial = SerialPort::new(0x3F8);
    loop {
        if let Some(config) = stack.config_v4() {
            let _ = writeln!(serial, "PHASE4B: IPv4 configured: {}", config.address);
            if let Some(gw) = config.gateway {
                let _ = writeln!(serial, "PHASE4B: gateway: {}", gw);
            }
            break;
        }
        embassy_time::Timer::after_millis(100).await;
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
