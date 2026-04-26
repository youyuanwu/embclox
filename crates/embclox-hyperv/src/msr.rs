//! Hyper-V MSR constants and raw access wrappers.

// Guest OS identification
pub const GUEST_OS_ID: u32 = 0x40000000;
// Hypercall page enable
pub const HYPERCALL: u32 = 0x40000001;

// SynIC registers
pub const SCONTROL: u32 = 0x40000080;
pub const SIEFP: u32 = 0x40000082;
pub const SIMP: u32 = 0x40000083;
pub const EOM: u32 = 0x40000084;
// SINT0 base — SINT[n] = SINT0 + n
pub const SINT0: u32 = 0x40000090;

/// VMBus uses SINT2.
pub const VMBUS_SINT: u32 = 2;
/// IDT vector for VMBus synthetic interrupts.
pub const VMBUS_VECTOR: u8 = 34;

#[allow(dead_code)]
/// Read a Model-Specific Register.
#[inline]
pub unsafe fn rdmsr(reg: u32) -> u64 {
    let low: u32;
    let high: u32;
    core::arch::asm!(
        "rdmsr",
        in("ecx") reg,
        out("eax") low,
        out("edx") high,
        options(nomem, nostack, preserves_flags),
    );
    ((high as u64) << 32) | (low as u64)
}

/// Write a Model-Specific Register.
#[inline]
pub unsafe fn wrmsr(reg: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    core::arch::asm!(
        "wrmsr",
        in("ecx") reg,
        in("eax") low,
        in("edx") high,
        options(nomem, nostack, preserves_flags),
    );
}

/// Set the Guest OS ID MSR to identify as an open-source OS.
///
/// Must be called before enabling the hypercall page. A non-zero
/// Guest OS ID is required by the hypervisor.
pub unsafe fn set_guest_os_id() {
    // Bit 63: open source OS
    // Bits 55:48: vendor = 0x01
    // Bits 15:0: build = 1
    let guest_id: u64 = 0x8100_0000_0001_0001;
    wrmsr(GUEST_OS_ID, guest_id);
    log::info!("Guest OS ID set to {:#x}", guest_id);
}
