//! `register_default_drivers` + `register_named_driver` helpers.

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

/// Register exactly one driver by `name()` (the same string each
/// driver's `PciDriver::name` / `VmBusDriver::name` returns).
///
/// Used by the `nic=<name>` cmdline filter in `examples-kernel`: when
/// set, the kernel skips `register_default_drivers` and calls this
/// instead so the registry contains only the matching driver. Probe
/// loop then matches and brings up that one NIC, ignoring everything
/// else on the bus.
///
/// Unknown names register nothing (the kernel will panic with a
/// `no NIC matched nic=<name>` diagnostic after `probe_all` returns
/// empty).
pub fn register_named_driver(registry: &mut DriverRegistry, name: &str) {
    match name {
        "e1000" => registry.register_pci(E1000Driver),
        "tulip" => registry.register_pci(TulipDriver),
        "netvsc" => registry.register_vmbus(NetvscDriver),
        other => {
            log::warn!("driver: nic={other} did not match any in-tree driver");
        }
    }
}
