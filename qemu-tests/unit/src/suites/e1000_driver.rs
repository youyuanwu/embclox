use embclox_core::dma_alloc::BootDmaAllocator;
use embclox_e1000::regs::*;
use embclox_e1000::{MmioRegs, RegisterAccess};

/// Global test context — shared by all e1000 driver tests.
static mut CTX: Option<(MmioRegs, BootDmaAllocator)> = None;

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
            .expect("e1000 driver test context not initialized")
            .0
    }
}

fn dma() -> &'static BootDmaAllocator {
    unsafe {
        &(*core::ptr::addr_of!(CTX))
            .as_ref()
            .expect("e1000 driver test context not initialized")
            .1
    }
}

fn new_device() -> embclox_e1000::E1000Device<MmioRegs, BootDmaAllocator> {
    // Reset + construct. Bus mastering is enabled once in main; each
    // test gets a fresh device by re-resetting and re-running ring
    // init, which is what E1000Device::new does internally.
    embclox_e1000::reset_device(regs());
    embclox_e1000::E1000Device::new(*regs(), dma().clone())
}

/// E1000 driver tests — each test creates and drops its own device,
/// getting a full reset cycle between tests.
#[embclox_test_macros::test_suite(name = "e1000_driver")]
mod tests {
    use super::*;

    /// Verify link_is_up() returns true on QEMU.
    #[test]
    fn link_is_up() {
        let dev = new_device();
        assert!(dev.link_is_up(), "link should be up on QEMU e1000");
    }

    /// Verify MAC address is consistent across device reinit.
    #[test]
    fn mac_consistent_across_reinit() {
        let dev1 = new_device();
        let mac1 = dev1.mac_address();
        drop(dev1);

        let dev2 = new_device();
        let mac2 = dev2.mac_address();
        assert_eq!(mac1, mac2, "MAC should be the same after device reset");
    }

    /// Transmit multiple frames and verify TX doesn't stall.
    #[test]
    fn transmit_multiple_frames() {
        let mut dev = new_device();
        let (_, mut tx) = dev.split();

        for i in 0u8..10 {
            let mut frame = [0u8; 64];
            frame[0..6].fill(0xff); // broadcast dest
            frame[12] = i; // vary payload
            tx.transmit(&frame);
        }
    }

    /// Transmit a maximum-sized Ethernet frame (1514 bytes).
    #[test]
    fn transmit_max_frame() {
        let mut dev = new_device();
        let (_, mut tx) = dev.split();

        let mut frame = [0u8; 1514];
        frame[0..6].fill(0xff);
        tx.transmit(&frame);
    }

    /// Enable and disable interrupts, verify IMS register reflects state.
    #[test]
    fn interrupt_enable_disable() {
        let dev = new_device();

        dev.enable_interrupts();
        let ims = regs().read_reg(IMS);
        assert!(ims != 0, "IMS should be non-zero after enable");

        dev.disable_interrupts();
        let ims = regs().read_reg(IMS);
        assert_eq!(ims, 0, "IMS should be zero after disable");
    }

    /// Verify transmit_with zero-copy API works.
    #[test]
    fn transmit_with_zero_copy() {
        let mut dev = new_device();
        let (_, mut tx) = dev.split();

        let result = tx.transmit_with(64, |buf| {
            buf[0..6].fill(0xff); // broadcast
            buf[6..12].fill(0xaa); // src
            buf[12] = 0x08;
            buf[13] = 0x00; // IPv4 ethertype
            42 // return value
        });
        assert_eq!(
            result.unwrap(),
            42,
            "transmit_with should return closure result"
        );
    }

    /// Verify enable_loopback sets RCTL.LBM bits correctly.
    /// Note: QEMU's e1000 does not implement MAC loopback behavior
    /// (packets are not actually looped back), so we only verify the
    /// register write — not the TX→RX data path.
    #[test]
    fn loopback_register_set() {
        let dev = new_device();
        dev.enable_loopback();
        let rctl = regs().read_reg(RCTL);
        assert!(
            rctl & RCTL_LBM_MAC != 0,
            "RCTL.LBM should be set after enable_loopback, RCTL={:#x}",
            rctl
        );
    }

    /// Send an ARP request to QEMU slirp gateway (10.0.2.2) and verify
    /// we receive an ARP reply. Tests the full TX→RX data path through
    /// the QEMU network backend.
    #[test]
    fn arp_round_trip() {
        let mut dev = new_device();
        let mac = dev.mac_address();

        // First send a gratuitous ARP so slirp learns our MAC
        // (required for slirp to route replies back to us)
        let garp: [u8; 42] = [
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // dst: broadcast
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], // src: our MAC
            0x08, 0x06, // ethertype: ARP
            0x00, 0x01, // hw type: Ethernet
            0x08, 0x00, // proto: IPv4
            0x06, // hw size
            0x04, // proto size
            0x00, 0x01, // opcode: request
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], // sender MAC
            10, 0, 2, 15, // sender IP: 10.0.2.15
            0, 0, 0, 0, 0, 0, // target MAC: zero
            10, 0, 2, 2, // target IP: 10.0.2.2 (gateway)
        ];
        {
            let (_, mut tx) = dev.split();
            tx.transmit(&garp);
        }

        // Now send an ARP request asking "who has 10.0.2.2?"
        let arp_request: [u8; 42] = [
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // dst: broadcast
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], // src: our MAC
            0x08, 0x06, // ethertype: ARP
            0x00, 0x01, // hw type: Ethernet
            0x08, 0x00, // proto: IPv4
            0x06, 0x04, // hw/proto size
            0x00, 0x01, // opcode: request
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], // sender MAC
            10, 0, 2, 15, // sender IP
            0, 0, 0, 0, 0, 0, // target MAC: zero (asking)
            10, 0, 2, 2, // target IP: gateway
        ];
        {
            let (_, mut tx) = dev.split();
            tx.transmit(&arp_request);
        }

        // Spin-poll for ARP reply (opcode 0x0002).
        // Slirp may need time to process, so retry with small delays.
        let mut got_reply = false;
        for attempt in 0..200 {
            // Busy-wait ~1ms between attempts (rough, no timer)
            if attempt > 0 {
                for _ in 0..100_000 {
                    core::hint::spin_loop();
                }
            }
            let (mut rx, _) = dev.split();
            let is_arp_reply = rx
                .recv_with(|data| {
                    if data.len() >= 42 && data[12] == 0x08 && data[13] == 0x06 {
                        let opcode = u16::from_be_bytes([data[20], data[21]]);
                        if opcode == 2 {
                            assert_eq!(
                                &data[28..32],
                                &[10, 0, 2, 2],
                                "ARP reply sender IP should be 10.0.2.2"
                            );
                            return true;
                        }
                    }
                    false
                })
                .unwrap_or(false);
            if is_arp_reply {
                got_reply = true;
                break;
            }
        }
        assert!(got_reply, "should receive ARP reply from gateway 10.0.2.2");
    }
}

pub use tests::suite;
