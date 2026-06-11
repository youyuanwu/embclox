use embclox_hal_x86::cpu_local::{self, MAX_CPUS};
use embclox_hal_x86::vector_alloc::CpuId;

/// `cpu_local` table and `CpuId` plumbing tests (phase 1, pre-AP).
///
/// Verifies the BSP slot is populated and that the read APIs behave
/// before APs exist.
#[embclox_test_macros::test_suite(name = "cpu_local")]
mod tests {
    use super::*;

    /// `MAX_CPUS` is the documented size of the per-CPU table.
    #[test]
    fn max_cpus_is_eight() {
        assert_eq!(MAX_CPUS, 8);
    }

    /// `init_bsp` is idempotent (the test harness has already called
    /// `embclox_hal_x86::init`, which has *not* called `init_bsp` yet
    /// because the unit-test runner skips APIC setup). We populate it
    /// here and any later call must not re-overwrite the slot.
    #[test]
    fn init_bsp_populates_then_is_stable() {
        cpu_local::init_bsp(0x42);
        let first = cpu_local::by_id(CpuId::Bsp).expect("BSP slot populated");
        assert_eq!(first.cpu_id, CpuId::Bsp);
        assert_eq!(first.apic_id, 0x42);

        // Second call must be a no-op (spin::Once::call_once contract).
        cpu_local::init_bsp(0x99);
        let second = cpu_local::by_id(CpuId::Bsp).unwrap();
        assert_eq!(second.apic_id, 0x42);
    }

    /// `current_cpu_id()` returns BSP after `init_bsp` runs (it
    /// reads `GS_BASE`, which `init_bsp` writes to 0).
    #[test]
    fn current_is_bsp() {
        cpu_local::init_bsp(0x42);
        assert_eq!(cpu_local::current_cpu_id(), CpuId::Bsp);
    }

    /// `CpuId::apic_id()` resolves through `cpu_local`.
    #[test]
    fn apic_id_resolves_via_cpu_local() {
        cpu_local::init_bsp(0x42);
        assert_eq!(CpuId::Bsp.apic_id(), 0x42);
    }

    /// Unpopulated AP slots return `None` from `by_id` and `0` from
    /// `apic_id()`.
    #[test]
    fn unpopulated_ap_slot_returns_none() {
        assert!(cpu_local::by_id(CpuId::Ap(3)).is_none());
        assert_eq!(CpuId::Ap(3).apic_id(), 0);
    }

    /// Out-of-range `Ap(n)` also returns `None`.
    #[test]
    fn out_of_range_ap_returns_none() {
        assert!(cpu_local::by_id(CpuId::Ap(MAX_CPUS as u8 + 1)).is_none());
    }
}

pub use tests::suite;
