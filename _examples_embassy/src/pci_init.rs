#![allow(dead_code)]

use log::*;
use x86_64::instructions::port::Port;

const PCI_CONFIG_ADDR: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;

const E1000_VENDOR: u16 = 0x8086;
const E1000_DEVICE_IDS: &[u16] = &[0x100E, 0x100F, 0x10D3];

const PCI_COMMAND: u8 = 0x04;
const PCI_BAR0: u8 = 0x10;

/// Result of PCI scan: physical BAR0 address of the e1000 device.
pub struct E1000PciInfo {
    pub bar0_phys: u64,
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
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

/// Scan PCI bus 0 for an Intel e1000 NIC. Returns BAR0 physical address.
pub fn pci_find_e1000() -> Option<E1000PciInfo> {
    for dev in 0..32u8 {
        let id = pci_read32(0, dev, 0, 0);
        if id == 0xFFFF_FFFF {
            continue;
        }
        let vendor = (id & 0xFFFF) as u16;
        let device = ((id >> 16) & 0xFFFF) as u16;

        if vendor == E1000_VENDOR && E1000_DEVICE_IDS.contains(&device) {
            info!(
                "PCI: found e1000 at 0:{}:0 — vendor={:#06x} device={:#06x}",
                dev, vendor, device
            );

            // Enable bus mastering + memory space + I/O space
            let cmd = pci_read16(0, dev, 0, PCI_COMMAND);
            pci_write16(0, dev, 0, PCI_COMMAND, cmd | 0x07);
            let cmd_readback = pci_read16(0, dev, 0, PCI_COMMAND);
            info!(
                "PCI: command register: {:#06x} -> {:#06x}",
                cmd, cmd_readback
            );

            // Read BAR0 (memory-mapped registers)
            let bar0_raw = pci_read32(0, dev, 0, PCI_BAR0);
            let bar0_phys = (bar0_raw & !0xF) as u64; // mask type bits

            info!("PCI: e1000 BAR0 = {:#010x}", bar0_phys);

            return Some(E1000PciInfo {
                bar0_phys,
                bus: 0,
                dev,
                func: 0,
            });
        }
    }
    None
}

/// Re-enable bus mastering for the e1000 device (call after device reset).
pub fn pci_enable_bus_mastering(dev: u8) {
    let cmd = pci_read16(0, dev, 0, PCI_COMMAND);
    pci_write16(0, dev, 0, PCI_COMMAND, cmd | 0x07); // IO + MEM + Bus Master
    let readback = pci_read16(0, dev, 0, PCI_COMMAND);
    info!("PCI: bus mastering re-enabled: cmd={:#06x}", readback);
}

/// Read the PCI command register.
pub fn pci_read_command(dev: u8) -> u16 {
    pci_read16(0, dev, 0, PCI_COMMAND)
}
