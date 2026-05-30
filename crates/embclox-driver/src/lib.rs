//! Bus / driver / device abstraction.
//!
//! See [`docs/design/driver-model.md`](../../../docs/design/driver-model.md)
//! for the design rationale.

#![no_std]
#![feature(abi_x86_interrupt)]

extern crate alloc;

pub mod bus;
pub mod defaults;
pub mod driver;
pub mod drivers;
pub mod error;
pub mod nic;
pub mod registry;

pub use bus::{Bus, VmBusEnum};
pub use defaults::{register_default_drivers, register_named_driver};
pub use driver::{PciDriver, ProbeCtx, ProbedNic, VmBusDriver};
pub use error::ProbeError;
pub use nic::{DynNic, EmbcloxNic};
pub use registry::{probe_all, DriverRegistry, PROBE_BUDGET};
