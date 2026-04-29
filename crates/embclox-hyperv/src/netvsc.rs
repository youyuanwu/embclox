//! NetVSC (Hyper-V synthetic NIC) driver — NVSP + RNDIS layers.
//!
//! Phase 1: NVSP channel setup (version negotiation, shared buffer GPADL).
//! Phase 2: RNDIS init (version, MAC query, packet filter).
//! Phase 3: Packet send/recv (RNDIS_PACKET_MSG).

use crate::channel::{self, Channel};
use crate::ffi;
use crate::guid;
use crate::nvsp_msg::{
    build_nvsp_init, build_nvsp_send_ndis_config, build_nvsp_send_ndis_version,
    build_nvsp_send_recv_buf, build_nvsp_send_rndis_pkt, build_nvsp_send_send_buf,
    build_rndis_init, build_rndis_query, build_rndis_set, nvsp_message_padded, parse_nvsp_response,
    parse_rndis_response, rndis_message_as_bytes, NvspResponse, RndisResponse, RNDIS_HEADER_SIZE,
};
use crate::nvsp_types::{
    NdisOid, NdisPacketFilter, NvspMessageType, NvspVersion, RndisMessageType, VmbusPacketType,
};
use crate::HvError;
use crate::VmBus;
use embassy_sync::waitqueue::AtomicWaker;
use embclox_dma::{DmaAllocator, DmaRegion};
use embclox_hal_x86::memory::MemoryMapper;
use log::*;

/// Waker for the NetVSC data path. Signal this from the SynIC SINT2 ISR
/// (or any other "channel may have new packets" event) to wake the
/// embassy task driving the NIC.
///
/// Safe to call `.wake()` from interrupt context — `AtomicWaker` is
/// designed for ISR → task notification.
pub static NETVSC_WAKER: AtomicWaker = AtomicWaker::new();

// Buffer sizes (our chosen allocation sizes, not protocol constants)
const NETVSC_RECV_BUF_SIZE: usize = 2 * 1024 * 1024; // 2 MB
const NETVSC_SEND_BUF_SIZE: usize = 1024 * 1024; // 1 MB
const NETVSC_RING_SIZE: usize = 256 * 1024; // 256 KB (128 KB × 2)

/// Maximum Ethernet frame size we buffer for the embassy RX queue.
const RX_FRAME_MAX: usize = 1600;

// RNDIS_PACKET_MSG header: rndis_packet struct + 8-byte RNDIS header
const RNDIS_PACKET_HDR_SIZE: usize = core::mem::size_of::<ffi::rndis_packet>() + RNDIS_HEADER_SIZE;

/// Build RNDIS_KEEPALIVE_CMPLT response.
///
/// This is a response we send back to the host (not a request), so we keep
/// a local builder using the KeepAliveComplete message type.
fn build_rndis_keepalive_cmplt(request_id: u32, buf: &mut [u8]) -> usize {
    let len = 16; // msg_type(4) + msg_len(4) + request_id(4) + status(4)
    buf[..len].fill(0);
    buf[0..4].copy_from_slice(&RndisMessageType::KeepAliveComplete.as_u32().to_le_bytes());
    buf[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    buf[8..12].copy_from_slice(&request_id.to_le_bytes());
    // status = RNDIS_STATUS_SUCCESS (0)
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
    // Response slot for synchronous RNDIS control messages.
    // Only one control message in flight at a time (like Linux's channel_init_pkt).
    ctrl_resp: [u8; 256],
    ctrl_resp_len: usize,
    ctrl_resp_ready: bool,
    // Track send buffer section availability
    send_section_free: bool,
    // Single-slot RX queue used by the embassy data path. `pump_channel`
    // fills it; `recv_with` drains it. When `Some`, no further data
    // packets are pulled from the VMBus ring (they remain there until
    // the slot is consumed).
    pending_rx: Option<PendingRx>,
}

struct PendingRx {
    data: [u8; RX_FRAME_MAX],
    len: usize,
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

        // Send NDIS version (fire-and-forget, per Linux netvsc_connect_vsp)
        {
            // NDIS 6.30 for NVSPv5+, NDIS 6.1 for v4 and below
            let (major, minor) = if nvsp_version > NvspVersion::V4.as_u32() {
                (6, 30)
            } else {
                (6, 1)
            };
            let msg = build_nvsp_send_ndis_version(major, minor);
            channel.send_raw(&nvsp_message_padded(&msg), 98)?;
            info!("NetVSC: sent NDIS_VER {}.{}", major, minor);
        }

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
        let msg =
            build_nvsp_send_recv_buf(recv_gpadl, crate::nvsp_types::buffer::RECEIVE_BUFFER_ID);
        channel.send(&nvsp_message_padded(&msg), 1)?;

        // Wait for SEND_RECV_BUF_COMPLETE (may arrive as completion type 11 with payload)
        let mut resp = [0u8; 256];
        let resp_len = loop {
            let (desc, len) = channel.recv_with_timeout(&mut resp, 50_000_000)?;
            info!(
                "NetVSC: buf_setup recv: type={} off8={} len8={} txid={} payload={}",
                desc.packet_type, desc.offset8, desc.len8, desc.transaction_id, len
            );
            if len > 0 {
                break len;
            }
        };
        if let Some(NvspResponse::RecvBufComplete { status, .. }) =
            parse_nvsp_response(&resp[..resp_len])
        {
            if status != ffi::NVSP_STAT_SUCCESS {
                error!("NetVSC: recv buffer setup failed: status {:#x}", status);
                return Err(HvError::HypercallFailed(status as u16));
            }
        } else {
            error!("NetVSC: recv buffer setup: unexpected response");
            return Err(HvError::HypercallFailed(0xFFFF));
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
        let msg = build_nvsp_send_send_buf(send_gpadl, crate::nvsp_types::buffer::SEND_BUFFER_ID);
        channel.send(&nvsp_message_padded(&msg), 2)?;

        // Wait for SEND_SEND_BUF_COMPLETE (may arrive as completion type 11 with payload)
        let resp_len = loop {
            let (_desc, len) = channel.recv_with_timeout(&mut resp, 5_000_000)?;
            if len > 0 {
                break len;
            }
        };
        let send_section_size = if let Some(NvspResponse::SendBufComplete {
            status,
            section_size,
        }) = parse_nvsp_response(&resp[..resp_len])
        {
            if status != ffi::NVSP_STAT_SUCCESS {
                error!("NetVSC: send buffer setup failed: status {:#x}", status);
                return Err(HvError::HypercallFailed(status as u16));
            }
            section_size
        } else {
            error!("NetVSC: send buffer setup: unexpected response");
            return Err(HvError::HypercallFailed(0xFFFF));
        };
        info!(
            "NetVSC: send buffer registered, section_size={}",
            send_section_size
        );

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
            ctrl_resp: [0; 256],
            ctrl_resp_len: 0,
            ctrl_resp_ready: false,
            send_section_free: true,
            pending_rx: None,
        };

        dev.rndis_init()?;

        // Drain TX completions and unsolicited messages before next control
        for _ in 0..10_000_000u64 {
            if !dev.poll_channel()? {
                // No more packets — wait a bit and try once more
                for _ in 0..100_000 {
                    core::hint::spin_loop();
                }
                dev.poll_channel()?;
                break;
            }
        }

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
        // Try from highest to lowest
        for &ver in NvspVersion::NEGOTIATE_ORDER {
            let msg = build_nvsp_init(ver.as_u32());
            channel.send(&nvsp_message_padded(&msg), 0)?;

            let mut resp = [0u8; 256];
            let (_desc, resp_len) = channel.recv_with_timeout(&mut resp, 5_000_000)?;

            if let Some(NvspResponse::InitComplete { status, .. }) =
                parse_nvsp_response(&resp[..resp_len])
            {
                if status == ffi::NVSP_STAT_SUCCESS {
                    // Version accepted.
                    // For NVSPv2+: send NDIS_CONFIG (fire-and-forget, like Linux)
                    if ver != NvspVersion::V1 {
                        let mtu: u32 = 1514 + 14; // MTU + ETH_HLEN
                        let cfg = build_nvsp_send_ndis_config(mtu, 1); // ieee8021q=1
                        channel.send_raw(&nvsp_message_padded(&cfg), 99)?;
                    }
                    return Ok(ver.as_u32());
                }
                trace!(
                    "NetVSC: NVSP version {:#x} rejected (status={})",
                    ver.as_u32(),
                    status
                );
            }
        }
        Err(HvError::VersionRejected)
    }

    // ── RNDIS over NVSP ─────────────────────────────────────────────

    /// Send an RNDIS control message via the send buffer.
    fn send_rndis_control(&mut self, rndis_msg: &[u8]) -> Result<(), HvError> {
        trace!(
            "NetVSC: send_rndis_control: len={} type={:#x}",
            rndis_msg.len(),
            if rndis_msg.len() >= 4 {
                u32::from_le_bytes(rndis_msg[0..4].try_into().unwrap())
            } else {
                0
            }
        );
        // Wait for send buffer section 0 to be free
        if !self.send_section_free {
            for _ in 0..50_000_000u64 {
                self.poll_channel()?;
                if self.send_section_free {
                    break;
                }
                for _ in 0..100 {
                    core::hint::spin_loop();
                }
            }
            if !self.send_section_free {
                error!("NetVSC: send section timeout");
                return Err(HvError::Timeout);
            }
        }

        assert!(rndis_msg.len() <= NETVSC_SEND_BUF_SIZE);
        let dst = self.send_buf.vaddr as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(rndis_msg.as_ptr(), dst, rndis_msg.len());
        }

        let nvsp = build_nvsp_send_rndis_pkt(1, 0, rndis_msg.len() as u32);
        let bytes = nvsp_message_padded(&nvsp);

        self.send_section_free = false; // Mark section as in use
        let txid = self.next_txid;
        self.next_txid += 1;
        self.channel.send(&bytes, txid)
    }

    // ── Channel dispatch loop ────────────────────────────────────

    /// Poll the VMBus channel and dispatch all pending packets.
    /// Returns true if any packet was processed.
    fn poll_channel(&mut self) -> Result<bool, HvError> {
        let mut raw = [0u8; 512];
        let mut processed = false;

        while let Some((desc, raw_len)) = self.channel.try_recv_raw(&mut raw)? {
            processed = true;
            trace!(
                "NetVSC: poll: pkt type={} off8={} len8={} raw_len={} txid={}",
                desc.packet_type,
                desc.offset8,
                desc.len8,
                raw_len,
                desc.transaction_id
            );
            match VmbusPacketType::from_u16(desc.packet_type) {
                Some(VmbusPacketType::DataUsingXferPages) => {
                    self.handle_xfer_page(&desc, &raw[..raw_len])?;
                }
                Some(VmbusPacketType::Completion) => {
                    // TX completion — check if NVSP SEND_RNDIS_PKT_COMPLETE
                    let nvsp_offset = (desc.offset8 as usize) * 8 - 16;
                    if nvsp_offset < raw_len && raw_len >= nvsp_offset + 4 {
                        let msg_type = u32::from_le_bytes(
                            raw[nvsp_offset..nvsp_offset + 4].try_into().unwrap(),
                        );
                        if msg_type == NvspMessageType::SendRndisPktComplete.as_u32() {
                            self.send_section_free = true;
                        }
                    }
                }
                Some(VmbusPacketType::DataInBand) => {
                    if raw_len >= 4 {
                        let nvsp_offset = (desc.offset8 as usize) * 8 - 16;
                        if nvsp_offset < raw_len {
                            let nvsp_type = u32::from_le_bytes(
                                raw[nvsp_offset..nvsp_offset + 4].try_into().unwrap(),
                            );
                            trace!("NetVSC: poll: in-band NVSP type={}", nvsp_type);
                        }
                    }
                }
                _ => {
                    trace!("NetVSC: poll: unknown pkt type={}", desc.packet_type);
                }
            }
        }
        Ok(processed)
    }

    /// Handle a VM_PKT_DATA_USING_XFER_PAGES packet from the host.
    fn handle_xfer_page(
        &mut self,
        desc: &crate::ring::VmPacketDescriptor,
        raw: &[u8],
    ) -> Result<(), HvError> {
        let mut buf = [0u8; 256];
        let len = self.parse_xfer_page_packet(raw, &mut buf);
        trace!(
            "NetVSC: xfer page: len={} rndis_type={:#x}",
            len,
            if len >= 4 {
                u32::from_le_bytes(buf[0..4].try_into().unwrap())
            } else {
                0
            }
        );

        // Send recv completion back to host (VM_PKT_COMP)
        let mut comp = [0u8; 8];
        comp[0..4].copy_from_slice(&NvspMessageType::SendRndisPktComplete.as_u32().to_le_bytes());
        comp[4..8].copy_from_slice(&ffi::NVSP_STAT_SUCCESS.to_le_bytes());
        let _ = self.channel.send_completion(&comp, desc.transaction_id);

        if len < RNDIS_HEADER_SIZE {
            return Ok(());
        }

        match parse_rndis_response(&buf[..len]) {
            // Control responses → store in ctrl_resp slot
            Some(
                RndisResponse::InitComplete { .. }
                | RndisResponse::QueryComplete { .. }
                | RndisResponse::SetComplete { .. },
            ) => {
                trace!(
                    "NetVSC: ctrl response len={} data={:02x?}",
                    len,
                    &buf[..len.min(40)]
                );
                let copy_len = len.min(self.ctrl_resp.len());
                self.ctrl_resp[..copy_len].copy_from_slice(&buf[..copy_len]);
                self.ctrl_resp_len = copy_len;
                self.ctrl_resp_ready = true;
            }
            // Keepalive → respond immediately
            Some(RndisResponse::KeepAliveComplete { req_id, .. }) => {
                let mut resp = [0u8; 16];
                let rlen = build_rndis_keepalive_cmplt(req_id, &mut resp);
                let _ = self.send_rndis_control_inner(&resp[..rlen]);
            }
            // Unsolicited (INDICATE_STATUS, data packets, etc.) → skip during init
            _ => {
                let rndis_type = u32::from_le_bytes(buf[0..4].try_into().unwrap());
                trace!("NetVSC: skipping RNDIS type {:#x}", rndis_type);
            }
        }
        Ok(())
    }

    /// Wait for a RNDIS control response by polling the channel dispatch loop.
    fn recv_rndis_response(&mut self, buf: &mut [u8]) -> Result<usize, HvError> {
        self.ctrl_resp_ready = false;
        for i in 0..100_000_000u64 {
            self.poll_channel()?;
            if self.ctrl_resp_ready {
                let len = self.ctrl_resp_len.min(buf.len());
                buf[..len].copy_from_slice(&self.ctrl_resp[..len]);
                self.ctrl_resp_ready = false;
                return Ok(len);
            }
            if i > 0 && i % 10_000_000 == 0 {
                info!(
                    "NetVSC: waiting for RNDIS response... ({}M iterations)",
                    i / 1_000_000
                );
            }
            for _ in 0..100 {
                core::hint::spin_loop();
            }
        }
        error!("NetVSC: RNDIS response timeout after 100M iterations");
        Err(HvError::Timeout)
    }

    /// Parse a VM_PKT_DATA_USING_XFER_PAGES packet and copy RNDIS data from recv buffer.
    /// raw[0..] is everything after the 16-byte VmPacketDescriptor:
    ///   [0..2] xfer_pageset_id (u16)
    ///   [2]    sender_owns_set (u8)
    ///   [3]    reserved (u8)
    ///   [4..8] range_cnt (u32)
    ///   [8..]  ranges as vmtransfer_page_range (byte_count + byte_offset) * range_cnt
    fn parse_xfer_page_packet(&self, raw: &[u8], out: &mut [u8]) -> usize {
        if raw.len() < 8 {
            return 0;
        }

        let range_cnt = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize;
        if range_cnt == 0
            || raw.len() < 8 + range_cnt * core::mem::size_of::<ffi::vmtransfer_page_range>()
        {
            return 0;
        }

        // Cast first range (RNDIS responses are typically single-range)
        let range = unsafe { &*(raw[8..].as_ptr() as *const ffi::vmtransfer_page_range) };
        let byte_count = range.ByteCount as usize;
        let byte_offset = range.ByteOffset as usize;

        if byte_offset + byte_count > NETVSC_RECV_BUF_SIZE || byte_count == 0 {
            warn!(
                "NetVSC: xfer page range out of bounds: off={} len={}",
                byte_offset, byte_count
            );
            return 0;
        }

        // Copy RNDIS data from recv buffer
        let recv_ptr = self._recv_buf.vaddr as *const u8;
        let copy_len = byte_count.min(out.len());
        unsafe {
            core::ptr::copy_nonoverlapping(recv_ptr.add(byte_offset), out.as_mut_ptr(), copy_len);
        }

        copy_len
    }

    // ── RNDIS init sequence ─────────────────────────────────────────

    fn rndis_init(&mut self) -> Result<(), HvError> {
        let req_id = self.next_request_id;
        self.next_request_id += 1;

        let msg = build_rndis_init(req_id, 0x4000);
        let msg_bytes = rndis_message_as_bytes(&msg);
        let send_len = msg.MessageLength as usize;
        info!(
            "NetVSC: sending RNDIS_INIT req_id={} msg_len={} struct_size={}",
            req_id,
            send_len,
            msg_bytes.len()
        );
        self.send_rndis_control(&msg_bytes[..send_len])?;

        let mut resp = [0u8; 256];
        let resp_len = self.recv_rndis_response(&mut resp)?;

        match parse_rndis_response(&resp[..resp_len]) {
            Some(RndisResponse::InitComplete { status, .. }) => {
                if !status.is_success() {
                    error!("NetVSC: RNDIS init failed: status {}", status);
                    return Err(HvError::HypercallFailed(status.0 as u16));
                }
            }
            _ => {
                error!("NetVSC: expected RNDIS INIT_CMPLT");
                return Err(HvError::VersionRejected);
            }
        }

        info!("NetVSC: RNDIS initialized (v1.0)");
        Ok(())
    }

    fn rndis_query_mac(&mut self) -> Result<(), HvError> {
        // Try current address first, then permanent
        for oid in [NdisOid::ETH_CURRENT_ADDRESS, NdisOid::ETH_PERMANENT_ADDRESS] {
            let req_id = self.next_request_id;
            self.next_request_id += 1;

            let mut msg = [0u8; 64];
            let len = build_rndis_query(req_id, oid, &[], &mut msg);
            self.send_rndis_control(&msg[..len])?;

            let mut resp = [0u8; 256];
            let resp_len = self.recv_rndis_response(&mut resp)?;

            match parse_rndis_response(&resp[..resp_len]) {
                Some(RndisResponse::QueryComplete { status, info, .. }) => {
                    if !status.is_success() {
                        info!("NetVSC: OID {} failed: status {}", oid, status);
                        continue;
                    }
                    if info.len() >= 6 {
                        self.mac.copy_from_slice(&info[..6]);
                        info!(
                            "NetVSC: MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} ({})",
                            self.mac[0],
                            self.mac[1],
                            self.mac[2],
                            self.mac[3],
                            self.mac[4],
                            self.mac[5],
                            oid,
                        );
                        return Ok(());
                    }
                }
                _ => continue,
            }
        }
        warn!("NetVSC: could not query MAC address, using default");
        Ok(())
    }

    fn rndis_query_mtu(&mut self) -> Result<(), HvError> {
        let req_id = self.next_request_id;
        self.next_request_id += 1;

        let mut msg = [0u8; 64];
        let len = build_rndis_query(req_id, NdisOid::GEN_MAXIMUM_FRAME_SIZE, &[], &mut msg);
        self.send_rndis_control(&msg[..len])?;

        let mut resp = [0u8; 256];
        let resp_len = self.recv_rndis_response(&mut resp)?;

        if let Some(RndisResponse::QueryComplete { status, info, .. }) =
            parse_rndis_response(&resp[..resp_len])
        {
            if status.is_success() && info.len() >= 4 {
                self.mtu = u32::from_le_bytes(info[0..4].try_into().unwrap());
                info!("NetVSC: MTU={}", self.mtu);
            }
        }

        Ok(())
    }

    fn rndis_set_packet_filter(&mut self) -> Result<(), HvError> {
        let req_id = self.next_request_id;
        self.next_request_id += 1;

        let filter_val = NdisPacketFilter::STANDARD.0;

        let mut msg = [0u8; 64];
        let len = build_rndis_set(
            req_id,
            NdisOid::GEN_CURRENT_PACKET_FILTER,
            &filter_val.to_le_bytes(),
            &mut msg,
        );
        self.send_rndis_control(&msg[..len])?;

        let mut resp = [0u8; 256];
        let resp_len = self.recv_rndis_response(&mut resp)?;

        match parse_rndis_response(&resp[..resp_len]) {
            Some(RndisResponse::SetComplete { status, .. }) => {
                if !status.is_success() {
                    warn!("NetVSC: set packet filter status: {}", status);
                }
            }
            _ => {
                error!("NetVSC: expected SET_CMPLT");
                return Err(HvError::VersionRejected);
            }
        }

        info!("NetVSC: packet filter set (directed+multicast+broadcast)");
        Ok(())
    }

    // ── Embassy-shaped data path (Phase 4a) ─────────────────────────
    //
    // The data path is structured as a non-blocking poll loop driven by the
    // VMBus ring. A single internal helper, `pump_channel`, drains every
    // pending VMBus packet into structured state:
    //   - TX completions update `send_section_free`
    //   - RX data packets (xfer-page) are extracted into `pending_rx`
    //   - Unsolicited control messages (keepalive) are answered inline
    //
    // The public API exposes two predicate methods (`has_tx_space`,
    // `has_rx_packet`) and two closure-based operations (`transmit_with`,
    // `recv_with`). This matches the shape used by `tulip_embassy.rs` and
    // lets `embassy_net_driver::Driver` plug in directly without any
    // additional spinning.

    /// Returns true if there's room in the send buffer for another TX.
    ///
    /// Drains pending TX completions from the VMBus channel as a side
    /// effect — call this before checking; if it returns false the caller
    /// should register a waker (see [`NETVSC_WAKER`]) and try again later.
    pub fn has_tx_space(&mut self) -> bool {
        let _ = self.pump_channel();
        self.send_section_free
    }

    /// Returns true if there's a buffered Ethernet frame ready to be
    /// consumed via [`Self::recv_with`].
    ///
    /// Drains the VMBus channel as a side effect — when it returns false
    /// the caller should register a waker (see [`NETVSC_WAKER`]) and try
    /// again later.
    pub fn has_rx_packet(&mut self) -> bool {
        let _ = self.pump_channel();
        self.pending_rx.is_some()
    }

    /// Build and send an Ethernet frame. The closure is given a writable
    /// `frame_len`-byte slice in the send buffer to fill with Ethernet
    /// payload.
    ///
    /// Returns `Err(HvError::Timeout)` if no TX slot is available — the
    /// embassy driver should call [`Self::has_tx_space`] first and only
    /// invoke this when it returns true.
    pub fn transmit_with<R, F: FnOnce(&mut [u8]) -> R>(
        &mut self,
        frame_len: usize,
        f: F,
    ) -> Result<R, HvError> {
        let rndis_len = RNDIS_PACKET_HDR_SIZE + frame_len;
        assert!(rndis_len <= NETVSC_SEND_BUF_SIZE);

        if !self.send_section_free {
            // Drain any pending completions and re-check.
            let _ = self.pump_channel();
            if !self.send_section_free {
                return Err(HvError::Timeout);
            }
        }

        // Build RNDIS_PACKET_MSG header in send buffer, then hand the
        // payload region to the closure.
        let dst = self.send_buf.vaddr as *mut u8;
        unsafe {
            let msg_type = RndisMessageType::Packet.as_u32();
            core::ptr::copy_nonoverlapping(msg_type.to_le_bytes().as_ptr(), dst, 4);
            core::ptr::copy_nonoverlapping(
                (rndis_len as u32).to_le_bytes().as_ptr(),
                dst.add(4),
                4,
            );
            let pkt = dst.add(RNDIS_HEADER_SIZE) as *mut ffi::rndis_packet;
            core::ptr::write_bytes(pkt, 0, 1);
            (*pkt).DataOffset = (core::mem::size_of::<ffi::rndis_packet>()) as u32;
            (*pkt).DataLength = frame_len as u32;
            // Linux netvsc_xmit always sets PerPacketInfoOffset to point
            // just past the rndis_packet struct (with PerPacketInfoLength=0
            // meaning "no PPIs follow"). Some host code paths read this
            // offset unconditionally; leaving it zero makes the host
            // dereference offset 0 within the message, which silently
            // corrupts/drops UDP frames (DHCP DISCOVER never gets a reply).
            // Reference: drivers/net/hyperv/netvsc_drv.c:511-515 in Linux.
            (*pkt).PerPacketInfoOffset = (core::mem::size_of::<ffi::rndis_packet>()) as u32;
        }

        // Closure fills the Ethernet payload region directly in the
        // shared send buffer (no extra copy).
        let payload = unsafe {
            core::slice::from_raw_parts_mut(
                (self.send_buf.vaddr as *mut u8).add(RNDIS_PACKET_HDR_SIZE),
                frame_len,
            )
        };
        let result = f(payload);

        let nvsp = build_nvsp_send_rndis_pkt(0, 0, rndis_len as u32);
        let bytes = nvsp_message_padded(&nvsp);
        let txid = self.next_txid;
        self.next_txid += 1;
        self.send_section_free = false;
        self.channel.send(&bytes, txid)?;
        Ok(result)
    }

    /// Consume the next buffered Ethernet frame. The closure is given a
    /// read/write slice of exactly `frame_len` bytes.
    ///
    /// Returns `None` if no frame is ready — the embassy driver should call
    /// [`Self::has_rx_packet`] first.
    pub fn recv_with<R, F: FnOnce(&mut [u8]) -> R>(&mut self, f: F) -> Option<R> {
        let mut frame = self.pending_rx.take()?;
        Some(f(&mut frame.data[..frame.len]))
    }

    /// Drive the VMBus channel forward: drain every pending packet into
    /// internal state. Safe to call repeatedly; idempotent when the ring
    /// is empty.
    fn pump_channel(&mut self) -> Result<(), HvError> {
        let mut raw = [0u8; 512];
        while let Some((desc, raw_len)) = self.channel.try_recv_raw(&mut raw)? {
            log::debug!(
                "NetVSC pump: type={} off8={} len8={} raw_len={} txid={}",
                desc.packet_type,
                desc.offset8,
                desc.len8,
                raw_len,
                desc.transaction_id
            );
            match VmbusPacketType::from_u16(desc.packet_type) {
                Some(VmbusPacketType::DataUsingXferPages) => {
                    // If our single-slot RX buffer is full, leave the
                    // packet on the ring — the host will not reclaim it
                    // until we send a completion. Stop draining.
                    if self.pending_rx.is_some() {
                        return Ok(());
                    }
                    let mut tmp = [0u8; RX_FRAME_MAX];
                    let rndis_len = self.parse_xfer_page_packet(&raw[..raw_len], &mut tmp);

                    // Always send completion so the host can release the
                    // recv-buffer slot, even if we end up dropping the
                    // payload.
                    let mut comp = [0u8; 8];
                    comp[0..4].copy_from_slice(
                        &NvspMessageType::SendRndisPktComplete.as_u32().to_le_bytes(),
                    );
                    comp[4..8].copy_from_slice(&ffi::NVSP_STAT_SUCCESS.to_le_bytes());
                    let _ = self.channel.send_completion(&comp, desc.transaction_id);

                    if rndis_len >= RNDIS_HEADER_SIZE {
                        let msg_type = u32::from_le_bytes(tmp[0..4].try_into().unwrap());
                        if msg_type == RndisMessageType::Packet.as_u32() {
                            // Extract Ethernet frame and queue it.
                            if let Some(eth_len) = extract_eth_frame(&mut tmp, rndis_len) {
                                let mut data = [0u8; RX_FRAME_MAX];
                                data[..eth_len].copy_from_slice(&tmp[..eth_len]);
                                self.pending_rx = Some(PendingRx { data, len: eth_len });
                            }
                        } else if msg_type == RndisMessageType::KeepAlive.as_u32() {
                            // Host keepalive — answer inline.
                            if let Some(RndisResponse::KeepAliveComplete { req_id, .. }) =
                                parse_rndis_response(&tmp[..rndis_len])
                            {
                                let mut resp = [0u8; 16];
                                let len = build_rndis_keepalive_cmplt(req_id, &mut resp);
                                let _ = self.send_rndis_control_inner(&resp[..len]);
                            }
                        }
                    }
                }
                Some(VmbusPacketType::Completion) => {
                    // TX completion: free our send-buffer section if this
                    // is the SEND_RNDIS_PKT_COMPLETE for our outstanding TX.
                    let nvsp_offset = (desc.offset8 as usize) * 8 - 16;
                    if nvsp_offset + 4 <= raw_len {
                        let msg_type = u32::from_le_bytes(
                            raw[nvsp_offset..nvsp_offset + 4].try_into().unwrap(),
                        );
                        if msg_type == NvspMessageType::SendRndisPktComplete.as_u32() {
                            self.send_section_free = true;
                        }
                    }
                }
                Some(VmbusPacketType::DataInBand) => {
                    // Unsolicited NVSP control (e.g., link status) — skip.
                }
                _ => {
                    // Unknown packet type — skip.
                }
            }
        }
        Ok(())
    }

    // ── Back-compat wrappers (kept for existing tests / examples) ───

    /// Transmit an Ethernet frame. Thin wrapper over [`Self::transmit_with`].
    pub fn transmit(&mut self, frame: &[u8]) -> Result<(), HvError> {
        // Mirror the original blocking behaviour for callers that haven't
        // moved to the embassy waker pattern yet: poll until TX space is
        // available, bounded by the same spin budget as before.
        if !self.has_tx_space() {
            for _ in 0..50_000_000u64 {
                let _ = self.pump_channel();
                if self.send_section_free {
                    break;
                }
                for _ in 0..100 {
                    core::hint::spin_loop();
                }
            }
            if !self.send_section_free {
                return Err(HvError::Timeout);
            }
        }
        self.transmit_with(frame.len(), |buf| buf.copy_from_slice(frame))
    }

    /// Try to receive an Ethernet frame. Thin wrapper over
    /// [`Self::has_rx_packet`] + [`Self::recv_with`].
    pub fn try_receive(&mut self, frame_buf: &mut [u8]) -> Result<Option<usize>, HvError> {
        if !self.has_rx_packet() {
            return Ok(None);
        }
        let n = self
            .recv_with(|frame| {
                let n = frame.len().min(frame_buf.len());
                frame_buf[..n].copy_from_slice(&frame[..n]);
                n
            })
            .unwrap_or(0);
        Ok(Some(n))
    }

    /// Send an RNDIS control message (non-mutable version for keepalive responses).
    fn send_rndis_control_inner(&self, rndis_msg: &[u8]) -> Result<(), HvError> {
        let dst = self.send_buf.vaddr as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(rndis_msg.as_ptr(), dst, rndis_msg.len());
        }

        let nvsp = build_nvsp_send_rndis_pkt(1, 0, rndis_msg.len() as u32);
        let bytes = nvsp_message_padded(&nvsp);

        self.channel.send(&bytes, 0)
    }
}

/// Pull the Ethernet frame out of an RNDIS_PACKET_MSG already buffered in
/// `buf`. Returns the Ethernet length and rewrites `buf[0..eth_len]` to
/// hold the bare Ethernet frame (header + payload).
fn extract_eth_frame(buf: &mut [u8], rndis_len: usize) -> Option<usize> {
    if rndis_len < RNDIS_HEADER_SIZE {
        return None;
    }
    match parse_rndis_response(&buf[..rndis_len]) {
        Some(RndisResponse::Packet {
            data_offset,
            data_len,
            ..
        }) => {
            let data_offset = data_offset as usize;
            let data_len = data_len as usize;
            // Data starts at offset RNDIS_HEADER_SIZE + data_offset within
            // the RNDIS message buffer.
            let frame_start = RNDIS_HEADER_SIZE + data_offset;
            if frame_start + data_len <= rndis_len && data_len <= buf.len() {
                buf.copy_within(frame_start..frame_start + data_len, 0);
                Some(data_len)
            } else {
                None
            }
        }
        _ => None,
    }
}
