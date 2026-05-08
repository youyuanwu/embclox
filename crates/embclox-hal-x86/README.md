# embclox-hal-x86

x86_64 hardware abstraction layer for the embclox example kernels.
Standardised on the **Limine boot protocol** (the framework's only supported
bootloader); the per-example bootloader divergence is gone.

## Modules

| Module | What |
|--------|------|
| `apic` | Local APIC (xAPIC MMIO): enable, periodic + one-shot timer, EOI |
| `ioapic` | I/O APIC: route external IRQs to LAPIC vectors |
| `pic` | Legacy 8259 PIC (only used to disable it before APIC takes over) |
| `idt` | Shared IDT singleton + `set_handler` |
| `pit` | TSC calibration via PIT channel 2 (bounded; returns `None` on Hyper-V Gen1 where PIT ch2 isn't emulated) |
| `time` | `embassy_time_driver::Driver` impl over TSC + alarm table |
| `runtime` | Shared APIC-timer ISR + executor loop + `block_on_hlt` |
| `memory` | Page-table mapper for MMIO ranges (`map_mmio`) |
| `heap` | Global heap (`linked_list_allocator::LockedHeap`) |
| `serial` | UART 16550 driver + `log` backend |
| `pci` | Type-1 PCI config-space scanner |
| `cmdline` | Bootloader-agnostic `net=dhcp` / `net=static` parser |
| `limine_boot` | Limine request statics (via macro) + `LimineBootInfo` collector |

## Boot init (the part every kernel runs)

```rust
// In your kmain crate:
embclox_hal_x86::limine_boot_requests!(limine_boot);

#[unsafe(no_mangle)]
unsafe extern "C" fn kmain() -> ! {
    let boot_info = limine_boot::collect();
    let mut p = embclox_hal_x86::init(
        boot_info,
        embclox_hal_x86::Config::default(),
    );
    // p: Peripherals { serial, pci: PciBus, memory: MemoryMapper }
    // ...
}
```

`limine_boot_requests!(<mod_name>)` declares the standard Limine request
statics (`HhdmRequest`, `ExecutableAddressRequest`, `ExecutableCmdlineRequest`,
`MemoryMapRequest`, `FramebufferRequest`, `StackSizeRequest`) inside a
private module in the calling crate. The generated `<mod_name>::collect()`
function reads the responses and returns a `LimineBootInfo` carrying the
HHDM offset, kernel virt/phys offset, and command-line string.

`MEMMAP_REQUEST` and `FRAMEBUFFER_REQUEST` are re-exported from the macro
module so kernels that need raw access (e.g. tulip/hyperv DMA pool init)
can still read them directly.

## Runtime API (post-`init`)

```rust
embclox_hal_x86::idt::init();
embclox_hal_x86::pic::disable();

let lapic_vaddr = p.memory.map_mmio(apic::LAPIC_PHYS_BASE, 0x1000).vaddr();
let mut lapic = LocalApic::new(lapic_vaddr);
lapic.enable();

let tsc_per_us = pit::calibrate_tsc_mhz().unwrap_or(default);
embclox_hal_x86::time::set_tsc_per_us(tsc_per_us);

embclox_hal_x86::runtime::start_apic_timer(lapic, tsc_per_us, 1_000);
// ... spawn embassy tasks ...
embclox_hal_x86::runtime::run_executor(executor);     // never returns
```

`runtime::block_on_hlt(future)` runs a single future to completion,
halting the CPU between polls — useful for boot-time async waits
before the embassy executor is up.

## Example crates that use this HAL

- `examples-e1000` — Intel e1000 NIC on QEMU
- `examples-tulip` — DEC Tulip NIC on QEMU + Hyper-V Gen1
- `examples-hyperv` — NetVSC synthetic NIC on Hyper-V/Azure Gen1
- `qemu-tests/unit` — host-side HAL/driver test harness

All four boot via Limine (BIOS+UEFI ISO from `cmake --build build --target
{name}-image`).

(No host-side tests — the global allocator prevents `cargo test`.
See `embclox-async` for testable runner logic.)

