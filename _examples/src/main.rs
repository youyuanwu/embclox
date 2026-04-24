#![no_std]
#![no_main]
//#![deny(warnings)]
#![allow(unused_variables)]
#![allow(dead_code)]

extern crate alloc;
extern crate device_tree;
extern crate lazy_static;
extern crate log;
extern crate pci;

mod boot;
mod e1000;
mod pci_impl;

use alloc::{boxed::Box, format, vec, vec::Vec};
use device_tree::util::SliceRead;
use device_tree::{DeviceTree, Node};
pub use log::*;
use pci::{scan_bus, Location, PCIDevice, BAR};
use pci_impl::*;

use crate::boot::{init_heap, logger};
use crate::e1000::Kernfn;

#[no_mangle]
extern "C" fn rust_main(_hartid: usize, device_tree_paddr: usize) {
    println!("\nHi\nTEST START");

    logger::init("DEBUG");

    info!("log initialized");

    init_heap();

    //init_dt(device_tree_paddr);

    e1000::e1000_init();

    println!("TEST END");
    boot::lang_items::abort();
}

fn init_dt(dtb: usize) {
    info!("device tree @ {:#x}", dtb);
    #[repr(C)]
    struct DtbHeader {
        be_magic: u32,
        be_size: u32,
    }
    let header = unsafe { &*(dtb as *const DtbHeader) };
    let magic = u32::from_be(header.be_magic);
    const DEVICE_TREE_MAGIC: u32 = 0xd00dfeed;
    assert_eq!(magic, DEVICE_TREE_MAGIC);
    let size = u32::from_be(header.be_size);
    let dtb_data = unsafe { core::slice::from_raw_parts(dtb as *const u8, size as usize) };
    let dt = DeviceTree::load(dtb_data).expect("failed to parse device tree");
    walk_dt_node(&dt.root);
}

fn walk_dt_node(dt: &Node) {
    if let Ok(compatible) = dt.prop_str("compatible") {
        if compatible == "pci-host-ecam-generic" || compatible == "sifive,fu740-pcie" {
            if let Some(reg) = dt.prop_raw("reg") {
                let paddr = reg.as_slice().read_be_u64(0).unwrap_or(0);
                let size = reg
                    .as_slice()
                    .read_be_u64(2 * core::mem::size_of::<u32>())
                    .unwrap_or(0);

                let address_cells = dt.prop_u32("#address-cells").unwrap_or(0) as usize;
                let size_cells = dt.prop_u32("#size-cells").unwrap_or(0) as usize;
                let ranges = dt.prop_cells("ranges").unwrap();
                info!(
                    "pci ranges: bus_addr@[{:x?}], cpu_paddr@[{:x?}], size@[{:x?}]",
                    ranges[0]..ranges[address_cells - 1],
                    ranges[address_cells]..ranges[address_cells + 2 - 1],
                    ranges[address_cells + 2]..ranges[address_cells + 2 + size_cells - 1]
                );

                info!("{:?} addr={:#x}, size={:#x}", compatible, paddr, size);
                pci_scan().unwrap();
            }
        }

        if compatible == "virtio,mmio" {
            if let Some(reg) = dt.prop_raw("reg") {
                let paddr = reg.as_slice().read_be_u64(0).unwrap_or(0);
                let size = reg
                    .as_slice()
                    .read_be_u64(2 * core::mem::size_of::<u32>())
                    .unwrap_or(0);

                info!(
                    "walk dt: {}, addr={:#x}, size={:#x}",
                    compatible, paddr, size
                );
                //virtio_probe(paddr, size);
            }
        }
    }
    for child in dt.children.iter() {
        walk_dt_node(child);
    }
}

pub fn pci_scan() -> Option<u32> {
    let mut dev_list = Vec::new();
    let pci_iter = unsafe { scan_bus(&PortOpsImpl, PCI_ACCESS) };
    info!("--------- PCI bus:device:function ---------");
    for dev in pci_iter {
        info!(
            "PCI: {}:{}:{} {:04x}:{:04x} ({} {}) irq: {}:{:?}",
            dev.loc.bus,
            dev.loc.device,
            dev.loc.function,
            dev.id.vendor_id,
            dev.id.device_id,
            dev.id.class,
            dev.id.subclass,
            dev.pic_interrupt_line,
            dev.interrupt_pin,
        );
        init_driver(&dev);
        dev_list.push(dev.loc);
    }
    info!("---------");

    let pci_num = dev_list.len();

    info!("Found PCI number is {}", pci_num);
    Some(pci_num as u32)
}

pub fn init_driver(dev: &PCIDevice) {
    let name = format!("enp{}s{}f{}", dev.loc.bus, dev.loc.device, dev.loc.function);
    match (dev.id.vendor_id, dev.id.device_id) {
        (0x8086, 0x100e) | (0x8086, 0x100f) | (0x8086, 0x10d3) => {
            if let Some(BAR::Memory(addr, _len, _, _)) = dev.bars[0] {
                info!(
                    "Found Intel E1000 {:?} dev {:?} BAR0 {:#x?}",
                    name, dev, addr
                );
                #[cfg(target_arch = "riscv64")]
                let addr = if addr == 0 { E1000_BASE as u64 } else { addr };

                let irq = unsafe { enable(dev.loc, addr) };
                let vaddr = addr as usize;

                let mut e1000_device =
                    e1000_driver::e1000::E1000Device::<Kernfn>::new(Kernfn, vaddr).unwrap();

                // e1000_device.e1000_transmit(&ping_frame);
                // let rx_buf = e1000_device.e1000_recv();
            }
        }

        _ => {}
    }
    if dev.id.class == 0x01 && dev.id.subclass == 0x06 {
        // Mass storage class
        // SATA subclass
        if let Some(BAR::Memory(addr, _len, _, _)) = dev.bars[5] {
            info!("Found AHCI dev {:?} BAR5 {:x?}", dev, addr);
        }
    }
}

/// Enable the pci device and its interrupt
/// Return assigned MSI interrupt number when applicable
unsafe fn enable(loc: Location, paddr: u64) -> Option<usize> {
    let ops = &PortOpsImpl;
    let am = PCI_ACCESS;

    if paddr != 0 {
        // reveal PCI regs by setting paddr
        let bar0_raw = am.read32(ops, loc, BAR0);
        am.write32(ops, loc, BAR0, (paddr & !0xfff) as u32); //Only for 32-bit decoding
        debug!(
            "BAR0 set from {:#x} to {:#x}",
            bar0_raw,
            am.read32(ops, loc, BAR0)
        );
    }

    // 23 and lower are used
    static mut MSI_IRQ: u32 = 23;

    let _orig = am.read16(ops, loc, PCI_COMMAND);
    // IO Space | MEM Space | Bus Mastering | Special Cycles | PCI Interrupt Disable
    // am.write32(ops, loc, PCI_COMMAND, (orig | 0x40f) as u32);

    // find MSI cap
    let mut msi_found = false;
    let mut cap_ptr = am.read8(ops, loc, PCI_CAP_PTR) as u16;
    let mut assigned_irq = None;
    while cap_ptr > 0 {
        let cap_id = am.read8(ops, loc, cap_ptr);
        if cap_id == PCI_CAP_ID_MSI {
            let orig_ctrl = am.read32(ops, loc, cap_ptr + PCI_MSI_CTRL_CAP);
            // The manual Volume 3 Chapter 10.11 Message Signalled Interrupts
            // 0 is (usually) the apic id of the bsp.
            //am.write32(ops, loc, cap_ptr + PCI_MSI_ADDR, 0xfee00000 | (0 << 12));
            am.write32(ops, loc, cap_ptr + PCI_MSI_ADDR, 0xfee00000);
            MSI_IRQ += 1;
            let irq = MSI_IRQ;
            assigned_irq = Some(irq as usize);
            // we offset all our irq numbers by 32
            if (orig_ctrl >> 16) & (1 << 7) != 0 {
                // 64bit
                am.write32(ops, loc, cap_ptr + PCI_MSI_DATA_64, irq + 32);
            } else {
                // 32bit
                am.write32(ops, loc, cap_ptr + PCI_MSI_DATA_32, irq + 32);
            }

            // enable MSI interrupt, assuming 64bit for now
            am.write32(ops, loc, cap_ptr + PCI_MSI_CTRL_CAP, orig_ctrl | 0x10000);
            debug!(
                "MSI control {:#b}, enabling MSI interrupt {}",
                orig_ctrl >> 16,
                irq
            );
            msi_found = true;
        }
        debug!("PCI device has cap id {} at {:#X}", cap_id, cap_ptr);
        cap_ptr = am.read8(ops, loc, cap_ptr + 1) as u16;
    }

    if !msi_found {
        // am.write16(ops, loc, PCI_COMMAND, (0x2) as u16);
        am.write16(ops, loc, PCI_COMMAND, 0x6);
        am.write32(ops, loc, PCI_INTERRUPT_LINE, 33);
        debug!("MSI not found, using PCI interrupt");
    }

    debug!("pci device enable done");
    assigned_irq
}
