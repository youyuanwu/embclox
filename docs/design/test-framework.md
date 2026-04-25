# Test Framework

## Overview

Two testing modes for bare-metal code running inside QEMU:

1. **Integration tests** — build a QEMU image, boot it, verify behavior
   from the host (log scanning, TCP probing). Good for end-to-end
   validation.

2. **Unit tests** — Rust-style `#[test]` functions compiled into the
   QEMU image. The guest runs all tests, prints results to serial,
   and exits with QEMU exit code. Good for testing e1000 driver
   internals and HAL modules that need real hardware (MMIO, interrupts).

## What goes where

**Rule of thumb**: if the assertion is about internal state (register
value, data structure contents), it's a unit test. If the assertion
requires an external observer (host connecting via TCP, scanning serial
logs), it's an integration test.

### Unit tests (in-guest)

Test individual modules where real hardware access is needed:

| Area | Example tests |
|------|---------------|
| e1000 regs | init rings, MAC address read, register read-back |
| e1000 tx/rx | transmit a single frame, receive a loopback frame |
| e1000 irq | interrupt enable, ICR acknowledge |
| HAL PCI | scan finds known device, read BAR0 |
| HAL memory | map_mmio returns UC mapping, phys_offset valid |
| HAL APIC | timer fires within expected interval |
| HAL IDT | handler registration, vector dispatch |
| HAL serial | write bytes appear on port |

### Integration tests (host probes/scans externally)

Test the full stack working together, verified from outside QEMU:

| Test | What it proves |
|------|----------------|
| TCP echo (existing) | boot → HAL → PCI → e1000 → Embassy → TCP round-trip |
| Large payload (future) | DMA ring wrapping, multi-descriptor frames |
| DHCP (future) | embassy-time alarms + network stack negotiation |
| Multiple connections (future) | concurrent TCP sessions |

Most new tests should be unit tests — they're faster to write and
pinpoint failures to a specific module.

## Mode 1: Integration Tests (log/probe)

Already partially implemented via `examples/test.sh`. Generalize to
support multiple test images, each with its own pass criteria.

```
Host                          QEMU Guest
─────                         ──────────
build image ──────────────►   boot
start QEMU                    run application
wait / probe ◄────────────    serial output
scan log for PASS/FAIL        QEMU exit (isa-debug-exit)
report result
```

### Test runner (`scripts/qemu-test.sh`)

Shared script that boots a QEMU image and checks pass/fail:

```sh
qemu-test.sh <image> [--probe tcp:PORT:STRING] [--log-match PATTERN] [--timeout SECS]
```

- `--probe tcp:PORT:STRING` — send STRING, expect echo (current approach)
- `--log-match PATTERN` — scan serial log for anchored regex
- `--timeout SECS` — max wait before declaring failure (default 30s)
- Adds `-device isa-debug-exit,iobase=0xf4,iosize=0x04` to QEMU
  (only test invocations, not the development `qemu` CMake target)

**Exit code remapping**: QEMU `isa-debug-exit` produces
`(guest_code << 1) | 1`, so guest writing 0 → host exit 1 (success),
guest writing 1 → host exit 3 (failure). The script remaps:

```sh
qemu-system-x86_64 ... ; rc=$?
case $rc in
    1) exit 0 ;;   # guest wrote 0 = success
    3) exit 1 ;;   # guest wrote 1 = failure
    *)  exit 1 ;;  # crash, timeout, or unexpected
esac
```

### Adding a new integration test

1. Create a new binary crate (e.g., in `qemu-tests/`)
2. Implement `#![no_std]` `#![no_main]` binary that performs the test
3. Print `TEST OK` to serial on success, then exit via `isa-debug-exit`
4. Add CMake/CI target to build image and run `qemu-test.sh`

## Mode 2: Unit Tests (in-guest)

A `#![no_std]` test harness that runs test functions inside the QEMU
guest. Inspired by Linux KUnit and the rko project's `#[rko_tests]`
macro approach.

### Architecture

```
qemu-tests/unit/
├── .cargo/config.toml  # target = "x86_64-unknown-none"
├── Cargo.toml          # binary crate, no_std + no_main
├── src/
│   ├── main.rs         # entry_point, HAL init, run test suites, exit
│   ├── harness.rs      # TestCase struct, runner, serial output
│   └── setup.rs        # shared boot/init sequence (see below)
└── suites/
    ├── mod.rs
    ├── e1000_regs.rs    # e1000 register-level tests
    ├── hal_memory.rs    # memory mapper tests
    └── hal_pci.rs       # PCI scan tests
```

### Workspace integration

Like `examples/`, the test crate needs:
- `.cargo/config.toml` with `target = "x86_64-unknown-none"`
- Added to workspace `members` but **not** `default-members`
- Uses `dep.workspace = true` for shared dependencies
- Separate `cargo clippy` invocation in CI (with `--manifest-path`)

### Test harness

Tests run sequentially. On success, all tests complete and QEMU exits
with success code. On failure (any `assert!` panic), the panic handler
prints the failure and exits QEMU immediately — no attempt to continue
after a panic since hardware state is untrusted.

```rust
pub struct TestCase {
    pub name: &'static str,
    pub func: fn(),
}

pub fn run_tests(suite: &str, tests: &[TestCase]) {
    assert!(!tests.is_empty(), "suite '{}' has no tests", suite);
    log::info!("# suite: {}", suite);
    for tc in tests {
        (tc.func)();
        log::info!("  PASS: {}", tc.name);
    }
}
```

If a test panics, the panic handler fires:

```rust
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    log::error!("PANIC: {}", info);
    qemu_exit(1); // failure
}
```

On success (all tests pass), main exits cleanly:

```rust
log::info!("=== {} passed ===", total);
qemu_exit(0); // success
```

### Test setup sequence

Each test binary must initialize the HAL before running tests. This
is the same boot sequence as `examples/src/main.rs` but without
Embassy/networking:

```rust
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    // 1. HAL init (serial, heap, memory mapper, PCI)
    let mut p = embclox_hal_x86::init(boot_info, Config::default());

    // 2. (Optional) IDT + interrupt setup if testing interrupts
    // embclox_hal_x86::idt::init();

    // 3. PCI scan + e1000 BAR0 mapping (if testing e1000)
    let pci_dev = p.pci.find_device_any(0x8086, &[0x100E, ...]);
    let bar0 = p.pci.read_bar0(&pci_dev);
    let mmio_vaddr = p.memory.map_mmio(bar0, 0x20000);

    // 4. Run test suites
    let ctx = TestContext { peripherals: p, mmio_vaddr, ... };
    e1000_regs::run(&ctx);
    hal_memory::run(&ctx);

    // 5. All passed
    log::info!("=== {} passed ===", count);
    qemu_exit(0);
}
```

**Note**: `embclox_hal_x86::init()` is a one-shot singleton (panics if
called twice). Each test binary gets one call. If tests need different
init configurations, use separate binaries.

### Suite isolation

**Phase 1**: all suites run in a single binary/QEMU boot. This is
pragmatic for a small test suite. Suites share HAL init and must be
order-independent (no suite should assume device state left by a prior
suite).

**Future**: for full isolation, split into separate binaries (one per
suite or Cargo `[[bin]]` targets). Each gets its own QEMU boot with
fresh hardware state. The host script iterates over suite images.

### QEMU exit

Use `isa-debug-exit` device to signal pass/fail to the host:

```rust
// In QEMU: -device isa-debug-exit,iobase=0xf4,iosize=0x04
fn qemu_exit(code: u32) -> ! {
    unsafe { x86_64::instructions::port::Port::new(0xf4).write(code); }
    loop { x86_64::instructions::hlt(); }
}
// exit code formula: (code << 1) | 1
// Write 0 → host sees exit 1 → script remaps to success
// Write 1 → host sees exit 3 → script remaps to failure
```

**Important**: only add `-device isa-debug-exit` to test QEMU
invocations, not the development `qemu` CMake target. An accidental
write to port 0xf4 during interactive debugging would kill the VM.

### Serial output format

```
=== embclox test runner ===
# suite: e1000_regs
  PASS: read_ctl_default
  PASS: write_ims
# suite: hal_memory
  PASS: map_mmio_returns_uc
  PASS: phys_offset_valid
=== 4 passed ===
```

On failure, the panic handler prints the failure location and QEMU
exits immediately — no summary line (the non-zero exit code is the
signal).

The host script uses `isa-debug-exit` code as the primary pass/fail
signal, not log scraping. Log output is captured for diagnostics.

### Test registration

**Phase 1 — Explicit arrays** (no proc macros):
```rust
// suites/e1000_regs.rs
pub fn suite() -> (&'static str, &'static [TestCase]) {
    ("e1000_regs", &[
        TestCase { name: "read_ctl_default", func: test_read_ctl_default },
        TestCase { name: "write_ims", func: test_write_ims },
    ])
}
```

Main collects and runs all suites:
```rust
let suites = [e1000_regs::suite(), hal_memory::suite()];
for (name, tests) in &suites {
    run_tests(name, tests);
}
```

**Phase 2 — Proc macro** (future, if boilerplate grows):
```rust
#[embclox_test_suite]
mod e1000_regs {
    #[test]
    fn read_ctl_default() { ... }
}
```

## Project layout

```
embclox/
├── crates/
│   ├── embclox-e1000/
│   └── embclox-hal-x86/
├── examples/                    # TCP echo example (existing)
├── qemu-tests/
│   └── unit/                    # Mode 2: in-guest unit tests
│       ├── .cargo/config.toml   # target = x86_64-unknown-none
│       ├── Cargo.toml
│       └── src/
├── scripts/
│   └── qemu-test.sh             # shared QEMU boot + exit-code remap
└── CMakeLists.txt               # targets: test-unit, test-integration
```

Integration tests remain in `examples/test.sh`, refactored to delegate
to `scripts/qemu-test.sh` internally (preserving existing CI path).

## CI integration

```yaml
- name: Clippy unit tests
  run: cargo clippy --manifest-path qemu-tests/unit/Cargo.toml
       --target x86_64-unknown-none -- -D warnings

- name: Build unit test image
  run: |
    cargo build --manifest-path qemu-tests/unit/Cargo.toml
    cargo run -p embclox-mkimage -- \
      target/x86_64-unknown-none/debug/embclox-unit-tests \
      target/x86_64-unknown-none/debug/embclox-unit-tests.img

- name: Unit tests
  run: bash scripts/qemu-test.sh target/.../embclox-unit-tests.img

- name: Integration tests
  working-directory: examples
  run: bash test.sh
```

## Implementation order

1. Create `scripts/qemu-test.sh` with `isa-debug-exit` and exit-code
   remapping (only for test invocations, not dev QEMU target)
2. Create `qemu-tests/unit/` with `.cargo/config.toml`, workspace
   membership (not in `default-members`), harness, and one smoke test
3. Refactor `examples/test.sh` to delegate to `scripts/qemu-test.sh`
   internally (preserve existing path for CI compatibility)
4. Add CMake + CI targets
5. Add more test suites as needed
