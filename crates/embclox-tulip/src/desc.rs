//! TX/RX descriptor types and ring management for the DEC 21140/21143.
//!
//! Each descriptor is 16 bytes. The DEC 21140 is a 32-bit PCI device,
//! so all buffer addresses are u32 — physical addresses MUST be below 4 GB.

/// Number of TX descriptors in the ring.
pub const TX_RING_SIZE: usize = 16;
/// Number of RX descriptors in the ring.
pub const RX_RING_SIZE: usize = 16;
/// Size of each packet buffer (max 2047 for DEC 21140 11-bit descriptor field).
pub const MBUF_SIZE: usize = 2048;
/// Buffer size value for descriptors (11-bit field, bits [10:0]).
/// DEC 21140 limits buffer size to 2047 (0x7FF). Use 2048-byte buffers
/// but tell the NIC 2047 — the extra byte is padding.
pub const DESC_BUF_SIZE: u32 = 2047;
/// Page size for DMA allocations.
pub const PAGE_SIZE: usize = 4096;

/// Tulip TX/RX descriptor (16 bytes).
///
/// Both TX and RX use the same layout. The OWN bit in `status` field
/// determines who owns the descriptor (NIC vs driver).
#[derive(Debug, Clone, Copy)]
#[repr(C, align(4))]
pub struct TulipDesc {
    /// Status/control word.
    /// Bit 31 (OWN): 1 = NIC owns, 0 = driver owns.
    /// RX: bits [29:16] = frame length, error bits in [14:0].
    /// TX: error bits.
    pub status: u32,
    /// Control word.
    /// Bits [10:0] = buffer 1 size.
    /// Bits [21:11] = buffer 2 size.
    /// Bit 24 = TER (TX End of Ring) / RER (RX End of Ring).
    /// Bit 25 = TCH (TX Chained) — buf2 is next descriptor address.
    /// TX: Bit 29 = LS (Last Segment), Bit 28 = FS (First Segment).
    pub control: u32,
    /// Physical address of buffer 1 (MUST be < 4 GB).
    pub buf1_addr: u32,
    /// Physical address of buffer 2, or next descriptor (if TCH set).
    pub buf2_addr: u32,
}

// Descriptor status bits
pub const DESC_OWN: u32 = 1 << 31;

// TX control bits
pub const TDES1_FS: u32 = 1 << 29; // First Segment
pub const TDES1_LS: u32 = 1 << 30; // Last Segment
pub const TDES1_TER: u32 = 1 << 25; // TX End of Ring
pub const TDES1_TCH: u32 = 1 << 24; // TX Chained
pub const TDES1_IC: u32 = 1 << 31; // Interrupt on Completion
pub const TDES1_SET: u32 = 1 << 27; // Setup Packet

/// Setup frame size (192 bytes = 16 perfect filter entries × 12 bytes).
pub const SETUP_FRAME_SIZE: usize = 192;

// RX control bits
pub const RDES1_RER: u32 = 1 << 25; // RX End of Ring

/// Extract received frame length from RX descriptor status.
/// Frame length is in bits [29:16] (14 bits).
pub fn rx_frame_length(status: u32) -> usize {
    ((status >> 16) & 0x3FFF) as usize
}

/// DMA allocation page counts.
pub const TX_RING_PAGES: usize =
    (TX_RING_SIZE * core::mem::size_of::<TulipDesc>()).div_ceil(PAGE_SIZE);
pub const RX_RING_PAGES: usize =
    (RX_RING_SIZE * core::mem::size_of::<TulipDesc>()).div_ceil(PAGE_SIZE);
pub const TX_BUF_PAGES: usize = (TX_RING_SIZE * MBUF_SIZE).div_ceil(PAGE_SIZE);
pub const RX_BUF_PAGES: usize = (RX_RING_SIZE * MBUF_SIZE).div_ceil(PAGE_SIZE);
