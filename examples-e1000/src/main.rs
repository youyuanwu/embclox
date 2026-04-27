#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;
extern crate embclox_hal_x86;

use bootloader_api::{BootInfo, BootloaderConfig, config::Mapping, entry_point};
use core::panic::PanicInfo;
use core::sync::atomic::AtomicUsize;
use embassy_net::{Ipv4Address, Ipv4Cidr, Stack, StackResources, StaticConfigV4};
use embclox_core::dma_alloc::BootDmaAllocator;
use embclox_core::e1000_embassy::E1000Embassy;
use embclox_core::mmio_regs::MmioRegs;
use embclox_hal_x86::apic::LocalApic;
use embclox_hal_x86::ioapic::IoApic;
use embedded_io_async::Write;
use log::*;
use static_cell::StaticCell;
use x86_64::structures::idt::InterruptStackFrame;

const BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

// Global e1000 MMIO base for ISR to read ICR
static E1000_REGS_BASE: AtomicUsize = AtomicUsize::new(0);

// Global LAPIC pointer for EOI from interrupt handlers
static mut LAPIC: Option<LocalApic> = None;

fn lapic() -> &'static LocalApic {
    unsafe {
        (*core::ptr::addr_of!(LAPIC))
            .as_ref()
            .expect("LAPIC not initialized")
    }
}

extern "x86-interrupt" fn apic_timer_handler(_frame: InterruptStackFrame) {
    embclox_hal_x86::time::on_timer_tick();
    lapic().end_of_interrupt();
}

extern "x86-interrupt" fn e1000_handler(_frame: InterruptStackFrame) {
    // Acknowledge e1000 interrupt by reading ICR (read-clear register)
    // Use the global MmioRegs pointer stored at init
    unsafe {
        let regs_base = E1000_REGS_BASE.load(core::sync::atomic::Ordering::Relaxed);
        if regs_base != 0 {
            // ICR is at word offset 0xC0/4 = 0x30
            core::ptr::read_volatile((regs_base as *const u32).add(0x000C0 / 4));
        }
    }
    // Wake the network runner task
    embclox_core::e1000_embassy::NET_WAKER.wake();
    lapic().end_of_interrupt();
}

extern "x86-interrupt" fn spurious_handler(_frame: InterruptStackFrame) {}

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    let mut p = embclox_hal_x86::init(boot_info, embclox_hal_x86::Config::default());
    info!("Booting embclox example...");

    // --- Interrupt infrastructure ---
    embclox_hal_x86::idt::init();
    embclox_hal_x86::pic::disable();

    // Map LAPIC (kept alive for program lifetime — used by ISR)
    let _lapic_mmio = p
        .memory
        .map_mmio(embclox_hal_x86::apic::LAPIC_PHYS_BASE, 0x1000);
    let mut lapic_dev = LocalApic::new(_lapic_mmio.vaddr());
    lapic_dev.enable();

    // Calibrate TSC via PIT
    let tsc_per_us = embclox_hal_x86::pit::calibrate_tsc_mhz();
    embclox_hal_x86::time::set_tsc_per_us(tsc_per_us);

    // Register handlers
    unsafe {
        embclox_hal_x86::idt::set_handler(32, apic_timer_handler);
        embclox_hal_x86::idt::set_handler(39, spurious_handler);
    }

    // Start APIC timer: periodic, ~1ms intervals
    // tsc_per_us * 1000 = ticks per ms. Divide by APIC divider (16) for APIC count.
    let apic_count = (tsc_per_us * 1000 / 16) as u32;
    lapic_dev.set_timer_periodic(32, 16, apic_count);

    // Store global LAPIC for ISR access
    unsafe { *core::ptr::addr_of_mut!(LAPIC) = Some(lapic_dev) };

    // Map IOAPIC (kept alive for program lifetime)
    let _ioapic_mmio = p
        .memory
        .map_mmio(embclox_hal_x86::ioapic::IOAPIC_PHYS_BASE, 0x1000);
    let mut ioapic = IoApic::new(_ioapic_mmio.vaddr());
    ioapic.log_info();

    // --- PCI + e1000 ---
    let pci_dev = p
        .pci
        .find_device_any(0x8086, &[0x100E, 0x100F, 0x10D3])
        .expect("e1000 device not found on PCI bus");
    let bar0_phys = p.pci.read_bar(&pci_dev, 0);

    // Map e1000 BAR0 MMIO (kept alive for program lifetime)
    let _e1000_mmio = p.memory.map_mmio(bar0_phys, 0x20000);
    info!("e1000 MMIO vaddr: {:#x}", _e1000_mmio.vaddr());

    let regs = MmioRegs::new(_e1000_mmio.vaddr());

    // Caller performs device reset before new() per driver contract
    embclox_core::e1000_helpers::reset_device(&regs);
    p.pci.enable_bus_mastering(&pci_dev);

    // Initialize e1000 driver
    let dma = BootDmaAllocator {
        kernel_offset: p.memory.kernel_offset(),
        phys_offset: p.memory.phys_offset(),
    };
    let mut e1000_device = embclox_e1000::E1000Device::new(regs, dma);
    info!("e1000 driver initialized");

    let mac = e1000_device.mac_address();
    info!(
        "MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    // Gratuitous ARP — QEMU slirp workaround
    let arp: [u8; 42] = [
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], 0x08,
        0x06, 0x00, 0x01, 0x08, 0x00, 0x06, 0x04, 0x00, 0x01, mac[0], mac[1], mac[2], mac[3],
        mac[4], mac[5], 10, 0, 2, 15, 0, 0, 0, 0, 0, 0, 10, 0, 2, 2,
    ];
    {
        let (_, mut tx) = e1000_device.split();
        tx.transmit(&arp);
    }
    info!("Sent gratuitous ARP");

    // --- E1000 interrupt setup ---
    // Store MMIO base for ISR
    E1000_REGS_BASE.store(_e1000_mmio.vaddr(), core::sync::atomic::Ordering::Relaxed);

    // Read e1000 IRQ from PCI interrupt line register
    let e1000_irq = (p.pci.read_config(&pci_dev, 0x3C) & 0xFF) as u8;
    info!("e1000 PCI IRQ line: {}", e1000_irq);

    // Register e1000 handler and route via IOAPIC
    unsafe { embclox_hal_x86::idt::set_handler(33, e1000_handler) };
    ioapic.enable_irq(e1000_irq, 33, 0);

    // Enable e1000 device interrupts
    e1000_device.enable_interrupts();
    info!("e1000 interrupts enabled (IRQ {} -> vector 33)", e1000_irq);

    let driver = E1000Embassy::new(e1000_device, mac);

    // Embassy networking stack with static IP
    let config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24),
        gateway: Some(Ipv4Address::new(10, 0, 2, 2)),
        dns_servers: Default::default(),
    });
    let seed = 0x1234_5678_9ABC_DEF0u64;
    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();
    let resources = RESOURCES.init(StackResources::new());

    let (stack, runner) = embassy_net::new(driver, config, resources, seed);
    static STACK: StaticCell<Stack> = StaticCell::new();
    let stack = &*STACK.init(stack);

    // Custom executor loop with hlt-on-idle
    static EXECUTOR: StaticCell<embassy_executor::raw::Executor> = StaticCell::new();
    let executor = EXECUTOR.init(embassy_executor::raw::Executor::new(core::ptr::null_mut()));

    let spawner = executor.spawner();
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(echo_task(stack).unwrap());

    info!("Starting executor with hlt-on-idle...");
    x86_64::instructions::interrupts::enable();
    loop {
        unsafe { executor.poll() };
        // Halt until next interrupt (APIC timer ~1ms or e1000 RX).
        // cli before hlt prevents race: interrupt between poll() and hlt
        // would be lost. enable_and_hlt atomically enables + halts.
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, E1000Embassy>) {
    info!("net_task: starting runner.run()");
    info!(
        "embassy time now: {}",
        embassy_time::Instant::now().as_micros()
    );
    runner.run().await;
}

#[embassy_executor::task]
async fn echo_task(stack: &'static Stack<'static>) {
    // Wait briefly for net_task to start
    embassy_time::Timer::after_millis(500).await;

    info!("Network is up! Starting TCP echo server on port 1234...");

    let mut socket_rx_buf = [0u8; 1024];
    let mut socket_tx_buf = [0u8; 1024];
    let mut read_buf = [0u8; 1024];

    loop {
        let mut socket =
            embassy_net::tcp::TcpSocket::new(*stack, &mut socket_rx_buf, &mut socket_tx_buf);
        info!("Waiting for TCP connection on port 1234...");
        if let Err(e) = socket.accept(1234).await {
            warn!("Accept error: {:?}", e);
            continue;
        }
        info!("TCP connection accepted");

        loop {
            let n = match socket.read(&mut read_buf).await {
                Ok(0) => {
                    info!("Connection closed by peer");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    warn!("Read error: {:?}", e);
                    break;
                }
            };
            if let Err(e) = socket.write_all(&read_buf[..n]).await {
                warn!("Write error: {:?}", e);
                break;
            }
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    error!("{}", info);
    loop {
        x86_64::instructions::hlt();
    }
}
