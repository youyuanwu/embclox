//! SynIC (Synthetic Interrupt Controller) initialization and message polling.
//!
//! The SynIC provides the message delivery mechanism for VMBus. The hypervisor
//! writes VMBus responses to the SIMP (SynIC Message Page) at SINT slot 2.

use crate::msr;
use embclox_dma::{DmaAllocator, DmaRegion};
use log::*;

/// SynIC message header (16 bytes) as defined by the Hyper-V TLFS.
#[repr(C)]
struct HvMessage {
    /// Message type (0 = no message, 1 = channel message, etc.)
    message_type: u32,
    /// Size of the payload in bytes (max 240).
    payload_size: u8,
    /// Bit 0: MessagePending — more messages queued for this SINT.
    message_flags: u8,
    _reserved: u16,
    /// Origination ID (sender identification).
    _origination_id: u64,
    /// Payload data (up to 240 bytes).
    payload: [u8; 240],
}

const _: () = assert!(core::mem::size_of::<HvMessage>() == 256);

/// SynIC state: owns the SIMP and SIEFP pages.
pub struct SynIC {
    simp: DmaRegion,
    siefp: DmaRegion,
}

impl SynIC {
    /// Initialize SynIC: allocate pages, configure MSRs, enable SINT2.
    pub fn new(dma: &impl DmaAllocator) -> Self {
        let simp = dma.alloc_coherent(4096, 4096);
        let siefp = dma.alloc_coherent(4096, 4096);

        unsafe {
            // Enable SynIC
            msr::wrmsr(msr::SCONTROL, 1);

            // Set SIMP (SynIC Message Page): GPA | enable
            msr::wrmsr(msr::SIMP, (simp.paddr as u64) | 1);

            // Set SIEFP (SynIC Event Flags Page): GPA | enable
            msr::wrmsr(msr::SIEFP, (siefp.paddr as u64) | 1);

            // Configure SINT2 for VMBus: IDT vector | auto-EOI (bit 17)
            let sint_msr = msr::SINT0 + msr::VMBUS_SINT;
            let sint_value = (msr::VMBUS_VECTOR as u64) | (1 << 17);
            msr::wrmsr(sint_msr, sint_value);
        }

        info!(
            "SynIC: SIMP={:#x}, SIEFP={:#x}, SINT{}=vector {}",
            simp.paddr,
            siefp.paddr,
            msr::VMBUS_SINT,
            msr::VMBUS_VECTOR
        );

        Self { simp, siefp }
    }

    /// Virtual address of the SIEFP (SynIC Event Flags Page).
    ///
    /// Layout: 16 contiguous 256-byte event-flag slots, indexed by SINT.
    /// VMBus uses SINT2 — the slot at byte offset `2 * 256 = 512`. Each
    /// slot is 2048 bits; bit `child_relid` is set by the host on
    /// HvSignalEvent for that channel. The guest must `sync_clear_bit`
    /// each set bit after handling, otherwise the host stops raising
    /// SINT2 for that channel.
    pub fn siefp_vaddr(&self) -> usize {
        self.siefp.vaddr
    }

    /// Virtual address of the SIMP (SynIC Message Page).
    ///
    /// Published by [`crate::try_init`] to [`crate::isr::publish_simp`]
    /// so the SINT2 ISR can peek the VMBus SINT slot's `message_type`
    /// and wake [`crate::isr::SIMP_WAKER`] on host-queued messages.
    pub fn simp_vaddr(&self) -> usize {
        self.simp.vaddr
    }

    /// Poll the SIMP for a message on the VMBus SINT slot.
    ///
    /// Returns the raw payload bytes if a message is present, or `None`.
    /// Call [`ack_message`] after processing the payload.
    pub fn poll_message(&self) -> Option<&[u8]> {
        let slot = self.message_slot();
        let msg_type = unsafe { core::ptr::read_volatile(&(*slot).message_type) };
        if msg_type == 0 {
            return None;
        }

        let size = unsafe { core::ptr::read_volatile(&(*slot).payload_size) } as usize;
        let size = size.min(240);

        let payload = unsafe { core::slice::from_raw_parts((*slot).payload.as_ptr(), size) };

        Some(payload)
    }

    /// Acknowledge the current message and drain any pending messages.
    ///
    /// Clears the message type field and writes MSR_EOM if MessagePending
    /// is set, signaling the hypervisor to deliver the next queued message.
    pub fn ack_message(&self) {
        let slot = self.message_slot();

        let flags = unsafe { core::ptr::read_volatile(&(*slot).message_flags) };

        // Clear message type to free the slot
        unsafe {
            core::ptr::write_volatile(&mut (*slot).message_type, 0);
        }

        // If MessagePending (bit 0), write EOM to drain the queue
        if flags & 1 != 0 {
            unsafe {
                msr::wrmsr(msr::EOM, 0);
            }
        }
    }

    /// Pointer to the VMBus SINT message slot in the SIMP page.
    fn message_slot(&self) -> *mut HvMessage {
        let offset = (msr::VMBUS_SINT as usize) * 256;
        (self.simp.vaddr + offset) as *mut HvMessage
    }
}

// ── Async helpers for boot-time SIMP polling under block_on_hlt ──

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Future that polls the SIMP slot for a VMBus channel message and
/// invokes `matcher` on each. `matcher` returns `Some(R)` to complete
/// the future with `Ok(R)`, or `None` to ack and keep waiting for the
/// next message. Returns `Err(HvError::Timeout)` if `deadline` (from
/// `embassy_time::Instant`) is reached.
///
/// Designed to be driven by `embclox_hal_x86::runtime::block_on_hlt`
/// during synchronous boot init, where the caller has already wired
/// the SINT2 ISR so the host's message-arrival IRQ wakes the CPU
/// from `hlt`.
///
/// # Important: matcher contract
///
/// Every message visible in the SIMP slot is passed to `matcher`
/// **exactly once** and then **acked unconditionally**. Acking
/// clears the SIMP slot and (if `MessagePending` is set) signals
/// `EOM` so the host delivers the next queued message — the
/// in-slot bytes are gone after the matcher returns.
///
/// The matcher controls the meaning of returning `None`:
///
/// - **Collect-and-continue pattern**: matcher consumes the payload
///   via a side effect (typically copying into a captured `Vec`)
///   and returns `None` to keep waiting for the next message. The
///   collected data outlives the ack. `request_offers_async` uses
///   this to gather N `OFFERCHANNEL` payloads before `Some(())` on
///   `ALLOFFERS_DELIVERED`.
/// - **Discard-and-continue pattern**: matcher genuinely doesn't
///   recognise the message type; returns `None` and the bytes are
///   lost. Today only used for unexpected message types on init
///   paths where loss is benign.
///
/// A matcher that needs to keep payload bytes **must** copy them
/// before returning `None`. The matcher is the only authority on
/// what "matched" means — returning `None` for a message you
/// actually wanted to keep is a silent data-loss bug.
///
/// Discarded messages are logged at `trace!` level so unexpected
/// host traffic can be diagnosed without flooding logs.
pub fn wait_for_match<'a, F, R>(
    synic: &'a SynIC,
    deadline: embassy_time::Instant,
    matcher: F,
) -> WaitForMatch<'a, F>
where
    F: FnMut(&[u8]) -> Option<R>,
{
    WaitForMatch {
        synic,
        deadline,
        matcher,
    }
}

pub struct WaitForMatch<'a, F> {
    synic: &'a SynIC,
    deadline: embassy_time::Instant,
    matcher: F,
}

impl<'a, F, R> Future for WaitForMatch<'a, F>
where
    F: FnMut(&[u8]) -> Option<R> + Unpin,
{
    type Output = Result<R, crate::HvError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Drain whatever messages are visible right now. Each iteration
        // either matches (return Ready), or doesn't match (ack + look
        // for the next one). When the SIMP slot is empty, check the
        // deadline and otherwise return Pending so block_on_hlt can
        // park the CPU until the next IRQ.
        loop {
            let payload_opt = self.synic.poll_message();
            match payload_opt {
                Some(payload) => {
                    let result = (self.matcher)(payload);
                    if result.is_none() {
                        // Discarded — log enough to diagnose unexpected
                        // host traffic without spamming success paths.
                        let msgtype = if payload.len() >= 4 {
                            u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]])
                        } else {
                            0
                        };
                        log::trace!(
                            "wait_for_match: discarding SIMP message type={} len={}",
                            msgtype,
                            payload.len()
                        );
                    }
                    self.synic.ack_message();
                    if let Some(r) = result {
                        return Poll::Ready(Ok(r));
                    }
                    // No match — loop and look for the next message.
                }
                None => {
                    // Register on SIMP_WAKER BEFORE re-checking the
                    // slot to close the wake/check race: if the host
                    // queues a message between the poll_message() above
                    // and the register() here, the ISR wake would be
                    // lost (no one is registered yet) and we'd sleep
                    // until the deadline. After registering, re-poll
                    // once — if the host raced us we observe the
                    // message now; otherwise the registration ensures
                    // any future wake reaches us.
                    crate::isr::SIMP_WAKER.register(cx.waker());
                    if self.synic.poll_message().is_some() {
                        // Loop back to the matched arm; do not
                        // duplicate the matcher logic here.
                        continue;
                    }
                    if embassy_time::Instant::now() >= self.deadline {
                        return Poll::Ready(Err(crate::HvError::Timeout));
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}
