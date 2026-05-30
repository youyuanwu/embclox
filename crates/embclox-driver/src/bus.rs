//! Bus abstraction.
//!
//! `PciBus` and `VmBus` already exist as concrete types in
//! `embclox-hal-x86` and `embclox-hyperv` respectively. This module
//! exposes the small `Bus` trait the registry uses for uniform
//! enumeration, plus a `VmBusEnum` newtype wrapper so we can hold an
//! `Option<VmBusEnum>` (Hyper-V detection may fail at boot, see the
//! design doc's "Bus detection" section).

use alloc::vec::Vec;
use embclox_hal_x86::pci::{PciBus, PciDevice};
use embclox_hyperv::{ChannelOffer, VmBus};

/// A bus that knows how to enumerate its attached devices.
pub trait Bus {
    type Device: Clone;
    /// Returns a freshly enumerated list of devices.
    ///
    /// Returning `Vec` (not `&[T]`) keeps the trait dyn-friendly and
    /// avoids requiring buses to cache their enumeration; today both
    /// concrete buses (`PciBus`, `VmBus`) build their list at boot and
    /// the cost is one-shot.
    fn enumerate(&self) -> Vec<Self::Device>;
}

impl Bus for PciBus {
    type Device = PciDevice;
    fn enumerate(&self) -> Vec<PciDevice> {
        PciBus::enumerate(self)
    }
}

/// Newtype around `VmBus` so we can implement `Bus` without orphan-rule
/// gymnastics. Wraps the live `VmBus` returned by
/// `embclox_hyperv::init`.
pub struct VmBusEnum<'a>(pub &'a VmBus);

impl<'a> Bus for VmBusEnum<'a> {
    type Device = ChannelOffer;
    fn enumerate(&self) -> Vec<ChannelOffer> {
        self.0.offers().to_vec()
    }
}
