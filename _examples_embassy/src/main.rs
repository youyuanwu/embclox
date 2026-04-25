#![no_std]
#![no_main]

extern crate alloc;
extern crate hal_x86; // pulls in critical_section, time_driver, heap, serial/logger

mod dma_alloc;
mod e1000_adapter;
mod mmio_regs;

use bootloader_api::{BootInfo, BootloaderConfig, config::Mapping, entry_point};
use core::panic::PanicInfo;
use dma_alloc::BootDmaAllocator;
use e1000_adapter::E1000Embassy;
use embassy_executor::Executor;
use embassy_net::{Ipv4Address, Ipv4Cidr, Stack, StackResources, StaticConfigV4};
use embedded_io_async::Write;
use log::*;
use mmio_regs::MmioRegs;
use static_cell::StaticCell;

const BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    let mut p = hal_x86::init(boot_info, hal_x86::Config::default());
    info!("Booting e1000-embassy example...");

    // PCI scan for e1000
    let pci_dev = p
        .pci
        .find_device_any(0x8086, &[0x100E, 0x100F, 0x10D3])
        .expect("e1000 device not found on PCI bus");
    let bar0_phys = p.pci.read_bar(&pci_dev, 0);

    // Map e1000 BAR0 MMIO with Uncacheable pages
    let e1000_vaddr = p.memory.map_mmio(bar0_phys, 0x20000);
    info!("e1000 MMIO vaddr: {:#x}", e1000_vaddr);

    let regs = MmioRegs::new(e1000_vaddr);

    // Caller performs device reset before new() per driver contract
    e1000_reset(&regs);
    p.pci.enable_bus_mastering(&pci_dev);

    // Initialize e1000 driver
    let dma = BootDmaAllocator {
        kernel_offset: p.memory.kernel_offset(),
        phys_offset: p.memory.phys_offset(),
    };
    let mut e1000_device = e1000::E1000Device::new(regs, dma);
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

    static EXECUTOR: StaticCell<Executor> = StaticCell::new();
    let executor = EXECUTOR.init(Executor::new());
    info!("Starting Embassy executor...");
    executor.run(|spawner| {
        spawner.spawn(net_task(runner).expect("net_task spawn token"));
        spawner.spawn(echo_task(stack).expect("echo_task spawn token"));
    });
}

fn e1000_reset(regs: &MmioRegs) {
    use e1000::RegisterAccess;
    use e1000::regs::*;

    regs.write_reg(IMS, 0);
    let ctl = regs.read_reg(CTL);
    regs.write_reg(CTL, ctl | CTL_RST);

    let mut timeout = 100_000u32;
    loop {
        if regs.read_reg(CTL) & CTL_RST == 0 {
            break;
        }
        timeout -= 1;
        assert!(timeout > 0, "e1000 reset timeout");
    }

    regs.write_reg(IMS, 0);
    regs.write_reg(CTL, CTL_SLU | CTL_ASDE);
    regs.write_reg(FCAL, 0);
    regs.write_reg(FCAH, 0);
    regs.write_reg(FCT, 0);
    regs.write_reg(FCTTV, 0);
    info!("e1000 device reset complete");
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
