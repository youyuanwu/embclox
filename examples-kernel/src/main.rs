//! Unified embclox kernel example.
//!
//! Boots whatever NIC is present (e1000, tulip, or NetVSC) via the
//! [`embclox_driver`] registry. See `README.md` and
//! [`docs/design/driver-model.md`](../../docs/design/driver-model.md).

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicUsize, Ordering};
use embassy_net::{Stack, StackResources};
use embclox_core::dma_alloc::BootDmaAllocator;
use embclox_driver::{
    DriverRegistry, DynNic, PROBE_BUDGET, ProbeCtx, ProbedNic, register_default_drivers,
};
use embclox_hal_x86::apic::LocalApic;
use embclox_hal_x86::ioapic::IoApic;
use embclox_hal_x86::vector_alloc::VectorAllocator;
use embedded_io_async::Write as AsyncWrite;
use log::*;
use static_cell::StaticCell;
use x86_64::structures::idt::InterruptStackFrame;

embclox_hal_x86::limine_boot_requests!(limine_boot);

// ---------- SINT2 ISR (only used on Hyper-V) ---------------------------

/// SIEFP virtual address, published after `embclox_hyperv::init` so the
/// SINT2 ISR can clear per-channel event flags.
static SIEFP_VADDR: AtomicUsize = AtomicUsize::new(0);

/// SynIC SINT2 → VMBus handler: clear the
/// event-flag bits the host set, wake `NETVSC_WAKER`. SINT MSR is
/// configured auto-EOI, so no LAPIC EOI here.
extern "x86-interrupt" fn vmbus_isr(_frame: InterruptStackFrame) {
    let siefp = SIEFP_VADDR.load(Ordering::Relaxed);
    if siefp != 0 {
        let slot = (siefp + (embclox_hyperv::msr::VMBUS_SINT as usize) * 256) as *mut u64;
        for i in 0..32usize {
            // SAFETY: SIEFP is a 4 KiB DMA page we own; SINT2 slot is in
            // bounds. Volatile to observe host writes.
            unsafe {
                let p = slot.add(i);
                let w = core::ptr::read_volatile(p);
                if w != 0 {
                    core::ptr::write_volatile(p, 0);
                }
            }
        }
    }
    embclox_hyperv::netvsc::NETVSC_WAKER.wake();
}

// ---------- Network config ---------------------------------------------

/// Default static-IP config used when the Limine cmdline doesn't set
/// `net=`. Tuned for QEMU SLIRP (`-netdev user`); Hyper-V/Azure should
/// pass `net=dhcp` (or static) on the cmdline.
const NET_DEFAULTS: embclox_hal_x86::cmdline::StaticDefaults =
    embclox_hal_x86::cmdline::StaticDefaults {
        ip: [10, 0, 2, 15],
        prefix: 24,
        gw: [10, 0, 2, 2],
    };

// ---------- kmain ------------------------------------------------------

#[unsafe(no_mangle)]
unsafe extern "C" fn kmain() -> ! {
    let boot_info = limine_boot::collect();
    let mut p = embclox_hal_x86::init(
        boot_info,
        embclox_hal_x86::Config {
            heap_size: 8 * 1024 * 1024,
            ..Default::default()
        },
    );
    info!("embclox kernel example booting (Limine)");
    // Marker scanned by scripts/hyperv-boot-test.ps1 to confirm the
    // kernel reached `kmain`. Kept the same string the original
    // examples-hyperv binary emitted so the PS test stayed unchanged
    // through Phase 3c.
    info!("HYPERV BOOT PASSED");

    // --- Interrupt + APIC infrastructure -------------------------------

    embclox_hal_x86::idt::init();
    embclox_hal_x86::pic::disable();

    let lapic_vaddr = p
        .memory
        .map_mmio(embclox_hal_x86::apic::LAPIC_PHYS_BASE, 0x1000)
        .vaddr();
    let mut lapic = LocalApic::new(lapic_vaddr);
    lapic.enable();

    // TSC calibration: prefer Hyper-V TSC freq MSR on Hyper-V, fall back
    // to the PIT, fall back to 2.4 GHz if neither works.
    let tsc_per_us = read_hv_tsc_freq()
        .or_else(embclox_hal_x86::pit::calibrate_tsc_mhz)
        .unwrap_or(2400);
    embclox_hal_x86::time::set_tsc_per_us(tsc_per_us);
    info!("TSC: {} cycles/us", tsc_per_us);

    embclox_hal_x86::runtime::start_apic_timer(lapic, tsc_per_us, 1_000);

    // IOAPIC
    let ioapic_vaddr = p
        .memory
        .map_mmio(embclox_hal_x86::ioapic::IOAPIC_PHYS_BASE, 0x1000)
        .vaddr();
    let mut ioapic = IoApic::new(ioapic_vaddr);
    ioapic.log_info();

    // --- Hyper-V VMBus (non-fatal) -------------------------------------

    let dma = BootDmaAllocator {
        kernel_offset: p.memory.kernel_offset(),
        phys_offset: p.memory.phys_offset(),
    };

    let hv_features = embclox_hyperv::detect::detect();
    let is_hyperv = matches!(&hv_features, Some(f) if f.has_synic && f.has_hypercall);

    let mut vmbus_holder: Option<embclox_hyperv::VmBus> = None;
    if is_hyperv {
        info!("Hyper-V: SynIC + hypercall present, initialising VMBus");
        // Install SINT2 ISR BEFORE VmBus::init so the host's first
        // INITIATE_CONTACT response can wake the synchronous boot loop.
        unsafe {
            embclox_hal_x86::idt::set_handler(embclox_hyperv::msr::VMBUS_VECTOR, vmbus_isr);
        }
        match embclox_hyperv::init(&dma, &mut p.memory) {
            Ok(vmbus) => {
                SIEFP_VADDR.store(vmbus.siefp_vaddr(), Ordering::Release);
                info!(
                    "VMBus: version={:#x}, {} offers",
                    vmbus.version(),
                    vmbus.offers().len()
                );
                // Marker for scripts/hyperv-boot-test.ps1.
                info!("VMBUS INIT PASSED");
                vmbus_holder = Some(vmbus);
            }
            Err(e) => warn!("VMBus init failed: {} (continuing without VMBus)", e),
        }
    } else {
        info!("Hyper-V not detected; PCI-only boot path");
    }

    // --- Driver registry + probe loop ----------------------------------

    // Vector reservations: 32 (APIC timer), 39 (spurious), 34 (SINT2,
    // reserved whenever we *might* be running on Hyper-V even if VMBus
    // init failed — see the design doc).
    let reservations: &[u8] = if is_hyperv { &[32, 34, 39] } else { &[32, 39] };
    let mut irq_alloc = VectorAllocator::new(32, 47, reservations);
    info!(
        "VectorAllocator: {} vectors free in 32..=47",
        irq_alloc.free_count()
    );

    let mut registry = DriverRegistry::new();
    register_default_drivers(&mut registry);

    let nics: alloc::vec::Vec<ProbedNic> = {
        let mut ctx = ProbeCtx {
            dma: &dma,
            memory: &mut p.memory,
            ioapic: &mut ioapic,
            irq_alloc: &mut irq_alloc,
            pci: &p.pci,
            vmbus: vmbus_holder.as_mut(),
        };
        embclox_driver::probe_all(&registry, &mut ctx, &p.pci)
    };
    // Registry is no longer needed; drop it before we hand off to embassy
    // so the driver instances are released back to the heap.
    drop(registry);

    if nics.is_empty() {
        panic!(
            "no recognised NIC; PCI enumerated {} devices, hyperv={}",
            p.pci.enumerate().len(),
            is_hyperv
        );
    }

    for n in &nics {
        info!("nic: {} priority={}", n.name, n.priority);
        // Marker for scripts/hyperv-boot-test.ps1.
        if n.name == "netvsc" {
            info!("NETVSC INIT PASSED");
        }
    }
    if nics.len() == PROBE_BUDGET {
        warn!(
            "probe budget ({}) hit; some matching devices may have been skipped",
            PROBE_BUDGET
        );
    }
    let primary = nics
        .into_iter()
        .min_by_key(|n| n.priority)
        .expect("nics non-empty");
    info!(
        "primary NIC: {} (priority {})",
        primary.name, primary.priority
    );

    // --- Embassy stack -------------------------------------------------

    let net_mode = embclox_hal_x86::cmdline::parse_net_mode(boot_info.cmdline, NET_DEFAULTS);
    let config = match net_mode {
        embclox_hal_x86::cmdline::NetMode::Dhcp => {
            info!("network: DHCPv4");
            embassy_net::Config::dhcpv4(Default::default())
        }
        embclox_hal_x86::cmdline::NetMode::Static { ip, prefix, gw } => {
            info!(
                "network: static {}.{}.{}.{}/{} gw {}.{}.{}.{}",
                ip[0], ip[1], ip[2], ip[3], prefix, gw[0], gw[1], gw[2], gw[3]
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

    let driver = DynNic(primary.driver);
    let (stack, runner) = embassy_net::new(driver, config, resources, 0xdead_beef_cafe_f00du64);
    static STACK: StaticCell<Stack> = StaticCell::new();
    let stack = &*STACK.init(stack);

    static EXECUTOR: StaticCell<embassy_executor::raw::Executor> = StaticCell::new();
    let executor = EXECUTOR.init(embassy_executor::raw::Executor::new(core::ptr::null_mut()));

    let spawner = executor.spawner();
    spawner.spawn(net_task(runner).expect("net_task SpawnToken"));
    spawner.spawn(echo_task(stack).expect("echo_task SpawnToken"));

    info!("kernel-example: starting embassy executor");
    embclox_hal_x86::runtime::run_executor(executor);
}

// ---------- helpers + tasks --------------------------------------------

fn read_hv_tsc_freq() -> Option<u64> {
    // Only valid on Hyper-V; reading the MSR on bare metal will #GP.
    // Gate on the same detect that drives VMBus init.
    if embclox_hyperv::detect::detect().is_some() {
        let hz = unsafe { embclox_hyperv::msr::rdmsr(embclox_hyperv::msr::TSC_FREQUENCY) };
        if hz != 0 {
            return Some(hz / 1_000_000);
        }
    }
    None
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, DynNic>) {
    runner.run().await
}

#[embassy_executor::task]
async fn echo_task(stack: &'static Stack<'static>) {
    // Wait for an IPv4 address (immediate for static, ~1-3s for DHCP).
    loop {
        if let Some(cfg) = stack.config_v4() {
            // Marker for scripts/hyperv-boot-test.ps1 — string format
            // inherited from the retired examples-hyperv so the PS
            // test stayed unchanged through Phase 3c.
            info!("PHASE4B: IPv4 configured: {}", cfg.address);
            if let Some(gw) = cfg.gateway {
                info!("PHASE4B: gateway: {}", gw);
            }
            break;
        }
        embassy_time::Timer::after_millis(100).await;
    }
    // Marker for scripts/hyperv-boot-test.ps1.
    info!("PHASE4B ECHO READY: TCP port 1234");

    let mut rx = [0u8; 1024];
    loop {
        let mut tx = [0u8; 1024];
        let mut socket = embassy_net::tcp::TcpSocket::new(*stack, &mut rx, &mut tx);
        socket.set_timeout(None);
        if socket.accept(1234).await.is_err() {
            continue;
        }
        info!("kernel-example: tcp client connected");
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
        info!("kernel-example: tcp client disconnected");
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    error!("PANIC: {}", info);
    loop {
        x86_64::instructions::hlt();
    }
}
