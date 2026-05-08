# examples-e1000

Bare-metal kernel demonstrating Intel e1000 NIC + embassy-net TCP echo
on QEMU/KVM. Boots via **Limine** (BIOS+UEFI ISO image) — same boot
pipeline as `examples-tulip` and `examples-hyperv`.

Static IP `10.0.2.15/24` matches QEMU SLIRP defaults.

## Build & run

```bash
# From workspace root:
cmake -B build
cmake --build build --target e1000-image    # produces build/e1000.iso
cmake --build build --target qemu-e1000     # boots the ISO under QEMU

# Or run the integration test:
ctest --test-dir build -R e1000-echo --output-on-failure
```

The `e1000-echo` ctest invocation boots the ISO with `-device e1000` +
host port-forward, opens a TCP connection on `localhost:5555`, and
verifies the kernel echoes back.

## What this example shows

- Limine boot via `embclox_hal_x86::limine_boot_requests!` macro
- Single-call HAL boot init (`embclox_hal_x86::init`)
- PCI scan + e1000 driver init from `embclox-e1000`
- IOAPIC routing for the PCI IRQ line (vector 33)
- APIC periodic timer + spurious ISR via `embclox_hal_x86::runtime`
- Embassy executor with `hlt`-on-idle (`runtime::run_executor`)
- TCP echo on port 1234 via `embassy-net`

## Source layout

- `src/main.rs` — boot, init, ISRs, executor
- `linker.ld` — Limine higher-half kernel layout
- `limine.conf` — bootloader entry pointing at `/boot/kernel`
- `CMakeLists.txt` — cargo build + xorriso ISO target + ctest definitions

