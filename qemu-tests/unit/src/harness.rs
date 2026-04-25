/// A single test case with name and function pointer.
pub struct TestCase {
    pub name: &'static str,
    pub func: fn(),
}

/// Run all tests in a suite sequentially. If any test panics,
/// the panic handler exits QEMU with a failure code.
pub fn run_suite(suite_name: &str, tests: &[TestCase]) -> usize {
    assert!(
        !tests.is_empty(),
        "suite '{}' has no tests — empty suites are not allowed",
        suite_name
    );
    log::info!("# suite: {}", suite_name);
    for tc in tests {
        (tc.func)();
        log::info!("  PASS: {}", tc.name);
    }
    tests.len()
}

/// Exit QEMU via isa-debug-exit device.
/// code=0 → host sees exit 1 (success after remapping)
/// code=1 → host sees exit 3 (failure after remapping)
pub fn qemu_exit(code: u32) -> ! {
    unsafe {
        x86_64::instructions::port::Port::new(0xf4).write(code);
    }
    loop {
        x86_64::instructions::hlt();
    }
}
