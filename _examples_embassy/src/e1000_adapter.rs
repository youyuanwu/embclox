use core::cell::UnsafeCell;
use core::task::Context;
use embassy_net_driver::{Capabilities, HardwareAddress, LinkState};

use crate::dma_alloc::BootDmaAllocator;
use crate::mmio_regs::MmioRegs;

type Dev = e1000::E1000Device<MmioRegs, BootDmaAllocator>;

pub struct E1000Embassy {
    // UnsafeCell needed because Driver::receive() returns both RxToken
    // and TxToken from &mut self. Safe: smoltcp consumes sequentially.
    device: UnsafeCell<Dev>,
    mac: [u8; 6],
}

// Safety: single-core, no preemption, smoltcp uses tokens sequentially.
unsafe impl Send for E1000Embassy {}

impl E1000Embassy {
    pub fn new(device: Dev, mac: [u8; 6]) -> Self {
        Self {
            device: UnsafeCell::new(device),
            mac,
        }
    }

    #[allow(clippy::mut_from_ref)]
    fn dev_mut(&self) -> &mut Dev {
        unsafe { &mut *self.device.get() }
    }
}

impl embassy_net_driver::Driver for E1000Embassy {
    type RxToken<'a>
        = RxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, cx: &mut Context) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let (rx, tx) = self.dev_mut().split();
        if rx.has_rx_packet() && tx.has_tx_space() {
            return Some((RxToken { parent: self }, TxToken { parent: self }));
        }
        cx.waker().wake_by_ref();
        None
    }

    fn transmit(&mut self, cx: &mut Context) -> Option<Self::TxToken<'_>> {
        let (_, tx) = self.dev_mut().split();
        if tx.has_tx_space() {
            return Some(TxToken { parent: self });
        }
        cx.waker().wake_by_ref();
        None
    }

    fn link_state(&mut self, cx: &mut Context) -> LinkState {
        cx.waker().wake_by_ref();
        LinkState::Up
    }

    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::default();
        caps.max_transmission_unit = 1514;
        caps
    }

    fn hardware_address(&self) -> HardwareAddress {
        HardwareAddress::Ethernet(self.mac)
    }
}

pub struct RxToken<'a> {
    parent: &'a E1000Embassy,
}

impl<'a> embassy_net_driver::RxToken for RxToken<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, f: F) -> R {
        let (mut rx, _) = self.parent.dev_mut().split();
        rx.recv_with(f).expect("packet was ready in receive()")
    }
}

pub struct TxToken<'a> {
    parent: &'a E1000Embassy,
}

impl<'a> embassy_net_driver::TxToken for TxToken<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let (_, mut tx) = self.parent.dev_mut().split();
        tx.transmit_with(len, f).expect("tx space was available")
    }
}
