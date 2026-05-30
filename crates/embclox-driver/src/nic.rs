//! Dyn-safe network device trait + `DynNic` adapter back to
//! `embassy_net_driver::Driver`.
//!
//! `embassy_net_driver::Driver` has GAT-typed RX/TX tokens and is not
//! dyn-compatible. The registry therefore returns `Box<dyn EmbcloxNic>`
//! using a callback-based shape, and a single `DynNic` newtype bridges
//! back to the upstream trait that `embassy_net::Stack` consumes.
//!
//! See `docs/design/driver-model.md` section "`EmbcloxNic`".

use alloc::boxed::Box;
use core::task::{Context, Waker};
use embassy_net_driver::{Capabilities, Driver, HardwareAddress, LinkState};

/// Dyn-safe network device interface.
///
/// Splits the embassy `Driver` API into peek-then-consume halves so the
/// trait can be `?Sized` / object-safe: readiness is reported via
/// `rx_ready`/`tx_ready`, the actual packet copy happens in
/// `recv_with`/`transmit_with`. Each in-tree NIC's existing
/// `embassy_net_driver::Driver` impl already follows this split
/// internally — the `EmbcloxNic` impl is a thin forward.
pub trait EmbcloxNic: Send {
    /// True if at least one received packet is buffered.
    fn rx_ready(&mut self) -> bool;

    /// True if there is room in the TX ring for at least one packet.
    fn tx_ready(&mut self) -> bool;

    /// Register `waker` to be woken when [`Self::rx_ready`] /
    /// [`Self::tx_ready`] may transition true. Single-waker drivers
    /// (the existing `AtomicWaker` pattern) overwrite previous
    /// registrations.
    fn register_waker(&mut self, waker: &Waker);

    /// Deliver one received packet. `rx_ready()` must have been observed
    /// `true`; panics otherwise.
    fn recv_with(&mut self, f: &mut dyn FnMut(&mut [u8]));

    /// Transmit one packet of length `len`. `tx_ready()` must have been
    /// observed `true`; panics otherwise.
    fn transmit_with(&mut self, len: usize, f: &mut dyn FnMut(&mut [u8]));

    fn link_state(&mut self, cx: &mut Context<'_>) -> LinkState;
    fn capabilities(&self) -> Capabilities;
    fn hardware_address(&self) -> HardwareAddress;
}

/// Adapter from `Box<dyn EmbcloxNic>` to `embassy_net_driver::Driver`.
/// Hand this to `embassy_net::Stack::new`.
pub struct DynNic(pub Box<dyn EmbcloxNic>);

impl Driver for DynNic {
    type RxToken<'a>
        = DynRxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = DynTxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, cx: &mut Context<'_>) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.0.rx_ready() && self.0.tx_ready() {
            let nic: *mut dyn EmbcloxNic = &mut *self.0;
            return Some((DynRxToken { nic }, DynTxToken { nic }));
        }
        self.0.register_waker(cx.waker());
        None
    }

    fn transmit(&mut self, cx: &mut Context<'_>) -> Option<Self::TxToken<'_>> {
        if self.0.tx_ready() {
            let nic: *mut dyn EmbcloxNic = &mut *self.0;
            return Some(DynTxToken { nic });
        }
        self.0.register_waker(cx.waker());
        None
    }

    fn link_state(&mut self, cx: &mut Context<'_>) -> LinkState {
        self.0.link_state(cx)
    }

    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    fn hardware_address(&self) -> HardwareAddress {
        self.0.hardware_address()
    }
}

/// RX token. Aliases the NIC through a raw pointer so it can be
/// returned alongside a `DynTxToken` from `receive` (the borrow
/// checker can't see that the two consume calls run sequentially
/// during a single smoltcp poll).
///
/// # Safety
/// The pointer is valid for `'a` because both tokens are constructed
/// from the `&mut self` borrow held by `DynNic::receive`/`transmit`.
/// smoltcp consumes the tokens before returning control to the
/// embassy executor, so accesses through the pointer are
/// single-threaded and non-overlapping in time.
pub struct DynRxToken<'a> {
    nic: *mut (dyn EmbcloxNic + 'a),
}

// Safety: see struct doc.
unsafe impl<'a> Send for DynRxToken<'a> {}

impl<'a> embassy_net_driver::RxToken for DynRxToken<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, f: F) -> R {
        let mut result: Option<R> = None;
        let mut f_holder = Some(f);
        let mut cb = |buf: &mut [u8]| {
            let f = f_holder.take().unwrap();
            result = Some(f(buf));
        };
        // SAFETY: see DynRxToken doc.
        let nic = unsafe { &mut *self.nic };
        nic.recv_with(&mut cb);
        result.expect("recv_with returned without invoking the callback")
    }
}

pub struct DynTxToken<'a> {
    nic: *mut (dyn EmbcloxNic + 'a),
}

// Safety: see DynRxToken.
unsafe impl<'a> Send for DynTxToken<'a> {}

impl<'a> embassy_net_driver::TxToken for DynTxToken<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut result: Option<R> = None;
        let mut f_holder = Some(f);
        let mut cb = |buf: &mut [u8]| {
            let f = f_holder.take().unwrap();
            result = Some(f(buf));
        };
        // SAFETY: see DynRxToken doc.
        let nic = unsafe { &mut *self.nic };
        nic.transmit_with(len, &mut cb);
        result.expect("transmit_with returned without invoking the callback")
    }
}
