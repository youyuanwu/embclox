//! Hyper-V hypercall page setup and HvPostMessage wrapper.

use crate::msr;
use crate::HvError;
use embclox_e1000::dma::{DmaAllocator, DmaRegion};
use embclox_hal_x86::memory::{MemoryMapper, MmioMapping};
use log::*;

/// Hypercall code for HvPostMessage.
const HV_POST_MESSAGE: u64 = 0x005C;

/// Hypercall code for HvSignalEvent.
const HV_SIGNAL_EVENT: u64 = 0x005D;

/// Fast hypercall flag (input in registers, not GPA).
const HV_HYPERCALL_FAST: u64 = 1 << 16;

/// HvPostMessage input structure (256 bytes, 8-byte aligned).
#[repr(C, align(8))]
struct HvPostMessageInput {
    connection_id: u32,
    _reserved: u32,
    message_type: u32,
    payload_size: u32,
    payload: [u8; 240],
}

/// Manages the hypercall page and provides HvPostMessage.
pub struct HypercallPage {
    /// DMA region for the hypercall code page (physical memory).
    _page: DmaRegion,
    /// Executable virtual mapping of the hypercall page.
    _code_mapping: MmioMapping,
    /// Virtual address to call into (executable).
    call_addr: u64,
    /// DMA region for HvPostMessage input buffer.
    msg_buffer: DmaRegion,
}

impl HypercallPage {
    /// Allocate and enable the hypercall page.
    ///
    /// The hypervisor fills the physical page with vmcall/vmmcall+ret code
    /// when we write its GPA to the hypercall MSR.
    pub fn new(dma: &impl DmaAllocator, memory: &mut MemoryMapper) -> Result<Self, HvError> {
        // Allocate a physical page for the hypervisor to fill with code
        let page = dma.alloc_coherent(4096, 4096);

        // Map it as executable (cached, no NO_EXECUTE)
        let code_mapping = memory.map_code(page.paddr as u64, 4096);
        let call_addr = code_mapping.vaddr() as u64;

        // Enable hypercall page: write GPA | enable bit to MSR
        let msr_value = (page.paddr as u64) | 1;
        unsafe { msr::wrmsr(msr::HYPERCALL, msr_value) };
        info!(
            "Hypercall page: paddr={:#x}, vaddr={:#x}",
            page.paddr, call_addr
        );

        // Allocate message buffer for HvPostMessage (needs stable GPA)
        let msg_buffer = dma.alloc_coherent(4096, 4096);

        Ok(Self {
            _page: page,
            _code_mapping: code_mapping,
            call_addr,
            msg_buffer,
        })
    }

    /// Make a raw hypercall into the hypervisor-provided code page.
    ///
    /// # Safety
    /// The hypercall page must have been enabled via MSR. Arguments must
    /// be valid GPAs for the given hypercall code.
    #[inline]
    unsafe fn raw_call(&self, code: u64, input_gpa: u64, output_gpa: u64) -> u64 {
        let page = self.call_addr;
        let result: u64;
        // Hyper-V hypercall calling convention:
        //   RCX = hypercall input value (call code | flags)
        //   RDX = input parameter GPA
        //   R8  = output parameter GPA
        //   RAX = return status
        core::arch::asm!(
            "call {page}",
            page = in(reg) page,
            inlateout("rcx") code => _,
            inlateout("rdx") input_gpa => _,
            inlateout("r8") output_gpa => _,
            lateout("rax") result,
            lateout("r9") _,
            lateout("r10") _,
            lateout("r11") _,
            lateout("rdi") _,
            lateout("rsi") _,
        );
        result
    }

    /// Send a VMBus control message via HvPostMessage hypercall.
    ///
    /// Retries on `HV_STATUS_INSUFFICIENT_BUFFERS` (0x13) up to 10 times
    /// with a spin delay between attempts.
    pub fn post_message(
        &self,
        connection_id: u32,
        msg_type: u32,
        data: &[u8],
    ) -> Result<(), HvError> {
        assert!(
            data.len() <= 240,
            "message payload too large: {}",
            data.len()
        );

        let input = self.msg_buffer.vaddr as *mut HvPostMessageInput;
        unsafe {
            let inp = &mut *input;
            inp.connection_id = connection_id;
            inp._reserved = 0;
            inp.message_type = msg_type;
            inp.payload_size = data.len() as u32;
            // Zero the payload area first, then copy data
            core::ptr::write_bytes(inp.payload.as_mut_ptr(), 0, 240);
            core::ptr::copy_nonoverlapping(data.as_ptr(), inp.payload.as_mut_ptr(), data.len());
        }

        let input_gpa = self.msg_buffer.paddr as u64;

        for retry in 0..10 {
            let status = unsafe { self.raw_call(HV_POST_MESSAGE, input_gpa, 0) };
            match status as u16 {
                0 => return Ok(()),
                0x13 => {
                    // HV_STATUS_INSUFFICIENT_BUFFERS — transient, retry
                    trace!("HvPostMessage: InsufficientBuffers, retry {}", retry);
                    for _ in 0..10_000 {
                        core::hint::spin_loop();
                    }
                }
                _ => {
                    error!("HvPostMessage failed: status {:#x}", status);
                    return Err(HvError::HypercallFailed(status as u16));
                }
            }
        }

        error!("HvPostMessage: retry exhausted after 10 attempts");
        Err(HvError::HypercallRetryExhausted)
    }

    /// Signal an event to the host via HvSignalEvent fast hypercall.
    ///
    /// This wakes the host to process new data in a VMBus channel's ring buffer.
    /// `connection_id` is the channel's connection ID from the offer.
    pub fn signal_event(&self, connection_id: u32) {
        let code = HV_SIGNAL_EVENT | HV_HYPERCALL_FAST;
        let input = connection_id as u64;
        let status = unsafe { self.raw_call(code, input, 0) };
        if status != 0 {
            trace!("HvSignalEvent: status {:#x}", status);
        }
    }
}
