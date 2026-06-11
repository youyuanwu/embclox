//! Per-CPU state table.
//!
//! Fixed-size array indexed by sequential `processor_id` (Limine's
//! position in `cpus()`). Each slot is populated once: BSP populates
//! slot 0 from [`init_bsp`] early in boot; APs populate their own
//! slots from [`init_ap`] inside `smp::bring_up_aps`.
//!
//! The currently-executing CPU's `processor_id` is held in `GS_BASE`
//! (kernel-mode `IA32_GS_BASE`, MSR `0xC000_0101`). Reading it costs
//! one `mov gs:OFFSET` instead of an MSR read, and the writer sets it
//! once during `init_bsp` / `ap_setup`. We have no userspace and
//! never `swapgs`, so the kernel-side GS base stays put for the
//! lifetime of the CPU.
//!
//! See [docs/design/smp-per-cpu-executors.md](../../../../docs/design/smp-per-cpu-executors.md).

use crate::vector_alloc::CpuId;
use spin::Once;
use x86_64::registers::model_specific::GsBase;
use x86_64::VirtAddr;

/// Maximum number of CPUs the per-CPU table is sized for. 8 covers
/// QEMU smoke tests, Hyper-V Gen1 / Azure standard SKUs, and leaves
/// headroom. Bigger boxes can bump the constant.
pub const MAX_CPUS: usize = 8;

/// Per-CPU runtime state. Populated once during boot for each CPU
/// that joins the kernel.
#[derive(Debug, Clone, Copy)]
pub struct CpuLocal {
    pub cpu_id: CpuId,
    /// LAPIC ID for IOAPIC routing. xAPIC IDs fit in `u8`; x2APIC is
    /// not supported yet.
    pub apic_id: u8,
}

static CPU_LOCALS: [Once<CpuLocal>; MAX_CPUS] = [const { Once::new() }; MAX_CPUS];

/// Convert a [`CpuId`] to its index in [`CPU_LOCALS`].
fn slot_of(cpu: CpuId) -> usize {
    match cpu {
        CpuId::Bsp => 0,
        CpuId::Ap(n) => n as usize,
    }
}

/// Convert a `processor_id` to a [`CpuId`].
fn cpu_id_of(processor_id: u8) -> CpuId {
    if processor_id == 0 {
        CpuId::Bsp
    } else {
        CpuId::Ap(processor_id)
    }
}

/// Set `GS_BASE` to `processor_id` so subsequent [`current_cpu_id`]
/// calls on this CPU return the right ID.
///
/// Called by [`init_bsp`] on the BSP and by `smp::ap_setup` on each
/// AP. The address stored is a small integer (the processor_id),
/// not a real pointer; we read it back as an integer via the inline
/// `mov gs:0` in [`current_cpu_id`].
///
/// # Safety
/// Writes the kernel `GS_BASE` MSR. Must be called in kernel mode
/// before any code on this CPU reads `current_cpu_id()`.
unsafe fn set_current(processor_id: u8) {
    GsBase::write(VirtAddr::new(processor_id as u64));
}

/// Populate the BSP's slot and set `GS_BASE`. Call once, after the
/// LAPIC is enabled and its ID is readable.
pub fn init_bsp(apic_id: u8) {
    CPU_LOCALS[0].call_once(|| CpuLocal {
        cpu_id: CpuId::Bsp,
        apic_id,
    });
    // Safety: called once from the BSP during init, before any code
    // reads current_cpu_id().
    unsafe { set_current(0) };
}

/// Populate an AP's slot and set its `GS_BASE`. Called by
/// `smp::ap_setup` from each AP's startup path.
///
/// # Panics
/// Panics if `processor_id == 0` (BSP slot, use [`init_bsp`]) or if
/// `processor_id >= MAX_CPUS`.
///
/// # Safety
/// Writes the calling CPU's `GS_BASE` MSR. Must be called exactly
/// once per AP, before that AP reads `current_cpu_id()`.
pub unsafe fn init_ap(processor_id: u8, apic_id: u8) {
    assert!(processor_id != 0, "AP processor_id 0 is reserved for BSP");
    let slot = processor_id as usize;
    assert!(slot < MAX_CPUS, "AP processor_id {} exceeds MAX_CPUS", slot);
    CPU_LOCALS[slot].call_once(|| CpuLocal {
        cpu_id: CpuId::Ap(processor_id),
        apic_id,
    });
    // Safety: precondition delegated to caller (we're inside an
    // already-unsafe function).
    unsafe { set_current(processor_id) };
}

/// Look up a CPU's local state.
///
/// Returns `None` if the slot has not been populated (out-of-range
/// `Ap(n)`, or AP that has not run its init yet).
pub fn by_id(cpu: CpuId) -> Option<&'static CpuLocal> {
    CPU_LOCALS.get(slot_of(cpu)).and_then(|once| once.get())
}

/// Identity of the CPU executing this call.
///
/// Reads `GS_BASE`. Returns [`CpuId::Bsp`] before any CPU has called
/// [`init_bsp`] / [`init_ap`] — early-boot code that runs before
/// `init_bsp` (e.g. `hal::init`) sees the BSP identity, which is
/// correct for the only CPU executing at that point.
pub fn current_cpu_id() -> CpuId {
    let processor_id = GsBase::read().as_u64() as u8;
    cpu_id_of(processor_id)
}

/// Local state for the CPU executing this call. Equivalent to
/// `by_id(current_cpu_id()).expect(...)`.
///
/// # Panics
/// Panics if [`init_bsp`] has not been called yet. Callers in driver
/// code that runs after `embclox_hal_x86::init` can rely on the BSP
/// slot being populated.
pub fn current() -> &'static CpuLocal {
    by_id(current_cpu_id()).expect("cpu_local::init_bsp() not called yet")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_of_layout() {
        assert_eq!(slot_of(CpuId::Bsp), 0);
        assert_eq!(slot_of(CpuId::Ap(1)), 1);
        assert_eq!(slot_of(CpuId::Ap(7)), 7);
    }

    #[test]
    fn cpu_id_of_zero_is_bsp() {
        assert_eq!(cpu_id_of(0), CpuId::Bsp);
        assert_eq!(cpu_id_of(1), CpuId::Ap(1));
        assert_eq!(cpu_id_of(7), CpuId::Ap(7));
    }
}
