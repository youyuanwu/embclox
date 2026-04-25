use critical_section::RawRestoreState;

struct X86CriticalSection;

critical_section::set_impl!(X86CriticalSection);

unsafe impl critical_section::Impl for X86CriticalSection {
    unsafe fn acquire() -> RawRestoreState {
        let was_enabled = x86_64::instructions::interrupts::are_enabled();
        x86_64::instructions::interrupts::disable();
        was_enabled
    }

    unsafe fn release(was_enabled: RawRestoreState) {
        if was_enabled {
            x86_64::instructions::interrupts::enable();
        }
    }
}
