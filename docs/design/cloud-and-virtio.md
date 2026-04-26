# Design: Cloud Deployment & Virtio-Net Driver

## Motivation

embclox currently runs on QEMU with an emulated Intel e1000 NIC. To run on
cloud VMs (AWS, GCP, Azure), we need to understand what network hardware each
cloud exposes and whether a new driver is required.

**TL;DR**: No cloud exposes e1000. GCP uses virtio-net natively; AWS and Azure
use proprietary NICs. Implementing virtio-net is the best next step — it
unlocks GCP + all KVM environments + local QEMU testing.

## Cloud NIC Landscape

| Cloud | Default NIC | Driver | Alt NIC |
|-------|-------------|--------|---------|
| **AWS EC2** | ENA (Elastic Network Adapter) | `ena` (proprietary, open-source) | Intel 82599 VF (legacy) |
| **GCP** | **virtio-net** (N1/E2) | `virtio_net` | gVNIC (C3/N2/T2D) |
| **Azure** | Hyper-V Synthetic (NetVSC) | `hv_netvsc` | Mellanox mlx5 / MANA |

### e1000 on Cloud?

**Not available.** e1000 is a QEMU/KVM emulated device only. None of the
three major clouds expose it. The existing `embclox-e1000` driver works for
local QEMU development but cannot drive any cloud NIC.

### virtio-net as Common Denominator

virtio-net is the most portable paravirtual NIC:

- **GCP**: ✅ Default on N1/E2 instances
- **AWS**: ❌ Uses proprietary ENA (old Xen-era virtio instances are deprecated)
- **Azure**: ❌ Uses Hyper-V synthetic NIC

virtio-net also works in any KVM/QEMU environment, making it the natural
second driver for embclox.

## Deployment Strategies

### Strategy 1: Nested Virtualization (Easiest)

Run QEMU inside a cloud VM with e1000 emulation. Zero driver changes.

| Cloud | Support | Instance Types | How |
|-------|---------|---------------|-----|
| AWS | ✅ Nitro | c5, m5, r5, t3 | Enabled by default (`vmx` in cpuinfo) |
| GCP | ✅ Explicit | N2, N2D, C2 | `--enable-nested-virtualization` |
| Azure | ✅ Supported | Dv3, Ev3, Dv4 | Enabled by default on supported SKUs |

Our existing `scripts/qemu-test.sh` works unchanged with `-enable-kvm` for
hardware acceleration. This is the recommended path for CI.

### Strategy 2: Native Cloud VM (Requires virtio-net)

Boot embclox as a custom kernel image directly on a cloud VM.

**Boot requirements**:

| Cloud | Image Format | Boot Mode | Import |
|-------|-------------|-----------|--------|
| AWS | RAW, VMDK, VHD | BIOS or UEFI (Nitro) | `aws ec2 import-image` via S3 |
| GCP | RAW (tar.gz) | BIOS or UEFI | `gcloud compute images import` |
| Azure | VHD only | BIOS (Gen1) or UEFI (Gen2) | Upload to Blob → Managed Disk |

**Additional drivers needed for native boot**: virtio-blk (disk) and
virtio-console (serial). These share the virtqueue transport, so implementing
virtio-net gets ~60% of the way to virtio-blk.

### Strategy 3: Cloud-Specific Drivers (Future)

For AWS native performance: ENA driver (open-source spec at
`github.com/amzn/amzn-drivers`). Out of scope for now.

## Virtio-Net Driver Design

### Spec Target

Virtio 1.0/1.1 with **split virtqueues**. Packed rings (1.1) add complexity
with minimal gain for a first driver. QEMU defaults to modern virtio-net-pci.

### Transport: PCI

Cloud x86_64 VMs universally use virtio-PCI. Reuse existing
`embclox-hal-x86::PciBus` with:

- Vendor: `0x1AF4` (Red Hat / Virtio)
- Device: `0x1000` (net, transitional) or `0x1041` (net, modern)

### Virtqueue Architecture

Split virtqueue: 3 physically contiguous DMA regions per queue.

```
┌─────────────────────────┐
│  Descriptor Table       │  16 bytes × queue_size
│  (addr, len, flags,     │
│   next)                 │
├─────────────────────────┤
│  Available Ring         │  flags(u16) + idx(u16) + ring[N] + used_event(u16)
│  (driver → device)      │
├─────────────────────────┤
│  Used Ring              │  flags(u16) + idx(u16) + ring[N](id+len) + avail_event(u16)
│  (device → driver)      │
└─────────────────────────┘
```

Descriptor entry (16 bytes):
```rust
#[repr(C)]
struct VirtqDesc {
    addr:  u64,   // guest-physical buffer address
    len:   u32,   // buffer length
    flags: u16,   // NEXT=1, WRITE=2, INDIRECT=4
    next:  u16,   // next descriptor index if chained
}
```

virtio-net uses 3 queues: receiveq (0), transmitq (1), controlq (2, optional).

### Feature Negotiation

Minimum features to negotiate:

| Feature | Bit | Priority |
|---------|-----|----------|
| `VIRTIO_F_VERSION_1` | 32 | **Required** (modern device) |
| `VIRTIO_NET_F_MAC` | 5 | **Required** (read MAC from config) |
| `VIRTIO_NET_F_STATUS` | 16 | Recommended (link status) |
| `VIRTIO_NET_F_MRG_RXBUF` | 15 | Recommended (multi-buffer RX) |
| `VIRTIO_NET_F_CSUM` | 0 | Future (TX checksum offload) |

### Device Init Sequence

1. Reset device (write 0 to status register)
2. Set `ACKNOWLEDGE` + `DRIVER` status bits
3. Read device features, write driver features (negotiate)
4. Set `FEATURES_OK`, verify device accepted
5. Allocate DMA for virtqueues (receiveq + transmitq)
6. Configure queue addresses in device registers
7. Pre-populate RX queue with empty buffers
8. Set `DRIVER_OK`
9. Enable interrupts

### Virtio-Net Header

Every packet is prepended with a 10-byte header:

```rust
#[repr(C)]
struct VirtioNetHdr {
    flags:       u8,   // NEEDS_CSUM, DATA_VALID
    gso_type:    u8,   // NONE=0 for basic operation
    hdr_len:     u16,
    gso_size:    u16,
    csum_start:  u16,
    csum_offset: u16,
}
```

TX: prepend all-zeros header + Ethernet frame → enqueue to transmitq.
RX: device writes header + Ethernet frame → strip header, return frame.

### Interrupt Model

Two options:
- **Legacy INTx**: Simpler, works with existing IOAPIC infrastructure.
  Use initially with QEMU `-device virtio-net-pci`.
- **MSI-X**: Per-queue interrupt vectors. Better performance, but requires
  new HAL support for MSI-X capability parsing and vector allocation.

Start with legacy INTx for parity with e1000; add MSI-X later.

## Crate Structure

Wrap the `virtio-drivers` crate (rcore-os, v0.13, `#![no_std]`). Implement
its `Hal` trait using our `BootDmaAllocator`/`MemoryMapper`, then wrap
`VirtIONet` in an embassy-net adapter.

```
crates/
├── embclox-e1000/          (existing — custom driver, no upstream crate)
├── embclox-core/
│   ├── src/
│   │   ├── e1000_embassy.rs    (existing)
│   │   ├── virtio_hal.rs       (new — Hal trait impl, ~80 LOC)
│   │   ├── virtio_embassy.rs   (new — Driver adapter, ~120 LOC)
│   │   └── ...
│   └── Cargo.toml              (add virtio-drivers dep)
└── embclox-hal-x86/        (existing — add MSI-X later)
```

No separate `embclox-virtio` crate needed — the driver logic lives in
`virtio-drivers`, and our glue goes in `embclox-core` alongside the existing
embassy adapters.

### Why wrap instead of custom?

| | Custom | Wrap `virtio-drivers` |
|---|---|---|
| **Code size** | ~800 LOC new | ~200 LOC glue |
| **Virtqueue impl** | Must write from scratch | Battle-tested upstream |
| **Feature negotiation** | Must implement spec | Handled by crate |
| **Maintenance** | We own all bugs | Community-maintained |
| **Consistency** | Matches e1000 patterns | Different `Hal` trait |
| **Flexibility** | Full control | Limited to crate API |

The e1000 driver is custom because no equivalent crate exists. For virtio,
the mature `virtio-drivers` crate eliminates the need to reimplement ~600
lines of spec-defined ring management.

## Infrastructure Reuse

| Abstraction | Reuse | Notes |
|-------------|-------|-------|
| `RegisterAccess` trait | ✅ Direct | Same volatile MMIO pattern |
| `DmaAllocator` trait | ✅ Direct | Virtqueues need coherent DMA |
| `BootDmaAllocator` | ✅ Direct | Same offset-based allocation |
| `MmioRegs` | ✅ Direct | Map virtio BAR, read/write config |
| `PciBus::find_device` | ✅ Direct | Scan for vendor `0x1AF4` |
| `MemoryMapper::map_mmio` | ✅ Direct | Map virtio PCI BAR |
| Embassy adapter pattern | ✅ Template | Same `Driver` trait impl |
| `AtomicWaker` / `NET_WAKER` | ✅ Direct | Same interrupt-driven wake |
| `e1000_helpers` pattern | ✅ Template | `reset_device()` / `new_device()` |

**New code needed**:
- Virtqueue ring management (split descriptors, avail/used rings)
- Feature negotiation state machine
- PCI common config capability parsing
- `VirtioNetHdr` prepend/strip

## QEMU Testing

Same `scripts/qemu-test.sh` infrastructure; swap the device flag:

```bash
# e1000 (current):
-device e1000,netdev=net0 -netdev user,id=net0

# virtio-net:
-device virtio-net-pci,netdev=net0 -netdev user,id=net0
```

PCI discovery finds vendor `0x1AF4` instead of `0x8086`. The test binary
can select driver at runtime based on PCI scan results, or we build
separate test binaries per driver.

## Architecture with Dual Drivers

```
┌──────────────────────────────────────────────────────┐
│  Application (TCP echo, etc.)                        │
├──────────────────────────────────────────────────────┤
│  embassy-net (IP/ARP/TCP via smoltcp)                │
├──────────────────────────────────────────────────────┤
│  embclox-core::*_embassy (Driver impls)              │
│  ┌──────────────────┐  ┌───────────────────────────┐ │
│  │ e1000_embassy     │  │ virtio_embassy            │ │
│  └──────────────────┘  └───────────────────────────┘ │
├──────────────────┬───────────────────────────────────┤
│  embclox-e1000   │  embclox-virtio                   │
│  (e1000 driver)  │  (virtio-net driver)              │
├──────────────────┴───────────────────────────────────┤
│  embclox-hal-x86 (serial, PCI, MMIO, heap, IRQ)     │
└──────────────────────────────────────────────────────┘
        ↕ MMIO / DMA
┌──────────────────────────────────────────────────────┐
│  QEMU / Cloud VM Hardware                            │
└──────────────────────────────────────────────────────┘
```

## Implementation Phases

### Phase 1: Minimal TX/RX

- PCI discovery for virtio-net device
- Feature negotiation (MAC + VERSION_1 only)
- Split virtqueue implementation (single RX queue, single TX queue)
- Basic send/receive with VirtioNetHdr
- QEMU smoke test: ARP round-trip through slirp gateway

### Phase 2: Embassy Integration

- `VirtioNetEmbassy` adapter implementing `embassy_net_driver::Driver`
- Legacy INTx interrupt handler
- TCP echo test via integration test

### Phase 3: Cloud Readiness

- MSI-X interrupt support in HAL
- Boot image generation for GCP (raw disk format)
- CI: nested virtualization test on cloud VM

### Phase 4: Additional virtio Devices (Future)

- virtio-blk (reuses virtqueue infrastructure)
- virtio-console (serial replacement)

## References

- [Virtio 1.2 Spec (OASIS)](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html)
- [virtio-drivers crate (rcore-os)](https://github.com/rcore-os/virtio-drivers)
- [AWS ENA driver (amzn)](https://github.com/amzn/amzn-drivers)
- [QEMU virtio-net docs](https://www.qemu.org/docs/master/system/devices/virtio-net.html)
