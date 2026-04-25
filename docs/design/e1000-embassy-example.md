# Design: E1000 Embassy Networking Example

## Overview

A bare-metal TCP echo server on x86_64 QEMU using `crates/e1000` driver
and `crates/hal-x86` platform HAL with [Embassy](https://embassy.dev)'s
async executor and `embassy-net` (smoltcp). Tested via `test.sh`.

## Architecture

```
┌─────────────────────────────────────────────┐
│  Application (TCP echo on port 1234)        │
├─────────────────────────────────────────────┤
│  embassy-net (IP/ARP/TCP via smoltcp)       │
├─────────────────────────────────────────────┤
│  Embassy adapter (UnsafeCell + split())     │
├──────────────┬──────────────────────────────┤
│  crates/e1000│  crates/hal-x86             │
│  (driver)    │  (serial, PCI, MMIO, heap)  │
└──────────────┴──────────────────────────────┘
        ↕ MMIO (UC-mapped)      ↕ DMA (phys_offset)
┌─────────────────────────────────────────────┐
│  QEMU x86_64 q35 + e1000 NIC               │
└─────────────────────────────────────────────┘
```

## Key Design Decisions

**Caller does device reset** — `main.rs` performs CTRL_RST, waits for
clear, sets SLU|ASDE, disables flow control, re-enables PCI bus
mastering, then calls `E1000Device::new()`.

**UnsafeCell for Embassy adapter** — Embassy's `Driver::receive()` needs
both tokens from `&mut self`. Adapter wraps device in `UnsafeCell`, each
token calls `split()` on consume. Safe: smoltcp consumes sequentially.

**Waker re-scheduling** — `cx.waker().wake_by_ref()` on `None` returns
keeps the `platform-spin` executor polling.

**DMA through `phys_offset`** — QEMU TCG DMA coherency requires
addresses via the bootloader's physical memory mapping.

**Gratuitous ARP** — QEMU slirp workaround for RX readiness.

## Project Layout

```
_examples_embassy/
    ├── .cargo/config.toml       # target = x86_64-unknown-none
    ├── Cargo.toml
    ├── test.sh                  # QEMU boot + TCP echo verification
    └── src/
        ├── main.rs              # boot, reset, init, executor, echo task
        ├── e1000_adapter.rs     # embassy-net-driver impl
        ├── dma_alloc.rs         # e1000::DmaAllocator impl
        └── mmio_regs.rs         # e1000::RegisterAccess impl

crates/e1000/                    # driver (see e1000-driver-refactor.md)
crates/hal-x86/                  # platform HAL (see hal-x86.md)
tools/mkimage/                   # BIOS disk image builder
```

Build: `cmake -B build && cmake --build build --target image`
Test: `cmake --build build --target test`

## Toolchain

Stable Rust with `RUSTC_BOOTSTRAP=1` (root `.cargo/config.toml`).
Static IP `10.0.2.15/24`, gateway `10.0.2.2` (QEMU slirp defaults).

## Future Work

- Interrupt-driven mode (IDT, IOAPIC, APIC timer)
- DHCP (requires working embassy-time alarms)

## References

- [Embassy](https://embassy.dev) / [embassy-net](https://docs.embassy.dev/embassy-net/)
- [bootloader crate](https://docs.rs/bootloader/)
- [Intel 82540 SDM](https://pdos.csail.mit.edu/6.828/2019/readings/hardware/8254x_GBe_SDM.pdf)
