# embclox-tulip

Minimal `no_std` driver for the DEC 21143 ("Tulip") NIC. Generic
over `embclox_dma::DmaAllocator`. Supports both MMIO and I/O port
CSR access (`CsrAccess::Mmio` / `CsrAccess::Io`); QEMU exposes
the device as I/O port BAR0 by default.

## API

```rust
let csr = CsrAccess::Io(io_base);     // or CsrAccess::Mmio(vaddr)
let mut device = TulipDevice::new(csr, dma);
let mac = device.mac();
device.enable_interrupts();

device.transmit_with(len, |buf| { ... });
device.try_receive(&mut buf);
```

Caller is responsible for:
- PCI scan + BAR0 + bus-mastering enable
- IDT + IOAPIC routing for the device IRQ
- ISR that ack/clears CSR5 status bits + wakes the embassy waker

For the embassy-net `Driver` impl + canonical wiring, see
`examples-kernel/src/main.rs` and
`crates/embclox-driver/src/drivers/tulip.rs`.
