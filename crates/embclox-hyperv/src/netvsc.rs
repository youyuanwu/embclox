//! NetVSC (Hyper-V synthetic NIC) driver — NVSP + RNDIS layers.
//!
//! Phase 1: NVSP channel setup (version negotiation, shared buffer GPADL).
//! Phase 2: RNDIS init (version, MAC query, packet filter).
//! Phase 3: Packet send/recv (RNDIS_PACKET_MSG).

use crate::channel::{self, Channel};
use crate::guid;
use crate::HvError;
use crate::VmBus;
use embclox_dma::{DmaAllocator, DmaRegion};
use embclox_hal_x86::memory::MemoryMapper;
use log::*;

// ── NVSP message types ──────────────────────────────────────────────────

const NVSP_MSG_TYPE_INIT: u32 = 1;
const NVSP_MSG_TYPE_INIT_COMPLETE: u32 = 2;
const NVSP_MSG1_TYPE_SEND_NDIS_VER: u32 = 100;
const NVSP_MSG1_TYPE_SEND_RECV_BUF: u32 = 101;
const NVSP_MSG1_TYPE_SEND_SEND_BUF: u32 = 104;
const NVSP_MSG1_TYPE_SEND_RNDIS_PKT: u32 = 107;
const NVSP_MSG1_TYPE_SEND_RNDIS_PKT_COMPLETE: u32 = 108;

// NVSPv2+ message types
const NVSP_MSG2_TYPE_SEND_NDIS_CONFIG: u32 = 196;

// NVSP protocol versions
const NVSP_PROTOCOL_VERSION_5: u32 = 0x30002; // WIN2012R2+
const NVSP_PROTOCOL_VERSION_4: u32 = 0x30001; // WIN2012
const NVSP_PROTOCOL_VERSION_1: u32 = 0x00002; // WIN2008

// NVSP receive buffer ID (arbitrary, must be non-zero)
const NETVSC_RECEIVE_BUFFER_ID: u16 = 0xCAFE;
const NETVSC_SEND_BUFFER_ID: u16 = 0xBEEF;

// Buffer sizes
const NETVSC_RECV_BUF_SIZE: usize = 2 * 1024 * 1024; // 2 MB
const NETVSC_SEND_BUF_SIZE: usize = 1024 * 1024; // 1 MB
const NETVSC_RING_SIZE: usize = 256 * 1024; // 256 KB (128 KB × 2)

// ── RNDIS constants ─────────────────────────────────────────────────────

const RNDIS_MSG_INIT: u32 = 0x0000_0002;
const RNDIS_MSG_INIT_CMPLT: u32 = 0x8000_0002;
const RNDIS_MSG_QUERY: u32 = 0x0000_0004;
const RNDIS_MSG_QUERY_CMPLT: u32 = 0x8000_0004;
const RNDIS_MSG_SET: u32 = 0x0000_0005;
const RNDIS_MSG_SET_CMPLT: u32 = 0x8000_0005;
const RNDIS_MSG_PACKET: u32 = 0x0000_0001;
const RNDIS_MSG_KEEPALIVE: u32 = 0x0000_0008;
const RNDIS_MSG_KEEPALIVE_CMPLT: u32 = 0x8000_0008;

// OIDs
const OID_802_3_PERMANENT_ADDRESS: u32 = 0x0101_0101;
const OID_GEN_MAXIMUM_FRAME_SIZE: u32 = 0x0001_0106;
const OID_GEN_CURRENT_PACKET_FILTER: u32 = 0x0001_010E;

// Packet filter flags
const NDIS_PACKET_TYPE_DIRECTED: u32 = 0x01;
const NDIS_PACKET_TYPE_MULTICAST: u32 = 0x02;
const NDIS_PACKET_TYPE_BROADCAST: u32 = 0x08;

// RNDIS_PACKET_MSG header size (44 bytes)
const RNDIS_PACKET_HDR_SIZE: usize = 44;

// ── NVSP message helpers ────────────────────────────────────────────────

// NvspMessage layout: msg_type(4) + body(variable), no padding.
// The host expects sizeof(nvsp_message) = 64 bytes minimum for all messages.
const NVSP_MESSAGE_SIZE: usize = 64;

/// Build an NVSP_MSG_TYPE_INIT message (padded to 64 bytes).
fn build_nvsp_init(version: u32, buf: &mut [u8]) -> usize {
    assert!(buf.len() >= NVSP_MESSAGE_SIZE);
    buf[..NVSP_MESSAGE_SIZE].fill(0);
    buf[0..4].copy_from_slice(&NVSP_MSG_TYPE_INIT.to_le_bytes());
    buf[4..8].copy_from_slice(&version.to_le_bytes());
    buf[8..12].copy_from_slice(&version.to_le_bytes());
    NVSP_MESSAGE_SIZE
}

/// Build NVSP_MSG1_TYPE_SEND_RECV_BUF message (padded to 64 bytes).
fn build_nvsp_send_recv_buf(gpadl_handle: u32, id: u16, buf: &mut [u8]) -> usize {
    assert!(buf.len() >= NVSP_MESSAGE_SIZE);
    buf[..NVSP_MESSAGE_SIZE].fill(0);
    buf[0..4].copy_from_slice(&NVSP_MSG1_TYPE_SEND_RECV_BUF.to_le_bytes());
    buf[4..8].copy_from_slice(&gpadl_handle.to_le_bytes());
    buf[8..10].copy_from_slice(&id.to_le_bytes());
    NVSP_MESSAGE_SIZE
}

/// Build NVSP_MSG1_TYPE_SEND_SEND_BUF message (padded to 64 bytes).
fn build_nvsp_send_send_buf(gpadl_handle: u32, id: u16, buf: &mut [u8]) -> usize {
    assert!(buf.len() >= NVSP_MESSAGE_SIZE);
    buf[..NVSP_MESSAGE_SIZE].fill(0);
    buf[0..4].copy_from_slice(&NVSP_MSG1_TYPE_SEND_SEND_BUF.to_le_bytes());
    buf[4..8].copy_from_slice(&gpadl_handle.to_le_bytes());
    buf[8..10].copy_from_slice(&id.to_le_bytes());
    NVSP_MESSAGE_SIZE
}

/// Build NVSP_MSG1_TYPE_SEND_RNDIS_PKT message (padded to 64 bytes).
/// channel_type: 1 = control, 0 = data
fn build_nvsp_send_rndis_pkt(
    channel_type: u32,
    send_buf_section_idx: u32,
    send_buf_section_size: u32,
    buf: &mut [u8],
) -> usize {
    assert!(buf.len() >= NVSP_MESSAGE_SIZE);
    buf[..NVSP_MESSAGE_SIZE].fill(0);
    buf[0..4].copy_from_slice(&NVSP_MSG1_TYPE_SEND_RNDIS_PKT.to_le_bytes());
    buf[4..8].copy_from_slice(&channel_type.to_le_bytes());
    buf[8..12].copy_from_slice(&send_buf_section_idx.to_le_bytes());
    buf[12..16].copy_from_slice(&send_buf_section_size.to_le_bytes());
    NVSP_MESSAGE_SIZE
}

/// Parse NVSP message type from received payload.
fn parse_nvsp_type(payload: &[u8]) -> Option<u32> {
    if payload.len() < 4 {
        return None;
    }
    Some(u32::from_le_bytes(payload[0..4].try_into().unwrap()))
}

// ── RNDIS message helpers ───────────────────────────────────────────────

/// Build RNDIS_INITIALIZE_MSG.
fn build_rndis_init(request_id: u32, buf: &mut [u8]) -> usize {
    // hdr: msg_type(4) + msg_len(4) + request_id(4) + major(4) + minor(4) + max_xfer(4)
    let len = 24;
    assert!(buf.len() >= len);
    buf[..len].fill(0);
    buf[0..4].copy_from_slice(&RNDIS_MSG_INIT.to_le_bytes());
    buf[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    buf[8..12].copy_from_slice(&request_id.to_le_bytes());
    buf[12..16].copy_from_slice(&1u32.to_le_bytes()); // major version
    buf[16..20].copy_from_slice(&0u32.to_le_bytes()); // minor version
    buf[20..24].copy_from_slice(&0x4000u32.to_le_bytes()); // max xfer size (16KB)
    len
}

/// Build RNDIS_QUERY_MSG.
fn build_rndis_query(request_id: u32, oid: u32, buf: &mut [u8]) -> usize {
    // hdr: msg_type(4) + msg_len(4) + request_id(4) + oid(4)
    //      + info_buf_len(4) + info_buf_offset(4) + device_vc_handle(4)
    let len = 28;
    assert!(buf.len() >= len);
    buf[..len].fill(0);
    buf[0..4].copy_from_slice(&RNDIS_MSG_QUERY.to_le_bytes());
    buf[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    buf[8..12].copy_from_slice(&request_id.to_le_bytes());
    buf[12..16].copy_from_slice(&oid.to_le_bytes());
    // info_buf_len, info_buf_offset, device_vc_handle = 0
    len
}

/// Build RNDIS_SET_MSG with a u32 value.
fn build_rndis_set_u32(request_id: u32, oid: u32, value: u32, buf: &mut [u8]) -> usize {
    // hdr(8) + request_id(4) + oid(4) + info_buf_len(4) + info_buf_offset(4) + vc_handle(4)
    // + value(4)
    let len = 32;
    assert!(buf.len() >= len);
    buf[..len].fill(0);
    buf[0..4].copy_from_slice(&RNDIS_MSG_SET.to_le_bytes());
    buf[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    buf[8..12].copy_from_slice(&request_id.to_le_bytes());
    buf[12..16].copy_from_slice(&oid.to_le_bytes());
    buf[16..20].copy_from_slice(&4u32.to_le_bytes()); // info_buf_len = 4
    buf[20..24].copy_from_slice(&20u32.to_le_bytes()); // info_buf_offset = 20 (from &request_id)
                                                       // vc_handle = 0
    buf[28..32].copy_from_slice(&value.to_le_bytes());
    len
}

/// Build RNDIS_KEEPALIVE_CMPLT response.
fn build_rndis_keepalive_cmplt(request_id: u32, buf: &mut [u8]) -> usize {
    // msg_type(4) + msg_len(4) + request_id(4) + status(4)
    let len = 16;
    assert!(buf.len() >= len);
    buf[..len].fill(0);
    buf[0..4].copy_from_slice(&RNDIS_MSG_KEEPALIVE_CMPLT.to_le_bytes());
    buf[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    buf[8..12].copy_from_slice(&request_id.to_le_bytes());
    // status = 0 (success)
    len
}

// ── PFN list for GPADL ──────────────────────────────────────────────────

/// Build PFN list from a DmaRegion (contiguous physical pages).
fn pfn_list(region: &DmaRegion) -> alloc::vec::Vec<u64> {
    let num_pages = region.size / 4096;
    let mut pfns = alloc::vec::Vec::with_capacity(num_pages);
    for i in 0..num_pages {
        pfns.push(((region.paddr + i * 4096) as u64) >> 12);
    }
    pfns
}

// ── NetvscDevice ────────────────────────────────────────────────────────

/// NetVSC synthetic NIC device.
pub struct NetvscDevice {
    channel: Channel,
    _recv_buf: DmaRegion,
    _recv_buf_gpadl: u32,
    send_buf: DmaRegion,
    _send_buf_gpadl: u32,
    nvsp_version: u32,
    mac: [u8; 6],
    mtu: u32,
    next_request_id: u32,
    next_txid: u64,
}

impl NetvscDevice {
    /// Initialize a NetVSC device: open channel, negotiate NVSP, set up
    /// shared buffers, negotiate RNDIS, query MAC and MTU, enable receive.
    pub fn init(
        vmbus: &mut VmBus,
        dma: &impl DmaAllocator,
        memory: &MemoryMapper,
    ) -> Result<Self, HvError> {
        // Find netvsc channel offer
        let offer = vmbus
            .find_offer(&guid::NETVSC)
            .ok_or(HvError::NotHyperV)?
            .clone();
        info!(
            "NetVSC: found channel relid={}, conn={}",
            offer.child_relid, offer.connection_id
        );

        // Open VMBus channel
        let channel = vmbus.open_channel(&offer, NETVSC_RING_SIZE, dma, memory)?;
        info!("NetVSC: channel opened");

        // ── Phase 1: NVSP init ──────────────────────────────────────

        // Negotiate NVSP version (try v5, fall back to v4, then v1)
        let nvsp_version = Self::negotiate_nvsp_version(&channel)?;
        info!("NetVSC: NVSP version {:#x} negotiated", nvsp_version);

        // Allocate and register receive buffer
        let recv_buf = dma.alloc_coherent(NETVSC_RECV_BUF_SIZE, 4096);
        let recv_pfns = pfn_list(&recv_buf);
        let recv_gpadl = channel::alloc_gpadl_handle();
        channel::create_gpadl(
            offer.child_relid,
            recv_gpadl,
            NETVSC_RECV_BUF_SIZE,
            &recv_pfns,
            &vmbus.hcall,
            &vmbus.synic,
        )?;
        info!(
            "NetVSC: recv buffer GPADL {} ({}KB)",
            recv_gpadl,
            NETVSC_RECV_BUF_SIZE / 1024
        );

        // Send NVSP_MSG1_TYPE_SEND_RECV_BUF
        let mut msg = [0u8; NVSP_MESSAGE_SIZE];
        let len = build_nvsp_send_recv_buf(recv_gpadl, NETVSC_RECEIVE_BUFFER_ID, &mut msg);
        channel.send(&msg[..len], 1)?;

        // Wait for SEND_RECV_BUF_COMPLETE (may arrive as completion type 11 with payload)
        let mut resp = [0u8; 256];
        let resp_len = loop {
            let (_desc, len) = channel.recv_with_timeout(&mut resp, 50_000_000)?;
            if len > 0 {
                break len;
            }
        };
        // RECV_BUF_COMPLETE: msg_type(4) + status(4) + num_sections(4) + ...
        // NVSP_STAT_SUCCESS = 1
        let recv_status = if resp_len >= 8 {
            u32::from_le_bytes(resp[4..8].try_into().unwrap())
        } else {
            0xFFFF
        };
        if recv_status != 1 {
            error!(
                "NetVSC: recv buffer setup failed: status {:#x}",
                recv_status
            );
            return Err(HvError::HypercallFailed(recv_status as u16));
        }
        info!("NetVSC: recv buffer registered");

        // Allocate and register send buffer
        let send_buf = dma.alloc_coherent(NETVSC_SEND_BUF_SIZE, 4096);
        let send_pfns = pfn_list(&send_buf);
        let send_gpadl = channel::alloc_gpadl_handle();
        channel::create_gpadl(
            offer.child_relid,
            send_gpadl,
            NETVSC_SEND_BUF_SIZE,
            &send_pfns,
            &vmbus.hcall,
            &vmbus.synic,
        )?;
        info!(
            "NetVSC: send buffer GPADL {} ({}KB)",
            send_gpadl,
            NETVSC_SEND_BUF_SIZE / 1024
        );

        // Send NVSP_MSG1_TYPE_SEND_SEND_BUF
        let len = build_nvsp_send_send_buf(send_gpadl, NETVSC_SEND_BUFFER_ID, &mut msg);
        channel.send(&msg[..len], 2)?;

        // Wait for SEND_SEND_BUF_COMPLETE (may arrive as completion type 11 with payload)
        let resp_len = loop {
            let (_desc, len) = channel.recv_with_timeout(&mut resp, 5_000_000)?;
            if len > 0 {
                break len;
            }
        };
        // SEND_BUF_COMPLETE: msg_type(4) + status(4) + section_size(4)
        // (msg_type=105, NVSP_STAT_SUCCESS=1)
        let send_status = if resp_len >= 8 {
            u32::from_le_bytes(resp[4..8].try_into().unwrap())
        } else {
            0xFFFF
        };
        if send_status != 1 {
            error!(
                "NetVSC: send buffer setup failed: status {:#x}",
                send_status
            );
            return Err(HvError::HypercallFailed(send_status as u16));
        }
        let send_section_size = if resp_len >= 12 {
            u32::from_le_bytes(resp[8..12].try_into().unwrap())
        } else {
            0
        };
        info!(
            "NetVSC: send buffer registered, section_size={}",
            send_section_size
        );

        // Send NDIS config and version (fire-and-forget, after buffer setup)
        if nvsp_version > NVSP_PROTOCOL_VERSION_1 {
            let mut ndis_msg = [0u8; NVSP_MESSAGE_SIZE];
            ndis_msg[0..4].copy_from_slice(&NVSP_MSG2_TYPE_SEND_NDIS_CONFIG.to_le_bytes());
            let mtu_with_hdr: u32 = 1514 + 14;
            ndis_msg[4..8].copy_from_slice(&mtu_with_hdr.to_le_bytes());
            ndis_msg[8..12].copy_from_slice(&1u32.to_le_bytes()); // ieee8021q=1
            channel.send_raw(&ndis_msg, 0)?;
        }
        {
            let mut ndis_msg = [0u8; NVSP_MESSAGE_SIZE];
            ndis_msg[0..4].copy_from_slice(&NVSP_MSG1_TYPE_SEND_NDIS_VER.to_le_bytes());
            ndis_msg[4..8].copy_from_slice(&0x0006u32.to_le_bytes()); // NDIS 6.30
            ndis_msg[8..12].copy_from_slice(&0x001eu32.to_le_bytes());
            channel.send_raw(&ndis_msg, 0)?;
        }

        info!("NetVSC: NVSP init complete");

        // ── Phase 2: RNDIS init ─────────────────────────────────────

        let mut dev = Self {
            channel,
            _recv_buf: recv_buf,
            _recv_buf_gpadl: recv_gpadl,
            send_buf,
            _send_buf_gpadl: send_gpadl,
            nvsp_version,
            mac: [0; 6],
            mtu: 1514,
            next_request_id: 1,
            next_txid: 100,
        };

        dev.rndis_init()?;
        dev.rndis_query_mac()?;
        dev.rndis_query_mtu()?;
        dev.rndis_set_packet_filter()?;

        info!(
            "NetVSC: ready, MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, MTU={}",
            dev.mac[0], dev.mac[1], dev.mac[2], dev.mac[3], dev.mac[4], dev.mac[5], dev.mtu,
        );

        Ok(dev)
    }

    /// MAC address assigned by the hypervisor.
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// Maximum transmission unit.
    pub fn mtu(&self) -> u32 {
        self.mtu
    }

    /// NVSP protocol version negotiated with the host.
    pub fn nvsp_version(&self) -> u32 {
        self.nvsp_version
    }

    // ── NVSP version negotiation ────────────────────────────────────

    fn negotiate_nvsp_version(channel: &Channel) -> Result<u32, HvError> {
        let versions = [
            NVSP_PROTOCOL_VERSION_5,
            NVSP_PROTOCOL_VERSION_4,
            NVSP_PROTOCOL_VERSION_1,
        ];
        for &ver in &versions {
            let mut msg = [0u8; NVSP_MESSAGE_SIZE];
            let len = build_nvsp_init(ver, &mut msg);
            channel.send(&msg[..len], 0)?;

            let mut resp = [0u8; 256];
            let (_desc, resp_len) = channel.recv_with_timeout(&mut resp, 5_000_000)?;

            if let Some(NVSP_MSG_TYPE_INIT_COMPLETE) = parse_nvsp_type(&resp[..resp_len]) {
                // INIT_COMPLETE: msg_type(4) + negotiated_ver(4) + max_mdl(4) + status(4)
                // NVSP_STAT_SUCCESS = 1
                let status = if resp_len >= 16 {
                    u32::from_le_bytes(resp[12..16].try_into().unwrap())
                } else {
                    0
                };
                if status == 1 {
                    return Ok(ver);
                }
                trace!(
                    "NetVSC: NVSP version {:#x} rejected (status={})",
                    ver,
                    status
                );
            }
        }
        Err(HvError::VersionRejected)
    }

    // ── RNDIS over NVSP ─────────────────────────────────────────────

    /// Send an RNDIS control message via the send buffer.
    fn send_rndis_control(&mut self, rndis_msg: &[u8]) -> Result<(), HvError> {
        // Write RNDIS message to the start of the send buffer
        assert!(rndis_msg.len() <= NETVSC_SEND_BUF_SIZE);
        let dst = self.send_buf.vaddr as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(rndis_msg.as_ptr(), dst, rndis_msg.len());
        }

        // Wrap in NVSP_MSG1_TYPE_SEND_RNDIS_PKT (channel_type=1 for control)
        let mut nvsp = [0u8; 64];
        let nvsp_len = build_nvsp_send_rndis_pkt(
            1, // control channel
            0, // send buffer section index 0
            rndis_msg.len() as u32,
            &mut nvsp,
        );

        let txid = self.next_txid;
        self.next_txid += 1;
        self.channel.send(&nvsp[..nvsp_len], txid)
    }

    /// Receive an RNDIS control response.
    /// The response arrives as a completion with NVSP data, then a separate
    /// in-band SEND_RNDIS_PKT message with transfer pages pointing to recv_buf.
    fn recv_rndis_response(&mut self, buf: &mut [u8]) -> Result<usize, HvError> {
        for _ in 0..50_000_000u64 {
            if let Some((_desc, len)) = self.channel.try_recv(buf)? {
                if len < 4 {
                    continue;
                }
                let nvsp_type = u32::from_le_bytes(buf[0..4].try_into().unwrap());
                match nvsp_type {
                    NVSP_MSG1_TYPE_SEND_RNDIS_PKT_COMPLETE => {
                        // TX completion — skip
                        continue;
                    }
                    NVSP_MSG1_TYPE_SEND_RNDIS_PKT => {
                        // Host sent RNDIS data via recv buffer.
                        // Read RNDIS message from recv buffer offset 0
                        // (host writes control responses at start of recv buf)
                        let recv_ptr = self._recv_buf.vaddr as *const u8;
                        let rndis_len = unsafe {
                            u32::from_le_bytes(
                                core::slice::from_raw_parts(recv_ptr.add(4), 4)
                                    .try_into()
                                    .unwrap(),
                            )
                        } as usize;
                        if rndis_len == 0 || rndis_len > NETVSC_RECV_BUF_SIZE {
                            warn!("NetVSC: invalid RNDIS len {} in recv buf", rndis_len);
                            continue;
                        }
                        let copy_len = rndis_len.min(buf.len());
                        unsafe {
                            core::ptr::copy_nonoverlapping(recv_ptr, buf.as_mut_ptr(), copy_len);
                        }
                        return Ok(copy_len);
                    }
                    _ => {
                        // Other NVSP types — skip
                        continue;
                    }
                }
            }
            for _ in 0..100 {
                core::hint::spin_loop();
            }
        }
        Err(HvError::Timeout)
    }

    // ── RNDIS init sequence ─────────────────────────────────────────

    fn rndis_init(&mut self) -> Result<(), HvError> {
        let req_id = self.next_request_id;
        self.next_request_id += 1;

        let mut msg = [0u8; 64];
        let len = build_rndis_init(req_id, &mut msg);
        self.send_rndis_control(&msg[..len])?;

        let mut resp = [0u8; 256];
        let resp_len = self.recv_rndis_response(&mut resp)?;

        if resp_len >= 4 {
            let rndis_type = u32::from_le_bytes(resp[0..4].try_into().unwrap());
            if rndis_type != RNDIS_MSG_INIT_CMPLT {
                error!("NetVSC: expected RNDIS INIT_CMPLT, got {:#x}", rndis_type);
                return Err(HvError::VersionRejected);
            }
        }

        // Check status at offset 12..16
        if resp_len >= 16 {
            let status = u32::from_le_bytes(resp[12..16].try_into().unwrap());
            if status != 0 {
                error!("NetVSC: RNDIS init failed: status {:#x}", status);
                return Err(HvError::HypercallFailed(status as u16));
            }
        }

        info!("NetVSC: RNDIS initialized (v1.0)");
        Ok(())
    }

    fn rndis_query_mac(&mut self) -> Result<(), HvError> {
        let req_id = self.next_request_id;
        self.next_request_id += 1;

        let mut msg = [0u8; 64];
        let len = build_rndis_query(req_id, OID_802_3_PERMANENT_ADDRESS, &mut msg);
        self.send_rndis_control(&msg[..len])?;

        let mut resp = [0u8; 256];
        let resp_len = self.recv_rndis_response(&mut resp)?;

        if resp_len >= 4 {
            let rndis_type = u32::from_le_bytes(resp[0..4].try_into().unwrap());
            if rndis_type != RNDIS_MSG_QUERY_CMPLT {
                error!("NetVSC: expected QUERY_CMPLT, got {:#x}", rndis_type);
                return Err(HvError::VersionRejected);
            }
        }

        // RNDIS_QUERY_CMPLT layout:
        //   hdr(8) + request_id(4) + status(4) + info_buf_len(4) + info_buf_offset(4)
        // info_buf_offset is from &request_id (offset 8 in message)
        if resp_len >= 24 {
            let info_buf_len = u32::from_le_bytes(resp[16..20].try_into().unwrap()) as usize;
            let info_buf_offset = u32::from_le_bytes(resp[20..24].try_into().unwrap()) as usize;
            // info buffer is at offset 8 + info_buf_offset (from start of message)
            let data_start = 8 + info_buf_offset;
            if info_buf_len >= 6 && data_start + 6 <= resp_len {
                self.mac.copy_from_slice(&resp[data_start..data_start + 6]);
                info!(
                    "NetVSC: MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    self.mac[0], self.mac[1], self.mac[2], self.mac[3], self.mac[4], self.mac[5],
                );
            }
        }

        Ok(())
    }

    fn rndis_query_mtu(&mut self) -> Result<(), HvError> {
        let req_id = self.next_request_id;
        self.next_request_id += 1;

        let mut msg = [0u8; 64];
        let len = build_rndis_query(req_id, OID_GEN_MAXIMUM_FRAME_SIZE, &mut msg);
        self.send_rndis_control(&msg[..len])?;

        let mut resp = [0u8; 256];
        let resp_len = self.recv_rndis_response(&mut resp)?;

        if resp_len >= 24 {
            let rndis_type = u32::from_le_bytes(resp[0..4].try_into().unwrap());
            if rndis_type == RNDIS_MSG_QUERY_CMPLT {
                let info_buf_len = u32::from_le_bytes(resp[16..20].try_into().unwrap()) as usize;
                let info_buf_offset = u32::from_le_bytes(resp[20..24].try_into().unwrap()) as usize;
                let data_start = 8 + info_buf_offset;
                if info_buf_len >= 4 && data_start + 4 <= resp_len {
                    self.mtu =
                        u32::from_le_bytes(resp[data_start..data_start + 4].try_into().unwrap());
                    info!("NetVSC: MTU={}", self.mtu);
                }
            }
        }

        Ok(())
    }

    fn rndis_set_packet_filter(&mut self) -> Result<(), HvError> {
        let req_id = self.next_request_id;
        self.next_request_id += 1;

        let filter =
            NDIS_PACKET_TYPE_DIRECTED | NDIS_PACKET_TYPE_MULTICAST | NDIS_PACKET_TYPE_BROADCAST;

        let mut msg = [0u8; 64];
        let len = build_rndis_set_u32(req_id, OID_GEN_CURRENT_PACKET_FILTER, filter, &mut msg);
        self.send_rndis_control(&msg[..len])?;

        let mut resp = [0u8; 256];
        let resp_len = self.recv_rndis_response(&mut resp)?;

        if resp_len >= 4 {
            let rndis_type = u32::from_le_bytes(resp[0..4].try_into().unwrap());
            if rndis_type != RNDIS_MSG_SET_CMPLT {
                error!("NetVSC: expected SET_CMPLT, got {:#x}", rndis_type);
                return Err(HvError::VersionRejected);
            }
        }

        // Check status
        if resp_len >= 16 {
            let status = u32::from_le_bytes(resp[12..16].try_into().unwrap());
            if status != 0 {
                warn!("NetVSC: set packet filter status: {:#x}", status);
            }
        }

        info!("NetVSC: packet filter set (directed+multicast+broadcast)");
        Ok(())
    }

    // ── Data path (Phase 3) ─────────────────────────────────────────

    /// Transmit an Ethernet frame via RNDIS_PACKET_MSG.
    pub fn transmit(&mut self, frame: &[u8]) -> Result<(), HvError> {
        let rndis_len = RNDIS_PACKET_HDR_SIZE + frame.len();
        assert!(rndis_len <= NETVSC_SEND_BUF_SIZE);

        // Build RNDIS_PACKET_MSG header + frame in send buffer
        let dst = self.send_buf.vaddr as *mut u8;
        unsafe {
            // Zero the header
            core::ptr::write_bytes(dst, 0, RNDIS_PACKET_HDR_SIZE);
            // msg_type
            core::ptr::copy_nonoverlapping(RNDIS_MSG_PACKET.to_le_bytes().as_ptr(), dst, 4);
            // msg_len
            core::ptr::copy_nonoverlapping(
                (rndis_len as u32).to_le_bytes().as_ptr(),
                dst.add(4),
                4,
            );
            // data_offset = 36 (from start of data_offset field, which is at byte 8)
            // So data starts at byte 8 + 36 = 44 = RNDIS_PACKET_HDR_SIZE
            core::ptr::copy_nonoverlapping(36u32.to_le_bytes().as_ptr(), dst.add(8), 4);
            // data_len
            core::ptr::copy_nonoverlapping(
                (frame.len() as u32).to_le_bytes().as_ptr(),
                dst.add(12),
                4,
            );
            // Copy Ethernet frame after header
            core::ptr::copy_nonoverlapping(
                frame.as_ptr(),
                dst.add(RNDIS_PACKET_HDR_SIZE),
                frame.len(),
            );
        }

        // Send NVSP_MSG1_TYPE_SEND_RNDIS_PKT (channel_type=0 for data)
        let mut nvsp = [0u8; 64];
        let nvsp_len = build_nvsp_send_rndis_pkt(
            0, // data channel
            0, // send buffer section index 0
            rndis_len as u32,
            &mut nvsp,
        );

        let txid = self.next_txid;
        self.next_txid += 1;
        self.channel.send(&nvsp[..nvsp_len], txid)
    }

    /// Try to receive an Ethernet frame. Returns the frame length, or None
    /// if no frame is available.
    pub fn try_receive(&mut self, frame_buf: &mut [u8]) -> Result<Option<usize>, HvError> {
        let mut pkt = [0u8; 256];
        if let Some((_desc, len)) = self.channel.try_recv(&mut pkt)? {
            if let Some(nvsp_type) = parse_nvsp_type(&pkt[..len]) {
                match nvsp_type {
                    NVSP_MSG1_TYPE_SEND_RNDIS_PKT => {
                        // Data from host — extract from receive buffer
                        return self.extract_data_frame(frame_buf);
                    }
                    NVSP_MSG1_TYPE_SEND_RNDIS_PKT_COMPLETE => {
                        // TX completion — ignore
                    }
                    _ => {
                        trace!("NetVSC: unexpected NVSP type {} in data path", nvsp_type);
                    }
                }
            }
        }
        Ok(None)
    }

    /// Extract a data frame from the receive buffer.
    fn extract_data_frame(&self, frame_buf: &mut [u8]) -> Result<Option<usize>, HvError> {
        let recv_ptr = self._recv_buf.vaddr as *const u8;

        // Read RNDIS header from receive buffer
        let rndis_type = unsafe {
            u32::from_le_bytes(core::slice::from_raw_parts(recv_ptr, 4).try_into().unwrap())
        };

        if rndis_type == RNDIS_MSG_PACKET {
            let data_offset = unsafe {
                u32::from_le_bytes(
                    core::slice::from_raw_parts(recv_ptr.add(8), 4)
                        .try_into()
                        .unwrap(),
                )
            } as usize;
            let data_len = unsafe {
                u32::from_le_bytes(
                    core::slice::from_raw_parts(recv_ptr.add(12), 4)
                        .try_into()
                        .unwrap(),
                )
            } as usize;

            // Data starts at offset 8 + data_offset from start of message
            let frame_start = 8 + data_offset;
            let copy_len = data_len.min(frame_buf.len());

            if frame_start + copy_len <= NETVSC_RECV_BUF_SIZE {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        recv_ptr.add(frame_start),
                        frame_buf.as_mut_ptr(),
                        copy_len,
                    );
                }
                return Ok(Some(copy_len));
            }
        } else if rndis_type == RNDIS_MSG_KEEPALIVE {
            // Respond to keepalive
            let req_id = unsafe {
                u32::from_le_bytes(
                    core::slice::from_raw_parts(recv_ptr.add(8), 4)
                        .try_into()
                        .unwrap(),
                )
            };
            let mut resp = [0u8; 16];
            let len = build_rndis_keepalive_cmplt(req_id, &mut resp);
            // Best-effort send
            let _ = self.send_rndis_control_inner(&resp[..len]);
        }

        Ok(None)
    }

    /// Send an RNDIS control message (non-mutable version for keepalive responses).
    fn send_rndis_control_inner(&self, rndis_msg: &[u8]) -> Result<(), HvError> {
        let dst = self.send_buf.vaddr as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(rndis_msg.as_ptr(), dst, rndis_msg.len());
        }

        let mut nvsp = [0u8; 64];
        let nvsp_len = build_nvsp_send_rndis_pkt(1, 0, rndis_msg.len() as u32, &mut nvsp);

        self.channel.send(&nvsp[..nvsp_len], 0)
    }
}
