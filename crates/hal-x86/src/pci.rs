use log::*;
use x86_64::instructions::port::Port;

const PCI_CONFIG_ADDR: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;
const PCI_COMMAND: u8 = 0x04;

pub struct PciBus;

pub struct PciDevice {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
}

impl PciBus {
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
                return Some(PciDevice {
                    bus: 0,
                    dev,
                    func: 0,
                    vendor: v,
                    device: d,
                });
            }
        }
        None
    }

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
                return Some(PciDevice {
                    bus: 0,
                    dev,
                    func: 0,
                    vendor: v,
                    device: d,
                });
            }
        }
        None
    }

    pub fn enable_bus_mastering(&self, dev: &PciDevice) {
        let cmd = pci_read16(dev.bus, dev.dev, dev.func, PCI_COMMAND);
        pci_write16(dev.bus, dev.dev, dev.func, PCI_COMMAND, cmd | 0x07);
        let readback = pci_read16(dev.bus, dev.dev, dev.func, PCI_COMMAND);
        info!("PCI: bus mastering enabled: cmd={:#06x}", readback);
    }

    pub fn read_bar(&self, dev: &PciDevice, bar: u8) -> u64 {
        let offset = 0x10 + bar * 4;
        let raw = pci_read32(dev.bus, dev.dev, dev.func, offset);
        (raw & !0xF) as u64
    }

    pub fn read_config(&self, dev: &PciDevice, offset: u8) -> u32 {
        pci_read32(dev.bus, dev.dev, dev.func, offset)
    }

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
