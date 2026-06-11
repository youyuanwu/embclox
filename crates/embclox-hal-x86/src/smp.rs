//! Application processor (AP) bring-up via the Limine MP request.
//!
//! Phase 3 of [docs/design/smp-per-cpu-executors.md](../../../../docs/design/smp-per-cpu-executors.md).
//!
//! `bring_up_aps` walks the [`MpResponse`] CPU list, populates a
//! [`CpuLocal`] slot for each AP, then writes its `goto_address`
//! field to launch the AP into the kernel-supplied `on_ap_ready`
//! callback. Limine guarantees that the `goto_address` store is
//! release-ordered relative to the AP's first instruction fetch, so
//! the AP sees the populated slot.
//!
//! `ap_setup` is the per-AP boot helper the kernel calls from inside
//! `on_ap_ready`: it loads `GS_BASE` with the processor id, loads the
//! shared IDT, enables the LAPIC, and starts the per-CPU APIC timer.

use crate::cpu_local::{self, MAX_CPUS};
use crate::vector_alloc::CpuId;
use core::sync::atomic::{AtomicU64, Ordering};
use limine::mp::Cpu;
use limine::response::MpResponse;

/// State an AP needs to finish its own boot, handed to the kernel's
/// `on_ap_ready` callback.
#[derive(Debug, Clone, Copy)]
pub struct ApInit {
    /// Identity of this AP. `CpuId::Ap(processor_id)`.
    pub cpu_id: CpuId,
    /// xAPIC ID. Copied from `cpus[i].lapic_id` for IOAPIC routing.
    pub apic_id: u8,
    /// TSC ticks-per-microsecond, calibrated on the BSP and copied
    /// to each AP so all CPUs share the same `embassy-time` epoch.
    pub tsc_per_us: u64,
}

/// Shared parameter every AP needs (TSC freq).
///
/// The BSP populates this with [`set_ap_init_params`] before calling
/// [`bring_up_aps`]; each AP reads it from inside its `goto_address`
/// thunk. The LAPIC MMIO VA is cached in [`crate::apic`] (via
/// [`crate::apic::init`] on the BSP) and is the same on every CPU,
/// so it doesn't need to be propagated through `ApInit`.
static AP_TSC_PER_US: AtomicU64 = AtomicU64::new(0);

/// Stash the parameters the AP startup path needs. Call once from
/// the BSP before [`bring_up_aps`].
pub fn set_ap_init_params(tsc_per_us: u64) {
    AP_TSC_PER_US.store(tsc_per_us, Ordering::Release);
}

/// Reconstruct an [`ApInit`] from inside an AP `goto_address` thunk.
///
/// `cpu.extra` carries the `processor_id` written by [`bring_up_aps`];
/// the shared TSC comes from the static populated by
/// [`set_ap_init_params`].
pub fn ap_init_from(cpu: &Cpu) -> ApInit {
    let processor_id = cpu.extra.load(Ordering::Acquire) as u8;
    ApInit {
        cpu_id: CpuId::Ap(processor_id),
        apic_id: cpu.lapic_id as u8,
        tsc_per_us: AP_TSC_PER_US.load(Ordering::Acquire),
    }
}

/// Kernel-supplied AP entry point. Wraps the AP body so the
/// `goto_address` thunk only has to call `ap_setup` + this. The
/// kernel writes a thunk like:
///
/// ```ignore
/// unsafe extern "C" fn ap_entry(cpu: &limine::mp::Cpu) -> ! {
///     let init = embclox_hal_x86::smp::ap_init_from(cpu);
///     unsafe { embclox_hal_x86::smp::ap_setup(init) };
///     my_kernel_on_ap_ready(init)
/// }
/// ```
pub type ApReady = fn(ApInit) -> !;

/// Bring up every AP reported by the bootloader, up to `max_aps`
/// (capped at [`MAX_CPUS`] - 1; one slot is reserved for the BSP).
///
/// Each AP's `goto_address` is written to `thunk`, which Limine
/// invokes with a `&Cpu` pointer. `thunk` must extract the per-AP
/// `ApInit` from somewhere (typically a side table indexed by
/// `cpu.lapic_id`) and call the kernel's [`ApReady`] callback.
///
/// `tsc_per_us` and `lapic_vaddr` are propagated to APs via the
/// returned `ApInit` table, which the caller populates from the
/// values it already computed during BSP setup.
///
/// Returns the number of APs successfully started (i.e. for which
/// `goto_address` was written). APs whose slot is full or whose
/// `lapic_id` equals the BSP's are skipped.
///
/// # Safety
/// - `thunk` must not return.
/// - Writing `goto_address` causes the AP to start executing
///   `thunk` immediately; the kernel must have finished any
///   per-AP state population *before* the write.
pub unsafe fn bring_up_aps(
    mp: &'static MpResponse,
    max_aps: usize,
    thunk: unsafe extern "C" fn(&Cpu) -> !,
) -> usize {
    let bsp_lapic_id = mp.bsp_lapic_id();
    let cap = max_aps.min(MAX_CPUS - 1);
    let mut started = 0usize;

    for (idx, cpu) in mp.cpus().iter().enumerate() {
        if started >= cap {
            break;
        }
        if cpu.lapic_id == bsp_lapic_id {
            // BSP entry; nothing to do, it's already running.
            continue;
        }

        // Assign a sequential processor_id starting at 1. We walk
        // cpus[] skipping the BSP and number the remainder 1, 2, 3,
        // ... in order encountered.
        let processor_id = (started + 1) as u8;

        // Hand the AP the processor_id via `extra`. The thunk reads
        // it back and feeds it into ap_setup, which populates the
        // CpuLocal slot on the AP itself (cpu_local::init_ap also
        // writes that AP's GS_BASE).
        cpu.extra
            .store(processor_id as u64, core::sync::atomic::Ordering::Release);

        // Release the AP. From here on the AP is running concurrently;
        // Limine guarantees the goto_address write is release-ordered
        // relative to the AP's first instruction fetch, so the AP sees
        // the extra value above.
        cpu.goto_address.write(thunk);

        log::info!(
            "smp: started AP processor_id={} (lapic_id={}, idx={})",
            processor_id,
            cpu.lapic_id,
            idx
        );
        started += 1;
    }

    log::info!("smp: {} AP(s) started", started);
    started
}

/// Per-AP boot helper. Call from the kernel's [`ApReady`] callback
/// before doing anything else on the AP.
///
/// Performs:
/// 1. Writes `GS_BASE` to `init.processor_id` so `current_cpu_id()`
///    returns the right value for this CPU.
/// 2. Loads the shared IDT register (`lidt`).
/// 3. Enables **this CPU's** LAPIC via [`crate::apic::enable`] and
///    programs its periodic timer via [`crate::runtime::ap_start_apic_timer`]
///    using the vector + period the BSP picked in
///    [`crate::runtime::start_apic_timer`]. The LAPIC MMIO VA is
///    already cached module-globally in [`crate::apic`]; LAPIC MMIO
///    is per-CPU-physical so these calls only ever touch this CPU's
///    LAPIC.
///
/// # Safety
/// Must be called exactly once per AP, on the AP itself, before any
/// other HAL code runs on that AP.
pub unsafe fn ap_setup(init: ApInit) {
    let processor_id = match init.cpu_id {
        CpuId::Bsp => panic!("ap_setup called with CpuId::Bsp"),
        CpuId::Ap(n) => n,
    };

    // Safety: AP context, called once per AP per the function contract.
    unsafe { cpu_local::init_ap(processor_id, init.apic_id) };

    // Safety: BSP already initialised the IDT static; we're just
    // loading the IDT register on this CPU.
    unsafe { crate::idt::load_current_cpu() };

    crate::apic::enable();
    crate::runtime::ap_start_apic_timer(init.tsc_per_us);

    log::info!(
        "smp: AP {} ready (apic_id={}, tsc/us={})",
        processor_id,
        init.apic_id,
        init.tsc_per_us
    );
}

/// Convenience: BSP-side read of `mp_response.bsp_lapic_id()` with
/// the truncation to `u8` xAPIC drivers expect.
pub fn bsp_lapic_id(mp: &MpResponse) -> u8 {
    mp.bsp_lapic_id() as u8
}

/// AP sanity-check: assert this AP's TSC is within `1 ms` of the BSP
/// reference. Returns the delta in TSC ticks (signed: positive if AP
/// ahead).
///
/// Hyper-V Gen1 / Azure / QEMU all start APs with TSC synchronised
/// to BSP per their respective documentation. This check catches
/// targets that violate that assumption before we rely on a shared
/// `embassy-time` epoch.
pub fn check_tsc_sync(bsp_tsc_at_ap_release: u64, tsc_per_us: u64) -> i64 {
    let ap_tsc = unsafe { core::arch::x86_64::_rdtsc() };
    let delta = ap_tsc as i64 - bsp_tsc_at_ap_release as i64;
    let one_ms_ticks = (tsc_per_us as i64) * 1_000;
    if delta.unsigned_abs() as i64 > one_ms_ticks {
        log::warn!(
            "smp: AP TSC out of sync (delta={} ticks, threshold={} ticks)",
            delta,
            one_ms_ticks
        );
    }
    delta
}
