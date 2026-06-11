//! IDT vector allocation for driver-installed ISRs.
//!
//! `VectorAllocator` hands out free vectors from a fixed pool, honouring
//! an explicit exclusion list at construction so the kernel's reserved
//! vectors (APIC timer = 32, spurious = 39, optional SynIC SINT2 = 34)
//! can never be re-issued to a PCI driver.
//!
//! One allocator covers the whole system: the IDT is shared across
//! all CPUs, so vector 33 is the same IDT slot wherever it's
//! delivered. Driver placement on a specific CPU is an IOAPIC routing
//! decision; see [`crate::vector_alloc::CpuId`] and
//! `embclox_driver::ProbeCtx::install_pci_isr_on`.
//!
//! `InstalledIsr` is the receipt callers receive: the allocated
//! vector and the [`CpuId`] the IOAPIC entry was pointed at.
//!
//! See [docs/design/driver-model.md](../../../../docs/design/driver-model.md)
//! sections "ISR registration" and "SMP-forward design choices §1",
//! and [docs/design/smp-per-cpu-executors.md](../../../../docs/design/smp-per-cpu-executors.md).

/// Logical CPU identity.
///
/// `CpuId::Ap(n)` carries Limine's sequential `processor_id`
/// (1..MAX_CPUS), not the LAPIC ID. The LAPIC ID lives in
/// [`crate::cpu_local::CpuLocal::apic_id`] and is looked up via
/// [`apic_id`](CpuId::apic_id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuId {
    /// The boot CPU (Limine `processor_id == 0`).
    Bsp,
    /// An application processor with sequential `processor_id`
    /// (1..[`crate::cpu_local::MAX_CPUS`]).
    Ap(u8),
}

impl CpuId {
    /// LAPIC ID for IOAPIC routing.
    ///
    /// Resolves via [`crate::cpu_local::by_id`]. Returns `0` (the BSP's
    /// conventional ID) if the slot has not been populated yet — this
    /// keeps the single-CPU pre-SMP code path working before
    /// [`crate::cpu_local::init_bsp`] is called.
    pub fn apic_id(self) -> u8 {
        crate::cpu_local::by_id(self)
            .map(|c| c.apic_id)
            .unwrap_or(0)
    }
}

/// Result of a successful `VectorAllocator::allocate()` (or the
/// corresponding `ProbeCtx::install_pci_isr`). Carries both pieces of
/// information drivers need to wire their interrupt: the IDT vector
/// they were assigned and the CPU it will be delivered to.
#[derive(Debug, Clone, Copy)]
pub struct InstalledIsr {
    pub vector: u8,
    pub cpu_id: CpuId,
}

/// Fixed-pool IDT vector allocator.
///
/// Constructed over the standard external-interrupt range (default
/// `33..=47`, the 15 vectors above the APIC timer that the IOAPIC can
/// route to), with an explicit list of vectors already claimed by the
/// kernel.
pub struct VectorAllocator {
    /// Bit `i` set => vector `start + i` is taken.
    used: u32,
    start: u8,
    end: u8,
}

impl VectorAllocator {
    /// Default external-interrupt range: vectors `33..=47`.
    ///
    /// 32 is the APIC timer ([`crate::runtime::APIC_TIMER_VECTOR`]) and
    /// 39 is the spurious vector ([`crate::runtime::SPURIOUS_VECTOR`]);
    /// both should appear in `reserved` if `start <= them <= end`.
    pub fn new(start: u8, end: u8, reserved: &[u8]) -> Self {
        assert!(start >= 32, "vectors 0..32 are CPU exceptions");
        assert!(end >= start, "empty allocator range");
        assert!(
            (end - start) < 32,
            "VectorAllocator currently uses a u32 bitmap; widen if you need >32 vectors"
        );
        let mut alloc = Self {
            used: 0,
            start,
            end,
        };
        for &v in reserved {
            if (start..=end).contains(&v) {
                alloc.used |= 1 << (v - start);
            }
        }
        alloc
    }

    /// Allocate the next free IDT vector. Returns `None` if the pool
    /// is exhausted.
    ///
    /// IDT vectors are a system-wide scarce resource (one shared IDT
    /// across all CPUs); the allocator returns the bare `u8` and the
    /// caller stamps the [`InstalledIsr`] with the target [`CpuId`]
    /// it routed the IOAPIC entry to.
    pub fn allocate(&mut self) -> Option<u8> {
        let span = (self.end - self.start + 1) as u32;
        for i in 0..span {
            if self.used & (1 << i) == 0 {
                self.used |= 1 << i;
                return Some(self.start + i as u8);
            }
        }
        None
    }

    /// Returns the count of vectors still available.
    pub fn free_count(&self) -> u32 {
        let span = (self.end - self.start + 1) as u32;
        let mask = if span == 32 {
            u32::MAX
        } else {
            (1 << span) - 1
        };
        (!self.used & mask).count_ones()
    }
}
