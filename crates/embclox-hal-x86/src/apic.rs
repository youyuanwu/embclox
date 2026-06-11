//! Local APIC (xAPIC) MMIO driver.
//!
//! The LAPIC is a per-CPU hardware singleton. Every logical CPU has
//! its own LAPIC register file mapped at the same physical address
//! [`LAPIC_PHYS_BASE`] (`0xFEE00000`); the CPU's memory controller
//! routes that paddr to *that core's* registers. As a consequence,
//! the same UC virtual mapping works for every CPU — a load or store
//! from CPU *k* always hits CPU *k*'s LAPIC, never another CPU's.
//!
//! This module reflects that hardware shape: there is no per-CPU
//! `LocalApic` object to instantiate. Every public function operates
//! on **the executing CPU's** LAPIC. There is no way to target a
//! different CPU's LAPIC from this module (cross-CPU IPI would be a
//! future addition that explicitly takes the destination as an arg
//! and still goes through this CPU's ICR register).
//!
//! ## Lifecycle
//!
//! 1. BSP maps `LAPIC_PHYS_BASE` once via
//!    [`crate::memory::MemoryMapper::map_mmio`] and stashes the VA in
//!    this module by calling [`init`].
//! 2. BSP (and later each AP) calls [`enable`] from its own context
//!    to turn on its LAPIC.
//! 3. ISRs call [`eoi`] to acknowledge interrupts.

use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Local APIC register offsets (from base address).
const APIC_ID: usize = 0x020;
const APIC_VERSION: usize = 0x030;
const APIC_TPR: usize = 0x080;
const APIC_EOI: usize = 0x0B0;
const APIC_SVR: usize = 0x0F0;
const APIC_LVT_TIMER: usize = 0x320;
const APIC_TIMER_INIT_CNT: usize = 0x380;
const APIC_TIMER_CURR_CNT: usize = 0x390;
const APIC_TIMER_DIV: usize = 0x3E0;

const APIC_SVR_ENABLE: u32 = 0x100;
const APIC_TIMER_PERIODIC: u32 = 1 << 17;
const APIC_TIMER_MASKED: u32 = 1 << 16;

/// LAPIC physical address (standard for xAPIC).
pub const LAPIC_PHYS_BASE: u64 = 0xFEE0_0000;

/// Shared LAPIC MMIO virtual address. Populated once by [`init`] on
/// the BSP; read by every other function. Same value on every CPU —
/// the per-CPU effect comes from the hardware aliasing of
/// `LAPIC_PHYS_BASE`, not from per-CPU state in this module.
static LAPIC_VADDR: AtomicUsize = AtomicUsize::new(0);

/// Cache the UC-mapped LAPIC MMIO virtual address.
///
/// Call once on the BSP after mapping `LAPIC_PHYS_BASE`. Every
/// subsequent call to [`enable`], [`id`], [`set_timer_periodic`],
/// [`eoi`], etc. — from any CPU — reads this value.
pub fn init(vaddr: usize) {
    LAPIC_VADDR.store(vaddr, Ordering::Release);
}

/// VA cached by [`init`]. Returns 0 if [`init`] has not been called.
pub fn vaddr() -> usize {
    LAPIC_VADDR.load(Ordering::Acquire)
}

#[inline]
fn read(offset: usize) -> u32 {
    unsafe { ptr::read_volatile((vaddr() + offset) as *const u32) }
}

#[inline]
fn write(offset: usize, value: u32) {
    unsafe { ptr::write_volatile((vaddr() + offset) as *mut u32, value) };
}

/// Enable **this CPU's** LAPIC with spurious vector 39.
///
/// Must be called on each CPU that wants to receive interrupts (BSP
/// at boot, each AP from `smp::ap_setup`).
pub fn enable() {
    write(APIC_SVR, APIC_SVR_ENABLE | 39);
    // Set task priority to 0 (accept all interrupts).
    write(APIC_TPR, 0);
    log::info!(
        "LAPIC enabled: ID={:#x}, version={:#x}",
        read(APIC_ID),
        read(APIC_VERSION)
    );
}

/// Configure **this CPU's** APIC timer in periodic mode.
///
/// - `vector`: interrupt vector (e.g., 32).
/// - `divider`: divide configuration (1, 2, 4, 8, 16, 32, 64, 128).
/// - `initial_count`: timer ticks between interrupts.
pub fn set_timer_periodic(vector: u8, divider: u8, initial_count: u32) {
    set_divider(divider);
    // Periodic mode + vector, not masked.
    write(APIC_LVT_TIMER, APIC_TIMER_PERIODIC | vector as u32);
    write(APIC_TIMER_INIT_CNT, initial_count);
    log::info!(
        "APIC timer: vector={}, divider={}, count={}",
        vector,
        divider,
        initial_count
    );
}

/// Mask (disable) **this CPU's** timer interrupt.
pub fn mask_timer() {
    let lvt = read(APIC_LVT_TIMER);
    write(APIC_LVT_TIMER, lvt | APIC_TIMER_MASKED);
}

/// Read **this CPU's** current timer count (for calibration).
pub fn timer_current_count() -> u32 {
    read(APIC_TIMER_CURR_CNT)
}

/// Set **this CPU's** initial timer count (for calibration).
pub fn set_timer_initial_count(count: u32) {
    write(APIC_TIMER_INIT_CNT, count);
}

/// Put **this CPU's** timer in one-shot mode with masked interrupt
/// (for calibration).
pub fn set_timer_oneshot_masked(divider: u8) {
    set_divider(divider);
    write(APIC_LVT_TIMER, APIC_TIMER_MASKED | 32); // masked, vector doesn't matter
}

fn set_divider(divider: u8) {
    // Divider encoding: 0=2, 1=4, 2=8, 3=16, 8=32, 9=64, 10=128, 11=1
    let val = match divider {
        1 => 0b1011,
        2 => 0b0000,
        4 => 0b0001,
        8 => 0b0010,
        16 => 0b0011,
        32 => 0b1000,
        64 => 0b1001,
        128 => 0b1010,
        _ => panic!("invalid APIC timer divider: {}", divider),
    };
    write(APIC_TIMER_DIV, val);
}

/// Send End-of-Interrupt to **this CPU's** LAPIC.
///
/// SynIC SINT vectors configured with auto-EOI (e.g. VMBus on
/// Hyper-V) must NOT call this — they ack themselves.
pub fn eoi() {
    write(APIC_EOI, 0);
}

/// Read **this CPU's** LAPIC ID (xAPIC: bits 31:24 of `APIC_ID`).
///
/// x2APIC would use a 32-bit value here; we don't support that yet so
/// the truncation to `u8` is safe.
pub fn id() -> u8 {
    (read(APIC_ID) >> 24) as u8
}
