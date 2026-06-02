//! SINT2 ISR + SIEFP publication for Hyper-V VMBus.
//!
//! The SINT2 ISR has signature `extern "x86-interrupt" fn(InterruptStackFrame)`
//! — no context — so the SIEFP page address is published into a static
//! by [`crate::try_init`] for the ISR to read. See
//! [`docs/design/hyperv-netvsc.md`](../../../docs/design/hyperv-netvsc.md)
//! for the rationale (post-init data path requires per-channel SIEFP
//! bit clearing or the host stops re-raising SINT2).

use core::sync::atomic::{AtomicUsize, Ordering};
use x86_64::structures::idt::InterruptStackFrame;

/// SIEFP page virtual address, populated by [`crate::try_init`].
/// Read by [`vmbus_isr`] without locking (single-CPU, plain word load).
static SIEFP_VADDR: AtomicUsize = AtomicUsize::new(0);

/// SynIC SINT2 → VMBus handler.
///
/// Clears every set bit in the SINT2 slot of the SIEFP and wakes
/// [`crate::netvsc::NETVSC_WAKER`]. SINT MSR is configured auto-EOI in
/// [`crate::synic::SynIC::new`], so no LAPIC EOI is needed.
///
/// Single-CPU, interrupt-context: the read-modify-write of each
/// SIEFP word is race-free because the host only ever sets bits we
/// haven't cleared.
pub extern "x86-interrupt" fn vmbus_isr(_frame: InterruptStackFrame) {
    let siefp = SIEFP_VADDR.load(Ordering::Relaxed);
    if siefp != 0 {
        let slot = (siefp + (crate::msr::VMBUS_SINT as usize) * 256) as *mut u64;
        for i in 0..32usize {
            // SAFETY: SIEFP is a 4 KiB DMA page we own; SINT2 slot
            // sits at offset 512 and is 256 bytes (32 × u64) wide.
            unsafe {
                let p = slot.add(i);
                let w = core::ptr::read_volatile(p);
                if w != 0 {
                    core::ptr::write_volatile(p, 0);
                }
            }
        }
    }
    crate::netvsc::NETVSC_WAKER.wake();
}

/// Publish the SIEFP page address to [`vmbus_isr`]. Called by
/// [`crate::try_init`] on success.
pub(crate) fn publish_siefp(vaddr: usize) {
    SIEFP_VADDR.store(vaddr, Ordering::Release);
}
