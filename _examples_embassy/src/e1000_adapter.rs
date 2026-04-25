use core::cell::UnsafeCell;
use core::task::Context;
use embassy_net_driver::{Capabilities, HardwareAddress, LinkState};

use crate::kernfn::Kernfn;

type E1000Dev = e1000_driver::e1000::E1000Device<'static, Kernfn>;

pub struct E1000Embassy {
    // UnsafeCell because Driver::receive() must hand out both RxToken
    // and TxToken, each needing &mut access. This is safe because smoltcp
    // consumes RxToken before TxToken (sequential, not concurrent).
    device: UnsafeCell<E1000Dev>,
    mac: [u8; 6],
}

// Safety: single-core, no preemption, smoltcp uses tokens sequentially.
unsafe impl Send for E1000Embassy {}

impl E1000Embassy {
    pub fn new(device: E1000Dev, mac: [u8; 6]) -> Self {
        Self {
            device: UnsafeCell::new(device),
            mac,
        }
    }

    fn dev(&self) -> &E1000Dev {
        unsafe { &*self.device.get() }
    }

    #[allow(clippy::mut_from_ref)]
    fn dev_mut(&self) -> &mut E1000Dev {
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
        if self.dev().has_rx_packet() && self.dev().has_tx_space() {
            return Some((RxToken { parent: self }, TxToken { parent: self }));
        }
        // Wake immediately so the executor re-polls us (busy-poll mode)
        cx.waker().wake_by_ref();
        None
    }

    fn transmit(&mut self, cx: &mut Context) -> Option<Self::TxToken<'_>> {
        if self.dev().has_tx_space() {
            return Some(TxToken { parent: self });
        }
        cx.waker().wake_by_ref();
        None
    }

    fn link_state(&mut self, cx: &mut Context) -> LinkState {
        // Wake so runner keeps polling
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
        self.parent
            .dev_mut()
            .e1000_recv_with(f)
            .expect("packet was ready in receive()")
    }
}

pub struct TxToken<'a> {
    parent: &'a E1000Embassy,
}

impl<'a> embassy_net_driver::TxToken for TxToken<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = [0u8; 1514];
        let result = f(&mut buf[..len]);
        self.parent.dev_mut().e1000_transmit(&buf[..len]);
        result
    }
}
