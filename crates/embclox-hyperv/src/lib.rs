//! Hyper-V VMBus driver for bare-metal x86_64 kernels.
//!
//! Provides CPUID detection, hypercall page setup, SynIC initialization,
//! and VMBus channel management. Only active when running on Hyper-V —
//! `detect()` returns `None` on QEMU or bare metal.

#![no_std]
#![feature(abi_x86_interrupt)]

extern crate alloc;

pub mod ffi;
pub mod nvsp_msg;
pub mod nvsp_types;

pub mod channel;
pub mod detect;
pub mod guid;
pub mod hypercall;
pub mod isr;
pub mod msr;
pub mod netvsc;
pub mod netvsc_embassy;
pub mod ring;
pub mod synic;
pub mod synthvid;
pub mod vmbus;

use core::sync::atomic::{AtomicU32, Ordering};

/// Debug crash stage. Set to a non-zero value to crash AFTER reaching
/// that stage during init. Useful for binary-search debugging on Hyper-V
/// where serial output is unavailable.
///
/// 0 = don't crash, 1 = after CPUID, 2 = after Guest OS ID,
/// 3 = after hypercall page, 4 = after SynIC, 5 = after version negotiate,
/// 6 = after offers
pub static CRASH_AFTER_STAGE: AtomicU32 = AtomicU32::new(0);

fn checkpoint(stage: u32) {
    if CRASH_AFTER_STAGE.load(Ordering::Relaxed) == stage {
        // Triple-fault: execute undefined instruction → VM crashes → state = Off
        unsafe { core::arch::asm!("ud2") };
    }
}

use embclox_dma::DmaAllocator;
use embclox_hal_x86::memory::MemoryMapper;

pub use channel::Channel;
pub use guid::Guid;
pub use vmbus::ChannelOffer;

/// Error type for Hyper-V / VMBus operations.
#[derive(Debug)]
pub enum HvError {
    /// Not running on Hyper-V hypervisor.
    NotHyperV,
    /// SynIC feature not available.
    NoSynIC,
    /// Hypercall MSR not available.
    NoHypercall,
    /// Hypercall returned a non-zero status code.
    HypercallFailed(u16),
    /// Hypercall returned InsufficientBuffers too many times.
    HypercallRetryExhausted,
    /// All VMBus protocol versions were rejected by the host.
    VersionRejected,
    /// Timed out waiting for a response from the host.
    Timeout,
}

impl core::fmt::Display for HvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HvError::NotHyperV => write!(f, "not running on Hyper-V"),
            HvError::NoSynIC => write!(f, "SynIC not available"),
            HvError::NoHypercall => write!(f, "hypercall MSR not available"),
            HvError::HypercallFailed(s) => write!(f, "hypercall failed: status {:#x}", s),
            HvError::HypercallRetryExhausted => write!(f, "hypercall retry exhausted"),
            HvError::VersionRejected => write!(f, "VMBus version rejected by host"),
            HvError::Timeout => write!(f, "timeout waiting for host response"),
        }
    }
}

/// VMBus connection handle.
///
/// Created by [`init`]. Holds the hypercall page, SynIC state,
/// negotiated protocol version, and discovered channel offers.
pub struct VmBus {
    pub(crate) hcall: hypercall::HypercallPage,
    pub(crate) synic: synic::SynIC,
    version: u32,
    offers: alloc::vec::Vec<ChannelOffer>,
    /// Child-to-parent monitor page (reserved for future use).
    _monitor_child_to_parent: embclox_dma::DmaRegion,
    _monitor_parent_to_child: embclox_dma::DmaRegion,
}

impl VmBus {
    /// The negotiated VMBus protocol version.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// All discovered channel offers (synthetic devices).
    pub fn offers(&self) -> &[ChannelOffer] {
        &self.offers
    }

    /// Find a channel offer by device type GUID.
    pub fn find_offer(&self, device_type: &Guid) -> Option<&ChannelOffer> {
        self.offers.iter().find(|o| o.device_type == *device_type)
    }

    /// Open a VMBus channel: allocate ring buffer, create GPADL, send OPENCHANNEL.
    ///
    /// `ring_size` is the total ring buffer size in bytes (split equally between
    /// send and receive). Must be at least 8192 and a multiple of 4096.
    /// Typical value: 262144 (256 KB).
    pub fn open_channel(
        &mut self,
        offer: &ChannelOffer,
        ring_size: usize,
        dma: &impl DmaAllocator,
        memory: &MemoryMapper,
    ) -> Result<Channel, HvError> {
        channel::open_channel(offer, ring_size, dma, memory, &self.hcall, &self.synic)
    }

    /// Virtual address of the SIEFP page (SynIC Event Flags).
    ///
    /// Internal accessor for [`try_init`] to wire up the SINT2 ISR.
    /// The ISR itself lives in [`isr`] and reaches the SIEFP through
    /// a published static — callers should not need this directly.
    pub(crate) fn siefp_vaddr(&self) -> usize {
        self.synic.siefp_vaddr()
    }
}

/// Initialize VMBus on Hyper-V.
///
/// Detects Hyper-V via CPUID, sets up the hypercall page and SynIC,
/// then performs VMBus version negotiation (INITIATE_CONTACT).
///
/// Returns `Err(HvError::NotHyperV)` if not running on Hyper-V.
pub fn init(dma: &impl DmaAllocator, memory: &mut MemoryMapper) -> Result<VmBus, HvError> {
    let features = detect::detect().ok_or(HvError::NotHyperV)?;
    checkpoint(1); // Stage 1: CPUID detection passed

    if !features.has_synic {
        return Err(HvError::NoSynIC);
    }
    if !features.has_hypercall {
        return Err(HvError::NoHypercall);
    }

    // Set Guest OS ID (must be done before enabling hypercall page)
    unsafe { msr::set_guest_os_id() };
    checkpoint(2); // Stage 2: Guest OS ID set

    // Set up hypercall page (allocate, map executable, enable via MSR)
    let hcall = hypercall::HypercallPage::new(dma, memory)?;
    checkpoint(3); // Stage 3: Hypercall page enabled

    // Set up SynIC (SIMP, SIEFP, SINT2)
    let synic = synic::SynIC::new(dma);
    checkpoint(4); // Stage 4: SynIC initialized

    // VMBus version negotiation
    let (version, monitor1, monitor2) = vmbus::connect(&hcall, &synic, dma)?;
    checkpoint(5); // Stage 5: VMBus version negotiated

    // Enumerate channel offers (synthetic devices)
    let offers = vmbus::request_offers(&hcall, &synic)?;
    checkpoint(6); // Stage 6: Channel offers received

    // Check for synthvid
    if offers.iter().any(|o| o.device_type == guid::SYNTHVID) {
        log::info!("VMBus: synthvid device found");
    } else {
        log::warn!("VMBus: no synthvid device (headless VM?)");
    }

    Ok(VmBus {
        hcall,
        synic,
        version,
        offers,
        _monitor_child_to_parent: monitor2,
        _monitor_parent_to_child: monitor1,
    })
}

/// Detect Hyper-V, install the SINT2 ISR, and initialise VMBus in one
/// call. Encapsulates the full "running on Hyper-V?" flow so example
/// kernels don't need to know about the SIEFP page, the SINT2 vector,
/// or the ordering constraint between ISR install and [`init`].
///
/// - `Ok(Some(vmbus))` — running on Hyper-V, VMBus is up, ISR is
///   wired, SIEFP address has been published to the ISR.
/// - `Ok(None)` — not running on Hyper-V (CPUID negative, or SynIC /
///   hypercall MSR unavailable). Non-fatal; the caller should
///   continue with PCI-only paths.
/// - `Err(_)` — Hyper-V detected but [`init`] failed. Caller decides
///   whether to abort or fall back.
///
/// The ISR is installed *before* [`init`] so the host's first
/// `INITIATE_CONTACT` response (delivered via SINT2) can wake the
/// synchronous boot loop.
pub fn try_init(
    dma: &impl DmaAllocator,
    memory: &mut MemoryMapper,
) -> Result<Option<VmBus>, HvError> {
    let features = match detect::detect() {
        Some(f) if f.has_synic && f.has_hypercall => f,
        Some(_) => {
            log::info!("Hyper-V detected but missing SynIC/hypercall; skipping VMBus");
            return Ok(None);
        }
        None => {
            log::info!("Hyper-V not detected; skipping VMBus");
            return Ok(None);
        }
    };
    let _ = features;

    log::info!("Hyper-V: SynIC + hypercall present, initialising VMBus");
    // Install SINT2 ISR BEFORE init: the host's first INITIATE_CONTACT
    // response delivers via SINT2.
    //
    // SAFETY: msr::VMBUS_VECTOR (= 34) is reserved by the example's
    // VectorAllocator; installing into the shared IDT is single-CPU
    // and happens once during boot.
    unsafe {
        embclox_hal_x86::idt::set_handler(msr::VMBUS_VECTOR, isr::vmbus_isr);
    }

    let vmbus = init(dma, memory)?;
    isr::publish_siefp(vmbus.siefp_vaddr());
    isr::publish_simp(vmbus.synic.simp_vaddr());
    log::info!(
        "VMBus: version={:#x}, {} offers",
        vmbus.version(),
        vmbus.offers().len()
    );
    Ok(Some(vmbus))
}
