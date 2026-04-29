use core::sync::atomic::{AtomicU64, Ordering};
use core::task::Waker;
use critical_section::Mutex;
use embassy_time_driver::Driver;

const MAX_ALARMS: usize = 8;

struct Alarm {
    at: u64,
    waker: Option<Waker>,
}

struct ApicTimeDriver {
    tsc_per_us: AtomicU64,
    alarms: Mutex<core::cell::RefCell<[Option<Alarm>; MAX_ALARMS]>>,
}

embassy_time_driver::time_driver_impl!(static DRIVER: ApicTimeDriver = ApicTimeDriver {
    tsc_per_us: AtomicU64::new(1),
    alarms: Mutex::new(core::cell::RefCell::new([
        None, None, None, None, None, None, None, None,
    ])),
});

impl Driver for ApicTimeDriver {
    fn now(&self) -> u64 {
        let tsc = unsafe { core::arch::x86_64::_rdtsc() };
        let tsc_per_us = self.tsc_per_us.load(Ordering::Relaxed);
        tsc / tsc_per_us
    }

    fn schedule_wake(&self, at: u64, waker: &Waker) {
        critical_section::with(|cs| {
            let mut alarms = self.alarms.borrow_ref_mut(cs);

            // Check if already expired
            if at <= self.now() {
                waker.wake_by_ref();
                return;
            }

            // Find existing alarm for this waker, or first empty slot
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
                // All slots full — wake immediately as fallback (busy-poll)
                waker.wake_by_ref();
            }
        });
    }
}

/// Set the TSC calibration value. Call once during init.
pub fn set_tsc_per_us(tsc_per_us: u64) {
    DRIVER.tsc_per_us.store(tsc_per_us, Ordering::Relaxed);
}

/// Called from the APIC timer interrupt handler.
/// Checks all alarm slots and wakes any that have expired.
pub fn on_timer_tick() {
    let now = DRIVER.now();
    critical_section::with(|cs| {
        let mut alarms = DRIVER.alarms.borrow_ref_mut(cs);
        for slot in alarms.iter_mut() {
            if let Some(alarm) = slot {
                if alarm.at <= now {
                    if let Some(waker) = alarm.waker.take() {
                        waker.wake();
                    }
                    *slot = None;
                }
            }
        }
    });
}
