# Design: x86_64 Bare-Metal HAL

## Overview

A Hardware Abstraction Layer for x86_64 bare-metal (QEMU + `bootloader`
crate) following Embassy HAL conventions. Provides platform primitives
(serial, PCI, memory mapping, heap, timers, critical sections) that
application and driver glue code can build on.

**Key principle:** The HAL provides platform services but does NOT
implement driver-specific traits. Driver trait implementations (e.g.,
`e1000::DmaAllocator`, `e1000::RegisterAccess`) live in the application
code, using HAL primitives internally.

```
┌──────────────────────────────────────────────┐
│  Application / Example                       │
│  - Implements e1000::DmaAllocator            │
│    using hal_x86::MemoryMapper               │
│  - Implements e1000::RegisterAccess           │
│    using hal_x86::memory::map_mmio()         │
├──────────────┬───────────────────────────────┤
│  crates/e1000│  crates/hal-x86              │
│  Driver      │  Platform primitives          │
│  (no platform│  (no driver knowledge)        │
│   deps)      │                               │
└──────────────┴───────────────────────────────┘
```

## Initialization

```rust
use hal_x86::{Config, Peripherals};

// init() can only be called once (AtomicBool guard, panics on second call).
// kernel_offset is computed from BootInfo, not configured.
let p: Peripherals = hal_x86::init(boot_info, Config::default());
```

```rust
pub struct Config {
    pub serial_port: u16,         // default: 0x3F8 (COM1)
    pub heap_size: usize,         // default: 1 MiB
}

pub struct Peripherals {
    pub serial: Serial,
    pub pci: PciBus,
    pub memory: MemoryMapper,
}
```

`init()` performs in order: serial → heap → memory mapper. Panics if
heap size is 0 or no usable memory in `BootInfo`. Computes
`kernel_offset` and `phys_offset` from `BootInfo` at runtime.

## Modules

### `serial` — UART 16550

```rust
pub struct Serial { /* Uart16550Tty<PioBackend> */ }
impl core::fmt::Write for Serial { ... }
```

Includes `log` crate integration. Exports `serial_print!`,
`serial_println!` macros. Panics if port is not responding at init.

### `pci` — PCI Bus Scanner

```rust
pub struct PciBus;

pub struct PciDevice {
    pub bus: u8, pub dev: u8, pub func: u8,
    pub vendor: u16, pub device: u16,
}

impl PciBus {
    pub fn find_device(&self, vendor: u16, device: u16) -> Option<PciDevice>;
    pub fn enable_bus_mastering(&self, dev: &PciDevice);
    pub fn read_bar(&self, dev: &PciDevice, bar: u8) -> u64;
    pub fn read_config(&self, dev: &PciDevice, offset: u8) -> u32;
    pub fn write_config(&self, dev: &PciDevice, offset: u8, val: u32);
}
```

`find_device` returns `Option` — caller decides how to handle absence.
BAR values are read dynamically per device (not stored in `PciDevice`).

### `memory` — MMIO & Physical Memory

```rust
pub struct MemoryMapper { /* phys_offset, kernel_offset, next_vaddr */ }

impl MemoryMapper {
    /// Map MMIO region with Uncacheable pages.
    /// Returns virtual address. Each call maps at a new vaddr
    /// (internal cursor advances, no overlap).
    pub fn map_mmio(&mut self, phys_base: u64, size: u64) -> usize;

    /// Bootloader's physical memory offset.
    pub fn phys_offset(&self) -> u64;

    /// Kernel virtual-to-physical offset (computed from BootInfo).
    pub fn kernel_offset(&self) -> u64;
}
```

`map_mmio` uses an advancing virtual address cursor (starts at
`0x4000_0000_0000`, increments by mapped size + guard page). No more
hardcoded overlap on repeated calls.

### `heap` — Global Allocator

```rust
/// Initialize global heap from bootloader memory map.
/// Panics if heap_size is 0 or no usable memory region found.
pub fn init(boot_info: &'static mut BootInfo, heap_size: usize);
```

### `time` — TSC Time Driver

Embassy `time-driver` implementation using `rdtsc`. `schedule_wake()`
always wakes immediately (busy-poll). Future: APIC timer.

### `critical_section` — CLI/STI

`critical-section` crate implementation using x86 interrupt
disable/enable.

## File structure

```
crates/hal-x86/
    ├── Cargo.toml
    └── src/
        ├── lib.rs              # init(), Config, Peripherals
        ├── serial.rs           # UART + log + macros
        ├── pci.rs              # x86 I/O port PCI
        ├── memory.rs           # MMIO mapping, virt/phys offsets
        ├── heap.rs             # Global allocator
        ├── time.rs             # TSC embassy-time-driver
        └── critical_section_impl.rs
```

## Future work

- Interrupt support (IDT, `bind_interrupts!`, IOAPIC)
- APIC timer (proper `embassy-time` alarms)
- `embedded-io::Write` for serial
- Multiple serial ports (COM1-COM4)
- ACPI for real hardware platform discovery

## References

- [Embassy](https://embassy.dev)
- [embassy-stm32](https://github.com/embassy-rs/embassy/tree/main/embassy-stm32)
- [embassy-nrf](https://github.com/embassy-rs/embassy/tree/main/embassy-nrf)
