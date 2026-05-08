---
name: embclox-examples
description: "How to develop bare-metal x86_64 Rust kernel examples in the embclox repository. Covers the example crate template, shared HAL/runtime modules, build + test workflow, network configuration via Limine cmdline, and the Hyper-V Gen1 / Azure deployment paths."
---

# embclox Example Development

Use this skill when adding a new example kernel under `examples-<name>/`,
modifying an existing one (`examples-e1000`, `examples-tulip`,
`examples-hyperv`), wiring a new device driver into an example, or
investigating why a CI/Hyper-V/Azure run misbehaves.

## Repository orientation

```
embclox/
├── crates/
│   ├── embclox-hal-x86/     # x86_64 HAL: APIC, IOAPIC, PIC, IDT, PIT,
│   │                        #   memory mapper, heap, serial, time
│   │                        #   driver, runtime (executor + APIC timer),
│   │                        #   limine_boot (request macro + collect)
│   ├── embclox-dma/         # DMA allocator trait + DmaRegion
│   ├── embclox-{e1000,tulip,hyperv}/  # device drivers
│   └── embclox-core/        # shared driver glue (e1000_embassy, etc.)
├── examples-e1000/          # Limine boot, e1000 NIC, QEMU/KVM
├── examples-tulip/          # Limine boot, Tulip NIC, QEMU SLIRP
├── examples-hyperv/         # Limine boot, NetVSC over VMBus, Hyper-V/Azure
├── qemu-tests/unit/         # Limine-booted host-side test harness
├── tests/infra/             # bicep templates for Azure deployment
├── scripts/                 # qemu-test.sh, hyperv-*.ps1, mkvhd.sh
├── docs/{design,dev}/       # architecture + dev guides
└── CMakeLists.txt + examples-*/CMakeLists.txt
```

The build system is **CMake on top of Cargo**. Cargo builds the kernel
ELF; CMake assembles ISO/VHD images and drives ctest. Always rebuild
the image artefact (ISO/VHD) before testing — `cargo build` alone is
not enough.

## Standard example structure

Every example follows the same skeleton (see `examples-e1000/src/main.rs`
for the cleanest reference):

1. `#![no_std] #![no_main] #![feature(abi_x86_interrupt)]`
2. **Limine boot setup** — call the HAL macro near the top of `main.rs`:
   ```rust
   embclox_hal_x86::limine_boot_requests!(limine_boot);
   ```
   This declares the standard Limine request statics (HHDM, kernel
   address, cmdline, memory map, framebuffer, stack size) inside a
   private `limine_boot` module and exposes a `collect()` function.
3. Define the kernel entry point: `#[unsafe(no_mangle)] unsafe extern "C" fn kmain() -> !`
4. **HAL init** — single call replaces serial/heap/MemoryMapper bring-up:
   ```rust
   let boot_info = limine_boot::collect();
   let mut p = embclox_hal_x86::init(boot_info, embclox_hal_x86::Config::default());
   // p.serial, p.pci, p.memory ready to use
   embclox_hal_x86::idt::init();          // shared IDT singleton
   embclox_hal_x86::pic::disable();       // remove legacy PIC
   ```
5. **LAPIC**: `let lapic_vaddr = p.memory.map_mmio(LAPIC_PHYS_BASE, 0x1000).vaddr();`
   then `lapic.enable()`.
6. **TSC calibration** + embassy time driver:
   ```rust
   let tsc_per_us = embclox_hal_x86::pit::calibrate_tsc_mhz()
       // On Hyper-V, prefer the synthetic MSR (exact):
       .or_else(|| read_hv_tsc_freq())
       .unwrap_or(default);
   embclox_hal_x86::time::set_tsc_per_us(tsc_per_us);
   ```
7. **Shared runtime** — installs APIC-timer ISR (vector 32) and
   spurious ISR (vector 39):
   ```rust
   embclox_hal_x86::runtime::start_apic_timer(lapic, tsc_per_us, 1_000);
   ```
8. Device discovery (PCI scan or VMBus offer scan), driver init.
9. Register device ISR via `embclox_hal_x86::idt::set_handler(vec, isr)`.
   The ISR must end with `embclox_hal_x86::runtime::lapic_eoi()` unless
   the source is a SynIC SINT vector with auto-EOI (Hyper-V VMBus).
10. Construct embassy-net `Stack` + `Runner` from a `Driver` impl.
11. Spawn `net_task` and an application task (e.g. echo).
12. Hand control to the executor:
    ```rust
    embclox_hal_x86::runtime::run_executor(executor);  // never returns
    ```

**Do not** write `loop { executor.poll(); on_timer_tick(); WAKER.wake(); spin }`
— that defeats the waker pattern and pins the CPU at 100%. Always use
`runtime::run_executor`.

## Network configuration

Network mode is selected at boot via the kernel command line, parsed by
`embclox_hal_x86::cmdline::parse_net_mode`. Tokens:

| Token | Effect |
|-------|--------|
| `net=dhcp` | embassy-net DHCPv4 |
| `net=static` | embassy-net static IPv4, uses `ip=`/`gw=` if present, else defaults |
| `ip=A.B.C.D/N` | Override static IP+prefix |
| `gw=A.B.C.D` | Override static gateway |

Each example provides its own `StaticDefaults` constant (e.g.
`192.168.234.50/24` for the dedicated `embclox-test` Hyper-V vSwitch).

For all examples, configure boot entries in `limine.conf` (and add it to
the ISO custom-command `DEPENDS` so cmake rebuilds when the conf
changes — see `examples-tulip/CMakeLists.txt`). Default boot entry
must match the CI environment:

- QEMU SLIRP CI → `cmdline: net=dhcp` (SLIRP only routes 10.0.2.x)
- Local Hyper-V manual test → `cmdline: net=static` (no DHCP server on
  the Internal vSwitch)
- Azure deployment → `cmdline: net=dhcp` via `limine-azure.conf`
  (Azure's DHCP server is production-grade)

The kernel reads its cmdline from `boot_info.cmdline` returned by
`limine_boot::collect()`.

## Build commands (verified)

```bash
# Per-crate build (host-side check; faster):
cargo check -p embclox-hal-x86

# Per-example build (cross to x86_64-unknown-none via .cargo/config.toml):
cd examples-hyperv && cargo build --release

# Image artefacts (CMake + xorriso/dd) — every example uses the same
# Limine ISO pipeline:
cmake -B build
cmake --build build --target e1000-image     # -> build/e1000.iso
cmake --build build --target unit-test-image # -> build/unit-tests.iso
cmake --build build --target tulip-image     # -> build/tulip.iso
cmake --build build --target hyperv-image    # -> build/hyperv.iso (local)
cmake --build build --target hyperv-vhd      # -> build/hyperv.vhd  (Azure)

# Run all CI tests (5 currently: e1000-echo, unit, tulip-{boot,echo}, hyperv-boot):
ctest --test-dir build --output-on-failure
```

**Workspace-wide `cargo clippy --workspace` FAILS** because each example
is a `no_std` binary with its own `panic_impl`. Always run clippy
per-crate from inside the example directory.

## Test infrastructure

Three layers, in increasing fidelity:

### 1. ctest (CI, ~60s total)

`scripts/qemu-test.sh` boots an ISO under QEMU UEFI and either:
- waits for a log line (`--log-match "TULIP INIT PASSED"`), or
- probes a TCP port (`--probe tcp:5556:hello-tulip`).

When adding a new example, mirror the pattern in `examples-tulip/CMakeLists.txt`:

```cmake
add_test(NAME <name>-boot
    COMMAND bash ${CMAKE_SOURCE_DIR}/scripts/qemu-test.sh
        ${ISO} --iso --log-match "<example>-init-marker" --timeout 15
        --qemu-args "-netdev user,id=net0 -device <model>,netdev=net0 -m 256M"
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)
```

### 2. Local Hyper-V Gen1 (manual, ~90s)

```powershell
# One-time, as Administrator (creates embclox-test Internal vSwitch on 192.168.234.0/24):
.\scripts\hyperv-setup-vswitch.ps1

# Per-test (from WSL or PowerShell):
powershell.exe -ExecutionPolicy Bypass -File scripts/hyperv-boot-test.ps1 \
    -Iso build/hyperv.iso
```

The script creates a Gen1 VM, attaches the ISO, reads serial via named
pipe, parses `PHASE4B: IPv4 configured: ...`, then probes TCP 1234. See
`docs/dev/HyperV-Testing.md` for why we use a dedicated Internal
vSwitch instead of the Default Switch (ICS pollution writeup).

### 3. Azure Gen1 (manual, ~5 min)

```bash
# 1. Build VHD (uses limine-azure.conf with net=dhcp)
cmake --build build --target hyperv-vhd

# 2. Create storage RG + storage account, upload VHD (one time per VHD):
az group create --name embclox-storage --location <region>
az deployment group create --resource-group embclox-storage \
    --template-file tests/infra/storage.bicep
# (then az storage blob upload --container vhds --file build/hyperv.vhd ...)

# 3. Create VM RG + VM (per test session):
az group create --name embclox-vm --location <region>
az deployment group create --resource-group embclox-vm \
    --template-file tests/infra/vm.bicep \
    --parameters storageResourceGroup=embclox-storage vhdName=hyperv.vhd

# 4. Verify TCP echo from public Internet:
echo HELLO | nc <public-ip> 1234   # returns HELLO

# 5. Cleanup:
az group delete --name embclox-vm --yes --no-wait
```

Auto-shutdown is configured in `vm.bicep` to bound costs.

## Writing a device ISR

Device interrupt service routines live in the example crate (the
register pokes are device-specific). Template:

```rust
extern "x86-interrupt" fn my_handler(_: InterruptStackFrame) {
    // 1. Acknowledge the device (read ICR / clear status bit / etc).
    //    DO NOT log from inside an ISR — printing serial is slow and
    //    the executor is suspended.
    unsafe { ack_my_device(); }

    // 2. Wake the embassy task that owns this device's data path.
    MY_DRIVER_WAKER.wake();

    // 3. End-of-Interrupt to the LAPIC.
    embclox_hal_x86::runtime::lapic_eoi();
}
```

Hyper-V SynIC SINT vectors are special: they configure auto-EOI in the
SINT MSR (bit 17) and so must NOT call `lapic_eoi`. See
`examples-hyperv/src/main.rs::vmbus_isr`.

Register the handler **after** `idt::init()`:

```rust
unsafe { embclox_hal_x86::idt::set_handler(VECTOR, my_handler); }
```

For PCI device IRQs that need to actually deliver (e1000), also enable
IOAPIC routing:

```rust
ioapic.enable_irq(pci_irq_line, VECTOR, /* lapic_id */ 0);
```

## Driver crate vs example split

Driver crates (`embclox-e1000`, `embclox-tulip`, `embclox-hyperv`) are
pure `no_std` libraries — they take a generic `DmaAllocator` and don't
know about bootloaders. The example crate is responsible for:

- Declaring Limine request statics via `embclox_hal_x86::limine_boot_requests!`
- Calling `embclox_hal_x86::init(boot_info, Config::default())` for HAL
  bring-up (serial, heap, memory mapper)
- Providing the `DmaAllocator` impl (often a bump allocator over an
  HHDM-mapped region from `limine_boot::MEMMAP_REQUEST`, or
  `BootDmaAllocator` for heap-backed DMA)
- Wiring interrupts (IDT vector, IOAPIC route, ISR body)
- Selecting the embassy-net config (DHCP/static via `boot_info.cmdline`)

If you find yourself writing bootloader-specific code inside a driver
crate, that's a smell — push it back into the example.

## Common pitfalls

- **Stale ISO**: cmake's ISO custom-command must `DEPENDS` on
  `limine.conf`/`limine-azure.conf`, otherwise a config-only edit will
  silently use the previous image. CI builds fresh; local builds don't.
- **Static IP on QEMU SLIRP**: SLIRP gives 10.0.2.x — a static IP from
  any other subnet is unreachable. Default cmdline must be `net=dhcp`
  for QEMU CI tests.
- **Default Switch on Hyper-V**: ICS pre-installs Permanent ARP entries
  for the entire NAT scope. Always test on `embclox-test`.
- **Forgetting `pic::disable()`**: PIT IRQ 0 fires on vector 32 by
  default — same vector our APIC timer ISR uses. Always disable.
- **ISR logging**: don't `info!`/`writeln!` inside an ISR; serial port
  writes are blocking and the executor is suspended.
- **Two `static`s for the time driver**: only ONE `ApicTimeDriver`
  static may exist; register it via `embassy_time_driver::time_driver_impl!`
  and mutate the SAME instance from `set_tsc_per_us` and `on_timer_tick`.

## Reference docs

- `docs/design/hal-x86.md` — HAL architecture
- `docs/design/hyperv-netvsc.md` — NetVSC + LAPIC runtime detail
- `docs/design/vmbus.md` — VMBus implementation
- `docs/design/test-framework.md` — qemu-test.sh design
- `docs/dev/Setup.md` — toolchain + qemu setup
- `docs/dev/HyperV-Testing.md` — Hyper-V vSwitch + ICS pollution
- `tests/infra/README.md` — Azure two-RG deployment guide

## Commit conventions

Per `.github/copilot-instructions.md`: never `git add` or `git commit`
unless the user explicitly asks. Commit messages should explain the
*why* of the change in the body, list verification steps performed
(ctest output, manual Hyper-V/Azure run), and end with the standard
`Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>`
trailer.
