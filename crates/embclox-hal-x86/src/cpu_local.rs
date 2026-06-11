//! Per-CPU state table.
//!
//! Fixed-size array indexed by sequential `processor_id` (Limine
//! numbering). Each slot is populated once: BSP populates slot 0 from
//! `init_bsp` early in boot; APs populate their own slots from
//! `init_ap` during `bring_up_aps` (phase 3, not yet implemented).
//!
//! Until AP bring-up lands, `current_cpu_id()` always returns
//! [`CpuId::Bsp`]. Phase 3 will replace this with a `GS_BASE` read.
//!
//! See [docs/design/smp-per-cpu-executors.md](../../../../docs/design/smp-per-cpu-executors.md).

use crate::vector_alloc::CpuId;
use spin::Once;

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

/// Populate the BSP's slot. Call once, after the LAPIC is enabled and
/// its ID is readable.
pub fn init_bsp(apic_id: u8) {
    CPU_LOCALS[0].call_once(|| CpuLocal {
        cpu_id: CpuId::Bsp,
        apic_id,
    });
}

/// Populate an AP's slot. Called from the AP's startup path during
/// `bring_up_aps` (phase 3).
///
/// # Panics
/// Panics if `processor_id == 0` (BSP slot, use [`init_bsp`]) or if
/// `processor_id >= MAX_CPUS`.
pub fn init_ap(processor_id: u8, apic_id: u8) {
    assert!(processor_id != 0, "AP processor_id 0 is reserved for BSP");
    let slot = processor_id as usize;
    assert!(slot < MAX_CPUS, "AP processor_id {} exceeds MAX_CPUS", slot);
    CPU_LOCALS[slot].call_once(|| CpuLocal {
        cpu_id: CpuId::Ap(processor_id),
        apic_id,
    });
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
/// **Phase 1 placeholder**: hardcoded to [`CpuId::Bsp`]. Phase 3 (AP
/// bring-up) will replace this with a `GS_BASE` read so each CPU
/// returns its own ID.
pub fn current_cpu_id() -> CpuId {
    CpuId::Bsp
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
}
