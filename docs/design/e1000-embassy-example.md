# Design: E1000 Embassy Networking Example

## Overview

A bare-metal TCP echo server on x86_64 QEMU, wrapping the `e1000-driver` crate
with [Embassy](https://embassy.dev)'s async executor and `embassy-net` (smoltcp)
networking stack. Boots via the `bootloader` crate, runs in polling mode with
static IP, and is tested automatically via `test.sh`.

## Architecture

```
┌─────────────────────────────────────────────┐
│  Application (TCP echo on port 1234)        │
├─────────────────────────────────────────────┤
│  embassy-net (IP/ARP/TCP via smoltcp)       │
├─────────────────────────────────────────────┤
│  embassy-net-driver adapter (UnsafeCell)    │
├─────────────────────────────────────────────┤
│  e1000-driver crate (E1000Device)           │
├─────────────────────────────────────────────┤
│  Platform: bootloader, heap, PCI, serial    │
└─────────────────────────────────────────────┘
        ↕ MMIO (UC-mapped)      ↕ DMA (phys_offset-mapped)
┌─────────────────────────────────────────────┐
│  QEMU x86_64 q35 + e1000 NIC               │
└─────────────────────────────────────────────┘
```

## E1000 Driver Modifications

Changes made to `external/e1000/src/e1000/e1000_inner.rs`:

1. **Fix `e1000_recv()` inverted return** — `Some` = data, `None` = nothing
2. **Fix reset sequence** — wait for `CTRL_RST` clear, set `SLU|ASDE`, disable flow control (per Redox OS reference)
3. **Add `e1000_recv_with(f)`** — zero-copy callback receive; passes DMA buffer directly to closure. Checks DD+EOP and errors before dispatch
4. **Add `has_rx_packet()` / `has_tx_space()`** — peek methods for the adapter's `receive()` guard
5. **Remove `net_rx()` stub** — was a no-op touching packet data prematurely
6. **Enable promiscuous mode** in RCTL (`UPE|MPE`)

## Key Design Decisions

**UnsafeCell for dual-token borrowing** — Embassy's `Driver::receive()` returns
`(RxToken, TxToken)` from `&mut self`. `UnsafeCell<E1000Device>` allows both
tokens to reference the device. Safe because smoltcp consumes them sequentially.

**Waker re-scheduling** — `receive()`/`transmit()`/`link_state()` call
`cx.waker().wake_by_ref()` on `None` returns. Without this, Embassy's
`platform-spin` executor starves the runner task.

**DMA vaddr through `phys_offset`** — `dma_alloc_coherent()` returns addresses
via the bootloader's physical memory mapping (not kernel segment) for QEMU TCG
DMA coherency.

**UC MMIO mapping** — The bootloader maps all physical memory as WB-cached 2MiB
pages. QEMU TCG doesn't dispatch MMIO through WB pages. BAR0 is remapped at
`0x4000_0000_0000` with `NO_CACHE` via `OffsetPageTable::map_to`.

**Bus mastering re-enable** — Device reset clears PCI command register. Must
re-enable bus mastering after `E1000Device::new()`. Use 16-bit PCI writes.

**Gratuitous ARP** — QEMU slirp caches RX readiness at boot. A TX (gratuitous
ARP) forces re-evaluation after init completes.

## Project Layout

```
_examples_embassy/               # workspace member (excluded from default-members)
    ├── .cargo/config.toml       # target = x86_64-unknown-none
    ├── Cargo.toml               # edition = 2024
    ├── Makefile                 # build / image / qemu / run
    ├── test.sh                  # automated QEMU boot + TCP echo test
    └── src/
        ├── main.rs              # entry point, executor, TCP echo task
        ├── e1000_adapter.rs     # embassy-net-driver impl
        ├── kernfn.rs            # KernelFunc (DMA alloc, phys_offset)
        ├── mmio.rs              # UC page mapping for BAR0
        ├── pci_init.rs          # x86 I/O port PCI scanner
        ├── heap.rs              # linked_list_allocator on BSS
        ├── serial.rs            # UART 16550 (COM1)
        ├── logger.rs            # log crate → serial
        ├── time_driver.rs       # TSC-based embassy-time stub
        └── critical_section_impl.rs

tools/mkimage/                   # workspace member — BIOS disk image builder
```

Build from `_examples_embassy/`: `make image` then `make qemu`, or just `./test.sh`.

## Toolchain & Dependencies

Stable Rust with `RUSTC_BOOTSTRAP=1` (root `.cargo/config.toml`).
Key crates: `embassy-executor 0.10` (`platform-spin`), `embassy-net 0.9`,
`embassy-net-driver 0.2`, `bootloader_api 0.11`, `x86_64 0.15`.
Static IP `10.0.2.15/24`, gateway `10.0.2.2` (QEMU slirp defaults).

## Future Work

- Interrupt-driven mode (IDT, IOAPIC, APIC timer)
- DHCP (requires working embassy-time alarms)
- Proper TSC calibration
- Dynamic `kernel_offset` calculation (currently hardcoded `0xFFFF000000`)

## References

- [Redox OS e1000d](https://github.com/redox-os/drivers/blob/master/net/e1000d/src/device.rs) — reset sequence reference
- [Embassy](https://embassy.dev) / [embassy-net](https://docs.embassy.dev/embassy-net/)
- [bootloader crate](https://docs.rs/bootloader/)
- [Intel 82540 SDM](https://pdos.csail.mit.edu/6.828/2019/readings/hardware/8254x_GBe_SDM.pdf)
