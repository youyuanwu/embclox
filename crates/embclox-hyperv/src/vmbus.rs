//! VMBus connection, version negotiation, and channel offer enumeration.
//!
//! Implements INITIATE_CONTACT / VERSION_RESPONSE handshake with
//! version fallback (WIN10 → WIN8_1), then REQUEST_OFFERS to
//! enumerate synthetic devices. Pre-WIN8_1 protocols are not
//! supported (they require an interrupt page).

use alloc::vec::Vec;

use crate::guid::Guid;
use crate::hypercall::HypercallPage;
use crate::synic::SynIC;
use crate::HvError;
use embclox_dma::{DmaAllocator, DmaRegion};
use log::*;

// VMBus message connection ID and type
const VMBUS_MESSAGE_CONNECTION_ID: u32 = 1;
const VMBUS_MESSAGE_TYPE_CHANNEL: u32 = 1;

// VMBus channel message types
const CHANNELMSG_OFFERCHANNEL: u32 = 1;
const CHANNELMSG_REQUEST_OFFERS: u32 = 3;
const CHANNELMSG_ALLOFFERS_DELIVERED: u32 = 4;
const CHANNELMSG_INITIATE_CONTACT: u32 = 14;
const CHANNELMSG_VERSION_RESPONSE: u32 = 15;

// VMBus protocol versions: (major << 16) | minor
const VERSION_WIN10: u32 = 4 << 16; // 0x0004_0000
const VERSION_WIN8_1: u32 = 3 << 16; // 0x0003_0000

/// A discovered VMBus channel offer (synthetic device).
#[derive(Debug, Clone)]
pub struct ChannelOffer {
    /// Device type GUID (e.g., synthvid, netvsc).
    pub device_type: Guid,
    /// Instance GUID (unique per device instance).
    pub instance_id: Guid,
    /// Channel ID used for OPEN_CHANNEL and ring buffer communication.
    pub child_relid: u32,
    /// Connection ID for sending messages to this channel.
    pub connection_id: u32,
    /// Monitor ID for this channel.
    pub monitor_id: u8,
    /// Whether this channel has a monitor page allocated.
    pub monitor_allocated: bool,
    /// Whether this channel uses a dedicated interrupt.
    pub is_dedicated_interrupt: bool,
}

/// Name a well-known device GUID for logging.
fn device_name(guid: &Guid) -> &'static str {
    if *guid == crate::guid::SYNTHVID {
        "synthvid"
    } else if *guid == crate::guid::NETVSC {
        "netvsc"
    } else if *guid == crate::guid::SYNTH_KEYBOARD {
        "keyboard"
    } else if *guid == crate::guid::HEARTBEAT {
        "heartbeat"
    } else {
        "unknown"
    }
}

/// INITIATE_CONTACT message (40 bytes).
#[repr(C)]
struct VmbusInitiateContact {
    msgtype: u32,
    _padding: u32,
    vmbus_version_requested: u32,
    target_vcpu: u32,
    /// For WIN8_1+: target_vcpu_index(u8) + feature_bits(u8) + reserved(6).
    target_info: u64,
    monitor_page1: u64,
    monitor_page2: u64,
}

/// Build the target_info field for INITIATE_CONTACT.
///
/// For WIN8_1+ protocol, the 8-byte union field is interpreted as:
///   byte 0: target_vcpu_index (0 = boot vCPU)
///   byte 1: feature_bits (0x01 = VMBUS_MESSAGE_FLAG_FEATURE_CAPABILITIES)
///   bytes 2-7: reserved (zero)
fn target_info_for_version(version: u32) -> u64 {
    if version >= VERSION_WIN8_1 {
        // Little-endian: byte 0 = vcpu_index=0, byte 1 = feature_bits=1
        0x0000_0000_0000_0100
    } else {
        // Pre-WIN8_1 requires interrupt page GPA — not supported
        0
    }
}

/// VERSION_RESPONSE message header (first 12 bytes of payload).
#[repr(C)]
struct VmbusVersionResponse {
    msgtype: u32,
    _padding: u32,
    version_supported: u8,
}

/// Perform VMBus version negotiation.
///
/// Tries WIN10, WIN8_1 in order. Returns the accepted version
/// and the monitor page DmaRegions.
/// or `Err(HvError::VersionRejected)`.
pub(crate) fn connect(
    hcall: &HypercallPage,
    synic: &SynIC,
    dma: &impl DmaAllocator,
) -> Result<(u32, DmaRegion, DmaRegion), HvError> {
    // Allocate monitor pages (required for INITIATE_CONTACT)
    let monitor1 = dma.alloc_coherent(4096, 4096);
    let monitor2 = dma.alloc_coherent(4096, 4096);

    let versions = [VERSION_WIN10, VERSION_WIN8_1];

    for &version in &versions {
        info!(
            "VMBus: trying version {}.{} ({:#x})",
            version >> 16,
            version & 0xFFFF,
            version
        );

        match try_version(hcall, synic, version, &monitor1, &monitor2) {
            Ok(()) => {
                info!(
                    "VMBus: version {}.{} accepted",
                    version >> 16,
                    version & 0xFFFF
                );
                return Ok((version, monitor1, monitor2));
            }
            Err(HvError::VersionRejected) => {
                info!(
                    "VMBus: version {}.{} rejected, trying next",
                    version >> 16,
                    version & 0xFFFF
                );
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    Err(HvError::VersionRejected)
}

/// Try a single VMBus version: send INITIATE_CONTACT, poll for VERSION_RESPONSE.
fn try_version(
    hcall: &HypercallPage,
    synic: &SynIC,
    version: u32,
    monitor1: &DmaRegion,
    monitor2: &DmaRegion,
) -> Result<(), HvError> {
    let msg = VmbusInitiateContact {
        msgtype: CHANNELMSG_INITIATE_CONTACT,
        _padding: 0,
        vmbus_version_requested: version,
        target_vcpu: 0,
        target_info: target_info_for_version(version),
        monitor_page1: monitor1.paddr as u64,
        monitor_page2: monitor2.paddr as u64,
    };

    let msg_bytes = unsafe {
        core::slice::from_raw_parts(
            &msg as *const VmbusInitiateContact as *const u8,
            core::mem::size_of::<VmbusInitiateContact>(),
        )
    };

    hcall.post_message(
        VMBUS_MESSAGE_CONNECTION_ID,
        VMBUS_MESSAGE_TYPE_CHANNEL,
        msg_bytes,
    )?;

    // Poll for VERSION_RESPONSE with timeout (~5 seconds at ~1GHz spin)
    for _ in 0..5_000_000u64 {
        if let Some(payload) = synic.poll_message() {
            if payload.len() >= core::mem::size_of::<VmbusVersionResponse>() {
                let resp = unsafe { &*(payload.as_ptr() as *const VmbusVersionResponse) };

                if resp.msgtype == CHANNELMSG_VERSION_RESPONSE {
                    let supported = resp.version_supported != 0;
                    synic.ack_message();

                    if supported {
                        return Ok(());
                    } else {
                        return Err(HvError::VersionRejected);
                    }
                }
            }
            // Not the message we expected — ack and continue polling
            synic.ack_message();
        }

        for _ in 0..1000 {
            core::hint::spin_loop();
        }
    }

    Err(HvError::Timeout)
}

/// Send REQUEST_OFFERS and collect all OFFERCHANNEL responses.
///
/// Returns a list of channel offers. The host sends one OFFERCHANNEL (type 1)
/// per synthetic device, followed by ALLOFFERS_DELIVERED (type 4).
pub(crate) fn request_offers(
    hcall: &HypercallPage,
    synic: &SynIC,
) -> Result<Vec<ChannelOffer>, HvError> {
    // REQUEST_OFFERS is just a header with msgtype=3
    let msg = [CHANNELMSG_REQUEST_OFFERS, 0u32];
    let msg_bytes = unsafe { core::slice::from_raw_parts(msg.as_ptr() as *const u8, 8) };

    hcall.post_message(
        VMBUS_MESSAGE_CONNECTION_ID,
        VMBUS_MESSAGE_TYPE_CHANNEL,
        msg_bytes,
    )?;

    let mut offers = Vec::new();

    // Poll for offers with timeout
    for _ in 0..10_000_000u64 {
        if let Some(payload) = synic.poll_message() {
            if payload.len() < 4 {
                synic.ack_message();
                continue;
            }

            let msgtype = u32::from_le_bytes(payload[0..4].try_into().unwrap());

            match msgtype {
                CHANNELMSG_OFFERCHANNEL => {
                    if let Some(offer) = parse_offer(payload) {
                        info!(
                            "VMBus offer: {} ({}) relid={} conn={}",
                            device_name(&offer.device_type),
                            offer.device_type,
                            offer.child_relid,
                            offer.connection_id
                        );
                        offers.push(offer);
                    } else {
                        warn!(
                            "VMBus: failed to parse OFFERCHANNEL (len={})",
                            payload.len()
                        );
                    }
                    synic.ack_message();
                }
                CHANNELMSG_ALLOFFERS_DELIVERED => {
                    synic.ack_message();
                    info!("VMBus: all offers delivered ({} devices)", offers.len());
                    return Ok(offers);
                }
                _ => {
                    trace!(
                        "VMBus: unexpected message type {} during offer collection",
                        msgtype
                    );
                    synic.ack_message();
                }
            }
        }

        for _ in 0..100 {
            core::hint::spin_loop();
        }
    }

    // If we got some offers but never saw ALLOFFERS_DELIVERED, return what we have
    if !offers.is_empty() {
        warn!(
            "VMBus: timeout waiting for ALLOFFERS_DELIVERED, returning {} offers",
            offers.len()
        );
        return Ok(offers);
    }

    Err(HvError::Timeout)
}

/// Parse an OFFERCHANNEL message payload into a ChannelOffer.
///
/// Layout (packed, 196 bytes):
///   0..8:   header (msgtype + padding)
///   8..24:  if_type GUID (16 bytes)
///   24..40: if_instance GUID (16 bytes)
///   40..48: int_latency (8 bytes)
///   48..52: if_revision (4 bytes)
///   52..56: server_ctx_area_size (4 bytes)
///   56..58: chn_flags (2 bytes)
///   58..60: mmio_megabytes (2 bytes)
///   60..62: sub_channel_index (2 bytes)
///   62..64: mmio_megabytes_optional2 (2 bytes)
///   64..184: user_def (120 bytes)
///   184..188: child_relid (u32)
///   188: monitorid (u8)
///   189: monitor_allocated (u8)
///   190..192: is_dedicated_interrupt (u16)
///   192..196: connection_id (u32)
fn parse_offer(payload: &[u8]) -> Option<ChannelOffer> {
    if payload.len() < 196 {
        return None;
    }

    let mut if_type = [0u8; 16];
    if_type.copy_from_slice(&payload[8..24]);

    let mut if_instance = [0u8; 16];
    if_instance.copy_from_slice(&payload[24..40]);

    let child_relid = u32::from_le_bytes(payload[184..188].try_into().unwrap());
    let monitor_id = payload[188];
    let monitor_allocated = payload[189] != 0;
    let is_dedicated_interrupt = u16::from_le_bytes(payload[190..192].try_into().unwrap());
    let connection_id = u32::from_le_bytes(payload[192..196].try_into().unwrap());

    Some(ChannelOffer {
        device_type: Guid::from_bytes(if_type),
        instance_id: Guid::from_bytes(if_instance),
        child_relid,
        connection_id,
        monitor_id,
        monitor_allocated,
        is_dedicated_interrupt: is_dedicated_interrupt != 0,
    })
}
