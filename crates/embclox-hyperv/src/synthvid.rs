//! Synthvid (Hyper-V synthetic video) protocol.
//!
//! Negotiates the synthvid version, maps VRAM, sets resolution,
//! and triggers display updates via dirty rectangles.
//!
//! All structs are `__packed` per the Linux hyperv_fb.c reference.
//! Every message has: pipe_msg_hdr(8) + synthvid_msg_hdr(8) + body.

use crate::channel::Channel;
use crate::guid;
use crate::HvError;
use embclox_e1000::dma::{DmaAllocator, DmaRegion};
use log::*;

// Synthvid message types
const SYNTHVID_VERSION_REQUEST: u32 = 1;
const SYNTHVID_VERSION_RESPONSE: u32 = 2;
const SYNTHVID_VRAM_LOCATION: u32 = 3;
const SYNTHVID_VRAM_LOCATION_ACK: u32 = 4;
const SYNTHVID_SITUATION_UPDATE: u32 = 5;
const SYNTHVID_DIRT: u32 = 10;

// Pipe message type
const PIPE_MSG_DATA: u32 = 1;

// Synthvid versions: (minor << 16) | major — opposite of VMBus!
const SYNTHVID_VERSION_WIN10: u32 = (5 << 16) | 3; // v3.5
const SYNTHVID_VERSION_WIN8: u32 = (2 << 16) | 3; // v3.2

/// Build a synthvid message: pipe_hdr(8) + vid_hdr(8) + body.
///
/// `vid_type`: synthvid message type (e.g., VERSION_REQUEST)
/// `body`: packed body bytes
/// Returns the complete message bytes.
fn build_msg(vid_type: u32, body: &[u8], buf: &mut [u8]) -> usize {
    let vid_hdr_size = 8 + body.len(); // synthvid_msg_hdr + body
    let total = 8 + vid_hdr_size; // pipe_hdr + vid_hdr + body
    assert!(total <= buf.len());

    // pipe_msg_hdr: type(u32) + size(u32)
    buf[0..4].copy_from_slice(&PIPE_MSG_DATA.to_le_bytes());
    buf[4..8].copy_from_slice(&(vid_hdr_size as u32).to_le_bytes());

    // synthvid_msg_hdr: type(u32) + size(u32)
    buf[8..12].copy_from_slice(&vid_type.to_le_bytes());
    buf[12..16].copy_from_slice(&(vid_hdr_size as u32).to_le_bytes());

    // body
    buf[16..16 + body.len()].copy_from_slice(body);

    total
}

/// Parse a received synthvid message type from ring buffer payload.
/// Skips pipe_hdr(8), reads vid_hdr.type at offset 8.
fn parse_vid_type(buf: &[u8]) -> Option<u32> {
    if buf.len() < 16 {
        return None;
    }
    Some(u32::from_le_bytes(buf[8..12].try_into().unwrap()))
}

/// Synthvid display device over a VMBus channel.
pub struct SynthvidDevice {
    channel: Channel,
    vram: DmaRegion,
    width: u32,
    height: u32,
    _bpp: u32,
    stride: u32,
    next_txid: u64,
}

impl SynthvidDevice {
    /// Initialize synthvid on an open VMBus channel.
    ///
    /// If `fb_phys` is provided, uses that GPA as VRAM (e.g., the UEFI GOP
    /// framebuffer address which the host already knows about). Otherwise
    /// allocates fresh VRAM from the DMA allocator.
    pub fn init(
        channel: Channel,
        width: u32,
        height: u32,
        dma: &impl DmaAllocator,
        fb_phys: Option<u64>,
        fb_vaddr: Option<usize>,
    ) -> Result<Self, HvError> {
        let bpp = 32u32;
        let stride = width;
        let vram_size = (width * height * (bpp / 8)) as usize;
        let vram_alloc = (vram_size + 4095) & !4095;

        // Use existing framebuffer GPA or allocate new VRAM
        let (vram, use_existing_fb) = if let (Some(phys), Some(vaddr)) = (fb_phys, fb_vaddr) {
            info!("Synthvid: using existing framebuffer at paddr={:#x}", phys);
            (
                embclox_e1000::dma::DmaRegion {
                    vaddr,
                    paddr: phys as usize,
                    size: vram_alloc,
                },
                true,
            )
        } else {
            (dma.alloc_coherent(vram_alloc, 4096), false)
        };
        let _ = use_existing_fb;

        let mut dev = Self {
            channel,
            vram,
            width,
            height,
            _bpp: bpp,
            stride,
            next_txid: 1,
        };

        dev.negotiate_version()?;
        dev.drain_recv();
        dev.set_vram_location()?;
        dev.drain_recv();
        dev.set_situation()?;
        dev.drain_recv();
        // Send initial full-screen dirty rect
        dev.dirt_full()?;
        dev.drain_recv();

        info!(
            "Synthvid: {}x{} @ {}bpp, VRAM at paddr={:#x}",
            width, height, bpp, dev.vram.paddr
        );

        Ok(dev)
    }

    /// Get a mutable pointer to the VRAM framebuffer (BGRX 32bpp).
    pub fn framebuffer(&self) -> *mut u8 {
        self.vram.vaddr as *mut u8
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }

    /// Physical address of the VRAM allocation.
    pub fn vram_paddr(&self) -> usize {
        self.vram.paddr
    }

    /// Notify the host that a rectangular region has been updated.
    pub fn dirty_rect(&mut self, x: i32, y: i32, w: i32, h: i32) -> Result<(), HvError> {
        // synthvid_dirt (packed): video_output(u8) + dirt_count(u8) + rect(4*i32)
        let mut body = [0u8; 18];
        body[0] = 0; // video_output
        body[1] = 1; // dirt_count
                     // rect: x1, y1, x2, y2 (s32, packed)
        body[2..6].copy_from_slice(&x.to_le_bytes());
        body[6..10].copy_from_slice(&y.to_le_bytes());
        body[10..14].copy_from_slice(&(x + w).to_le_bytes());
        body[14..18].copy_from_slice(&(y + h).to_le_bytes());

        let mut msg = [0u8; 64];
        let len = build_msg(SYNTHVID_DIRT, &body, &mut msg);

        let txid = self.next_txid;
        self.next_txid += 1;
        self.channel.send_raw(&msg[..len], txid)
    }

    /// Send a full-screen dirty rectangle.
    pub fn dirt_full(&mut self) -> Result<(), HvError> {
        self.dirty_rect(0, 0, self.width as i32, self.height as i32)
    }

    /// Write a pixel at (x, y) in BGRX format.
    pub fn put_pixel(&self, x: u32, y: u32, r: u8, g: u8, b: u8) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = ((y * self.stride + x) * 4) as usize;
        let fb = self.framebuffer();
        unsafe {
            *fb.add(offset) = b;
            *fb.add(offset + 1) = g;
            *fb.add(offset + 2) = r;
            *fb.add(offset + 3) = 0;
        }
    }

    // --- Private protocol methods ---

    /// Drain pending messages from the recv ring.
    fn drain_recv(&self) {
        let mut buf = [0u8; 256];
        for _ in 0..50 {
            for _ in 0..50_000 {
                core::hint::spin_loop();
            }
            match self.channel.try_recv(&mut buf) {
                Ok(Some(_)) => {} // discard and continue
                _ => break,
            }
        }
    }

    fn negotiate_version(&mut self) -> Result<(), HvError> {
        let versions = [SYNTHVID_VERSION_WIN10, SYNTHVID_VERSION_WIN8];

        for &version in &versions {
            let major = version & 0xFFFF;
            let minor = version >> 16;
            info!("Synthvid: trying version {}.{}", major, minor);

            // Body: version(u32) — packed, 4 bytes
            let body = version.to_le_bytes();
            let mut msg = [0u8; 32];
            let len = build_msg(SYNTHVID_VERSION_REQUEST, &body, &mut msg);

            let txid = self.next_txid;
            self.next_txid += 1;
            self.channel.send_raw(&msg[..len], txid)?;

            // Wait for VERSION_RESPONSE
            let mut buf = [0u8; 256];
            let (_, rlen) = self.channel.recv_with_timeout(&mut buf, 5_000_000)?;

            if rlen >= 22 {
                // pipe_hdr(8) + vid_hdr(8) + version(4) + is_accepted(1) + max_outputs(1)
                if let Some(vid_type) = parse_vid_type(&buf) {
                    if vid_type == SYNTHVID_VERSION_RESPONSE {
                        let is_accepted = buf[20]; // offset 16 (body start) + 4 (version)
                        if is_accepted != 0 {
                            info!("Synthvid: version {}.{} accepted", major, minor);
                            return Ok(());
                        }
                        info!("Synthvid: version {}.{} rejected", major, minor);
                        continue;
                    }
                }
            }
            warn!(
                "Synthvid: unexpected response len={} during version negotiation",
                rlen
            );
        }

        Err(HvError::VersionRejected)
    }

    fn set_vram_location(&mut self) -> Result<(), HvError> {
        // synthvid_vram_location (packed):
        //   u64 user_ctx (8) + u8 is_vram_gpa_specified (1) + u64 vram_gpa (8) = 17 bytes
        let mut body = [0u8; 17];
        // user_ctx = 0 (already zero)
        body[8] = 1; // is_vram_gpa_specified
        body[9..17].copy_from_slice(&(self.vram.paddr as u64).to_le_bytes());

        let mut msg = [0u8; 48];
        let len = build_msg(SYNTHVID_VRAM_LOCATION, &body, &mut msg);

        let txid = self.next_txid;
        self.next_txid += 1;
        self.channel.send_raw(&msg[..len], txid)?;

        // Wait for VRAM_LOCATION_ACK
        let mut buf = [0u8; 256];
        let (_, rlen) = self.channel.recv_with_timeout(&mut buf, 5_000_000)?;

        if rlen >= 16 {
            if let Some(vid_type) = parse_vid_type(&buf) {
                if vid_type == SYNTHVID_VRAM_LOCATION_ACK {
                    info!("Synthvid: VRAM location acknowledged");
                    return Ok(());
                }
            }
        }

        warn!("Synthvid: unexpected response to VRAM_LOCATION");
        Err(HvError::Timeout)
    }

    fn set_situation(&mut self) -> Result<(), HvError> {
        // synthvid_situation_update (packed):
        //   u64 user_ctx (8) + u8 video_output_count (1) +
        //   video_output_situation[1] (packed: u8+u32+u8+u32+u32+u32 = 18) = 27 bytes
        let mut body = [0u8; 27];
        // user_ctx = 0
        body[8] = 1; // video_output_count
                     // video_output_situation[0] (packed):
        body[9] = 1; // active
        body[10..14].copy_from_slice(&0u32.to_le_bytes()); // vram_offset
        body[14] = 32; // depth_bits
        body[15..19].copy_from_slice(&self.width.to_le_bytes()); // width_pixels
        body[19..23].copy_from_slice(&self.height.to_le_bytes()); // height_pixels
        let pitch = self.width * 4; // bytes per row
        body[23..27].copy_from_slice(&pitch.to_le_bytes()); // pitch_bytes

        let mut msg = [0u8; 64];
        let len = build_msg(SYNTHVID_SITUATION_UPDATE, &body, &mut msg);

        let txid = self.next_txid;
        self.next_txid += 1;
        self.channel.send_raw(&msg[..len], txid)?;

        Ok(())
    }
}

/// Initialize synthvid display from a VmBus connection.
///
/// Finds the synthvid offer, opens the channel, and initializes the display.
/// Returns `None` if no synthvid device is available (headless VM).
pub fn init_display(
    vmbus: &mut crate::VmBus,
    width: u32,
    height: u32,
    dma: &impl DmaAllocator,
    memory: &embclox_hal_x86::memory::MemoryMapper,
    fb_phys: Option<u64>,
    fb_vaddr: Option<usize>,
) -> Result<Option<SynthvidDevice>, HvError> {
    let offer = match vmbus.find_offer(&guid::SYNTHVID) {
        Some(o) => o.clone(),
        None => {
            info!("Synthvid: no device found (headless VM)");
            return Ok(None);
        }
    };

    // 256KB ring buffer (128KB send + 128KB receive)
    let channel = vmbus.open_channel(&offer, 256 * 1024, dma, memory)?;
    let device = SynthvidDevice::init(channel, width, height, dma, fb_phys, fb_vaddr)?;

    Ok(Some(device))
}
