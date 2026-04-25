use crate::desc::*;
use crate::dma::{DmaAllocator, DmaRegion};
use crate::error::InterruptStatus;
use crate::regs::{
    self, RegisterAccess, RXD_STAT_DD, RXD_STAT_EOP, TXD_CMD_EOP, TXD_CMD_RS, TXD_STAT_DD,
};
use core::cmp::min;
use core::sync::atomic::{fence, Ordering};
use log::*;

pub struct E1000Device<R: RegisterAccess, D: DmaAllocator> {
    regs: R,
    dma: D,
    tx_ring_dma: DmaRegion,
    rx_ring_dma: DmaRegion,
    tx_bufs_dma: DmaRegion,
    rx_bufs_dma: DmaRegion,
    tx: TxRingState,
    rx: RxRingState,
}

pub struct RxHalf<'a, R: RegisterAccess> {
    regs: &'a R,
    rx: &'a mut RxRingState,
}

pub struct TxHalf<'a, R: RegisterAccess> {
    regs: &'a R,
    tx: &'a mut TxRingState,
}

impl<R: RegisterAccess, D: DmaAllocator> E1000Device<R, D> {
    /// Create and initialize an e1000 device.
    ///
    /// The caller must have already:
    /// 1. Performed a device reset (write CTRL_RST, wait for clear)
    /// 2. Re-enabled PCI bus mastering
    /// 3. Set CTRL_SLU | CTRL_ASDE
    /// 4. Disabled flow control (FCAL, FCAH, FCT, FCTTV = 0)
    ///
    /// `new()` configures TX/RX rings and enables the device.
    /// Panics if DMA allocation fails.
    pub fn new(regs: R, dma: D) -> Self {
        info!("Initializing E1000 device");

        let tx_ring_dma = dma.alloc_coherent(TX_RING_PAGES * PAGE_SIZE, PAGE_SIZE);
        let rx_ring_dma = dma.alloc_coherent(RX_RING_PAGES * PAGE_SIZE, PAGE_SIZE);
        let tx_bufs_dma = dma.alloc_coherent(TX_BUF_PAGES * PAGE_SIZE, PAGE_SIZE);
        let rx_bufs_dma = dma.alloc_coherent(RX_BUF_PAGES * PAGE_SIZE, PAGE_SIZE);

        let tx = TxRingState {
            ring_vaddr: tx_ring_dma.vaddr,
            ring_paddr: tx_ring_dma.paddr,
            bufs_vaddr: tx_bufs_dma.vaddr,
            bufs_paddr: tx_bufs_dma.paddr,
        };
        let rx = RxRingState {
            ring_vaddr: rx_ring_dma.vaddr,
            ring_paddr: rx_ring_dma.paddr,
            bufs_vaddr: rx_bufs_dma.vaddr,
            bufs_paddr: rx_bufs_dma.paddr,
        };

        let mut dev = Self {
            regs,
            dma,
            tx_ring_dma,
            rx_ring_dma,
            tx_bufs_dma,
            rx_bufs_dma,
            tx,
            rx,
        };
        dev.init_rings();
        dev
    }

    fn init_rings(&mut self) {
        // Initialize TX descriptors
        let tx_bufs_paddr = self.tx.bufs_paddr;
        let ring = self.tx.ring_mut();
        for (i, desc) in ring.iter_mut().enumerate() {
            desc.addr = (tx_bufs_paddr + i * MBUF_SIZE) as u64;
            desc.status = TXD_STAT_DD;
            desc.cmd = 0;
            desc.length = 0;
            desc.cso = 0;
            desc.css = 0;
            desc.special = 0;
        }

        // Initialize RX descriptors
        let rx_bufs_paddr = self.rx.bufs_paddr;
        let ring = self.rx.ring_mut();
        for (i, desc) in ring.iter_mut().enumerate() {
            desc.addr = (rx_bufs_paddr + i * MBUF_SIZE) as u64;
            desc.status = 0;
            desc.length = 0;
            desc.csum = 0;
            desc.errors = 0;
            desc.special = 0;
        }

        fence(Ordering::Release);

        // TX setup
        self.regs.write_reg(
            regs::TCTL,
            regs::TCTL_EN
                | regs::TCTL_PSP
                | (0x10 << regs::TCTL_CT_SHIFT)
                | (0x40 << regs::TCTL_COLD_SHIFT),
        );
        self.regs.write_reg(regs::TIPG, 10 | (8 << 10) | (6 << 20));
        self.regs.write_reg(regs::TDBAL, self.tx.ring_paddr as u32);
        self.regs
            .write_reg(regs::TDBAH, (self.tx.ring_paddr >> 32) as u32);
        self.regs.write_reg(
            regs::TDLEN,
            (TX_RING_SIZE * core::mem::size_of::<TxDesc>()) as u32,
        );
        self.regs.write_reg(regs::TDT, 0);
        self.regs.write_reg(regs::TDH, 0);

        // RX setup
        self.regs.write_reg(
            regs::RCTL,
            (regs::RCTL_EN
                | regs::RCTL_BAM
                | regs::RCTL_UPE
                | regs::RCTL_MPE
                | regs::RCTL_SZ_2048
                | regs::RCTL_SECRC)
                & !(0b11 << 10),
        );
        self.regs.write_reg(regs::RFCTL, 0);
        self.regs.write_reg(regs::RDBAL, self.rx.ring_paddr as u32);
        self.regs
            .write_reg(regs::RDBAH, (self.rx.ring_paddr >> 32) as u32);
        self.regs.write_reg(
            regs::RDLEN,
            (RX_RING_SIZE * core::mem::size_of::<RxDesc>()) as u32,
        );
        self.regs.write_reg(regs::RDH, 0);
        self.regs.write_reg(regs::RDT, (RX_RING_SIZE - 1) as u32);

        // Multicast table
        for i in 0..(4096 / 32) {
            self.regs.write_reg(regs::MTA + i, 0);
        }

        // Interrupt timers
        self.regs.write_reg(regs::TIDV, 0);
        self.regs.write_reg(regs::TADV, 0);
        self.regs.write_reg(regs::RDTR, 0);
        self.regs.write_reg(regs::RADV, 0);
        self.regs.write_reg(regs::ITR, 0);

        // Enable RX interrupts
        self.regs.write_reg(regs::IMS, regs::IMS_ENABLE_MASK);
        // Clear pending
        self.regs.read_reg(regs::ICR);
        self.write_flush();

        info!("E1000 ring init complete");
    }

    fn write_flush(&self) {
        self.regs.read_reg(regs::STAT);
    }

    /// Read MAC address from RAL/RAH registers.
    pub fn mac_address(&self) -> [u8; 6] {
        let ral = self.regs.read_reg(regs::RAL);
        let rah = self.regs.read_reg(regs::RAH);
        [
            ral as u8,
            (ral >> 8) as u8,
            (ral >> 16) as u8,
            (ral >> 24) as u8,
            rah as u8,
            (rah >> 8) as u8,
        ]
    }

    /// Check if the link is up.
    pub fn link_is_up(&self) -> bool {
        self.regs.read_reg(regs::STAT) & 0x02 != 0
    }

    /// Split into separate RX and TX handles for concurrent use.
    pub fn split(&mut self) -> (RxHalf<'_, R>, TxHalf<'_, R>) {
        (
            RxHalf {
                regs: &self.regs,
                rx: &mut self.rx,
            },
            TxHalf {
                regs: &self.regs,
                tx: &mut self.tx,
            },
        )
    }

    /// Enable device interrupts.
    pub fn enable_interrupts(&self) {
        self.regs.write_reg(regs::IMS, regs::IMS_ENABLE_MASK);
        self.write_flush();
    }

    /// Disable all device interrupts.
    pub fn disable_interrupts(&self) {
        self.regs.write_reg(regs::IMC, !0);
        self.write_flush();
    }

    /// Acknowledge and return interrupt status.
    pub fn handle_interrupt(&self) -> InterruptStatus {
        let icr = self.regs.read_reg(regs::ICR);
        self.regs.write_reg(regs::ICR, icr);
        InterruptStatus { icr }
    }
}

impl<R: RegisterAccess, D: DmaAllocator> Drop for E1000Device<R, D> {
    fn drop(&mut self) {
        self.dma.free_coherent(&self.tx_ring_dma);
        self.dma.free_coherent(&self.rx_ring_dma);
        self.dma.free_coherent(&self.tx_bufs_dma);
        self.dma.free_coherent(&self.rx_bufs_dma);
    }
}

// --- RxHalf ---

impl<R: RegisterAccess> RxHalf<'_, R> {
    /// Check if a received packet is ready without consuming it.
    pub fn has_rx_packet(&self) -> bool {
        let rindex = (self.regs.read_reg(regs::RDT) as usize + 1) % RX_RING_SIZE;
        let desc = &self.rx.ring()[rindex];
        desc.addr != 0 && (desc.status & RXD_STAT_DD) != 0
    }

    /// Receive one packet via zero-copy callback.
    /// Returns `None` if no complete packet is available.
    pub fn recv_with<T>(&mut self, f: impl FnOnce(&mut [u8]) -> T) -> Option<T> {
        let rindex = (self.regs.read_reg(regs::RDT) as usize + 1) % RX_RING_SIZE;
        let bufs_vaddr = self.rx.bufs_vaddr;
        let ring = self.rx.ring_mut();

        if ring[rindex].addr == 0 {
            return None;
        }
        let status = ring[rindex].status;
        if (status & (RXD_STAT_DD | RXD_STAT_EOP)) != (RXD_STAT_DD | RXD_STAT_EOP) {
            return None;
        }
        if ring[rindex].errors != 0 {
            ring[rindex].status = 0;
            self.regs.write_reg(regs::RDT, rindex as u32);
            self.regs.read_reg(regs::STAT);
            return None;
        }

        fence(Ordering::Acquire);
        let len = min(ring[rindex].length as usize, MBUF_SIZE);
        let buf_addr = bufs_vaddr + rindex * MBUF_SIZE;
        let buf = unsafe { core::slice::from_raw_parts_mut(buf_addr as *mut u8, len) };
        let result = f(buf);

        fence(Ordering::Release);
        buf[..min(64, len)].fill(0);
        ring[rindex].status = 0;
        self.regs.write_reg(regs::RDT, rindex as u32);
        self.regs.read_reg(regs::STAT);
        fence(Ordering::Release);

        Some(result)
    }
}

// --- TxHalf ---

impl<R: RegisterAccess> TxHalf<'_, R> {
    /// Check if a TX descriptor is available.
    pub fn has_tx_space(&self) -> bool {
        let tindex = self.regs.read_reg(regs::TDT) as usize;
        (self.tx.ring()[tindex].status & TXD_STAT_DD) != 0
    }

    /// Transmit a packet (copies data into DMA buffer).
    pub fn transmit(&mut self, packet: &[u8]) {
        let tindex = self.regs.read_reg(regs::TDT) as usize;
        let bufs_vaddr = self.tx.bufs_vaddr;
        let ring = self.tx.ring_mut();

        if (ring[tindex].status & TXD_STAT_DD) == 0 {
            warn!("TX ring full, dropping packet");
            return;
        }

        let len = min(packet.len(), MBUF_SIZE);
        let buf_addr = bufs_vaddr + tindex * MBUF_SIZE;
        let buf = unsafe { core::slice::from_raw_parts_mut(buf_addr as *mut u8, len) };
        buf.copy_from_slice(&packet[..len]);

        ring[tindex].length = len as u16;
        ring[tindex].status = 0;
        ring[tindex].cmd = TXD_CMD_RS | TXD_CMD_EOP;

        self.regs
            .write_reg(regs::TDT, ((tindex + 1) % TX_RING_SIZE) as u32);
        self.regs.read_reg(regs::STAT);
        fence(Ordering::Release);
    }

    /// Transmit via zero-copy callback. The closure writes directly
    /// into the DMA buffer. `len` must be ≤ MBUF_SIZE.
    pub fn transmit_with<T>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> T) -> Option<T> {
        let tindex = self.regs.read_reg(regs::TDT) as usize;
        let bufs_vaddr = self.tx.bufs_vaddr;
        let ring = self.tx.ring_mut();

        if (ring[tindex].status & TXD_STAT_DD) == 0 {
            return None;
        }

        let len = min(len, MBUF_SIZE);
        let buf_addr = bufs_vaddr + tindex * MBUF_SIZE;
        let buf = unsafe { core::slice::from_raw_parts_mut(buf_addr as *mut u8, len) };
        let result = f(buf);

        ring[tindex].length = len as u16;
        ring[tindex].status = 0;
        ring[tindex].cmd = TXD_CMD_RS | TXD_CMD_EOP;

        self.regs
            .write_reg(regs::TDT, ((tindex + 1) % TX_RING_SIZE) as u32);
        self.regs.read_reg(regs::STAT);
        fence(Ordering::Release);

        Some(result)
    }
}
