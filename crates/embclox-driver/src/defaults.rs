//! `register_default_drivers` helper.

use crate::drivers::{E1000Driver, NetvscDriver, TulipDriver};
use crate::registry::DriverRegistry;

/// Register every in-tree driver with the registry. Applications that
/// want a custom subset should call `registry.register_pci()` /
/// `registry.register_vmbus()` directly instead.
///
/// Forgetting to add a new in-tree driver here is a runtime defect
/// (no NICs detected); see the design doc's "T-3: Driver registration"
/// trade-off for CI mitigation.
pub fn register_default_drivers(registry: &mut DriverRegistry) {
    registry.register_pci(E1000Driver);
    registry.register_pci(TulipDriver);
    registry.register_vmbus(NetvscDriver);
}
