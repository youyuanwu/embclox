use alloc::{boxed::Box, vec};
use log::*;

use crate::{print, println};

pub struct Kernfn;
impl e1000_driver::e1000::KernelFunc for Kernfn {
    const PAGE_SIZE: usize = 1 << 12;

    fn dma_alloc_coherent(&mut self, pages: usize) -> (usize, usize) {
        let paddr: Box<[u32]> = if pages == 1 {
            Box::new([0; 1024]) // 4096
        } else if pages == 8 {
            Box::new([0; 1024 * 8]) // 4096
        } else {
            info!("Alloc {} pages failed", pages);
            Box::new([0; 1024])
        };

        let len = paddr.len();

        let paddr = Box::into_raw(paddr) as *const u32 as usize;
        let vaddr = paddr;
        println!("alloc paddr: {:#x}, len={}", paddr, len);

        (vaddr, paddr)
    }

    fn dma_free_coherent(&mut self, vaddr: usize, pages: usize) {
        trace!("dealloc_dma {} @ {:#x} unimplemented!", pages, vaddr);
    }
}

pub fn e1000_init() {
    e1000_driver::pci::pci_init();

    let mut e1000_device = e1000_driver::e1000::E1000Device::<Kernfn>::new(
        Kernfn,
        e1000_driver::pci::E1000_REGS as usize,
    )
    .unwrap();

    // MAC 52:54:00:12:34:56
    let ping_frame: Box<[u8]> = Box::new([
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x52, 0x54, 0x00, 0x12, 0x34, 0x56, 0x08, 0x06, 0x00,
        0x01, 0x08, 0x00, 0x06, 0x04, 0x00, 0x01, 0x52, 0x54, 0x00, 0x12, 0x34, 0x56, 0x0a, 0x00,
        0x02, 0x0f, //10.0.2.15
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x02, 0x02,
    ]); //ping 10.0.2.2

    msdelay(500);
    e1000_device.e1000_transmit(&ping_frame);
    e1000_device.e1000_transmit(&ping_frame);
    e1000_device.e1000_transmit(&ping_frame);
    e1000_device.e1000_transmit(&ping_frame);

    let mut c = 12;
    loop {
        let rx_buf = e1000_device.e1000_recv();
        if let Some(vecdeque) = rx_buf {
            debug!("e1000 recv num {}", vecdeque.len());
            for v in vecdeque.iter() {
                print_hex_dump(v, v.len());
            }
        }
        c -= 1;
        if c <= 0 {
            break;
        }
        msdelay(100);
    }
}

pub fn print_hex_dump(buf: &[u8], len: usize) {
    //let mut linebuf: [char; 16] = [0 as char; 16];

    use alloc::string::String;
    let mut linebuf = String::with_capacity(32);
    let buf_len = buf.len();

    for i in 0..len {
        if (i % 16) == 0 {
            print!("\t{:?}\nHEX DUMP: ", linebuf);
            //linebuf.fill(0 as char);
            linebuf.clear();
        }

        if i >= buf_len {
            print!(" {:02x}", 0);
        } else {
            print!(" {:02x}", buf[i]);
            //linebuf[i%16] = buf[i] as char;
            linebuf.push(buf[i] as char);
        }
    }
    print!("\t{:?}\n", linebuf);
}

pub fn get_cycle() -> u64 {
    use core::arch::asm;
    let mut cycle: u64 = 0;
    unsafe {
        asm!("csrr {}, time", out(reg) cycle);
    }
    cycle
}

// qemu
pub const TIMER_CLOCK: u64 = 10000000;

// 微秒(us)
pub fn usdelay(us: u64) {
    let mut t1: u64 = get_cycle();
    let t2 = t1 + us * (TIMER_CLOCK / 1000000);

    while t2 >= t1 {
        t1 = get_cycle();
    }
}

// 毫秒(ms)
#[allow(unused)]
pub fn msdelay(ms: u64) {
    usdelay(ms * 1000);
}