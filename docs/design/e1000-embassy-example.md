# Design: E1000 Embassy Networking Example

## Overview

A bare-metal TCP echo server on x86_64 QEMU using the `e1000` driver crate
with [Embassy](https://embassy.dev)'s async executor and `embassy-net` (smoltcp).
Boots via the `bootloader` crate, runs in polling mode with static IP, tested
via `test.sh`.

## Architecture

```
┌─────────────────────────────────────────────┐
│  Application (TCP echo on port 1234)        │
├─────────────────────────────────────────────┤
│  embassy-net (IP/ARP/TCP via smoltcp)       │
├─────────────────────────────────────────────┤
│  Embassy adapter (UnsafeCell + split())     │
├─────────────────────────────────────────────┤
│  crates/e1000 (RegisterAccess + DmaAlloc)   │
├─────────────────────────────────────────────┤
│  Platform: bootloader, heap, PCI, serial    │
└─────────────────────────────────────────────┘
        ↕ MMIO (UC-mapped)      ↕ DMA (phys_offset-mapped)
┌─────────────────────────────────────────────┐
│  QEMU x86_64 q35 + e1000 NIC               │
└─────────────────────────────────────────────┘
```

## Key Design Decisions

**Caller does device reset** — `main.rs` performs CTRL_RST, waits for
clear, sets SLU|ASDE, disables flow control, and re-enables PCI bus
mastering before calling `E1000Device::new()`.

**UnsafeCell for Embassy adapter** — Embassy's `Driver::receive()` returns
`(RxToken, TxToken)` from `&mut self`. The adapter wraps the device in
`UnsafeCell` and each token calls `split()` on consume. Safe because
smoltcp consumes tokens sequentially.

**Waker re-scheduling** — `receive()`/`transmit()`/`link_state()` call
`cx.waker().wake_by_ref()` on `None` returns to keep the `platform-spin`
executor polling.

**DMA through `phys_offset`** — `DmaAllocator` returns addresses via the
bootloader's physical memory mapping for QEMU TCG DMA coherency.

**UC MMIO mapping** — BAR0 remapped at `0x4000_0000_0000` with `NO_CACHE`
via `OffsetPageTable::map_to` (bootloader maps all phys memory as WB).

**Gratuitous ARP** — QEMU slirp workaround: a TX forces re-evaluation of
RX readiness after init.

## Project Layout

```
_examples_embassy/
    ├── .cargo/config.toml       # target = x86_64-unknown-none
    ├── Cargo.toml
    ├── test.sh                  # QEMU boot + TCP echo verification
    └── src/
        ├── main.rs              # boot, reset, init, executor, echo task
        ├── e1000_adapter.rs     # embassy-net-driver impl (UnsafeCell + split)
        ├── mmio_regs.rs         # RegisterAccess impl for UC-mapped MMIO
        ├── kernfn.rs            # DmaAllocator impl (phys_offset mapping)
        ├── mmio.rs              # UC page mapping for BAR0
        ├── pci_init.rs          # x86 I/O port PCI scanner
        ├── heap.rs              # linked_list_allocator on BSS
        ├── serial.rs            # UART 16550 (COM1)
        ├── logger.rs            # log crate → serial
        ├── time_driver.rs       # TSC-based embassy-time stub
        └── critical_section_impl.rs

crates/e1000/                    # e1000 driver crate (see e1000-driver-refactor.md)
tools/mkimage/                   # BIOS disk image builder
```

Build: `cmake -B build && cmake --build build --target image`
Test: `cmake --build build --target test`

## Toolchain & Dependencies

Stable Rust with `RUSTC_BOOTSTRAP=1` (root `.cargo/config.toml`).
Key crates: `e1000` (local), `embassy-executor 0.10`, `embassy-net 0.9`,
`bootloader_api 0.11`, `x86_64 0.15`, `uart_16550 0.6`.
Static IP `10.0.2.15/24`, gateway `10.0.2.2` (QEMU slirp defaults).

## Future Work

- Interrupt-driven mode (IDT, IOAPIC, APIC timer)
- DHCP (requires working embassy-time alarms)
- Dynamic `kernel_offset` calculation (currently hardcoded `0xFFFF000000`)

## References

- [Embassy](https://embassy.dev) / [embassy-net](https://docs.embassy.dev/embassy-net/)
- [bootloader crate](https://docs.rs/bootloader/)
- [Intel 82540 SDM](https://pdos.csail.mit.edu/6.828/2019/readings/hardware/8254x_GBe_SDM.pdf)
