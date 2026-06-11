use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();

extern "x86-interrupt" fn default_handler(_frame: InterruptStackFrame) {}

fn idt() -> &'static mut InterruptDescriptorTable {
    unsafe { &mut *core::ptr::addr_of_mut!(IDT) }
}

/// Initialize the IDT with default handlers and load it on the BSP.
pub fn init() {
    let idt = idt();
    for i in 32u8..48 {
        idt[i].set_handler_fn(default_handler);
    }
    idt.load();
    log::info!("IDT initialized");
}

/// Load the IDT register (`lidt`) on the calling CPU.
///
/// `init()` already does this on the BSP; APs call this from
/// `smp::ap_setup` so they share the same global IDT image without
/// re-initialising its slots.
///
/// # Safety
/// The IDT static must already have been initialised by [`init`] on
/// some CPU. Calling this before [`init`] leaves the calling CPU with
/// an IDT full of zero descriptors.
pub unsafe fn load_current_cpu() {
    idt().load();
}

/// Register an interrupt handler for a specific vector (32-255).
///
/// # Safety
/// The handler must be correct for the interrupt source.
pub unsafe fn set_handler(vector: u8, handler: extern "x86-interrupt" fn(InterruptStackFrame)) {
    assert!(vector >= 32, "vectors 0-31 are CPU exceptions");
    let idt = idt();
    idt[vector].set_handler_fn(handler);
    idt.load();
}
