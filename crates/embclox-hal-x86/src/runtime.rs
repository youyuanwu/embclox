//! Shared executor + APIC-timer plumbing for the example kernels.
//!
//! Each example wires a slightly different bootloader, NIC driver, and
//! VMBus/IOAPIC IRQ source, but the *executor side* is identical:
//!
//! - APIC timer ISR that advances [`crate::time`] and EOIs the LAPIC.
//! - Spurious-vector ISR (required by APIC enable).
//! - A canonical executor loop that polls embassy and `hlt`s between
//!   interrupts.
//!
//! This module owns those pieces so the examples don't each re-implement
//! them. Device-specific ISRs stay in the example crate but call
//! [`lapic_eoi`] from here for the End-of-Interrupt write.
//!
//! ## Vectors used
//!
//! | Vector | Purpose |
//! |--------|---------|
//! | 32     | APIC periodic timer (drives `embassy_time` alarms) |
//! | 39     | Spurious interrupt (APIC requirement) |
//!
//! Examples should pick device IRQ vectors elsewhere (e.g., 33 for the
//! NIC, or whatever SynIC SINT vector the example uses).
//!
//! ## Usage
//!
//! ```ignore
//! // After enabling LAPIC and calibrating TSC:
//! embclox_hal_x86::time::set_tsc_per_us(tsc_per_us);
//! embclox_hal_x86::idt::init();
//! embclox_hal_x86::runtime::start_apic_timer(lapic, tsc_per_us, 1_000);
//!
//! // ... spawn embassy tasks ...
//! embclox_hal_x86::runtime::run_executor(executor);  // never returns
//! ```

use crate::apic::LocalApic;
use embassy_executor::raw::Executor;
use x86_64::structures::idt::InterruptStackFrame;

/// IDT vector used for the APIC periodic timer.
pub const APIC_TIMER_VECTOR: u8 = 32;

/// IDT vector used for the spurious interrupt handler. The LAPIC SVR
/// register is programmed with this value during [`crate::apic::LocalApic::enable`].
pub const SPURIOUS_VECTOR: u8 = 39;

/// LAPIC handle stashed at [`start_apic_timer`] so ISRs can issue the
/// End-of-Interrupt write. Single-core only.
static mut LAPIC: Option<LocalApic> = None;

/// EOI helper for use inside any device ISR routed through the LAPIC.
///
/// SynIC SINT vectors configured with auto-EOI (e.g. VMBus on Hyper-V)
/// must NOT call this — they ack themselves.
///
/// # Safety
/// Caller is asserting we're in a single-core context where the LAPIC
/// stashed by [`start_apic_timer`] is the right one for this CPU.
pub fn lapic_eoi() {
    unsafe {
        if let Some(lapic) = (*core::ptr::addr_of!(LAPIC)).as_ref() {
            lapic.end_of_interrupt();
        }
    }
}

/// APIC periodic-timer ISR. Advances embassy alarms then EOIs.
extern "x86-interrupt" fn apic_timer_isr(_frame: InterruptStackFrame) {
    crate::time::on_timer_tick();
    lapic_eoi();
}

/// LAPIC spurious-vector ISR. Per Intel SDM the spurious vector must be
/// installed but does not need to EOI.
extern "x86-interrupt" fn spurious_isr(_frame: InterruptStackFrame) {}

/// Install the APIC timer + spurious ISRs and start the periodic timer.
///
/// Caller must have already:
/// - enabled the LAPIC (`lapic.enable()`)
/// - called [`crate::time::set_tsc_per_us`] with the calibrated frequency
/// - called [`crate::idt::init`] so the IDT exists
///
/// `tsc_per_us` must match the value passed to `set_tsc_per_us`; we use
/// it to derive the LAPIC count from `period_us`.
///
/// `period_us` selects the timer period (1000 µs = 1 ms is a reasonable
/// default for embassy alarm granularity). The LAPIC divider is fixed
/// at 16, so the maximum representable period is bounded by
/// `u32::MAX * 16 / tsc_per_us` µs.
pub fn start_apic_timer(mut lapic: LocalApic, tsc_per_us: u64, period_us: u32) {
    unsafe {
        crate::idt::set_handler(APIC_TIMER_VECTOR, apic_timer_isr);
        crate::idt::set_handler(SPURIOUS_VECTOR, spurious_isr);
    }

    // LAPIC count = (TSC ticks for one period) / divider. With divider=16
    // and tsc_per_us ~2000 (2 GHz), 1 ms ≈ 125_000 — well within u32.
    let count = ((tsc_per_us * period_us as u64) / 16) as u32;
    lapic.set_timer_periodic(APIC_TIMER_VECTOR, 16, count);

    unsafe {
        *core::ptr::addr_of_mut!(LAPIC) = Some(lapic);
    }

    log::info!(
        "runtime: APIC timer started (vector={}, period={}us, count={})",
        APIC_TIMER_VECTOR,
        period_us,
        count
    );
}

/// Canonical executor loop: enable interrupts, poll embassy, `hlt`
/// until the next interrupt. Never returns.
///
/// The `disable` + `enable_and_hlt` pair around the halt is required to
/// avoid the classic race where an interrupt fires between `poll()` and
/// `hlt`, leaving the CPU asleep with a pending wake. `enable_and_hlt`
/// is the atomic `sti; hlt` instruction sequence.
pub fn run_executor(executor: &'static Executor) -> ! {
    x86_64::instructions::interrupts::enable();
    loop {
        unsafe { executor.poll() };
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}
