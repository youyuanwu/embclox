# embclox

Bare-metal `no_std` Rust kernels for x86_64 with `embassy-net` TCP/IP,
targeting QEMU and Hyper-V (including Azure Gen1).

## What's here

| Crate / dir | Purpose |
|-------------|---------|
| [`crates/embclox-hal-x86/`](crates/embclox-hal-x86/README.md) | x86_64 HAL: APIC, IOAPIC, PIC, IDT, PIT, memory mapper, heap, serial, embassy-time driver, shared executor + APIC-timer runtime |
| [`crates/embclox-async/`](crates/embclox-async/README.md) | Pure-`core` one-future runner (`block_on_with`); `block_on_hlt` wrapper for boot-time async waits |
| [`crates/embclox-dma/`](crates/embclox-dma/README.md) | `DmaAllocator` trait + `DmaRegion` |
| [`crates/embclox-e1000/`](crates/embclox-e1000/README.md) | Intel e1000 NIC driver |
| [`crates/embclox-tulip/`](crates/embclox-tulip/README.md) | DEC 21143 (Tulip) NIC driver |
| [`crates/embclox-hyperv/`](crates/embclox-hyperv/README.md) | Hyper-V VMBus + NetVSC driver |
| [`crates/embclox-core/`](crates/embclox-core/README.md) | Shared driver glue (e1000_embassy, BootDmaAllocator, etc.) |
| [`examples-e1000/`](examples-e1000/README.md) | Limine boot, e1000 NIC, QEMU/KVM TCP echo |
| [`examples-tulip/`](examples-tulip/README.md) | Limine boot, Tulip NIC, QEMU SLIRP TCP echo |
| [`examples-hyperv/`](examples-hyperv/README.md) | Limine boot, NetVSC over VMBus, local Hyper-V + Azure Gen1 TCP echo |
| `qemu-tests/unit/` | no_std unit tests run inside QEMU |
| [`tests/infra/`](tests/infra/README.md) | Bicep templates for Azure Gen1 deployment |
| `scripts/` | `qemu-test.sh`, `hyperv-*.ps1`, `mkvhd.sh` |

## Build & test

```bash
cmake -B build
cmake --build build --target tulip-image hyperv-image
ctest --test-dir build --output-on-failure   # 5 tests, ~60s
```

Per-crate (faster):

```bash
cargo check -p embclox-hal-x86
cargo test  -p embclox-async        # host-side unit tests
cd examples-hyperv && cargo build --release
```

`cargo clippy --workspace` does NOT work — examples are `no_std`
binaries with their own `panic_impl`. Run clippy per-crate.

## Running an example

**QEMU (Tulip):**
```bash
cmake --build build --target qemu-tulip
```

**Local Hyper-V Gen1 (NetVSC):**
```powershell
.\scripts\hyperv-setup-vswitch.ps1   # one-time, as Administrator
powershell.exe -ExecutionPolicy Bypass -File scripts/hyperv-boot-test.ps1 \
    -Iso build/hyperv.iso
```

**Azure Gen1:** see [`tests/infra/README.md`](tests/infra/README.md).

## Network configuration

Boot entries select DHCP vs static IP via Limine cmdline (parsed by
`embclox_hal_x86::cmdline`). Tokens: `net=dhcp`, `net=static`,
`ip=A.B.C.D/N`, `gw=A.B.C.D`.

## Documentation

- `docs/dev/Setup.md` — toolchain + QEMU setup
- `docs/dev/HyperV-Testing.md` — Hyper-V vSwitch + ICS pollution writeup
- `docs/design/hal-x86.md` — HAL architecture
- `docs/design/hyperv-netvsc.md` — NetVSC + LAPIC runtime
- `docs/design/vmbus.md` — VMBus channel protocol
- `docs/design/async-boot-init.md` — `block_on_hlt` async-init runner
- `docs/design/test-framework.md` — `qemu-test.sh` design