//! Unified embclox kernel example.
//!
//! Boots whatever NIC is present (e1000, tulip, or NetVSC) via the
//! [`embclox_driver`] registry. See `README.md` and
//! [`docs/design/driver-model.md`](../../docs/design/driver-model.md).
//!
//! The application-side embassy tasks (`net_task`, `echo_task`,
//! `ap_heartbeat_task`) and the AP entry thunk live in [`app`]; this
//! file owns boot, probe, and the BSP-side executor handoff.

#![no_std]
#![no_main]

extern crate alloc;

mod app;

use core::panic::PanicInfo;
use embassy_net::{Stack, StackResources};
use embclox_core::dma_alloc::BootDmaAllocator;
use embclox_driver::{
    DriverRegistry, DynNic, PROBE_BUDGET, ProbeCtx, ProbedNic, register_default_drivers,
};
use embclox_hal_x86::ioapic::IoApic;
use embclox_hal_x86::vector_alloc::VectorAllocator;
use log::*;
use static_cell::StaticCell;

embclox_hal_x86::limine_boot_requests!(limine_boot);

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

    embclox_hal_x86::apic::init(
        p.memory
            .map_mmio(embclox_hal_x86::apic::LAPIC_PHYS_BASE, 0x1000)
            .vaddr(),
    );
    embclox_hal_x86::apic::enable();
    embclox_hal_x86::cpu_local::init_bsp(embclox_hal_x86::apic::id());

    // TSC calibration: prefer Hyper-V TSC freq MSR on Hyper-V, fall back
    // to the PIT, fall back to 2.4 GHz if neither works.
    let tsc_per_us = read_hv_tsc_freq()
        .or_else(embclox_hal_x86::pit::calibrate_tsc_mhz)
        .unwrap_or(2400);
    embclox_hal_x86::time::set_tsc_per_us(tsc_per_us);
    info!("TSC: {} cycles/us", tsc_per_us);

    embclox_hal_x86::runtime::start_apic_timer(tsc_per_us);

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

    let mut vmbus_holder = match embclox_hyperv::try_init(&dma, &mut p.memory) {
        Ok(opt) => opt,
        Err(e) => {
            warn!("VMBus init failed: {} (continuing without VMBus)", e);
            None
        }
    };
    let is_hyperv = vmbus_holder.is_some();
    if is_hyperv {
        // Marker for scripts/hyperv-boot-test.ps1.
        info!("VMBUS INIT PASSED");
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
    // `nic=<name>` cmdline filter: register only the matching driver.
    // Useful for forcing the tulip path on a Hyper-V VM that also has
    // VMBus (NetVSC would otherwise always win on priority). When
    // absent (the common case) we register the full default set.
    let nic_filter = embclox_hal_x86::cmdline::parse_nic_filter(boot_info.cmdline);
    match nic_filter {
        Some(name) => {
            info!("nic filter: registering only '{}'", name);
            embclox_driver::register_named_driver(&mut registry, name);
        }
        None => register_default_drivers(&mut registry),
    }

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
        match nic_filter {
            Some(name) => panic!(
                "no NIC matched nic={name}; PCI enumerated {} devices, hyperv={}",
                p.pci.enumerate().len(),
                is_hyperv
            ),
            None => panic!(
                "no recognised NIC; PCI enumerated {} devices, hyperv={}",
                p.pci.enumerate().len(),
                is_hyperv
            ),
        }
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

    // --- Optional SMP AP bring-up --------------------------------------
    //
    // Gated by `smp=on` in the Limine cmdline. When absent, APs sit in
    // Limine's spin loop and the kernel runs single-CPU as before.
    let smp_cfg = embclox_hal_x86::cmdline::parse_smp(boot_info.cmdline);
    if smp_cfg.enabled {
        if let Some(mp) = limine_boot::mp_response() {
            let max_aps = smp_cfg
                .max_aps
                .unwrap_or(embclox_hal_x86::cpu_local::MAX_CPUS - 1);
            embclox_hal_x86::smp::set_ap_init_params(tsc_per_us);
            // Safety: ap_entry never returns, and we have populated the
            // AP init params above (release-ordered).
            let started = unsafe { embclox_hal_x86::smp::bring_up_aps(mp, max_aps, app::ap_entry) };
            info!("smp: {} of {} requested AP(s) brought up", started, max_aps);
        } else {
            warn!("smp=on but Limine returned no MP response; running single-CPU");
        }
    }

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
    spawner.spawn(app::net_task(runner).expect("net_task SpawnToken"));
    spawner.spawn(app::echo_task(stack).expect("echo_task SpawnToken"));

    info!("kernel-example: starting embassy executor");
    embclox_hal_x86::runtime::run_executor(executor);
}

// ---------- helpers ---------------------------------------------------

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

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    error!("PANIC: {}", info);
    loop {
        x86_64::instructions::hlt();
    }
}
