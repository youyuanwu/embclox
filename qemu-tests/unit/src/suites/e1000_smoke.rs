use embclox_core::dma_alloc::BootDmaAllocator;
use embclox_e1000::MmioRegs;
use embclox_e1000::RegisterAccess;
use embclox_e1000::regs::*;

/// Global test context set by main before running suites.
static mut CTX: Option<(MmioRegs, BootDmaAllocator)> = None;

/// Initialize the e1000 test context. Called once from main.
///
/// # Safety
/// Must be called before `suite()` and only from single-threaded init.
pub unsafe fn init(regs: MmioRegs, dma: BootDmaAllocator) {
    unsafe {
        *core::ptr::addr_of_mut!(CTX) = Some((regs, dma));
    }
}

fn regs() -> &'static MmioRegs {
    unsafe {
        &(*core::ptr::addr_of!(CTX))
            .as_ref()
            .expect("e1000 test context not initialized")
            .0
    }
}

fn dma() -> &'static BootDmaAllocator {
    unsafe {
        &(*core::ptr::addr_of!(CTX))
            .as_ref()
            .expect("e1000 test context not initialized")
            .1
    }
}

/// e1000 driver smoke tests. Requires device setup (reset, bus mastering)
/// done in main before running. Context is stored in a static because
/// test functions are `fn()` with no arguments.
#[embclox_test_macros::test_suite(name = "e1000_smoke")]
mod tests {
    use super::*;

    /// STATUS register bit 1 (link up) should be set on QEMU's e1000.
    #[test]
    fn status_link_up() {
        let status = regs().read_reg(STAT);
        assert!(
            status & 0x2 != 0,
            "e1000 link should be up, STATUS={:#x}",
            status
        );
    }

    /// RAL/RAH registers should contain a non-zero MAC address.
    #[test]
    fn mac_address_nonzero() {
        let ral = regs().read_reg(RAL);
        let rah = regs().read_reg(RAH);
        assert!(ral != 0 || rah != 0, "MAC address should not be zero");
    }

    /// Initialize the device, verify MAC via driver API, split into
    /// RX/TX halves, confirm no pending RX packets, and transmit a frame.
    #[test]
    fn init_device_and_split() {
        let mut dev = embclox_e1000::E1000Device::new(*regs(), dma().clone());
        let mac = dev.mac_address();
        assert!(mac != [0; 6], "MAC should not be all zeros");

        let (mut rx, mut tx) = dev.split();
        let received = rx.recv_with(|_data| {
            panic!("should not receive any packet");
        });
        assert!(received.is_none(), "no packets should be pending");

        let test_frame: [u8; 64] = [0xff; 64];
        tx.transmit(&test_frame);
    }
}

pub use tests::suite;
