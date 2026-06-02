//! SINT2 ISR + per-channel waker table for Hyper-V VMBus.
//!
//! The SINT2 ISR has signature `extern "x86-interrupt" fn(InterruptStackFrame)`
//! — no context — so the SIMP / SIEFP page addresses are published into
//! statics by [`crate::try_init`] for the ISR to read. See
//! [`docs/design/hyperv-netvsc.md`](../../../docs/design/hyperv-netvsc.md)
//! for the rationale (post-init data path requires per-channel SIEFP
//! bit clearing or the host stops re-raising SINT2).
//!
//! ## Wakers
//!
//! Two surfaces are exposed to async code:
//!
//! - [`SIMP_WAKER`] is woken whenever a SIMP message is queued for the
//!   VMBus SINT slot. Used by [`crate::synic::wait_for_match`] so
//!   boot-time control-plane futures (version negotiation, GPADL
//!   creation, channel open) sleep on `hlt` until the host responds,
//!   instead of polling on the 1 ms APIC tick fallback.
//!
//! - [`channel_waker`] returns a `&'static AtomicWaker` indexed by VMBus
//!   `child_relid`. The ISR walks the SIEFP SINT2 slot, clears each set
//!   bit, and wakes only the corresponding channel waker. Lets multi-
//!   device guests dispatch RX edges precisely (each synthetic device
//!   sleeps on its own waker) instead of broadcasting one global wake.

use core::sync::atomic::{AtomicUsize, Ordering};
use embassy_sync::waitqueue::AtomicWaker;
use x86_64::structures::idt::InterruptStackFrame;

/// SIEFP page virtual address, populated by [`crate::try_init`].
static SIEFP_VADDR: AtomicUsize = AtomicUsize::new(0);

/// SIMP page virtual address, populated by [`crate::try_init`].
static SIMP_VADDR: AtomicUsize = AtomicUsize::new(0);

/// Waker for SIMP message arrivals on the VMBus SINT slot.
///
/// Register with `SIMP_WAKER.register(cx.waker())` before returning
/// `Poll::Pending` from a future that's waiting on
/// [`crate::synic::SynIC::poll_message`]. The SINT2 ISR wakes this
/// whenever it observes a non-zero `message_type` in the SIMP slot.
pub static SIMP_WAKER: AtomicWaker = AtomicWaker::new();

/// Number of per-channel waker slots. Linux observes VMBus `child_relid`
/// values in the low single digits to low tens; 64 is comfortably above
/// the actual maximum on a Gen1/Gen2 guest and keeps the table to
/// 64 * sizeof(AtomicWaker) ≈ 1 KiB.
///
/// `relid` is reduced modulo this size when indexing, so collisions
/// produce spurious wakeups rather than missed ones. Spurious wakeups
/// are harmless — embassy will re-poll, observe no work, and re-park.
pub const CHANNEL_WAKER_SLOTS: usize = 64;

static CHANNEL_WAKERS: [AtomicWaker; CHANNEL_WAKER_SLOTS] =
    [const { AtomicWaker::new() }; CHANNEL_WAKER_SLOTS];

/// Per-channel waker keyed by VMBus `child_relid`.
///
/// Drivers register their task here when they need to be woken on the
/// next host signal for their channel:
///
/// ```ignore
/// embclox_hyperv::isr::channel_waker(self.channel.child_relid)
///     .register(cx.waker());
/// ```
///
/// The ISR resolves SIEFP bits → `relid` → this table and wakes only
/// the matching slot. Multiple channels mapping to the same slot
/// (because of `% CHANNEL_WAKER_SLOTS`) is safe but wakes them all.
pub fn channel_waker(relid: u32) -> &'static AtomicWaker {
    &CHANNEL_WAKERS[(relid as usize) % CHANNEL_WAKER_SLOTS]
}

/// SynIC SINT2 → VMBus handler.
///
/// 1. If a SIMP message is queued on the VMBus SINT slot, wake
///    [`SIMP_WAKER`].
/// 2. Walk the 32-word SIEFP SINT2 slot. For every set bit, clear
///    it and wake [`channel_waker`] for the corresponding `relid`.
///
/// SINT MSR is configured auto-EOI in [`crate::synic::SynIC::new`], so
/// no LAPIC EOI is needed.
///
/// Single-CPU, interrupt-context: the read-modify-write of each SIEFP
/// word is race-free because the host only ever sets bits we haven't
/// cleared.
pub extern "x86-interrupt" fn vmbus_isr(_frame: InterruptStackFrame) {
    // (1) SIMP edge: peek the message_type u32 at the start of the
    // SINT2 slot. Wake without clearing — synic::ack_message handles
    // the actual drain after the waker-woken future reads the slot.
    let simp = SIMP_VADDR.load(Ordering::Relaxed);
    if simp != 0 {
        let msg_type_ptr = (simp + (crate::msr::VMBUS_SINT as usize) * 256) as *const u32;
        // SAFETY: SIMP is a 4 KiB DMA page we own; SINT2 slot sits at
        // byte offset 512 and is 256 bytes wide. `message_type` is the
        // first u32 of `HvMessage`.
        let msg_type = unsafe { core::ptr::read_volatile(msg_type_ptr) };
        if msg_type != 0 {
            SIMP_WAKER.wake();
        }
    }

    // (2) SIEFP scan: clear set bits and wake per-channel wakers.
    let siefp = SIEFP_VADDR.load(Ordering::Relaxed);
    if siefp != 0 {
        let slot = (siefp + (crate::msr::VMBUS_SINT as usize) * 256) as *mut u64;
        for word_idx in 0..32usize {
            // SAFETY: SIEFP slot bounds checked above.
            unsafe {
                let p = slot.add(word_idx);
                let mut w = core::ptr::read_volatile(p);
                if w == 0 {
                    continue;
                }
                // Clear the word in one write before walking bits.
                // The host won't re-set our just-cleared bits during
                // the bit-walk because Hyper-V uses edge-triggered
                // signalling — see /memories/repo/hyperv-vmbus.md.
                core::ptr::write_volatile(p, 0);

                while w != 0 {
                    let bit = w.trailing_zeros() as usize;
                    let relid = (word_idx * 64 + bit) as u32;
                    channel_waker(relid).wake();
                    w &= w - 1; // clear lowest set bit
                }
            }
        }
    }
}

/// Publish the SIEFP page address to [`vmbus_isr`]. Called by
/// [`crate::try_init`] on success.
pub(crate) fn publish_siefp(vaddr: usize) {
    SIEFP_VADDR.store(vaddr, Ordering::Release);
}

/// Publish the SIMP page address to [`vmbus_isr`]. Called by
/// [`crate::try_init`] on success.
pub(crate) fn publish_simp(vaddr: usize) {
    SIMP_VADDR.store(vaddr, Ordering::Release);
}
