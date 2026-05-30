//! Per-NIC driver implementations.
//!
//! Each module exposes:
//! - A marker `*Driver` type implementing [`crate::PciDriver`] or
//!   [`crate::VmBusDriver`].
//! - A private `Nic*` struct implementing [`crate::EmbcloxNic`].
//! - A `static extern "x86-interrupt" fn` ISR plus the
//!   driver-private static slot it reads.
//!
//! Caveat: the static-slot pattern caps each driver type at one
//! probed instance per boot. See the design doc's "Multi-instance
//! limitation" section.

pub mod e1000;
pub mod netvsc;
pub mod tulip;

pub use e1000::E1000Driver;
pub use netvsc::NetvscDriver;
pub use tulip::TulipDriver;
