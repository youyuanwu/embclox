#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use embassy_net::{Stack, StackResources};
use embclox_dma::{DmaAllocator, DmaRegion};
use embclox_hyperv::netvsc_embassy::NetvscEmbassy;
use embedded_io_async::Write as AsyncWrite;
use log::*;
use static_cell::StaticCell;

embclox_hal_x86::limine_boot_requests!(limine_boot);

/// DMA allocator using Limine HHDM-mapped physical memory pool.
struct LimineDmaAllocator {
    hhdm_offset: u64,
}

/// Physical page allocator from Limine usable memory (sub-4GB).
static DMA_PHYS_NEXT: AtomicUsize = AtomicUsize::new(0);
static DMA_PHYS_END: AtomicUsize = AtomicUsize::new(0);

fn init_dma_pool() {
    let memmap = limine_boot::MEMMAP_REQUEST
        .get_response()
        .expect("Limine MemoryMapRequest response missing");
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

impl DmaAllocator for LimineDmaAllocator {
    fn alloc_coherent(&self, size: usize, align: usize) -> DmaRegion {
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
static VMBUS_IRQ_COUNT: AtomicU32 = AtomicU32::new(0);

/// SynIC SINT2 → VMBus IDT vector handler.
///
/// We only wake the netvsc waker — the SINT MSR is configured with auto-EOI
/// (bit 17), so no explicit LAPIC EOI write is required.
extern "x86-interrupt" fn vmbus_isr(_frame: x86_64::structures::idt::InterruptStackFrame) {
    VMBUS_IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
    embclox_hyperv::netvsc::NETVSC_WAKER.wake();
}

#[unsafe(no_mangle)]
unsafe extern "C" fn kmain() -> ! {
    let boot_info = limine_boot::collect();
    let mut p = embclox_hal_x86::init(
        boot_info,
        embclox_hal_x86::Config {
            heap_size: 4 * 1024 * 1024,
            ..Default::default()
        },
    );
    info!("embclox Hyper-V example booting via Limine");

    // Init DMA pool from Limine memory map (sub-4GB usable region)
    init_dma_pool();

    info!("HYPERV BOOT PASSED");

    // Scan PCI bus
    info!("Scanning PCI bus...");
    for slot in 0..32u8 {
        let id = p.pci.read_config(
            &embclox_hal_x86::pci::PciDevice {
                bus: 0,
                dev: slot,
                func: 0,
                vendor: 0,
                device: 0,
            },
            0x00,
        );
        let vendor = (id & 0xFFFF) as u16;
        if vendor != 0xFFFF {
            let device = ((id >> 16) & 0xFFFF) as u16;
            info!("  PCI 00:{:02}.0 {:04x}:{:04x}", slot, vendor, device);
        }
    }

    // --- Hyper-V VMBus initialization ---

    // Initialize the IDT + disable the legacy PIC before any handler
    // registration. Both the runtime APIC timer (started below) and
    // the SynIC SINT2 handler use the shared HAL IDT.
    embclox_hal_x86::idt::init();
    embclox_hal_x86::pic::disable();

    let dma = LimineDmaAllocator {
        hhdm_offset: boot_info.hhdm_offset,
    };

    match embclox_hyperv::detect::detect() {
        Some(features) => {
            info!(
                "Hyper-V detected: synic={}, hypercall={}",
                features.has_synic, features.has_hypercall
            );

            // Disable debug crash stages
            embclox_hyperv::CRASH_AFTER_STAGE.store(0, Ordering::Relaxed);

            // Calibrate the TSC and start the APIC periodic timer + register
            // vmbus_isr BEFORE VMBus init runs. embclox_hyperv::init drives
            // its synchronous boot phase via block_on_hlt internally; that
            // runner needs (a) the SINT2 IRQ wired so host VMBus messages
            // can wake the CPU from hlt, and (b) the APIC timer firing so
            // the deadline-check on each iteration eventually fires even
            // if the host never replies.
            let tsc_per_us = read_hv_tsc_freq()
                .or_else(embclox_hal_x86::pit::calibrate_tsc_mhz)
                .unwrap_or(2400);
            embclox_hal_x86::time::set_tsc_per_us(tsc_per_us);
            info!("TSC calibrated: {} cycles/us", tsc_per_us);

            let lapic_vaddr = p
                .memory
                .map_mmio(embclox_hal_x86::apic::LAPIC_PHYS_BASE, 0x1000)
                .vaddr();
            let mut lapic = embclox_hal_x86::apic::LocalApic::new(lapic_vaddr);
            lapic.enable();
            embclox_hal_x86::runtime::start_apic_timer(lapic, tsc_per_us, 1_000);

            unsafe {
                embclox_hal_x86::idt::set_handler(embclox_hyperv::msr::VMBUS_VECTOR, vmbus_isr);
            }
            info!(
                "IDT installed (SINT2 vector {})",
                embclox_hyperv::msr::VMBUS_VECTOR
            );

            match embclox_hyperv::init(&dma, &mut p.memory) {
                Ok(mut vmbus) => {
                    info!(
                        "VMBus initialized: version={:#x}, {} channel offers",
                        vmbus.version(),
                        vmbus.offers().len()
                    );

                    for offer in vmbus.offers() {
                        info!(
                            "  Channel {}: type={} instance={}",
                            offer.child_relid, offer.device_type, offer.instance_id
                        );

                        // Identify well-known devices
                        if offer.device_type == embclox_hyperv::guid::SYNTHVID {
                            info!("    -> Synthvid (display)");
                        } else if offer.device_type == embclox_hyperv::guid::NETVSC {
                            info!("    -> NetVSC (network)");
                        }
                    }

                    info!("VMBUS INIT PASSED");

                    // --- NetVSC init ---
                    info!("Starting NetVSC init...");
                    match embclox_hyperv::netvsc::NetvscDevice::init(&mut vmbus, &dma, &p.memory) {
                        Ok(netvsc) => {
                            let mac = netvsc.mac();
                            info!(
                                "NETVSC INIT PASSED: MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} MTU={}",
                                mac[0],
                                mac[1],
                                mac[2],
                                mac[3],
                                mac[4],
                                mac[5],
                                netvsc.mtu(),
                            );

                            // --- Phase 4: hand the device to embassy and run ---
                            // From this point on the kernel main thread is the
                            // embassy executor's hlt loop; it never returns.
                            run_embassy(netvsc, p.memory, boot_info.cmdline);
                        }
                        Err(e) => {
                            error!("NetVSC init failed: {}", e);
                        }
                    }
                }
                Err(e) => {
                    error!("VMBus init failed: {}", e);
                }
            }
        }
        None => {
            info!("Not running on Hyper-V (QEMU or bare metal)");
        }
    }

    info!("Halting.");
    hcf()
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    error!("PANIC: {}", info);
    hcf()
}

fn hcf() -> ! {
    loop {
        x86_64::instructions::hlt();
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
    _memory: embclox_hal_x86::memory::MemoryMapper,
    cmdline: &str,
) -> ! {
    // Enable Debug to see VMBus packet activity in Azure debugging.
    log::set_max_level(log::LevelFilter::Debug);

    info!("PHASE4B: cmdline = '{}'", cmdline);
    let net_mode = embclox_hal_x86::cmdline::parse_net_mode(cmdline, NET_DEFAULTS);

    let mac = netvsc.mac();
    let driver;
    let config;
    match net_mode {
        embclox_hal_x86::cmdline::NetMode::Dhcp => {
            // No gratuitous ARP — the DHCP DISCOVER itself announces us.
            info!("PHASE4B: network mode = DHCPv4");
            driver = NetvscEmbassy::new(netvsc);
            config = embassy_net::Config::dhcpv4(Default::default());
        }
        embclox_hal_x86::cmdline::NetMode::Static { ip, prefix, gw } => {
            // Send a gratuitous ARP so the host learns our MAC before
            // any TCP traffic flows.
            send_gratuitous_arp(&mut netvsc, mac, ip);
            info!(
                "PHASE4B: network mode = static {}.{}.{}.{}/{} gw={}.{}.{}.{}",
                ip[0], ip[1], ip[2], ip[3], prefix, gw[0], gw[1], gw[2], gw[3],
            );
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

    info!("PHASE4B: starting embassy executor");
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
    loop {
        if let Some(config) = stack.config_v4() {
            info!("PHASE4B: IPv4 configured: {}", config.address);
            if let Some(gw) = config.gateway {
                info!("PHASE4B: gateway: {}", gw);
            }
            break;
        }
        embassy_time::Timer::after_millis(100).await;
    }
    info!("PHASE4B ECHO READY: TCP port 1234");

    let mut rx = [0u8; 1024];
    loop {
        let mut tx = [0u8; 1024];
        let mut socket = embassy_net::tcp::TcpSocket::new(*stack, &mut rx, &mut tx);
        socket.set_timeout(None);
        if socket.accept(1234).await.is_err() {
            continue;
        }
        info!("PHASE4B: tcp client connected");
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
        info!("PHASE4B: tcp client disconnected");
    }
}
