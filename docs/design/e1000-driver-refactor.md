# Design: E1000 Driver Crate

## Overview

The `crates/e1000` crate is a clean e1000 NIC driver extracted from the
[elliott10/e1000-driver](https://github.com/elliott10/e1000-driver) fork.
It provides trait-based abstractions for register access and DMA, a
`split()` API for concurrent RX/TX use, and zero-copy packet I/O.

`no_std`, no `alloc` dependency. The original `external/e1000` is
retained as reference but removed from the workspace.

## Traits

### RegisterAccess

```rust
/// MMIO register access. Offset is a word index (byte offset / 4).
/// Uses &self — MMIO side-effects are in hardware, not Rust memory.
pub trait RegisterAccess {
    fn read_reg(&self, offset: usize) -> u32;
    fn write_reg(&self, offset: usize, value: u32);
}
```

Implementations must use volatile reads/writes. If `R: Copy` (e.g., a
raw base pointer wrapper), each split half gets its own copy.

### DmaAllocator

```rust
pub struct DmaRegion { pub vaddr: usize, pub paddr: usize, pub size: usize }

pub trait DmaAllocator {
    fn alloc_coherent(&self, size: usize, align: usize) -> DmaRegion;  // panics on failure
    fn free_coherent(&self, region: &DmaRegion);
}
```

Uses `&self` (interior mutability for stateful allocators). Panics on
failure — boot-time, fixed system, no recovery path. The driver makes
4 bulk allocations (TX ring, RX ring, TX buffers, RX buffers).

## Device API

```rust
impl<R: RegisterAccess, D: DmaAllocator> E1000Device<R, D> {
    /// Caller must reset device and re-enable PCI bus mastering first.
    pub fn new(regs: R, dma: D) -> Self;
    pub fn mac_address(&self) -> [u8; 6];
    pub fn link_is_up(&self) -> bool;
    pub fn split(&mut self) -> (RxHalf<'_, R>, TxHalf<'_, R>);

    // Interrupts — &self, callable even while split
    pub fn enable_interrupts(&self);
    pub fn disable_interrupts(&self);
    pub fn handle_interrupt(&self) -> InterruptStatus;
}

impl RxHalf<'_, R> {
    pub fn has_rx_packet(&self) -> bool;
    pub fn recv_with<T>(&mut self, f: impl FnOnce(&mut [u8]) -> T) -> Option<T>;
}

impl TxHalf<'_, R> {
    pub fn has_tx_space(&self) -> bool;
    pub fn transmit(&mut self, packet: &[u8]);
    pub fn transmit_with<T>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> T) -> Option<T>;
}
```

### Split design

`RxHalf` and `TxHalf` share `&R` (register access via `&self`) and hold
`&mut` to disjoint ring state. Interrupt methods use `&self` so they
remain callable while split. The Embassy adapter still uses `UnsafeCell`
because the `Driver::receive()` trait signature requires both tokens from
`&mut self`, but each token calls `split()` independently on consume.

## Caller responsibilities

Before calling `new()`, the caller must:
1. Perform device reset (write `CTRL_RST`, wait for clear)
2. Re-enable PCI bus mastering (reset clears command register)
3. Set `CTRL_SLU | CTRL_ASDE` (link up, auto-speed)
4. Clear flow control registers (FCAL, FCAH, FCT, FCTTV)

`new()` handles post-reset init: TX/RX ring setup, TCTL, RCTL,
interrupt configuration.

## File structure

```
crates/e1000/
    ├── Cargo.toml    # no_std, log dependency only
    └── src/
        ├── lib.rs    # re-exports
        ├── regs.rs   # RegisterAccess trait + register constants
        ├── dma.rs    # DmaAllocator trait + DmaRegion
        ├── desc.rs   # TxDesc, RxDesc, ring state, constants
        ├── device.rs # E1000Device, RxHalf, TxHalf, Drop
        └── error.rs  # InterruptStatus
```

## References

- [Intel 82540 SDM](https://pdos.csail.mit.edu/6.828/2019/readings/hardware/8254x_GBe_SDM.pdf)
- [Redox OS e1000d](https://github.com/redox-os/drivers/blob/master/net/e1000d/src/device.rs)
- [original fork](external/e1000/) (retained as reference)
