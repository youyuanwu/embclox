//! Hyper-V detection via CPUID.

/// Hyper-V feature flags discovered from CPUID.
pub struct HvFeatures {
    /// Maximum hypervisor CPUID leaf (from CPUID 0x40000000.EAX).
    pub max_leaf: u32,
    /// SynIC registers are accessible (CPUID 0x40000003.EAX bit 2).
    pub has_synic: bool,
    /// Hypercall MSRs are accessible (CPUID 0x40000003.EAX bit 5).
    pub has_hypercall: bool,
}

/// Detect Hyper-V hypervisor via CPUID 0x40000000.
///
/// Returns `None` if not running on Hyper-V.
pub fn detect() -> Option<HvFeatures> {
    // CPUID leaf 0x40000000: hypervisor vendor ID
    let r = core::arch::x86_64::__cpuid(0x40000000);

    // Check "Microsoft Hv" signature (EBX:ECX:EDX)
    if r.ebx != 0x7263694D  // "Micr"
        || r.ecx != 0x666F736F // "osof"
        || r.edx != 0x76482074
    // "t Hv"
    {
        return None;
    }

    let max_leaf = r.eax;

    // CPUID leaf 0x40000003: partition privilege flags
    if max_leaf < 0x40000003 {
        return Some(HvFeatures {
            max_leaf,
            has_synic: false,
            has_hypercall: false,
        });
    }

    let features = core::arch::x86_64::__cpuid(0x40000003);

    Some(HvFeatures {
        max_leaf,
        has_synic: features.eax & (1 << 2) != 0,
        has_hypercall: features.eax & (1 << 5) != 0,
    })
}
