# embclox-e1000

Minimal `no_std` driver for the Intel 8254x (e1000) family of NICs.
Generic over `embclox_dma::DmaAllocator`.

## API

```rust
let mut device = E1000Device::new(regs, dma);
let mac = device.mac_address();
device.enable_interrupts();

let (mut rx, mut tx) = device.split();
tx.transmit(&frame);                  // blocking
let pkt = rx.receive();               // returns Option<Frame>
```

Caller is responsible for:
- PCI scan + BAR0 mapping (`embclox_hal_x86::pci`)
- Wrapping BAR0 in `embclox_e1000::MmioRegs::new(base_vaddr)`
- Calling `embclox_e1000::reset_device(&regs)` first
- Enabling bus mastering on the PCI device
- Routing the device IRQ via the IOAPIC + installing an ISR that
  reads ICR + wakes the embassy waker

For the embassy-net `Driver` impl + canonical wiring, see the
registry wrapper at
`crates/embclox-driver/src/drivers/e1000.rs` (used by
`examples-kernel`).

## Supported devices

- 0x100E (82540EM)
- 0x100F (82545EM)
- 0x10D3 (82574L)
