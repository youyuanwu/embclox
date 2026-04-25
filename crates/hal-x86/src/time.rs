use core::task::Waker;
use embassy_time_driver::Driver;

struct TscTimeDriver;

impl Driver for TscTimeDriver {
    fn now(&self) -> u64 {
        unsafe { core::arch::x86_64::_rdtsc() / 1000 }
    }

    fn schedule_wake(&self, _at: u64, waker: &Waker) {
        waker.wake_by_ref();
    }
}

embassy_time_driver::time_driver_impl!(static DRIVER: TscTimeDriver = TscTimeDriver);
