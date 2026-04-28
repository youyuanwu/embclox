//! Embassy `Driver` adapter for the NetVSC synthetic NIC.
//!
//! Wraps [`crate::netvsc::NetvscDevice`] and implements
//! [`embassy_net_driver::Driver`]. Wakes are delivered via the global
//! [`crate::netvsc::NETVSC_WAKER`] which the SynIC SINT2 ISR signals on
//! every channel event.
//!
//! Mirrors the pattern in `crates/embclox-core/src/tulip_embassy.rs` —
//! the only change is the underlying device and waker.

use crate::netvsc::{NetvscDevice, NETVSC_WAKER};
use core::cell::UnsafeCell;
use core::task::Context;
use embassy_net_driver::{Capabilities, HardwareAddress, LinkState};

/// Embassy network driver adapter for the Hyper-V synthetic NIC.
///
/// # Safety
/// Single-core only. The SynIC SINT2 ISR must only touch
/// [`NETVSC_WAKER`] (which is `AtomicWaker` and ISR-safe), never the
/// device itself.
pub struct NetvscEmbassy {
    device: UnsafeCell<NetvscDevice>,
    mac: [u8; 6],
    mtu: u32,
}

// Safety: single-core. ISR only touches AtomicWaker (not device).
unsafe impl Send for NetvscEmbassy {}

impl NetvscEmbassy {
    pub fn new(device: NetvscDevice) -> Self {
        let mac = device.mac();
        let mtu = device.mtu();
        Self {
            device: UnsafeCell::new(device),
            mac,
            mtu,
        }
    }

    #[allow(clippy::mut_from_ref)]
    fn dev_mut(&self) -> &mut NetvscDevice {
        unsafe { &mut *self.device.get() }
    }
}

impl embassy_net_driver::Driver for NetvscEmbassy {
    type RxToken<'a>
        = RxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, cx: &mut Context) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let dev = self.dev_mut();
        if dev.has_rx_packet() && dev.has_tx_space() {
            return Some((RxToken { parent: self }, TxToken { parent: self }));
        }
        NETVSC_WAKER.register(cx.waker());
        None
    }

    fn transmit(&mut self, cx: &mut Context) -> Option<Self::TxToken<'_>> {
        if self.dev_mut().has_tx_space() {
            return Some(TxToken { parent: self });
        }
        NETVSC_WAKER.register(cx.waker());
        None
    }

    fn link_state(&mut self, cx: &mut Context) -> LinkState {
        NETVSC_WAKER.register(cx.waker());
        // We don't track real link state from the host; once NetVSC init
        // succeeded the link is treated as up. NDIS_INDICATE_STATUS would
        // be the proper signal if/when we wire it up.
        LinkState::Up
    }

    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::default();
        caps.max_transmission_unit = self.mtu as usize;
        caps
    }

    fn hardware_address(&self) -> HardwareAddress {
        HardwareAddress::Ethernet(self.mac)
    }
}

pub struct RxToken<'a> {
    parent: &'a NetvscEmbassy,
}

impl<'a> embassy_net_driver::RxToken for RxToken<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, f: F) -> R {
        self.parent
            .dev_mut()
            .recv_with(f)
            .expect("RxToken issued without a buffered frame")
    }
}

pub struct TxToken<'a> {
    parent: &'a NetvscEmbassy,
}

impl<'a> embassy_net_driver::TxToken for TxToken<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        self.parent
            .dev_mut()
            .transmit_with(len, f)
            .expect("TxToken issued without TX space")
    }
}
