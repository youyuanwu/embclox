//! Hyper-V device GUIDs.

/// A 128-bit GUID in Microsoft's mixed-endian format.
///
/// In memory, the first three fields (Data1, Data2, Data3) are
/// little-endian, and the last field (Data4) is big-endian.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    bytes: [u8; 16],
}

impl Guid {
    /// Create a GUID from the standard `{Data1-Data2-Data3-Data4}` fields.
    pub const fn from_fields(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        let d1 = data1.to_le_bytes();
        let d2 = data2.to_le_bytes();
        let d3 = data3.to_le_bytes();
        Guid {
            bytes: [
                d1[0], d1[1], d1[2], d1[3], d2[0], d2[1], d3[0], d3[1], data4[0], data4[1],
                data4[2], data4[3], data4[4], data4[5], data4[6], data4[7],
            ],
        }
    }

    /// Create a GUID from raw bytes (as they appear in memory).
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Guid { bytes }
    }

    /// Raw bytes of this GUID.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.bytes
    }
}

impl core::fmt::Debug for Guid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let b = &self.bytes;
        write!(
            f,
            "{{{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            b[3], b[2], b[1], b[0], // Data1 (LE → display BE)
            b[5], b[4],             // Data2
            b[7], b[6],             // Data3
            b[8], b[9],             // Data4[0..2]
            b[10], b[11], b[12], b[13], b[14], b[15], // Data4[2..8]
        )
    }
}

impl core::fmt::Display for Guid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self, f)
    }
}

// Well-known Hyper-V device GUIDs

/// Synthvid (synthetic video adapter).
pub const SYNTHVID: Guid = Guid::from_fields(
    0xDA0A7802,
    0xE377,
    0x4AAC,
    [0x8E, 0x77, 0x05, 0x58, 0xEB, 0x10, 0x73, 0xF8],
);

/// NetVSC (synthetic network adapter).
pub const NETVSC: Guid = Guid::from_fields(
    0xF8615163,
    0xDF3E,
    0x46C5,
    [0x91, 0x3F, 0xF2, 0xD2, 0xF9, 0x65, 0xED, 0x0E],
);

/// Synthetic keyboard.
pub const SYNTH_KEYBOARD: Guid = Guid::from_fields(
    0xF912AD6D,
    0x2B17,
    0x48EA,
    [0xBD, 0x65, 0xF9, 0x27, 0xA6, 0x1C, 0x76, 0x84],
);

/// Heartbeat / Integration Services.
pub const HEARTBEAT: Guid = Guid::from_fields(
    0x57164F39,
    0x9115,
    0x4E78,
    [0xAB, 0x55, 0x38, 0x2F, 0x3B, 0xD5, 0x42, 0x2D],
);
