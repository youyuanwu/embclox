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
//! // After init+enable LAPIC and calibrating TSC:
//! embclox_hal_x86::apic::init(lapic_vaddr);
//! embclox_hal_x86::apic::enable();
//! embclox_hal_x86::time::set_tsc_per_us(tsc_per_us);
//! embclox_hal_x86::idt::init();
//! embclox_hal_x86::runtime::start_apic_timer(tsc_per_us, 1_000);
//!
//! // ... spawn embassy tasks ...
//! embclox_hal_x86::runtime::run_executor(executor);  // never returns
//! ```

use embassy_executor::raw::Executor;
use x86_64::structures::idt::InterruptStackFrame;

/// IDT vector used for the APIC periodic timer.
pub const APIC_TIMER_VECTOR: u8 = 32;

/// IDT vector used for the spurious interrupt handler. The LAPIC SVR
/// register is programmed with this value during [`crate::apic::enable`].
pub const SPURIOUS_VECTOR: u8 = 39;

/// APIC periodic-timer period. 1 ms is the embassy alarm granularity
/// we target; the BSP and every AP program their LAPIC timer with this
/// same period via [`start_apic_timer`] / [`ap_start_apic_timer`].
pub const APIC_TIMER_PERIOD_US: u32 = 1_000;

/// LAPIC divider used by the periodic timer. Fixed at 16; the
/// [`start_apic_timer`] / [`ap_start_apic_timer`] callers compute
/// `count = (tsc_per_us * APIC_TIMER_PERIOD_US) / APIC_TIMER_DIVIDER`.
const APIC_TIMER_DIVIDER: u8 = 16;

/// EOI helper for use inside any device ISR routed through the LAPIC.
///
/// SynIC SINT vectors configured with auto-EOI (e.g. VMBus on Hyper-V)
/// must NOT call this — they ack themselves.
///
/// Forwards to [`crate::apic::eoi`], which acts on **the executing
/// CPU's** LAPIC (LAPIC MMIO is per-CPU-physical, so the underlying
/// MMIO store always hits the right LAPIC regardless of which CPU
/// originally set up the mapping).
pub fn lapic_eoi() {
    crate::apic::eoi();
}

/// APIC periodic-timer ISR. Advances embassy alarms then EOIs.
extern "x86-interrupt" fn apic_timer_isr(_frame: InterruptStackFrame) {
    crate::time::on_timer_tick();
    lapic_eoi();
}

/// LAPIC spurious-vector ISR. Per Intel SDM the spurious vector must be
/// installed but does not need to EOI.
extern "x86-interrupt" fn spurious_isr(_frame: InterruptStackFrame) {}

/// Compute the LAPIC initial-count for the periodic timer at the
/// shared [`APIC_TIMER_PERIOD_US`] / [`APIC_TIMER_DIVIDER`].
fn timer_count(tsc_per_us: u64) -> u32 {
    // With divider=16 and tsc_per_us ~2000 (2 GHz), 1 ms approx 125_000 -
    // well within u32.
    ((tsc_per_us * APIC_TIMER_PERIOD_US as u64) / APIC_TIMER_DIVIDER as u64) as u32
}

/// Install the APIC timer + spurious ISRs (shared IDT) and start
/// **this CPU's** periodic timer at [`APIC_TIMER_PERIOD_US`].
///
/// Caller must have already:
/// - called [`crate::apic::init`] and [`crate::apic::enable`] (BSP).
/// - called [`crate::time::set_tsc_per_us`] with the calibrated frequency.
/// - called [`crate::idt::init`] so the IDT exists.
///
/// `tsc_per_us` must match the value passed to `set_tsc_per_us`; we
/// use it to derive the LAPIC count.
pub fn start_apic_timer(tsc_per_us: u64) {
    unsafe {
        crate::idt::set_handler(APIC_TIMER_VECTOR, apic_timer_isr);
        crate::idt::set_handler(SPURIOUS_VECTOR, spurious_isr);
    }

    let count = timer_count(tsc_per_us);
    crate::apic::set_timer_periodic(APIC_TIMER_VECTOR, APIC_TIMER_DIVIDER, count);

    log::info!(
        "runtime: APIC timer started (vector={}, period={}us, count={})",
        APIC_TIMER_VECTOR,
        APIC_TIMER_PERIOD_US,
        count
    );
}

/// Program **this CPU's** APIC timer at the shared [`APIC_TIMER_PERIOD_US`].
///
/// Called from each AP's `smp::ap_setup` after the AP has enabled its
/// LAPIC. The IDT handlers were already installed once by
/// [`start_apic_timer`] on the BSP and are shared across CPUs, so we
/// only need to program this CPU's LAPIC timer here.
///
/// `tsc_per_us` is the same value the BSP calibrated; the AP receives
/// it via `ApInit`.
pub fn ap_start_apic_timer(tsc_per_us: u64) {
    let count = timer_count(tsc_per_us);
    crate::apic::set_timer_periodic(APIC_TIMER_VECTOR, APIC_TIMER_DIVIDER, count);
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

// ── block_on_hlt: one-future runner with idle sleep ────────────

pub use embclox_async::block_on_with;

/// Run a single future to completion, halting the CPU between polls.
///
/// Thin wrapper over [`embclox_async::block_on_with`] that supplies an
/// x86 `sti; hlt` park function. Suitable for synchronous boot phases
/// where you want a real `Future` API but cannot run the full embassy
/// executor yet.
///
/// # Caller contract
///
/// - Some interrupt source must be able to fire to break the CPU out
///   of `hlt` — typically the APIC periodic timer started by
///   [`start_apic_timer`] plus the device-specific IRQs the future
///   polls for (e.g. SynIC SINT2 for VMBus).
/// - The future itself must not call `hlt` or any other blocking
///   primitive — it must return [`core::task::Poll::Pending`] when not
///   ready and let `block_on_hlt` perform the halt.
///
/// # Example
///
/// ```ignore
/// // After idt::init + lapic.enable + start_apic_timer:
/// let result: Result<NetvscDevice, HvError> =
///     embclox_hal_x86::runtime::block_on_hlt(async {
///         init_vmbus_async(&dma, &mut memory).await
///     });
/// ```
pub fn block_on_hlt<F: core::future::Future>(fut: F) -> F::Output {
    block_on_with(fut, || {
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::interrupts::enable_and_hlt();
    })
}
