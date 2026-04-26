//! Minimal TSC-based embassy time driver for Limine-booted kernels.
//!
//! Uses TSC (no calibration needed for basic operation — assumes ~1 GHz).
//! Embassy's tick-hz is 1MHz, so now() returns microseconds.

use core::sync::atomic::{AtomicU64, Ordering};
use core::task::Waker;

const MAX_ALARMS: usize = 4;

struct Alarm {
    at: u64,
    waker: Option<Waker>,
}

struct TscTimeDriver {
    tsc_per_us: AtomicU64,
    alarms: critical_section::Mutex<core::cell::RefCell<[Option<Alarm>; MAX_ALARMS]>>,
}

embassy_time_driver::time_driver_impl!(static DRIVER: TscTimeDriver = TscTimeDriver {
    tsc_per_us: AtomicU64::new(1000), // default ~1 GHz; calibrate for accuracy
    alarms: critical_section::Mutex::new(core::cell::RefCell::new([
        None, None, None, None,
    ])),
});

impl embassy_time_driver::Driver for TscTimeDriver {
    fn now(&self) -> u64 {
        let tsc = unsafe { core::arch::x86_64::_rdtsc() };
        tsc / self.tsc_per_us.load(Ordering::Relaxed)
    }

    fn schedule_wake(&self, at: u64, waker: &Waker) {
        critical_section::with(|cs| {
            if at <= self.now() {
                waker.wake_by_ref();
                return;
            }

            let mut alarms = self.alarms.borrow_ref_mut(cs);
            let mut empty_slot = None;
            for (i, slot) in alarms.iter_mut().enumerate() {
                if let Some(alarm) = slot {
                    if alarm.waker.as_ref().is_some_and(|w| w.will_wake(waker)) {
                        alarm.at = at;
                        return;
                    }
                } else if empty_slot.is_none() {
                    empty_slot = Some(i);
                }
            }

            if let Some(i) = empty_slot {
                alarms[i] = Some(Alarm {
                    at,
                    waker: Some(waker.clone()),
                });
            } else {
                waker.wake_by_ref(); // All slots full — busy-poll fallback
            }
        });
    }
}

/// Calibrate TSC using PIT channel 2 (50ms measurement).
pub fn calibrate_tsc() {
    // PIT channel 2: gate on, mode 0 (one-shot), ~50ms
    let count: u16 = 59659; // 1193182 Hz / 20 = ~50ms

    outb(0x61, (inb(0x61) & 0x0C) | 0x01); // Gate on, speaker off
    outb(0x43, 0xB0); // Channel 2, lobyte/hibyte, mode 0
    outb(0x42, (count & 0xFF) as u8);
    outb(0x42, (count >> 8) as u8);

    // Reset gate to start counting
    let gate = inb(0x61);
    outb(0x61, gate & !0x01);
    outb(0x61, gate | 0x01);

    let start = unsafe { core::arch::x86_64::_rdtsc() };
    // Wait for PIT output bit (bit 5 of port 0x61)
    while inb(0x61) & 0x20 == 0 {
        core::hint::spin_loop();
    }
    let end = unsafe { core::arch::x86_64::_rdtsc() };

    let tsc_per_50ms = end - start;
    let tsc_per_us = tsc_per_50ms / 50_000;
    if tsc_per_us > 0 {
        DRIVER.tsc_per_us.store(tsc_per_us, Ordering::Relaxed);
    }
    // If calibration failed (e.g. Hyper-V doesn't emulate PIT ch2),
    // keep the default of 1000 (~1 GHz).
}

/// Check expired alarms — call from timer interrupt or poll loop.
pub fn check_alarms() {
    use embassy_time_driver::Driver;
    let now = DRIVER.now();
    critical_section::with(|cs| {
        let mut alarms = DRIVER.alarms.borrow_ref_mut(cs);
        for slot in alarms.iter_mut() {
            if let Some(alarm) = slot
                && alarm.at <= now
            {
                if let Some(waker) = alarm.waker.take() {
                    waker.wake();
                }
                *slot = None;
            }
        }
    });
}

fn outb(port: u16, value: u8) {
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack))
    };
}

fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack))
    };
    value
}
