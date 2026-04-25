#![no_std]
#![no_main]

extern crate alloc;

mod critical_section_impl;
mod e1000_adapter;
mod heap;
mod kernfn;
mod logger;
mod mmio;
mod pci_init;
mod serial;
mod time_driver;

use bootloader_api::{BootInfo, BootloaderConfig, config::Mapping, entry_point};
use core::panic::PanicInfo;
use e1000_adapter::E1000Embassy;
use embassy_executor::Executor;
use embassy_net::{Ipv4Address, Ipv4Cidr, Stack, StackResources, StaticConfigV4};
use embedded_io_async::Write;
use kernfn::Kernfn;
use log::*;
use static_cell::StaticCell;

const BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    serial::init();
    info!("Booting e1000-embassy example...");

    let phys_offset = boot_info
        .physical_memory_offset
        .into_option()
        .expect("physical_memory_offset not available");

    heap::init(boot_info);
    info!("Heap initialized");
    info!("Physical memory offset: {:#x}", phys_offset);

    // PCI scan for e1000
    let pci_info = pci_init::pci_find_e1000().expect("e1000 device not found on PCI bus");

    // Kernel virtual-to-physical offset (bootloader virtual_address_offset - kernel_load_phys)
    let kernel_virt_to_phys: u64 = 0xFFFF000000;

    // Map e1000 BAR0 MMIO with Uncacheable 4KB pages (required for device register access)
    let e1000_vaddr = mmio::map_mmio(
        phys_offset,
        kernel_virt_to_phys,
        pci_info.bar0_phys,
        0x20000,
    );
    info!("e1000 MMIO vaddr: {:#x}", e1000_vaddr);

    // Initialize e1000 driver
    let kfn = Kernfn {
        kernel_offset: kernel_virt_to_phys,
        phys_offset,
    };
    let mut e1000_device = e1000_driver::e1000::E1000Device::<Kernfn>::new(kfn, e1000_vaddr)
        .expect("e1000 init failed");
    info!("e1000 driver initialized");

    // Re-enable PCI bus mastering AFTER device reset (reset clears the command register)
    pci_init::pci_enable_bus_mastering(pci_info.dev);

    // Read MAC from the device's RAL/RAH registers
    let mac = unsafe {
        let regs_ptr = e1000_vaddr as *const u32;
        let ral = core::ptr::read_volatile(regs_ptr.add(0x5400 / 4));
        let rah = core::ptr::read_volatile(regs_ptr.add(0x5404 / 4));
        [
            ral as u8,
            (ral >> 8) as u8,
            (ral >> 16) as u8,
            (ral >> 24) as u8,
            rah as u8,
            (rah >> 8) as u8,
        ]
    };
    info!(
        "MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    // Send a gratuitous ARP to trigger QEMU's e1000 model to re-evaluate RX readiness.
    // After device reset + re-init, QEMU's slirp may have cached rx_can_recv=false.
    // A TX triggers qemu_flush_queued_packets() which re-polls rx_can_recv.
    let arp: [u8; 42] = [
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], 0x08,
        0x06, 0x00, 0x01, 0x08, 0x00, 0x06, 0x04, 0x00, 0x01, mac[0], mac[1], mac[2], mac[3],
        mac[4], mac[5], 10, 0, 2, 15, 0, 0, 0, 0, 0, 0, 10, 0, 2, 2,
    ];
    e1000_device.e1000_transmit(&arp);
    info!("Sent gratuitous ARP");
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

    // Start executor
    static EXECUTOR: StaticCell<Executor> = StaticCell::new();
    let executor = EXECUTOR.init(Executor::new());
    info!("Starting Embassy executor...");
    executor.run(|spawner| {
        spawner.spawn(net_task(runner).expect("net_task spawn token"));
        spawner.spawn(echo_task(stack).expect("echo_task spawn token"));
    });
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
