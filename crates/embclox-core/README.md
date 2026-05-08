# embclox-core

Glue code shared between the device drivers and the example
binaries.

## Modules

| Module | Purpose |
|--------|---------|
| `dma_alloc` | `BootDmaAllocator` — `DmaAllocator` impl that translates kernel-heap virt addresses to physical via the offsets `embclox_hal_x86::init` derives from Limine (HHDM + kernel virt/phys base) |
| `mmio_regs` | Generic 32-bit MMIO register accessor (`MmioRegs`) used by e1000 |
| `e1000_embassy` | `embassy_net_driver::Driver` impl for `embclox_e1000::E1000Device` |
| `e1000_helpers` | `reset_device(&regs)` — software reset sequence required before `E1000Device::new` |

This crate exists to keep the driver crates pure (no
bootloader-specific code) while still providing ready-to-use
glue for the example binaries.
