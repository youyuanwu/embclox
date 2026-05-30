//! Driver registry — owned `Vec<Box<dyn ...>>` of registered drivers,
//! plus the probe loop.

use crate::bus::{Bus, VmBusEnum};
use crate::driver::{PciDriver, ProbeCtx, ProbedNic, VmBusDriver};
use alloc::boxed::Box;
use alloc::vec::Vec;
use embclox_hal_x86::pci::PciBus;

/// Hard cap on number of NICs the registry will probe before bailing.
///
/// Bounded so a hostile or misconfigured PCI topology cannot exhaust
/// `BootDmaAllocator` via probe-and-discard (~1 MiB per probed NIC).
/// See the design doc's "Probe-then-discard cost" section.
pub const PROBE_BUDGET: usize = 4;

/// Registry of compiled-in drivers.
///
/// Built mutably during `kernel_main`, then borrowed shared for the
/// probe loop. Dropped after probing completes.
#[derive(Default)]
pub struct DriverRegistry {
    pci: Vec<Box<dyn PciDriver>>,
    vmbus: Vec<Box<dyn VmBusDriver>>,
}

impl DriverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a PCI driver. Boxes the driver internally so callers
    /// write `registry.register_pci(E1000Driver)` instead of
    /// `Box::new(E1000Driver)`.
    pub fn register_pci(&mut self, drv: impl PciDriver + 'static) {
        log::info!("driver: registered PCI '{}'", drv.name());
        self.pci.push(Box::new(drv));
    }

    pub fn register_vmbus(&mut self, drv: impl VmBusDriver + 'static) {
        log::info!("driver: registered VMBus '{}'", drv.name());
        self.vmbus.push(Box::new(drv));
    }

    pub fn pci(&self) -> &[Box<dyn PciDriver>] {
        &self.pci
    }

    pub fn vmbus(&self) -> &[Box<dyn VmBusDriver>] {
        &self.vmbus
    }
}

/// Run the full probe loop against `pci` and `ctx.vmbus`, returning
/// every NIC that probed successfully.
///
/// Probe failures are logged and the loop continues. The caller is
/// responsible for picking a primary (e.g. `min_by_key(|n| n.priority)`)
/// and panicking when the returned vec is empty.
pub fn probe_all(
    registry: &DriverRegistry,
    ctx: &mut ProbeCtx<'_>,
    pci: &PciBus,
) -> Vec<ProbedNic> {
    use alloc::collections::BTreeSet;

    let mut nics: Vec<ProbedNic> = Vec::new();
    let mut claimed: BTreeSet<(u8, u8, u8)> = BTreeSet::new();

    for dev in pci.enumerate() {
        if nics.len() >= PROBE_BUDGET {
            log::warn!(
                "probe: PROBE_BUDGET ({}) reached; stopping PCI scan",
                PROBE_BUDGET
            );
            break;
        }
        let key = (dev.bus, dev.dev, dev.func);
        if claimed.contains(&key) {
            continue;
        }
        for drv in registry.pci() {
            if drv.matches(&dev) {
                match drv.probe(dev, ctx) {
                    Ok(nic) => {
                        log::info!(
                            "probe: {} claimed {:04x}:{:04x} at {}:{}:{}",
                            drv.name(),
                            dev.vendor,
                            dev.device,
                            dev.bus,
                            dev.dev,
                            dev.func
                        );
                        claimed.insert(key);
                        nics.push(ProbedNic::new(nic, drv.priority(), drv.name()));
                        break;
                    }
                    Err(e) => log::warn!(
                        "probe: {} failed on {:04x}:{:04x}: {}",
                        drv.name(),
                        dev.vendor,
                        dev.device,
                        e
                    ),
                }
            }
        }
    }

    // Collect VMBus offers up-front (immutable borrow of vmbus) so the
    // probe loop body can re-borrow `ctx.vmbus` mutably for each probe.
    let offers: Vec<_> = match ctx.vmbus.as_deref() {
        Some(vmbus) => VmBusEnum(vmbus).enumerate(),
        None => Vec::new(),
    };
    for offer in offers {
        if nics.len() >= PROBE_BUDGET {
            log::warn!(
                "probe: PROBE_BUDGET ({}) reached; stopping VMBus scan",
                PROBE_BUDGET
            );
            break;
        }
        for drv in registry.vmbus() {
            if drv.matches(&offer) {
                match drv.probe(offer.clone(), ctx) {
                    Ok(nic) => {
                        log::info!(
                            "probe: {} claimed VMBus relid={}",
                            drv.name(),
                            offer.child_relid
                        );
                        nics.push(ProbedNic::new(nic, drv.priority(), drv.name()));
                        break;
                    }
                    Err(e) => log::warn!(
                        "probe: vmbus {} failed on relid={}: {}",
                        drv.name(),
                        offer.child_relid,
                        e
                    ),
                }
            }
        }
    }

    nics
}
