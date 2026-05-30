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

/// Hyper-V TSC frequency MSR (cycles per second).
pub const TSC_FREQUENCY: u32 = 0x40000022;
/// Hyper-V LAPIC frequency MSR (Hz).
pub const APIC_FREQUENCY: u32 = 0x40000023;

/// VMBus uses SINT2.
pub const VMBUS_SINT: u32 = 2;
/// IDT vector for VMBus synthetic interrupts.
pub const VMBUS_VECTOR: u8 = 34;

#[allow(dead_code)]
/// Read a Model-Specific Register.
///
/// # Safety
/// Caller must ensure `reg` is a valid MSR available on the current CPU.
/// Reading invalid or privileged MSRs from non-ring-0 code triggers a
/// general-protection fault.
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
///
/// # Safety
/// Caller must ensure `reg` is writable on the current CPU and that
/// `value` is meaningful for the target MSR. Writing to MSRs can change
/// privileged CPU state (paging, interrupt delivery, hypervisor
/// interface) — incorrect values may crash the system.
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
///
/// Format (matches Linux `generate_guest_id` in
/// `include/asm-generic/mshyperv.h`):
///   bits 63..48: HV_LINUX_VENDOR_ID = 0x8100
///   bits 47..16: kernel version (mimics LINUX_VERSION_CODE for 6.5.0)
///   bits 15..0:  build number (we use 1)
///
/// # Safety
/// Must run with CPL=0 (kernel mode). The hypervisor uses this value to
/// decide which interface to expose; setting it after other Hyper-V MSRs
/// have been initialised may invalidate them.
pub unsafe fn set_guest_os_id() {
    // 0x8100 = HV_LINUX_VENDOR_ID (Linux's value).
    // 0x00060500 = LINUX_VERSION_CODE for 6.5.0 ((6<<16)|(5<<8)|0).
    // 0x0001 = build number.
    let guest_id: u64 = (0x8100u64 << 48) | (0x0006_0500u64 << 16) | 0x0001;
    wrmsr(GUEST_OS_ID, guest_id);
    log::info!("Guest OS ID set to {:#x} (Linux-style)", guest_id);
}
