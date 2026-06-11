//! `embassy-time` driver backed by per-CPU APIC-timer alarm slots.
//!
//! The `embassy_time_driver::time_driver_impl!` macro registers a
//! single global driver; per-CPU support is achieved by sharding the
//! alarm slots array by [`crate::cpu_local::current_cpu_id`]. Each
//! CPU's APIC timer ISR calls [`on_timer_tick`], which only walks
//! the calling CPU's slots. `schedule_wake` runs in the context of
//! a polling task — tasks don't migrate (per
//! [docs/design/smp-per-cpu-executors.md](../../../../docs/design/smp-per-cpu-executors.md))
//! so the slot it writes is the one the same CPU's timer will read.
//!
//! `now()` stays global (TSC read; invariant TSC across cores).

use crate::cpu_local::{self, MAX_CPUS};
use crate::vector_alloc::CpuId;
use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::Waker;
use critical_section::Mutex;
use embassy_time_driver::Driver;

const MAX_ALARMS: usize = 8;

struct Alarm {
    at: u64,
    waker: Option<Waker>,
}

type AlarmSlots = Mutex<RefCell<[Option<Alarm>; MAX_ALARMS]>>;

struct ApicTimeDriver {
    tsc_per_us: AtomicU64,
    /// One alarm-slot table per CPU, indexed by `processor_id`.
    alarms: [AlarmSlots; MAX_CPUS],
}

embassy_time_driver::time_driver_impl!(static DRIVER: ApicTimeDriver = ApicTimeDriver {
    tsc_per_us: AtomicU64::new(1),
    alarms: [const { Mutex::new(RefCell::new([
        const { None }, const { None }, const { None }, const { None },
        const { None }, const { None }, const { None }, const { None },
    ])) }; MAX_CPUS],
});

/// Convert a `CpuId` to its slot index in `DRIVER.alarms`.
fn slot_of(cpu: CpuId) -> usize {
    match cpu {
        CpuId::Bsp => 0,
        CpuId::Ap(n) => n as usize,
    }
}

impl Driver for ApicTimeDriver {
    fn now(&self) -> u64 {
        let tsc = unsafe { core::arch::x86_64::_rdtsc() };
        let tsc_per_us = self.tsc_per_us.load(Ordering::Relaxed);
        tsc / tsc_per_us
    }

    fn schedule_wake(&self, at: u64, waker: &Waker) {
        // Routing: tasks don't migrate, so the slot we register the
        // waker in is the same CPU whose APIC timer will fire
        // on_timer_tick() and walk it.
        let cpu_slot = slot_of(cpu_local::current_cpu_id());
        critical_section::with(|cs| {
            let mut alarms = self.alarms[cpu_slot].borrow_ref_mut(cs);

            // Check if already expired.
            if at <= self.now() {
                waker.wake_by_ref();
                return;
            }

            // Find existing alarm for this waker, or first empty slot.
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
                // All slots full — wake immediately as fallback (busy-poll).
                waker.wake_by_ref();
            }
        });
    }
}

/// Set the TSC calibration value. Call once during init from BSP;
/// the same value is propagated to APs via
/// [`crate::smp::set_ap_init_params`].
pub fn set_tsc_per_us(tsc_per_us: u64) {
    DRIVER.tsc_per_us.store(tsc_per_us, Ordering::Relaxed);
}

/// Called from each CPU's APIC timer interrupt handler.
/// Checks the calling CPU's alarm slots and wakes any that expired.
pub fn on_timer_tick() {
    let now = DRIVER.now();
    let cpu_slot = slot_of(cpu_local::current_cpu_id());
    critical_section::with(|cs| {
        let mut alarms = DRIVER.alarms[cpu_slot].borrow_ref_mut(cs);
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
