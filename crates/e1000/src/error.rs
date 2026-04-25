/// Interrupt status returned by `handle_interrupt`.
pub struct InterruptStatus {
    pub icr: u32,
}

impl InterruptStatus {
    pub fn rx_ready(&self) -> bool {
        self.icr & 0x80 != 0
    }
    pub fn link_status_change(&self) -> bool {
        self.icr & 0x04 != 0
    }
}
