#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod tulip_embassy;

use core::sync::atomic::{AtomicUsize, Ordering};
use embassy_net::{Stack, StackResources};
use embclox_dma::{DmaAllocator, DmaRegion};
use embedded_io_async::Write as AsyncWrite;
use log::*;
use static_cell::StaticCell;
use x86_64::structures::idt::InterruptStackFrame;

embclox_hal_x86::limine_boot_requests!(limine_boot);

/// DMA allocator backed by a sub-4 GiB Limine "usable" region. Phase-0 keeps
/// the tulip-local bump allocator; the device's CSR/descriptor expectations
/// match the existing `BootDmaAllocator` model (HHDM-mapped vaddr,
/// physically-contiguous ring memory) but tulip historically uses physical
/// pages outside the kernel heap.
struct LimineDmaAllocator {
    hhdm_offset: u64,
}

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

/// Default static network configuration when the cmdline doesn't specify
/// `ip=`/`gw=`. Matches `scripts/hyperv-setup-vswitch.ps1`.
const NET_DEFAULTS: embclox_hal_x86::cmdline::StaticDefaults =
    embclox_hal_x86::cmdline::StaticDefaults {
        ip: [192, 168, 234, 50],
        prefix: 24,
        gw: [192, 168, 234, 1],
    };

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
    info!("embclox Tulip example booting via Limine UEFI");

    // Init DMA pool from Limine memory map (sub-4GB usable region)
    init_dma_pool();

    // Calibrate TSC for embassy time driver. PIT works on QEMU and on
    // Hyper-V Gen1; on hosts where it doesn't, fall back to a 1 GHz
    // assumption (timing will be off but the kernel boots).
    let tsc_per_us = embclox_hal_x86::pit::calibrate_tsc_mhz().unwrap_or(1000);
    embclox_hal_x86::time::set_tsc_per_us(tsc_per_us);
    info!("TSC calibrated: {} cycles/us", tsc_per_us);

    // --- Interrupt + APIC timer infrastructure (shared runtime) ---
    embclox_hal_x86::idt::init();
    embclox_hal_x86::pic::disable();
    let lapic_vaddr = p
        .memory
        .map_mmio(embclox_hal_x86::apic::LAPIC_PHYS_BASE, 0x1000)
        .vaddr();
    let mut lapic = embclox_hal_x86::apic::LocalApic::new(lapic_vaddr);
    lapic.enable();
    embclox_hal_x86::runtime::start_apic_timer(lapic, tsc_per_us, 1_000);

    // Map and initialize the IOAPIC so we can route the Tulip PCI IRQ
    // line through it. Without this routing the tulip_handler ISR
    // never fires; the new hlt-on-idle executor relies on IRQ delivery
    // to wake embassy-net's runner from idle.
    let ioapic_vaddr = p
        .memory
        .map_mmio(embclox_hal_x86::ioapic::IOAPIC_PHYS_BASE, 0x1000)
        .vaddr();
    let mut ioapic = embclox_hal_x86::ioapic::IoApic::new(ioapic_vaddr);
    ioapic.log_info();

    // Scan PCI for Tulip NIC
    info!("Scanning PCI bus for Tulip NIC...");
    let pci_dev = p
        .pci
        .find_device_any(0x1011, &[0x0009, 0x0019])
        .expect("No Tulip NIC found on PCI bus");
    info!(
        "Found Tulip: bus={} dev={} func={} device=0x{:04x}",
        pci_dev.bus, pci_dev.dev, pci_dev.func, pci_dev.device
    );

    p.pci.enable_bus_mastering(&pci_dev);
    let bar0_raw = p.pci.read_config(&pci_dev, 0x10);
    let is_io = (bar0_raw & 1) != 0;

    let csr_access = if is_io {
        let io_base = (bar0_raw & !0x3) as u16;
        info!("Tulip: I/O port {:#x}", io_base);
        embclox_tulip::csr::CsrAccess::Io(io_base)
    } else {
        let bar0_phys = p.pci.read_bar(&pci_dev, 0);
        let mmio_base = bar0_phys as usize + boot_info.hhdm_offset as usize;
        info!("Tulip: MMIO {:#x}", mmio_base);
        embclox_tulip::csr::CsrAccess::Mmio(mmio_base)
    };

    // Store CSR access for ISR
    #[allow(clippy::deref_addrof)]
    unsafe {
        *&raw mut CSR_FOR_ISR = Some(csr_access);
    }

    let dma = LimineDmaAllocator {
        hhdm_offset: boot_info.hhdm_offset,
    };
    let mut device = embclox_tulip::TulipDevice::new(csr_access, dma);
    let mac = device.mac();
    info!(
        "Tulip MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    // Send gratuitous ARP for QEMU slirp
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
    arp[38..42].copy_from_slice(&[10, 0, 2, 2]); // target IP
    device.transmit_with(60, |buf| buf.copy_from_slice(&arp));
    info!("Sent gratuitous ARP");

    info!("TULIP INIT PASSED");

    // --- Interrupt setup ---
    unsafe { embclox_hal_x86::idt::set_handler(33, tulip_handler) };

    let tulip_irq = (p.pci.read_config(&pci_dev, 0x3C) & 0xFF) as u8;
    info!("Tulip PCI IRQ line: {}", tulip_irq);

    // Route the IRQ to vector 33 on BSP (LAPIC ID 0).
    ioapic.enable_irq(tulip_irq, 33, 0);

    // Enable device interrupts
    device.enable_interrupts();

    // --- Embassy networking ---
    let driver = crate::tulip_embassy::TulipEmbassy::new(device, mac);

    // Network mode is selected by Limine cmdline (see limine.conf).
    // Default = DHCP for QEMU SLIRP CI; static option for Hyper-V testing.
    info!("Tulip: cmdline = '{}'", boot_info.cmdline);
    let net_mode = embclox_hal_x86::cmdline::parse_net_mode(boot_info.cmdline, NET_DEFAULTS);
    let config = match net_mode {
        embclox_hal_x86::cmdline::NetMode::Dhcp => {
            info!("Tulip: network mode = DHCPv4");
            embassy_net::Config::dhcpv4(Default::default())
        }
        embclox_hal_x86::cmdline::NetMode::Static { ip, prefix, gw } => {
            info!(
                "Tulip: network mode = static {}.{}.{}.{}/{} gw={}.{}.{}.{}",
                ip[0], ip[1], ip[2], ip[3], prefix, gw[0], gw[1], gw[2], gw[3],
            );
            embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
                address: embassy_net::Ipv4Cidr::new(
                    embassy_net::Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]),
                    prefix,
                ),
                gateway: Some(embassy_net::Ipv4Address::new(gw[0], gw[1], gw[2], gw[3])),
                dns_servers: heapless::Vec::new(),
            })
        }
    };

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

    info!("Starting Embassy executor...");
    embclox_hal_x86::runtime::run_executor(executor);
}

// --- Global state for ISR ---
static mut CSR_FOR_ISR: Option<embclox_tulip::csr::CsrAccess> = None;

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
    embclox_hal_x86::runtime::lapic_eoi();
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
            let addr = config.address;
            info!("DHCP assigned: {}", addr);
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
    error!("PANIC: {}", info);
    loop {
        x86_64::instructions::hlt();
    }
}
