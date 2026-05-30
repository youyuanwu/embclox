use alloc::vec::Vec;
use log::*;
use x86_64::instructions::port::Port;

const PCI_CONFIG_ADDR: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;
const PCI_COMMAND: u8 = 0x04;
const PCI_HEADER_TYPE: u8 = 0x0E;
const PCI_CLASS_REV: u8 = 0x08;

/// x86 PCI bus scanner using I/O ports 0xCF8/0xCFC.
pub struct PciBus;

/// A PCI device found during bus enumeration.
#[derive(Debug, Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    /// PCI class code (high byte of register 0x08+3).
    pub class: u8,
    /// PCI subclass (next byte down).
    pub subclass: u8,
}

impl PciBus {
    /// Find a PCI device by vendor and device ID on bus 0.
    pub fn find_device(&self, vendor: u16, device_id: u16) -> Option<PciDevice> {
        for dev in 0..32u8 {
            let id = pci_read32(0, dev, 0, 0);
            if id == 0xFFFF_FFFF {
                continue;
            }
            let v = (id & 0xFFFF) as u16;
            let d = ((id >> 16) & 0xFFFF) as u16;
            if v == vendor && d == device_id {
                info!("PCI: found {:04x}:{:04x} at 0:{}:0", v, d, dev);
                let (class, subclass) = read_class(0, dev, 0);
                return Some(PciDevice {
                    bus: 0,
                    dev,
                    func: 0,
                    vendor: v,
                    device: d,
                    class,
                    subclass,
                });
            }
        }
        None
    }

    /// Find a PCI device matching any of the given device IDs.
    pub fn find_device_any(&self, vendor: u16, device_ids: &[u16]) -> Option<PciDevice> {
        for dev in 0..32u8 {
            let id = pci_read32(0, dev, 0, 0);
            if id == 0xFFFF_FFFF {
                continue;
            }
            let v = (id & 0xFFFF) as u16;
            let d = ((id >> 16) & 0xFFFF) as u16;
            if v == vendor && device_ids.contains(&d) {
                info!("PCI: found {:04x}:{:04x} at 0:{}:0", v, d, dev);
                let (class, subclass) = read_class(0, dev, 0);
                return Some(PciDevice {
                    bus: 0,
                    dev,
                    func: 0,
                    vendor: v,
                    device: d,
                    class,
                    subclass,
                });
            }
        }
        None
    }

    /// Enumerate every PCI device reachable from bus 0.
    ///
    /// Walks bus 0 device 0..32, descending into functions 1..8 only on
    /// multi-function devices (header-type bit 7). Bus-to-bus bridge
    /// traversal is not yet supported \u2014 enough for QEMU q35/pc and
    /// Hyper-V Gen1, which both keep all endpoint NICs on bus 0.
    pub fn enumerate(&self) -> Vec<PciDevice> {
        let mut out = Vec::new();
        for dev in 0..32u8 {
            let id0 = pci_read32(0, dev, 0, 0);
            if id0 == 0xFFFF_FFFF {
                continue;
            }
            push_dev(&mut out, 0, dev, 0, id0);
            // Multi-function device? Bit 7 of header type at offset 0x0E.
            let header = (pci_read32(0, dev, 0, PCI_HEADER_TYPE & 0xFC)
                >> ((PCI_HEADER_TYPE & 3) * 8)) as u8;
            if header & 0x80 != 0 {
                for func in 1..8u8 {
                    let idf = pci_read32(0, dev, func, 0);
                    if idf == 0xFFFF_FFFF {
                        continue;
                    }
                    push_dev(&mut out, 0, dev, func, idf);
                }
            }
        }
        out
    }

    /// Enable PCI bus mastering, memory space, and I/O space for a device.
    pub fn enable_bus_mastering(&self, dev: &PciDevice) {
        let cmd = pci_read16(dev.bus, dev.dev, dev.func, PCI_COMMAND);
        pci_write16(dev.bus, dev.dev, dev.func, PCI_COMMAND, cmd | 0x07);
        let readback = pci_read16(dev.bus, dev.dev, dev.func, PCI_COMMAND);
        info!("PCI: bus mastering enabled: cmd={:#06x}", readback);
    }

    /// Read a BAR (Base Address Register) value for a device.
    pub fn read_bar(&self, dev: &PciDevice, bar: u8) -> u64 {
        let offset = 0x10 + bar * 4;
        let raw = pci_read32(dev.bus, dev.dev, dev.func, offset);
        let bar_type = (raw >> 1) & 0x3;

        if bar_type == 0x2 {
            // 64-bit BAR: upper 32 bits in the next register
            let upper = pci_read32(dev.bus, dev.dev, dev.func, offset + 4);
            ((upper as u64) << 32) | ((raw & !0xF) as u64)
        } else {
            (raw & !0xF) as u64
        }
    }

    /// Read a 32-bit PCI configuration register.
    pub fn read_config(&self, dev: &PciDevice, offset: u8) -> u32 {
        pci_read32(dev.bus, dev.dev, dev.func, offset)
    }

    /// Write a 32-bit PCI configuration register.
    pub fn write_config(&self, dev: &PciDevice, offset: u8, val: u32) {
        pci_write32(dev.bus, dev.dev, dev.func, offset, val);
    }
}

fn pci_config_address(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    (1u32 << 31)
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC)
}

fn pci_read32(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    let addr = pci_config_address(bus, dev, func, offset);
    unsafe {
        Port::new(PCI_CONFIG_ADDR).write(addr);
        Port::<u32>::new(PCI_CONFIG_DATA).read()
    }
}

fn pci_write32(bus: u8, dev: u8, func: u8, offset: u8, val: u32) {
    let addr = pci_config_address(bus, dev, func, offset);
    unsafe {
        Port::new(PCI_CONFIG_ADDR).write(addr);
        Port::new(PCI_CONFIG_DATA).write(val);
    }
}

fn pci_read16(bus: u8, dev: u8, func: u8, offset: u8) -> u16 {
    let val = pci_read32(bus, dev, func, offset & 0xFC);
    ((val >> ((offset & 2) * 8)) & 0xFFFF) as u16
}

fn pci_write16(bus: u8, dev: u8, func: u8, offset: u8, val: u16) {
    let shift = (offset & 2) * 8;
    let old = pci_read32(bus, dev, func, offset & 0xFC);
    let mask = !(0xFFFFu32 << shift);
    let new = (old & mask) | ((val as u32) << shift);
    pci_write32(bus, dev, func, offset & 0xFC, new);
}

fn read_class(bus: u8, dev: u8, func: u8) -> (u8, u8) {
    let w = pci_read32(bus, dev, func, PCI_CLASS_REV);
    let class = ((w >> 24) & 0xFF) as u8;
    let subclass = ((w >> 16) & 0xFF) as u8;
    (class, subclass)
}

fn push_dev(out: &mut Vec<PciDevice>, bus: u8, dev: u8, func: u8, id: u32) {
    let vendor = (id & 0xFFFF) as u16;
    let device = ((id >> 16) & 0xFFFF) as u16;
    let (class, subclass) = read_class(bus, dev, func);
    info!(
        "PCI: enum {:04x}:{:04x} at {}:{}:{} class={:#04x}/{:#04x}",
        vendor, device, bus, dev, func, class, subclass
    );
    out.push(PciDevice {
        bus,
        dev,
        func,
        vendor,
        device,
        class,
        subclass,
    });
}
