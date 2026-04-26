//! SynIC (Synthetic Interrupt Controller) initialization and message polling.
//!
//! The SynIC provides the message delivery mechanism for VMBus. The hypervisor
//! writes VMBus responses to the SIMP (SynIC Message Page) at SINT slot 2.

use crate::msr;
use embclox_e1000::dma::{DmaAllocator, DmaRegion};
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
    _siefp: DmaRegion,
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

        Self {
            simp,
            _siefp: siefp,
        }
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
