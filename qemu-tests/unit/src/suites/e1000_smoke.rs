use crate::dma_alloc::BootDmaAllocator;
use crate::harness::TestCase;
use crate::mmio_regs::MmioRegs;
use embclox_e1000::RegisterAccess;
use embclox_e1000::regs::*;

/// Global test context set by main before running suites.
static mut CTX: Option<E1000TestCtx> = None;

pub struct E1000TestCtx {
    pub regs: MmioRegs,
    pub kernel_offset: u64,
    pub phys_offset: u64,
}

/// Initialize the e1000 test context. Called once from main.
///
/// # Safety
/// Must be called before `suite()` and only from single-threaded init.
pub unsafe fn init(regs: MmioRegs, kernel_offset: u64, phys_offset: u64) {
    unsafe {
        *core::ptr::addr_of_mut!(CTX) = Some(E1000TestCtx {
            regs,
            kernel_offset,
            phys_offset,
        });
    }
}

fn ctx() -> &'static E1000TestCtx {
    unsafe {
        (*core::ptr::addr_of!(CTX))
            .as_ref()
            .expect("e1000 test context not initialized")
    }
}

pub fn suite() -> (&'static str, &'static [TestCase]) {
    (
        "e1000_smoke",
        &[
            TestCase {
                name: "status_link_up",
                func: test_status_link_up,
            },
            TestCase {
                name: "mac_address_nonzero",
                func: test_mac_address_nonzero,
            },
            TestCase {
                name: "init_device_and_split",
                func: test_init_device_and_split,
            },
        ],
    )
}

fn test_status_link_up() {
    let regs = &ctx().regs;
    let status = regs.read_reg(STAT);
    // Bit 1 = link up
    assert!(
        status & 0x2 != 0,
        "e1000 link should be up, STATUS={:#x}",
        status
    );
}

fn test_mac_address_nonzero() {
    let regs = &ctx().regs;
    let ral = regs.read_reg(RAL);
    let rah = regs.read_reg(RAH);
    assert!(ral != 0 || rah != 0, "MAC address should not be zero");
}

fn test_init_device_and_split() {
    let c = ctx();
    let dma = BootDmaAllocator {
        kernel_offset: c.kernel_offset,
        phys_offset: c.phys_offset,
    };
    let mut dev = embclox_e1000::E1000Device::new(c.regs, dma);
    let mac = dev.mac_address();
    assert!(mac != [0; 6], "MAC should not be all zeros");

    // Test split
    let (mut rx, mut tx) = dev.split();
    // RX should have no packets pending (no traffic sent)
    let received = rx.recv_with(|_data| {
        panic!("should not receive any packet");
    });
    assert!(received.is_none(), "no packets should be pending");

    // TX: transmit a small frame (will be sent to QEMU slirp, no receiver needed)
    let test_frame: [u8; 64] = [0xff; 64];
    tx.transmit(&test_frame);
}
