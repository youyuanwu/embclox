//! VMBus ring buffer operations.
//!
//! Each VMBus channel has two ring buffers (send + receive), each consisting
//! of a 4KB control page followed by a circular data buffer. Messages are
//! wrapped in `VmPacketDescriptor` headers and padded to 8-byte boundaries.

use core::sync::atomic::{fence, Ordering};

/// VMBus packet descriptor (16 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VmPacketDescriptor {
    pub packet_type: u16,
    /// Offset from packet start to payload, in 8-byte units.
    pub offset8: u16,
    /// Total packet length in 8-byte units.
    pub len8: u16,
    pub flags: u16,
    pub transaction_id: u64,
}

pub const VM_PKT_DATA_INBAND: u16 = 6;
pub const VMBUS_DATA_PACKET_FLAG_COMPLETION_REQUESTED: u16 = 1;

/// One half of a VMBus ring buffer (send or receive).
pub struct RingHalf {
    /// Virtual address of the ring (control page + data).
    base: usize,
    /// Size of the data area in bytes (total_size - 4096).
    data_size: usize,
}

/// Error from ring buffer operations.
#[derive(Debug)]
pub enum RingError {
    /// Send ring is full.
    Full,
    /// Ring indices or packet header are invalid.
    Corrupt,
}

impl RingHalf {
    /// Create a ring half. `base` is the vaddr of the region; first 4096 bytes
    /// are the control page, remainder is the circular data buffer.
    pub fn new(base: usize, total_size: usize) -> Self {
        assert!(total_size > 4096 && total_size.is_multiple_of(4096));

        // Set feature_bits to enable flow control (Linux sets this to 1).
        // Ring buffer control page layout:
        //   offset 0: write_index (u32)
        //   offset 4: read_index (u32)
        //   offset 8: interrupt_mask (u32)
        //   offset 12: pending_send_sz (u32)
        //   offset 16..63: reserved (12 * u32)
        //   offset 64: feature_bits (u32) — bit 0 = flow control
        let feature_bits_ptr = (base + 64) as *mut u32;
        unsafe {
            core::ptr::write_volatile(feature_bits_ptr, 1);
        }

        Self {
            base,
            data_size: total_size - 4096,
        }
    }

    fn read_write_index(&self) -> u32 {
        unsafe { core::ptr::read_volatile(self.base as *const u32) }
    }

    fn set_write_index(&self, val: u32) {
        unsafe { core::ptr::write_volatile(self.base as *mut u32, val) }
    }

    fn read_read_index(&self) -> u32 {
        unsafe { core::ptr::read_volatile((self.base + 4) as *const u32) }
    }

    fn set_read_index(&self, val: u32) {
        unsafe { core::ptr::write_volatile((self.base + 4) as *mut u32, val) }
    }

    fn data_ptr(&self) -> *mut u8 {
        (self.base + 4096) as *mut u8
    }

    /// Copy `src` into the ring data buffer at `offset`, wrapping around.
    /// Returns the new offset after the copy.
    unsafe fn copy_to(&self, offset: usize, src: &[u8]) -> usize {
        let off = offset % self.data_size;
        let first = (self.data_size - off).min(src.len());
        let data = self.data_ptr();
        core::ptr::copy_nonoverlapping(src.as_ptr(), data.add(off), first);
        if first < src.len() {
            core::ptr::copy_nonoverlapping(src.as_ptr().add(first), data, src.len() - first);
        }
        (off + src.len()) % self.data_size
    }

    /// Copy from the ring data buffer at `offset` into `dst`, wrapping around.
    /// Returns the new offset after the copy.
    unsafe fn copy_from(&self, offset: usize, dst: &mut [u8]) -> usize {
        let off = offset % self.data_size;
        let first = (self.data_size - off).min(dst.len());
        let data = self.data_ptr();
        core::ptr::copy_nonoverlapping(data.add(off), dst.as_mut_ptr(), first);
        if first < dst.len() {
            core::ptr::copy_nonoverlapping(data, dst.as_mut_ptr().add(first), dst.len() - first);
        }
        (off + dst.len()) % self.data_size
    }

    /// Write a VMBus in-band data packet to the send ring.
    ///
    /// Builds a `VmPacketDescriptor` header, writes header + payload + padding
    /// + 8-byte trailing indices, then updates write_index.
    pub fn send_packet(
        &self,
        payload: &[u8],
        transaction_id: u64,
        flags: u16,
    ) -> Result<(), RingError> {
        let header_size = 16usize; // size of VmPacketDescriptor
        let packet_len = align8(header_size + payload.len());
        let total_write = packet_len + 8; // + trailing prev_indices

        let write_idx = self.read_write_index() as usize;
        let read_idx = self.read_read_index() as usize;
        fence(Ordering::Acquire);

        let avail = if write_idx >= read_idx {
            self.data_size - (write_idx - read_idx)
        } else {
            read_idx - write_idx
        };

        // Need at least total_write + 1 bytes free (can't fill completely)
        if avail <= total_write {
            return Err(RingError::Full);
        }

        let desc = VmPacketDescriptor {
            packet_type: VM_PKT_DATA_INBAND,
            offset8: (header_size / 8) as u16,
            len8: (packet_len / 8) as u16,
            flags,
            transaction_id,
        };

        let desc_bytes =
            unsafe { core::slice::from_raw_parts(&desc as *const _ as *const u8, header_size) };

        let mut pos = unsafe { self.copy_to(write_idx, desc_bytes) };
        if !payload.is_empty() {
            pos = unsafe { self.copy_to(pos, payload) };
        }

        // Zero-pad to 8-byte alignment
        let pad = packet_len - header_size - payload.len();
        if pad > 0 {
            let zeros = [0u8; 8];
            pos = unsafe { self.copy_to(pos, &zeros[..pad]) };
        }

        // Trailing prev_indices: (old_write << 32) | read_index
        let prev = ((write_idx as u64) << 32) | (read_idx as u64);
        unsafe { self.copy_to(pos, &prev.to_le_bytes()) };

        fence(Ordering::Release);
        self.set_write_index(((write_idx + total_write) % self.data_size) as u32);

        Ok(())
    }

    /// Read a packet from the receive ring.
    ///
    /// Copies the payload into `buf` and returns `(descriptor, payload_len)`.
    /// Returns `Ok(None)` if no packet is available.
    pub fn recv_packet(
        &self,
        buf: &mut [u8],
    ) -> Result<Option<(VmPacketDescriptor, usize)>, RingError> {
        let write_idx = self.read_write_index() as usize;
        fence(Ordering::Acquire);
        let read_idx = self.read_read_index() as usize;

        let avail = if write_idx >= read_idx {
            write_idx - read_idx
        } else {
            self.data_size - (read_idx - write_idx)
        };

        if avail == 0 {
            return Ok(None);
        }
        if avail < 16 {
            return Err(RingError::Corrupt);
        }

        // Read descriptor
        let mut desc_bytes = [0u8; 16];
        let pos = unsafe { self.copy_from(read_idx, &mut desc_bytes) };

        let desc =
            unsafe { core::ptr::read_unaligned(desc_bytes.as_ptr() as *const VmPacketDescriptor) };

        let packet_len = (desc.len8 as usize) * 8;
        let total_read = packet_len + 8; // + trailing indices

        if packet_len < 16 || total_read > avail {
            return Err(RingError::Corrupt);
        }

        // Calculate payload location and size
        let data_offset = (desc.offset8 as usize) * 8;
        if data_offset > packet_len {
            return Err(RingError::Corrupt);
        }
        let payload_len = packet_len - data_offset;

        // Skip to payload (if offset8 > 2, skip padding between header and data)
        let mut payload_pos = pos;
        if data_offset > 16 {
            let mut skip = [0u8; 64];
            let skip_len = data_offset - 16;
            payload_pos = unsafe { self.copy_from(pos, &mut skip[..skip_len]) };
        }

        // Copy payload
        let copy_len = payload_len.min(buf.len());
        if copy_len > 0 {
            unsafe { self.copy_from(payload_pos, &mut buf[..copy_len]) };
        }

        // Advance read index past packet + trailing indices
        fence(Ordering::Release);
        self.set_read_index(((read_idx + total_read) % self.data_size) as u32);

        Ok(Some((desc, copy_len)))
    }
}

/// Round up to 8-byte alignment.
fn align8(n: usize) -> usize {
    (n + 7) & !7
}
