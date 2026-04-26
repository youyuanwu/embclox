#![no_std]
#![no_main]

extern crate alloc;
extern crate embclox_hal_x86;

mod framebuffer;

use bootloader_api::{BootInfo, BootloaderConfig, config::Mapping, entry_point};
use core::panic::PanicInfo;
use embclox_core::dma_alloc::BootDmaAllocator;
use log::*;

const BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    // Grab framebuffer info BEFORE HAL init consumes boot_info
    let fb_ptr = boot_info
        .framebuffer
        .as_ref()
        .map(|fb| fb.buffer().as_ptr() as usize);
    let fb_info = boot_info.framebuffer.as_ref().map(|fb| fb.info().clone());

    // Initialize HAL: serial logging, heap, memory mapper
    let mut p = embclox_hal_x86::init(boot_info, embclox_hal_x86::Config::default());
    info!("embclox Hyper-V example booting...");

    // Get the physical address of the bootloader's GOP framebuffer.
    // On Hyper-V, this GPA is the synthvid PCI BAR — the host already knows it.
    let fb_phys = fb_ptr.and_then(|vaddr| p.memory.translate_addr(vaddr as u64));
    if let (Some(phys), Some(info)) = (fb_phys, &fb_info) {
        info!(
            "GOP framebuffer: {}x{} paddr={:#x}",
            info.width, info.height, phys
        );
    }

    // Set up framebuffer writer (if available) for on-screen output
    // This works on QEMU via UEFI GOP. On Hyper-V Gen2, the screen is
    // black until we bring up VMBus synthvid — but we still init the
    // writer for later use.

    // Try VMBus initialization
    let dma = BootDmaAllocator {
        kernel_offset: p.memory.kernel_offset(),
        phys_offset: p.memory.phys_offset(),
    };

    match embclox_hyperv::detect::detect() {
        Some(_) => {
            embclox_hyperv::CRASH_AFTER_STAGE.store(0, core::sync::atomic::Ordering::Relaxed);

            match embclox_hyperv::init(&dma, &mut p.memory) {
                Ok(mut vmbus) => {
                    // Find Hyper-V Video PCI device BAR0 (now handles 64-bit BARs)
                    let video_pci = p.pci.find_device_any(0x1414, &[0x5353]);
                    let bar0 = video_pci
                        .as_ref()
                        .map(|dev| p.pci.read_bar(dev, 0))
                        .unwrap_or(0);

                    let (fb_phys, fb_vaddr) = if bar0 != 0 {
                        let mapping = p.memory.map_mmio(bar0, 8 * 1024 * 1024);
                        (Some(bar0), Some(mapping.vaddr()))
                    } else {
                        (None, None)
                    };

                    match embclox_hyperv::synthvid::init_display(
                        &mut vmbus, 1024, 768, &dma, &p.memory, fb_phys, fb_vaddr,
                    ) {
                        Ok(Some(mut display)) => {
                            let w = display.width();
                            let h = display.height();
                            let stride = display.stride();
                            let fb = display.framebuffer();
                            for y in 0..h {
                                for x in 0..w {
                                    let off = ((y * stride + x) * 4) as usize;
                                    unsafe {
                                        *fb.add(off) = 0xFF;
                                        *fb.add(off + 1) = 0x00;
                                        *fb.add(off + 2) = 0x00;
                                        *fb.add(off + 3) = 0xFF;
                                    }
                                }
                            }
                            for y in 100..200 {
                                for x in 50..600 {
                                    display.put_pixel(x, y, 0xFF, 0xFF, 0xFF);
                                }
                            }
                            let _ = display.dirt_full();
                            loop {
                                core::hint::spin_loop();
                            }
                        }
                        Ok(None) => unsafe { core::arch::asm!("ud2") },
                        Err(_) => unsafe { core::arch::asm!("int3") },
                    }
                }
                Err(_) => {}
            }
        }
        None => {
            info!("Not running on Hyper-V (QEMU or bare metal)");
        }
    }

    info!("Halting.");
    // Heartbeat loop so serial output is visible even if PuTTY connects late
    let mut tick = 0u64;
    loop {
        for _ in 0..500_000_000u64 {
            core::hint::spin_loop();
        }
        tick += 1;
        info!("heartbeat {}", tick);
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    error!("{}", info);
    loop {
        x86_64::instructions::hlt();
    }
}
