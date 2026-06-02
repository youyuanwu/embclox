//! Hardware reset sequence for the e1000.
//!
//! Must be called on the raw register accessor **before** constructing
//! [`crate::E1000Device`]. The reset is what brings the device into a
//! known state where ring init can succeed.

use crate::regs::*;
use crate::RegisterAccess;

/// Reset an e1000 device to a clean post-reset state.
///
/// Disables interrupts, triggers a hardware reset, waits for completion,
/// then configures CTRL with `SLU | ASDE` and clears flow control registers.
/// Panics if reset doesn't complete within ~100k iterations.
///
/// Callers also need to enable PCI bus mastering (separately, via the
/// PCI bus interface) before constructing the device.
pub fn reset_device<R: RegisterAccess>(regs: &R) {
    regs.write_reg(IMS, 0);
    let ctl = regs.read_reg(CTL);
    regs.write_reg(CTL, ctl | CTL_RST);

    let mut timeout = 100_000u32;
    loop {
        if regs.read_reg(CTL) & CTL_RST == 0 {
            break;
        }
        timeout -= 1;
        assert!(timeout > 0, "e1000 reset timeout");
    }

    regs.write_reg(IMS, 0);
    regs.write_reg(CTL, CTL_SLU | CTL_ASDE);
    regs.write_reg(FCAL, 0);
    regs.write_reg(FCAH, 0);
    regs.write_reg(FCT, 0);
    regs.write_reg(FCTTV, 0);
}
