//! VMBus channel management: GPADL creation, channel open, send/receive.

use crate::hypercall::HypercallPage;
use crate::ring::{RingHalf, VmPacketDescriptor, VMBUS_DATA_PACKET_FLAG_COMPLETION_REQUESTED};
use crate::synic::SynIC;
use crate::vmbus::ChannelOffer;
use crate::HvError;
use core::sync::atomic::AtomicU32;
use embclox_dma::{DmaAllocator, DmaRegion};
use embclox_hal_x86::memory::MemoryMapper;
use log::*;

// VMBus channel message types
const CHANNELMSG_OPENCHANNEL: u32 = 5;
const CHANNELMSG_OPENCHANNEL_RESULT: u32 = 6;
const CHANNELMSG_GPADL_HEADER: u32 = 8;
const CHANNELMSG_GPADL_BODY: u32 = 9;
const CHANNELMSG_GPADL_CREATED: u32 = 10;

// VMBus message connection ID and type
const VMBUS_MESSAGE_CONNECTION_ID: u32 = 1;
const VMBUS_MESSAGE_TYPE_CHANNEL: u32 = 1;

/// Next GPADL handle (monotonically increasing).
static NEXT_GPADL: AtomicU32 = AtomicU32::new(1);

pub(crate) fn alloc_gpadl_handle() -> u32 {
    NEXT_GPADL.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

/// An open VMBus channel with send/receive ring buffers.
pub struct Channel {
    pub child_relid: u32,
    pub connection_id: u32,
    _ring_buf: DmaRegion,
    gpadl_handle: u32,
    send: RingHalf,
    recv: RingHalf,
    /// Reference to the hypercall page for HvSignalEvent.
    hcall: *const HypercallPage,
}

impl Channel {
    /// Send a VMBus in-band data packet through the channel.
    pub fn send(&self, payload: &[u8], transaction_id: u64) -> Result<(), HvError> {
        self.send
            .send_packet(
                payload,
                transaction_id,
                VMBUS_DATA_PACKET_FLAG_COMPLETION_REQUESTED,
            )
            .map_err(|e| {
                error!("ring send failed: {:?}", e);
                HvError::HypercallFailed(0xFFFF)
            })?;
        self.signal_host();
        Ok(())
    }

    /// Send a VMBus in-band data packet with flags=0 (no completion requested).
    pub fn send_raw(&self, payload: &[u8], transaction_id: u64) -> Result<(), HvError> {
        self.send
            .send_packet(payload, transaction_id, 0)
            .map_err(|e| {
                error!("ring send failed: {:?}", e);
                HvError::HypercallFailed(0xFFFF)
            })?;
        self.signal_host();
        Ok(())
    }

    /// Signal the host that new data is available in the send ring.
    ///
    /// Uses the HvSignalEvent fast hypercall to wake the host's VMBus
    /// worker thread so it reads our ring buffer.
    fn signal_host(&self) {
        unsafe { &*self.hcall }.signal_event(self.connection_id);
    }

    /// Try to receive a packet from the channel.
    ///
    /// Returns `Ok(Some((desc, payload_len)))` if a packet was read into `buf`,
    /// or `Ok(None)` if no packet is available.
    pub fn try_recv(&self, buf: &mut [u8]) -> Result<Option<(VmPacketDescriptor, usize)>, HvError> {
        self.recv.recv_packet(buf).map_err(|e| {
            error!("ring recv failed: {:?}", e);
            HvError::HypercallFailed(0xFFFF)
        })
    }

    /// Wait for a packet on this channel, up to `timeout`.
    ///
    /// Drives [`Self::wait_for_packet_async`] under
    /// `embclox_hal_x86::runtime::block_on_hlt` so the CPU sleeps
    /// between SINT2 IRQ wake-ups instead of spinning. Suitable for
    /// boot-time control-plane RECVs (NVSP/RNDIS init responses).
    ///
    /// Caller must have already wired the SINT2 ISR + APIC timer
    /// (the runtime helpers handle this; see
    /// `examples-kernel/src/main.rs` for the canonical setup order).
    pub fn recv_with_timeout(
        &self,
        buf: &mut [u8],
        timeout: embassy_time::Duration,
    ) -> Result<(VmPacketDescriptor, usize), HvError> {
        embclox_hal_x86::runtime::block_on_hlt(self.wait_for_packet_async(buf, timeout))
    }

    /// Async version of [`Self::recv_with_timeout`]: poll the channel
    /// ring until [`Self::try_recv`] returns `Some`, or `timeout`
    /// elapses.
    ///
    /// Unlike [`crate::synic::wait_for_match`], no payload is
    /// "discarded" on no-match — there's no matcher; if a packet
    /// arrives, it's returned to the caller as-is. The caller is
    /// responsible for any further filtering or re-poll loops at the
    /// protocol level (e.g. NVSP type discrimination).
    pub async fn wait_for_packet_async(
        &self,
        buf: &mut [u8],
        timeout: embassy_time::Duration,
    ) -> Result<(VmPacketDescriptor, usize), HvError> {
        let deadline = embassy_time::Instant::now() + timeout;
        WaitForPacket {
            channel: self,
            buf,
            deadline,
        }
        .await
    }

    /// Send a VMBus packet with a page buffer (GPA direct) for RNDIS control messages.
    pub fn send_with_page_buffer(
        &self,
        user_data: &[u8],
        page_pfn: u64,
        page_offset: u32,
        page_len: u32,
        transaction_id: u64,
    ) -> Result<(), HvError> {
        self.send
            .send_packet_with_page_buffer(
                user_data,
                page_pfn,
                page_offset,
                page_len,
                transaction_id,
                VMBUS_DATA_PACKET_FLAG_COMPLETION_REQUESTED,
            )
            .map_err(|e| {
                error!("ring send page buf failed: {:?}", e);
                HvError::HypercallFailed(0xFFFF)
            })?;
        self.signal_host();
        Ok(())
    }

    /// Try to receive a raw packet (including transfer page headers for type 7).
    /// Returns all bytes after the 16-byte descriptor. Use `desc.offset8` to
    /// find the NVSP payload within the returned data.
    pub fn try_recv_raw(
        &self,
        buf: &mut [u8],
    ) -> Result<Option<(VmPacketDescriptor, usize)>, HvError> {
        self.recv.recv_packet_raw(buf).map_err(|e| {
            error!("ring recv raw failed: {:?}", e);
            HvError::HypercallFailed(0xFFFF)
        })
    }

    /// Send a VMBus completion packet (VM_PKT_COMP, type 11) for a received xfer page packet.
    /// The transaction_id must match the original packet's transaction_id.
    pub fn send_completion(&self, payload: &[u8], transaction_id: u64) -> Result<(), HvError> {
        self.send.send_comp(payload, transaction_id).map_err(|e| {
            error!("ring send comp failed: {:?}", e);
            HvError::HypercallFailed(0xFFFF)
        })?;
        self.signal_host();
        Ok(())
    }

    /// GPADL handle for this channel (for debug/logging).
    pub fn gpadl_handle(&self) -> u32 {
        self.gpadl_handle
    }
}

/// Open a VMBus channel: allocate ring buffer, create GPADL, send OPENCHANNEL.
pub(crate) fn open_channel(
    offer: &ChannelOffer,
    ring_size: usize,
    dma: &impl DmaAllocator,
    memory: &MemoryMapper,
    hcall: &HypercallPage,
    synic: &SynIC,
) -> Result<Channel, HvError> {
    assert!(ring_size >= 8192 && ring_size.is_multiple_of(4096));
    let half_size = ring_size / 2;

    // Allocate ring buffer memory
    let ring_buf = dma.alloc_coherent(ring_size, 4096);
    info!(
        "Channel {}: ring buffer {}KB at vaddr={:#x} paddr={:#x}",
        offer.child_relid,
        ring_size / 1024,
        ring_buf.vaddr,
        ring_buf.paddr
    );

    // Build PFN list by translating each page
    let num_pages = ring_size / 4096;
    let pfns = build_pfn_list(&ring_buf, num_pages, memory);

    // Create GPADL
    let gpadl_handle = alloc_gpadl_handle();
    create_gpadl(
        offer.child_relid,
        gpadl_handle,
        ring_size,
        &pfns,
        hcall,
        synic,
    )?;

    // Send OPENCHANNEL
    let send_pages = half_size / 4096;
    open_channel_msg(
        offer.child_relid,
        gpadl_handle,
        send_pages as u32,
        hcall,
        synic,
    )?;

    // Create ring halves
    let send = RingHalf::new(ring_buf.vaddr, half_size);
    let recv = RingHalf::new(ring_buf.vaddr + half_size, half_size);

    info!(
        "Channel {} open: gpadl={}, send={}KB recv={}KB",
        offer.child_relid,
        gpadl_handle,
        half_size / 1024,
        half_size / 1024
    );

    crate::checkpoint(7); // Stage 7: Channel opened successfully

    Ok(Channel {
        child_relid: offer.child_relid,
        connection_id: offer.connection_id,
        _ring_buf: ring_buf,
        gpadl_handle,
        send,
        recv,
        hcall: hcall as *const HypercallPage,
    })
}

/// Build a PFN list by translating each page's virtual address to physical.
fn build_pfn_list(
    region: &DmaRegion,
    num_pages: usize,
    memory: &MemoryMapper,
) -> alloc::vec::Vec<u64> {
    let mut pfns = alloc::vec::Vec::with_capacity(num_pages);
    for i in 0..num_pages {
        // The DmaRegion vaddr is through phys_offset mapping.
        // We need the actual physical address for each page.
        // Since BootDmaAllocator uses a linear mapping: paddr = base_paddr + offset
        let paddr = region.paddr + i * 4096;
        pfns.push((paddr as u64) >> 12);
    }
    // Verify first page via page table walk if translate_addr is available
    if let Some(translated) = memory.translate_addr(region.vaddr as u64) {
        let expected = region.paddr as u64;
        if translated != expected {
            warn!(
                "PFN verify: translate_addr={:#x} vs paddr={:#x} (using paddr)",
                translated, expected
            );
        }
    }
    pfns
}

/// Create a GPADL by sending GPADL_HEADER (+ GPADL_BODY if needed) and
/// waiting for GPADL_CREATED.
pub(crate) fn create_gpadl(
    child_relid: u32,
    gpadl_handle: u32,
    byte_count: usize,
    pfns: &[u64],
    hcall: &HypercallPage,
    synic: &SynIC,
) -> Result<(), HvError> {
    // GPADL_HEADER layout:
    //   0..8: header (msgtype=8, padding=0)
    //   8..12: child_relid
    //   12..16: gpadl handle
    //   16..18: range_buflen
    //   18..20: rangecount = 1
    //   20..24: byte_count
    //   24..28: byte_offset = 0
    //   28..: PFNs (8 bytes each)
    // Max payload = 240 bytes → max PFNs in header = (240 - 28) / 8 = 26

    let header_overhead = 28usize;
    let max_pfns_in_header = (240 - header_overhead) / 8;
    let pfns_in_header = pfns.len().min(max_pfns_in_header);
    let range_buflen = 8 + pfns.len() * 8; // byte_count(4) + byte_offset(4) + PFNs

    let mut msg = [0u8; 240];
    // msgtype
    msg[0..4].copy_from_slice(&CHANNELMSG_GPADL_HEADER.to_le_bytes());
    // child_relid
    msg[8..12].copy_from_slice(&child_relid.to_le_bytes());
    // gpadl handle
    msg[12..16].copy_from_slice(&gpadl_handle.to_le_bytes());
    // range_buflen
    msg[16..18].copy_from_slice(&(range_buflen as u16).to_le_bytes());
    // rangecount = 1
    msg[18..20].copy_from_slice(&1u16.to_le_bytes());
    // byte_count
    msg[20..24].copy_from_slice(&(byte_count as u32).to_le_bytes());
    // byte_offset = 0 (already zero)
    // PFNs
    for (i, &pfn) in pfns[..pfns_in_header].iter().enumerate() {
        let off = 28 + i * 8;
        msg[off..off + 8].copy_from_slice(&pfn.to_le_bytes());
    }

    let msg_len = header_overhead + pfns_in_header * 8;
    hcall.post_message(
        VMBUS_MESSAGE_CONNECTION_ID,
        VMBUS_MESSAGE_TYPE_CHANNEL,
        &msg[..msg_len],
    )?;

    // Send GPADL_BODY messages for remaining PFNs
    let mut remaining = &pfns[pfns_in_header..];
    let mut msg_number = 0u32;
    let body_overhead = 16usize; // header(8) + msgnumber(4) + gpadl(4)
    let max_pfns_per_body = (240 - body_overhead) / 8;

    while !remaining.is_empty() {
        let count = remaining.len().min(max_pfns_per_body);
        let mut body = [0u8; 240];

        body[0..4].copy_from_slice(&CHANNELMSG_GPADL_BODY.to_le_bytes());
        body[8..12].copy_from_slice(&msg_number.to_le_bytes());
        body[12..16].copy_from_slice(&gpadl_handle.to_le_bytes());

        for (i, &pfn) in remaining[..count].iter().enumerate() {
            let off = body_overhead + i * 8;
            body[off..off + 8].copy_from_slice(&pfn.to_le_bytes());
        }

        let body_len = body_overhead + count * 8;
        hcall.post_message(
            VMBUS_MESSAGE_CONNECTION_ID,
            VMBUS_MESSAGE_TYPE_CHANNEL,
            &body[..body_len],
        )?;

        remaining = &remaining[count..];
        msg_number += 1;
    }

    // Wait for GPADL_CREATED
    let deadline = embassy_time::Instant::now() + embassy_time::Duration::from_secs(5);
    let status = embclox_hal_x86::runtime::block_on_hlt(crate::synic::wait_for_match(
        synic,
        deadline,
        |payload| {
            if payload.len() < 20 {
                return None;
            }
            let msgtype = u32::from_le_bytes(payload[0..4].try_into().unwrap());
            if msgtype != CHANNELMSG_GPADL_CREATED {
                return None;
            }
            Some(u32::from_le_bytes(payload[16..20].try_into().unwrap()))
        },
    ))?;

    if status == 0 {
        info!("GPADL {} created successfully", gpadl_handle);
        Ok(())
    } else {
        error!("GPADL creation failed: status {:#x}", status);
        Err(HvError::HypercallFailed(status as u16))
    }
}

/// Send OPENCHANNEL and wait for OPENCHANNEL_RESULT.
fn open_channel_msg(
    child_relid: u32,
    gpadl_handle: u32,
    downstream_page_offset: u32,
    hcall: &HypercallPage,
    synic: &SynIC,
) -> Result<(), HvError> {
    // OPENCHANNEL layout:
    //   0..4: msgtype = 5
    //   4..8: padding = 0
    //   8..12: child_relid
    //   12..16: openid (= child_relid for simplicity)
    //   16..20: ringbuffer_gpadlhandle
    //   20..24: target_vp = 0
    //   24..28: downstream_ringbuffer_pageoffset
    //   28..148: userdata (120 bytes, zeros)

    let mut msg = [0u8; 148];
    msg[0..4].copy_from_slice(&CHANNELMSG_OPENCHANNEL.to_le_bytes());
    msg[8..12].copy_from_slice(&child_relid.to_le_bytes());
    msg[12..16].copy_from_slice(&child_relid.to_le_bytes()); // openid = relid
    msg[16..20].copy_from_slice(&gpadl_handle.to_le_bytes());
    // target_vp = 0 (already zero)
    msg[24..28].copy_from_slice(&downstream_page_offset.to_le_bytes());
    // userdata[0]: pipe mode = 1 (VMBUS_PIPE_TYPE_MESSAGE)
    // This tells the host to use message-based pipe protocol (required by synthvid).
    msg[28] = 1;

    hcall.post_message(
        VMBUS_MESSAGE_CONNECTION_ID,
        VMBUS_MESSAGE_TYPE_CHANNEL,
        &msg,
    )?;

    // Wait for OPENCHANNEL_RESULT
    let deadline = embassy_time::Instant::now() + embassy_time::Duration::from_secs(5);
    let status = embclox_hal_x86::runtime::block_on_hlt(crate::synic::wait_for_match(
        synic,
        deadline,
        |payload| {
            if payload.len() < 20 {
                return None;
            }
            let msgtype = u32::from_le_bytes(payload[0..4].try_into().unwrap());
            if msgtype != CHANNELMSG_OPENCHANNEL_RESULT {
                return None;
            }
            Some(u32::from_le_bytes(payload[16..20].try_into().unwrap()))
        },
    ))?;

    if status == 0 {
        info!("Channel {} opened successfully", child_relid);
        Ok(())
    } else {
        error!("Channel open failed: status {:#x}", status);
        Err(HvError::HypercallFailed(status as u16))
    }
}

// ── Async helpers for boot-time channel ring polling under block_on_hlt ──

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Future that polls a [`Channel`]'s receive ring buffer until a packet
/// arrives or `deadline` (from `embassy_time::Instant`) is reached.
///
/// Designed for boot-time control-plane RECVs (NVSP/RNDIS handshakes)
/// driven by `embclox_hal_x86::runtime::block_on_hlt`. Caller has
/// already wired the SINT2 ISR; the host raises SINT2 whenever it
/// writes to the channel ring, which wakes `block_on_hlt` from `hlt`,
/// which re-polls this future, which re-checks the ring.
struct WaitForPacket<'a, 'b> {
    channel: &'a Channel,
    buf: &'b mut [u8],
    deadline: embassy_time::Instant,
}

impl<'a, 'b> Future for WaitForPacket<'a, 'b> {
    type Output = Result<(VmPacketDescriptor, usize), HvError>;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        // Safety: WaitForPacket holds two non-overlapping borrows
        // (self.channel: &Channel, self.buf: &mut [u8]); we just need
        // to project them out without moving the pinned future.
        let this = unsafe { self.get_unchecked_mut() };
        match this.channel.try_recv(this.buf) {
            Ok(Some(result)) => Poll::Ready(Ok(result)),
            Ok(None) => {
                if embassy_time::Instant::now() >= this.deadline {
                    Poll::Ready(Err(HvError::Timeout))
                } else {
                    Poll::Pending
                }
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}
