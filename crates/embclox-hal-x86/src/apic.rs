use core::ptr;

/// Local APIC register offsets (from base address).
const APIC_ID: usize = 0x020;
const APIC_VERSION: usize = 0x030;
const APIC_TPR: usize = 0x080;
const APIC_EOI: usize = 0x0B0;
const APIC_SVR: usize = 0x0F0;
const APIC_LVT_TIMER: usize = 0x320;
const APIC_TIMER_INIT_CNT: usize = 0x380;
const APIC_TIMER_CURR_CNT: usize = 0x390;
const APIC_TIMER_DIV: usize = 0x3E0;

const APIC_SVR_ENABLE: u32 = 0x100;
const APIC_TIMER_PERIODIC: u32 = 1 << 17;
const APIC_TIMER_MASKED: u32 = 1 << 16;

/// LAPIC physical address (standard for xAPIC).
pub const LAPIC_PHYS_BASE: u64 = 0xFEE0_0000;

/// Local APIC (Advanced Programmable Interrupt Controller).
pub struct LocalApic {
    base: usize,
}

impl LocalApic {
    /// Create a new LAPIC instance from a UC-mapped virtual address.
    /// Map `LAPIC_PHYS_BASE` via `MemoryMapper::map_mmio()` first.
    pub fn new(base: usize) -> Self {
        Self { base }
    }

    fn read(&self, offset: usize) -> u32 {
        unsafe { ptr::read_volatile((self.base + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        unsafe { ptr::write_volatile((self.base + offset) as *mut u32, value) }
    }

    /// Enable the LAPIC with spurious vector 39.
    pub fn enable(&mut self) {
        // Set spurious interrupt vector to 39 and enable APIC
        self.write(APIC_SVR, APIC_SVR_ENABLE | 39);
        // Set task priority to 0 (accept all interrupts)
        self.write(APIC_TPR, 0);
        log::info!(
            "LAPIC enabled: ID={:#x}, version={:#x}",
            self.read(APIC_ID),
            self.read(APIC_VERSION)
        );
    }

    /// Configure the APIC timer in periodic mode.
    /// `vector`: interrupt vector (e.g., 32)
    /// `initial_count`: timer ticks between interrupts
    /// `divider`: divide configuration (see `set_divider`)
    pub fn set_timer_periodic(&mut self, vector: u8, divider: u8, initial_count: u32) {
        self.set_divider(divider);
        // Periodic mode + vector, not masked
        self.write(APIC_LVT_TIMER, APIC_TIMER_PERIODIC | vector as u32);
        self.write(APIC_TIMER_INIT_CNT, initial_count);
        log::info!(
            "APIC timer: vector={}, divider={}, count={}",
            vector,
            divider,
            initial_count
        );
    }

    /// Mask (disable) the timer interrupt.
    pub fn mask_timer(&mut self) {
        let lvt = self.read(APIC_LVT_TIMER);
        self.write(APIC_LVT_TIMER, lvt | APIC_TIMER_MASKED);
    }

    /// Read current timer count (for calibration).
    pub fn timer_current_count(&self) -> u32 {
        self.read(APIC_TIMER_CURR_CNT)
    }

    /// Set initial timer count (for calibration).
    pub fn set_timer_initial_count(&mut self, count: u32) {
        self.write(APIC_TIMER_INIT_CNT, count);
    }

    /// Set timer in one-shot mode with masked interrupt (for calibration).
    pub fn set_timer_oneshot_masked(&mut self, divider: u8) {
        self.set_divider(divider);
        self.write(APIC_LVT_TIMER, APIC_TIMER_MASKED | 32); // masked, vector doesn't matter
    }

    fn set_divider(&mut self, divider: u8) {
        // Divider encoding: 0=2, 1=4, 2=8, 3=16, 8=32, 9=64, 10=128, 11=1
        let val = match divider {
            1 => 0b1011,
            2 => 0b0000,
            4 => 0b0001,
            8 => 0b0010,
            16 => 0b0011,
            32 => 0b1000,
            64 => 0b1001,
            128 => 0b1010,
            _ => panic!("invalid APIC timer divider: {}", divider),
        };
        self.write(APIC_TIMER_DIV, val);
    }

    /// Send End-of-Interrupt.
    pub fn end_of_interrupt(&self) {
        self.write(APIC_EOI, 0);
    }

    /// Read this CPU's LAPIC ID (xAPIC: bits 31:24 of the APIC_ID
    /// register). x2APIC would use a 32-bit value here; we don't
    /// support that yet so the truncation to `u8` is safe.
    pub fn id(&self) -> u8 {
        (self.read(APIC_ID) >> 24) as u8
    }
}
