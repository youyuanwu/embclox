//! DEC 21140/21143 Tulip device driver.

extern crate alloc;

use crate::csr::CsrAccess;
use crate::csr::*;
use crate::desc::*;
use crate::eeprom;
use core::sync::atomic::{fence, Ordering};
use embclox_dma::{DmaAllocator, DmaRegion};
use log::*;

/// Main Tulip device structure. Generic over DMA allocator.
pub struct TulipDevice<D: DmaAllocator> {
    csr: CsrAccess,
    dma: D,
    tx_ring_dma: DmaRegion,
    rx_ring_dma: DmaRegion,
    tx_bufs_dma: DmaRegion,
    rx_bufs_dma: DmaRegion,
    tx_next: usize,
    rx_next: usize,
    mac: [u8; 6],
}

fn assert_addr32(paddr: usize, what: &str) {
    assert!(
        paddr <= 0xFFFF_FFFF,
        "Tulip: {} physical address {:#x} exceeds 32-bit limit",
        what,
        paddr
    );
}

impl<D: DmaAllocator> TulipDevice<D> {
    /// Create and initialize a Tulip device.
    ///
    /// `csr` specifies the register access mode (MMIO or I/O ports).
    /// Performs software reset, allocates DMA rings, reads MAC, enables TX/RX.
    pub fn new(csr: CsrAccess, dma: D) -> Self {
        info!("Initializing Tulip device");

        // Software reset
        unsafe { csr.write(CSR0, CSR0_SWR) };
        for _ in 0..10_000 {
            core::hint::spin_loop();
        }
        // Bus mode: burst length = 8 longwords, transmit auto poll interval
        // TAP bits [19:17] = 1 (200µs auto-poll), PBL bits [13:8] = 8
        unsafe { csr.write(CSR0, (1 << 17) | (8 << 8)) };

        // Allocate DMA memory
        let tx_ring_dma = dma.alloc_coherent(TX_RING_PAGES * PAGE_SIZE, PAGE_SIZE);
        let rx_ring_dma = dma.alloc_coherent(RX_RING_PAGES * PAGE_SIZE, PAGE_SIZE);
        let tx_bufs_dma = dma.alloc_coherent(TX_BUF_PAGES * PAGE_SIZE, PAGE_SIZE);
        let rx_bufs_dma = dma.alloc_coherent(RX_BUF_PAGES * PAGE_SIZE, PAGE_SIZE);

        assert_addr32(tx_ring_dma.paddr, "TX ring");
        assert_addr32(rx_ring_dma.paddr, "RX ring");
        assert_addr32(tx_bufs_dma.paddr, "TX buffers");
        assert_addr32(rx_bufs_dma.paddr, "RX buffers");
        assert_addr32(tx_bufs_dma.paddr + tx_bufs_dma.size - 1, "TX buffers end");
        assert_addr32(rx_bufs_dma.paddr + rx_bufs_dma.size - 1, "RX buffers end");

        // Initialize TX descriptors via volatile writes
        for i in 0..TX_RING_SIZE {
            let desc_ptr = (tx_ring_dma.vaddr + i * 16) as *mut u32;
            let ctrl = if i == TX_RING_SIZE - 1 { TDES1_TER } else { 0 };
            unsafe {
                core::ptr::write_volatile(desc_ptr, 0); // status
                core::ptr::write_volatile(desc_ptr.add(1), ctrl); // control
                core::ptr::write_volatile(
                    desc_ptr.add(2),
                    (tx_bufs_dma.paddr + i * MBUF_SIZE) as u32,
                ); // buf1
                core::ptr::write_volatile(desc_ptr.add(3), 0); // buf2
            }
        }

        // Initialize RX descriptors via volatile writes
        for i in 0..RX_RING_SIZE {
            let desc_ptr = (rx_ring_dma.vaddr + i * 16) as *mut u32;
            let mut ctrl = DESC_BUF_SIZE;
            if i == RX_RING_SIZE - 1 {
                ctrl |= RDES1_RER;
            }
            let buf_paddr = (rx_bufs_dma.paddr + i * MBUF_SIZE) as u32;
            unsafe {
                // Write control, buf1, buf2 first, then status (OWN) last
                core::ptr::write_volatile(desc_ptr.add(1), ctrl); // control
                core::ptr::write_volatile(desc_ptr.add(2), buf_paddr); // buf1_addr
                core::ptr::write_volatile(desc_ptr.add(3), 0); // buf2_addr
                fence(Ordering::Release);
                core::ptr::write_volatile(desc_ptr, DESC_OWN); // status (OWN=1)
            }
        }

        fence(Ordering::SeqCst);

        // Set descriptor list base addresses
        unsafe {
            csr.write(CSR3, rx_ring_dma.paddr as u32);
            csr.write(CSR4, tx_ring_dma.paddr as u32);
        }

        // Read MAC address
        let mac = unsafe { eeprom::read_mac(&csr) }.unwrap_or_else(|| {
            let fallback = eeprom::random_mac(0xDEC21140);
            warn!(
                "Tulip: EEPROM MAC read failed, using random MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                fallback[0], fallback[1], fallback[2], fallback[3], fallback[4], fallback[5]
            );
            fallback
        });
        info!(
            "Tulip: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );

        // Enable TX only first (need to send setup frame before RX works)
        // CSR6: ST=1, PR=1 (promiscuous), SF=1 (store and forward)
        unsafe { csr.write(CSR6, CSR6_ST | (1 << 6) | (1 << 21)) };

        let mut dev = TulipDevice {
            csr,
            dma,
            tx_ring_dma,
            rx_ring_dma,
            tx_bufs_dma,
            rx_bufs_dma,
            tx_next: 0,
            rx_next: 0,
            mac,
        };

        // Send setup frame to program the receive address filter.
        // Required by the 21140 — without this, Hyper-V won't pass frames.
        dev.send_setup_frame();

        // Now enable RX
        let csr6 = unsafe { dev.csr.read(CSR6) };
        unsafe { dev.csr.write(CSR6, csr6 | CSR6_SR) };
        unsafe { dev.csr.write(CSR2, 1) }; // RX poll demand

        info!("Tulip: device initialized");
        dev
    }

    /// Send a perfect-filter setup frame (192 bytes).
    /// Programs the NIC's MAC address filter with our MAC + broadcast.
    fn send_setup_frame(&mut self) {
        let desc_ptr = (self.tx_ring_dma.vaddr + self.tx_next * 16) as *mut u32;

        // Wait for descriptor to be available
        fence(Ordering::Acquire);
        let status = unsafe { core::ptr::read_volatile(desc_ptr) };
        assert!(
            status & DESC_OWN == 0,
            "TX descriptor not available for setup frame"
        );

        // Build 192-byte setup frame in the TX buffer.
        // Perfect filtering mode: 16 entries × 12 bytes.
        // Each entry is 3 × 4 bytes: [addr0|addr1, pad, addr2|addr3, pad, addr4|addr5, pad]
        let buf_vaddr = self.tx_bufs_dma.vaddr + self.tx_next * MBUF_SIZE;
        let buf =
            unsafe { core::slice::from_raw_parts_mut(buf_vaddr as *mut u8, SETUP_FRAME_SIZE) };

        // Fill all 16 entries with broadcast first
        for entry in 0..16 {
            let off = entry * 12;
            // Each entry: bytes [0..1] = addr[0..1], [2..3] = pad
            //             bytes [4..5] = addr[2..3], [6..7] = pad
            //             bytes [8..9] = addr[4..5], [10..11] = pad
            buf[off] = 0xFF;
            buf[off + 1] = 0xFF;
            buf[off + 2] = 0xFF; // pad
            buf[off + 3] = 0xFF; // pad
            buf[off + 4] = 0xFF;
            buf[off + 5] = 0xFF;
            buf[off + 6] = 0xFF; // pad
            buf[off + 7] = 0xFF; // pad
            buf[off + 8] = 0xFF;
            buf[off + 9] = 0xFF;
            buf[off + 10] = 0xFF; // pad
            buf[off + 11] = 0xFF; // pad
        }

        // Entry 0: our MAC address
        buf[0] = self.mac[0];
        buf[1] = self.mac[1];
        buf[4] = self.mac[2];
        buf[5] = self.mac[3];
        buf[8] = self.mac[4];
        buf[9] = self.mac[5];

        fence(Ordering::Release);

        // Setup frame descriptor: SET bit + size 192
        let mut ctrl = TDES1_SET | (SETUP_FRAME_SIZE as u32 & 0x7FF);
        if self.tx_next == TX_RING_SIZE - 1 {
            ctrl |= TDES1_TER;
        }
        unsafe {
            core::ptr::write_volatile(desc_ptr.add(1), ctrl);
            fence(Ordering::Release);
            core::ptr::write_volatile(desc_ptr, DESC_OWN);
        }

        // Kick TX
        unsafe { self.csr.write(CSR1, 1) };

        // Wait for setup frame to complete
        for _ in 0..100_000u32 {
            fence(Ordering::Acquire);
            let s = unsafe { core::ptr::read_volatile(desc_ptr) };
            if s & DESC_OWN == 0 {
                info!("Tulip: setup frame sent");
                self.tx_next = (self.tx_next + 1) % TX_RING_SIZE;
                return;
            }
            core::hint::spin_loop();
        }
        warn!("Tulip: setup frame timeout");
        self.tx_next = (self.tx_next + 1) % TX_RING_SIZE;
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    pub fn enable_interrupts(&self) {
        unsafe {
            self.csr
                .write(CSR7, CSR7_TIE | CSR7_RIE | CSR7_NIE | CSR7_AIE)
        };
    }

    /// Acknowledge interrupts. Mask/unmask to avoid W1C race.
    pub fn handle_interrupt(&self) -> u32 {
        unsafe { self.csr.write(CSR7, 0) };
        fence(Ordering::SeqCst);
        let status = unsafe { self.csr.read(CSR5) };
        unsafe { self.csr.write(CSR5, status) };
        unsafe {
            self.csr
                .write(CSR7, CSR7_TIE | CSR7_RIE | CSR7_NIE | CSR7_AIE)
        };
        status
    }

    pub fn has_rx_packet(&self) -> bool {
        let desc_ptr: *const u32 = (self.rx_ring_dma.vaddr + self.rx_next * 16) as *const u32;
        fence(Ordering::Acquire);
        let status: u32 = unsafe { core::ptr::read_volatile(desc_ptr) };
        status & DESC_OWN == 0
    }

    pub fn has_tx_space(&self) -> bool {
        let desc_ptr: *const u32 = (self.tx_ring_dma.vaddr + self.tx_next * 16) as *const u32;
        fence(Ordering::Acquire);
        let status: u32 = unsafe { core::ptr::read_volatile(desc_ptr) };
        status & DESC_OWN == 0
    }

    pub fn recv_with<R>(&mut self, f: impl FnOnce(&mut [u8]) -> R) -> Option<R> {
        let desc_ptr = (self.rx_ring_dma.vaddr + self.rx_next * 16) as *mut u32;

        fence(Ordering::Acquire);
        let status = unsafe { core::ptr::read_volatile(desc_ptr) };
        if status & DESC_OWN != 0 {
            return None;
        }

        let len = rx_frame_length(status);
        let buf_vaddr = self.rx_bufs_dma.vaddr + self.rx_next * MBUF_SIZE;
        let buf = unsafe { core::slice::from_raw_parts_mut(buf_vaddr as *mut u8, len) };
        let result = f(buf);

        let mut ctrl = DESC_BUF_SIZE;
        if self.rx_next == RX_RING_SIZE - 1 {
            ctrl |= RDES1_RER;
        }
        unsafe {
            core::ptr::write_volatile(desc_ptr.add(1), ctrl);
            fence(Ordering::Release);
            core::ptr::write_volatile(desc_ptr, DESC_OWN);
        }
        self.rx_next = (self.rx_next + 1) % RX_RING_SIZE;
        unsafe { self.csr.write(CSR2, 1) };
        Some(result)
    }

    pub fn transmit_with<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> Option<R> {
        assert!(len <= MBUF_SIZE);
        let desc_ptr = (self.tx_ring_dma.vaddr + self.tx_next * 16) as *mut u32;

        fence(Ordering::Acquire);
        let status = unsafe { core::ptr::read_volatile(desc_ptr) };
        if status & DESC_OWN != 0 {
            return None;
        }

        let buf_vaddr = self.tx_bufs_dma.vaddr + self.tx_next * MBUF_SIZE;
        let buf = unsafe { core::slice::from_raw_parts_mut(buf_vaddr as *mut u8, len) };
        let result = f(buf);

        let mut ctrl = (len as u32) & 0x7FF;
        ctrl |= TDES1_FS | TDES1_LS;
        if self.tx_next == TX_RING_SIZE - 1 {
            ctrl |= TDES1_TER;
        }
        unsafe {
            core::ptr::write_volatile(desc_ptr.add(1), ctrl);
            fence(Ordering::Release);
            core::ptr::write_volatile(desc_ptr, DESC_OWN);
        }
        self.tx_next = (self.tx_next + 1) % TX_RING_SIZE;
        unsafe { self.csr.write(CSR1, 1) };
        Some(result)
    }
}

impl<D: DmaAllocator> Drop for TulipDevice<D> {
    fn drop(&mut self) {
        unsafe { self.csr.write(CSR7, 0) };
        unsafe { self.csr.write(CSR6, 0) };
        unsafe { self.csr.write(CSR0, CSR0_SWR) };
        for _ in 0..10_000 {
            core::hint::spin_loop();
        }
        unsafe {
            self.dma.free_coherent(&self.tx_ring_dma);
            self.dma.free_coherent(&self.rx_ring_dma);
            self.dma.free_coherent(&self.tx_bufs_dma);
            self.dma.free_coherent(&self.rx_bufs_dma);
        }
    }
}
