use core::mem::size_of;

pub const TX_RING_SIZE: usize = 256;
pub const RX_RING_SIZE: usize = 256;
pub const MBUF_SIZE: usize = 2048;
pub const PAGE_SIZE: usize = 4096;

// Compile-time alignment check: ring size * 16-byte descriptors must be 128-byte aligned.
const _: () = assert!((TX_RING_SIZE * size_of::<TxDesc>()).is_multiple_of(128));
const _: () = assert!((RX_RING_SIZE * size_of::<RxDesc>()).is_multiple_of(128));

/// Transmit descriptor [Intel 82540 SDM 3.3.3]
#[derive(Debug, Clone, Copy)]
#[repr(C, align(16))]
pub struct TxDesc {
    pub addr: u64,
    pub length: u16,
    pub cso: u8,
    pub cmd: u8,
    pub status: u8,
    pub css: u8,
    pub special: u16,
}

/// Receive descriptor [Intel 82540 SDM 3.2.3]
#[derive(Debug, Clone, Copy)]
#[repr(C, align(16))]
pub struct RxDesc {
    pub addr: u64,
    pub length: u16,
    pub csum: u16,
    pub status: u8,
    pub errors: u8,
    pub special: u16,
}

/// Mutable state for the RX descriptor ring.
pub struct RxRingState {
    pub ring_vaddr: usize,
    pub ring_paddr: usize,
    pub bufs_vaddr: usize,
    pub bufs_paddr: usize,
}

/// Mutable state for the TX descriptor ring.
pub struct TxRingState {
    pub ring_vaddr: usize,
    pub ring_paddr: usize,
    pub bufs_vaddr: usize,
    pub bufs_paddr: usize,
}

impl RxRingState {
    pub fn ring(&self) -> &[RxDesc] {
        unsafe { core::slice::from_raw_parts(self.ring_vaddr as *const RxDesc, RX_RING_SIZE) }
    }
    pub fn ring_mut(&mut self) -> &mut [RxDesc] {
        unsafe { core::slice::from_raw_parts_mut(self.ring_vaddr as *mut RxDesc, RX_RING_SIZE) }
    }
    pub fn buf_vaddr(&self, index: usize) -> usize {
        self.bufs_vaddr + index * MBUF_SIZE
    }
    pub fn buf_paddr(&self, index: usize) -> usize {
        self.bufs_paddr + index * MBUF_SIZE
    }
}

impl TxRingState {
    pub fn ring(&self) -> &[TxDesc] {
        unsafe { core::slice::from_raw_parts(self.ring_vaddr as *const TxDesc, TX_RING_SIZE) }
    }
    pub fn ring_mut(&mut self) -> &mut [TxDesc] {
        unsafe { core::slice::from_raw_parts_mut(self.ring_vaddr as *mut TxDesc, TX_RING_SIZE) }
    }
    pub fn buf_vaddr(&self, index: usize) -> usize {
        self.bufs_vaddr + index * MBUF_SIZE
    }
}

/// Allocation page counts for DMA regions.
pub const TX_RING_PAGES: usize = (TX_RING_SIZE * size_of::<TxDesc>()).div_ceil(PAGE_SIZE);
pub const RX_RING_PAGES: usize = (RX_RING_SIZE * size_of::<RxDesc>()).div_ceil(PAGE_SIZE);
pub const TX_BUF_PAGES: usize = (TX_RING_SIZE * MBUF_SIZE).div_ceil(PAGE_SIZE);
pub const RX_BUF_PAGES: usize = (RX_RING_SIZE * MBUF_SIZE).div_ceil(PAGE_SIZE);
