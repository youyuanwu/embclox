# Design: Hyper-V NetVSC Driver (hv_netvsc)

## Motivation

Hyper-V is available locally (Windows desktop/server) and powers Azure VMs.
Unlike VMware and QEMU which expose e1000, Hyper-V exposes **synthetic
network adapters** via VMBus. There is no virtio support on Hyper-V — VMBus
is the only paravirtual transport. To run embclox natively on Hyper-V, we
need a NetVSC (Network Virtual Service Client) driver.

> **Note**: Hyper-V Gen1 VMs offer a "Legacy Network Adapter" that emulates
> a DEC 21140 (Tulip), **not** e1000. This is a different chip entirely and
> not useful without writing a Tulip driver.

## Architecture Overview

The Hyper-V networking stack has 4 layers, each of which must be implemented:

```
┌────────────────────────────────────────────────┐
│  embassy-net / smoltcp  (existing)             │
├────────────────────────────────────────────────┤
│  NetVSC driver  (RNDIS protocol)               │
│  Send: RNDIS_PACKET → VMBus ring               │
│  Recv: VMBus ring → RNDIS_PACKET → frame       │
├────────────────────────────────────────────────┤
│  VMBus transport  (ring buffers, channels)      │
│  Channel offer/open, GPADL, sendpacket/recvpkt  │
├────────────────────────────────────────────────┤
│  Hyper-V hypercall layer  (MSRs, SynIC)         │
│  wrmsr, vmcall/vmmcall, synthetic interrupts    │
├────────────────────────────────────────────────┤
│  Hardware  (Hyper-V hypervisor)                  │
└────────────────────────────────────────────────┘
```

Compare with e1000 (1 layer) and virtio-net via `virtio-drivers` (1 layer
of glue):

| | e1000 | virtio-net | hv_netvsc |
|---|---|---|---|
| Layers | 1 (MMIO regs) | 1 (wrap crate) | 4 (hypercall → VMBus → RNDIS → NetVSC) |
| Transport | PCI MMIO | PCI virtqueue | VMBus (hypercalls + shared memory) |
| Interrupt | IOAPIC / INTx | IOAPIC / MSI-X | SynIC (synthetic interrupt controller) |
| Existing crate | None (custom) | `virtio-drivers` ✅ | None ❌ |

## Layer 1: Hyper-V Hypercall Interface

The guest communicates with Hyper-V via MSRs and the `vmcall` instruction.

### Key MSRs

| MSR | Address | Purpose |
|-----|---------|---------|
| `HV_X64_MSR_GUEST_OS_ID` | `0x40000000` | Identify guest OS to hypervisor |
| `HV_X64_MSR_HYPERCALL` | `0x40000001` | Hypercall page setup |
| `HV_X64_MSR_VP_INDEX` | `0x40000002` | Virtual processor index |
| `HV_X64_MSR_SCONTROL` | `0x40000080` | SynIC enable/disable |
| `HV_X64_MSR_SIMP` | `0x40000083` | SynIC message page GPA |
| `HV_X64_MSR_SIEFP` | `0x40000082` | SynIC event flags page GPA |
| `HV_X64_MSR_SINT0..15` | `0x40000090+n` | Synthetic interrupt vector config |

### Init Sequence

1. Write `HV_X64_MSR_GUEST_OS_ID` to identify as a guest
2. Allocate a page, write GPA to `HV_X64_MSR_HYPERCALL` to set up hypercall page
3. Enable SynIC via `HV_X64_MSR_SCONTROL`
4. Allocate message + event pages, write GPAs to `SIMP`/`SIEFP`
5. Configure SINT vectors (map to IDT vectors for interrupt delivery)

### Key Hypercalls

| Hypercall | Code | Purpose |
|-----------|------|---------|
| `HV_POST_MESSAGE` | `0x005c` | Send a message to the host |
| `HV_SIGNAL_EVENT` | `0x005d` | Signal a synthetic interrupt/event |

Hypercalls are issued via the hypercall page (not direct `vmcall`).

**Estimated LOC**: ~300 (MSR wrappers, hypercall page setup, SynIC init)

## Layer 2: VMBus Transport

VMBus is a channel-based IPC mechanism between guest and host. Each device
(network, storage, etc.) gets its own channel.

### Channel Lifecycle

```
Host                              Guest
  │                                  │
  │── ChannelOffer (GUID, id) ──────>│  1. Host offers device
  │                                  │
  │<── CreateGpadl (ring buf GPAs) ──│  2. Guest shares ring buffer memory
  │── GpadlCreated (handle) ────────>│
  │                                  │
  │<── OpenChannel (gpadl, vector) ──│  3. Guest opens channel
  │── OpenResult (status) ──────────>│
  │                                  │
  │<══ Ring buffer data flow ═══════>│  4. Bidirectional data transfer
```

### Ring Buffer

Each channel has two ring buffers (TX and RX) in guest-allocated DMA memory:

```rust
#[repr(C)]
struct HvRingBuffer {
    write_index: u32,
    read_index: u32,
    interrupt_mask: u32,
    pending_send_size: u32,
    reserved: [u32; 12],
    buffer: [u8],  // variable-length circular buffer
}
```

Messages are 8-byte aligned with a 16-byte header per packet.

### GPADL (Guest Physical Address Descriptor List)

GPADL shares guest physical pages with the host. The guest sends a list
of PFNs (Page Frame Numbers), the host maps them, and returns a handle
used in subsequent operations.

**Estimated LOC**: ~800 (channel management, ring buffer read/write,
GPADL creation, message parsing)

## Layer 3: RNDIS Protocol

NetVSC uses RNDIS (Remote NDIS) — Microsoft's protocol for encapsulating
network frames over an abstract transport.

### Message Types

| Message | Type Code | Direction | Purpose |
|---------|-----------|-----------|---------|
| `RNDIS_INITIALIZE_MSG` | `0x00000002` | Guest → Host | Init RNDIS session |
| `RNDIS_INITIALIZE_CMPLT` | `0x80000002` | Host → Guest | Init response |
| `RNDIS_QUERY_MSG` | `0x00000004` | Guest → Host | Query OIDs (MAC, etc.) |
| `RNDIS_QUERY_CMPLT` | `0x80000004` | Host → Guest | Query response |
| `RNDIS_SET_MSG` | `0x00000005` | Guest → Host | Set config (filters) |
| `RNDIS_PACKET_MSG` | `0x00000001` | Both | Data packet (Ethernet frame) |

### Data Flow

**TX**: Ethernet frame → wrap in `RNDIS_PACKET_MSG` header → write to
VMBus ring → signal host

**RX**: Host writes `RNDIS_PACKET_MSG` to VMBus ring → SynIC interrupt →
strip RNDIS header → Ethernet frame

### RNDIS Init Sequence

1. Send `RNDIS_INITIALIZE_MSG` (version, max transfer size)
2. Receive `RNDIS_INITIALIZE_CMPLT` (status, capabilities)
3. Query `OID_802_3_PERMANENT_ADDRESS` → get MAC address
4. Query `OID_GEN_MAXIMUM_FRAME_SIZE` → get MTU
5. Set `OID_GEN_CURRENT_PACKET_FILTER` → enable receive
6. Ready to send/receive `RNDIS_PACKET_MSG`

**Estimated LOC**: ~600 (message types, init handshake, OID queries,
packet wrap/unwrap)

## Layer 4: NetVSC / Embassy Adapter

Thin layer that ties RNDIS to `embassy_net_driver::Driver`, following the
same pattern as `e1000_embassy.rs`.

**Estimated LOC**: ~150

## Linux Source Reference

The primary reference for porting is the Linux kernel:

| File | LOC (approx) | Layer |
|------|-------------|-------|
| `arch/x86/hyperv/hv_init.c` | 400 | Hypercall setup |
| `drivers/hv/hv.c` | 300 | Hypercall wrappers |
| `drivers/hv/hv_synic.c` | 200 | SynIC init |
| `drivers/hv/channel.c` | 800 | Channel open/close |
| `drivers/hv/ring_buffer.c` | 500 | Ring buffer ops |
| `drivers/hv/connection.c` | 400 | VMBus connection |
| `drivers/hv/channel_mgmt.c` | 600 | Channel management |
| `drivers/net/hyperv/netvsc.c` | 1600 | NetVSC driver |
| `drivers/net/hyperv/rndis_filter.c` | 1200 | RNDIS protocol |
| **Total** | **~6000** | |

Not all of this needs porting — many Linux abstractions (workqueues,
netdev, NAPI) don't apply to bare-metal. Realistic port estimate is
~1800–2500 LOC of new Rust code.

## Comparison: Effort vs. Alternatives

| Approach | New Code | Crate Reuse | Testing |
|----------|----------|-------------|---------|
| **e1000** (done) | 620 LOC | None needed | QEMU ✅ |
| **virtio-net** | ~200 LOC glue | `virtio-drivers` | QEMU ✅ |
| **hv_netvsc** | ~1800-2500 LOC | None available | Hyper-V only ❌ |

## Crate Structure

```
crates/
├── embclox-hyperv/             (new — ~1200 LOC)
│   ├── src/
│   │   ├── lib.rs
│   │   ├── hypercall.rs        # MSR wrappers, hypercall page
│   │   ├── synic.rs            # SynIC init, SINT vectors
│   │   ├── vmbus.rs            # Channel lifecycle, GPADL
│   │   ├── ring_buffer.rs      # Ring buffer read/write
│   │   └── message.rs          # VMBus message types
│   └── Cargo.toml
├── embclox-netvsc/             (new — ~800 LOC)
│   ├── src/
│   │   ├── lib.rs
│   │   ├── rndis.rs            # RNDIS message types + init
│   │   ├── device.rs           # NetVSC device (send/recv)
│   │   └── oid.rs              # OID constants + query helpers
│   └── Cargo.toml
├── embclox-core/
│   ├── src/
│   │   ├── e1000_embassy.rs    (existing)
│   │   ├── virtio_embassy.rs   (future)
│   │   ├── netvsc_embassy.rs   (new — ~150 LOC)
│   │   └── ...
│   └── Cargo.toml
└── embclox-hal-x86/            (existing — extend for SynIC)
```

Two separate crates because `embclox-hyperv` (VMBus transport) is reusable
for future Hyper-V devices (storvsc for disk, kvp for key-value pairs).

## Infrastructure Reuse

| Abstraction | Reusable? | Notes |
|-------------|-----------|-------|
| `DmaAllocator` trait | ✅ | Ring buffers + GPADL need coherent DMA |
| `BootDmaAllocator` | ✅ | Same heap-based allocation |
| Embassy adapter pattern | ✅ | Same `Driver` trait impl |
| `AtomicWaker` | ✅ | SynIC interrupt → wake executor |
| PCI discovery | ❌ | VMBus uses ACPI, not PCI |
| `MmioRegs` / `RegisterAccess` | ❌ | VMBus uses ring buffers, not MMIO |
| IOAPIC routing | ❌ | SynIC replaces IOAPIC for VMBus devices |
| `MemoryMapper::map_mmio` | ❌ | No MMIO BARs for synthetic devices |

**Key difference**: VMBus devices are **not PCI devices**. They are
discovered via ACPI (or the VMBus offer protocol), not PCI enumeration.
This means a significant portion of `embclox-hal-x86` doesn't apply.

## Testing Strategy

### Overview

Unlike e1000/virtio which test in QEMU, hv_netvsc requires a real Hyper-V
hypervisor. There is no QEMU emulation of VMBus.

### Local Hyper-V Testing

Our bootloader v0.11 produces BIOS images (`create_bios_image`), which
work with Hyper-V **Generation 1** VMs.

**Workflow**:

```
1. cargo build (kernel ELF)
2. embclox-mkimage (raw .img)
3. qemu-img convert -f raw -O vpc disk.img disk.vhd
4. PowerShell: create Gen1 VM, attach VHD, COM1 → named pipe
5. Start-VM, read serial output from pipe
6. Parse output for PASS/FAIL, Stop-VM, cleanup
```

**PowerShell automation** (`scripts/hyperv-test.ps1`):

```powershell
param(
    [string]$Image = "target/x86_64-unknown-none/debug/embclox-unit-tests.img",
    [string]$VMName = "embclox-test"
)

$VHD = "$Image.vhd"
qemu-img convert -f raw -O vpc $Image $VHD

# Create Gen1 VM with serial + network
New-VM -Name $VMName -Generation 1 -MemoryStartupBytes 256MB -SwitchName "Default Switch"
Add-VMHardDiskDrive -VMName $VMName -Path (Resolve-Path $VHD)
Set-VMComPort -VMName $VMName -Number 1 -Path "\\.\pipe\$VMName-com1"

Start-VM -Name $VMName

# Read serial output from named pipe (timeout after 60s)
# Parse for "[PASS]" or "[FAIL]" markers

Stop-VM -Name $VMName -TurnOff -Force
Remove-VM -Name $VMName -Force
Remove-Item $VHD
```

### Key Constraints

| Concern | Detail |
|---------|--------|
| **VM generation** | Must use Gen1 (BIOS boot). Gen2 is UEFI only. |
| **COM port** | Gen1 only — serial output via named pipe `\\.\pipe\<name>` |
| **Exit code** | No `isa-debug-exit` — must parse serial log for PASS/FAIL markers |
| **Disk format** | Hyper-V needs VHD, not raw. One `qemu-img convert` step. |
| **Network** | "Default Switch" provides NAT (like QEMU slirp) for ARP/DHCP tests |
| **Platform** | Requires Windows with Hyper-V enabled — cannot run on Linux |

### CI: GitHub Actions

**GitHub-hosted Windows runners do NOT officially support nested
virtualization / Hyper-V.** Some community reports show it occasionally
working, but it is undocumented, unsupported, and may break at any time.

| Runner Type | Hyper-V? | Notes |
|-------------|----------|-------|
| `windows-latest` (hosted) | ❌ Not supported | No guaranteed VT-x exposure |
| Windows larger runners | ❌ Not documented | Same Azure infra, same limitation |
| **Self-hosted Windows** | ✅ Full control | Enable Hyper-V, install QEMU for VHD convert |
| **Azure VM (self-hosted)** | ✅ With nested virt | Dv3/Ev3/Dv4 series support nested virt |

**Recommended CI approach**:

1. **QEMU tests on Linux** (existing): e1000 + future virtio-net tests
   run on `ubuntu-latest` — no changes needed.
2. **Hyper-V tests on self-hosted**: Set up a Windows self-hosted runner
   (local machine or Azure VM) with Hyper-V. Run `hyperv-test.ps1` in
   a separate CI job gated on the runner label.

```yaml
jobs:
  qemu-tests:
    runs-on: ubuntu-latest
    # ... existing ctest workflow

  hyperv-tests:
    runs-on: [self-hosted, windows, hyperv]
    if: github.event_name == 'push'  # skip on PRs to avoid slow CI
    steps:
      - uses: actions/checkout@v4
      - run: cargo build --manifest-path qemu-tests/unit/Cargo.toml --target x86_64-unknown-none
      - run: cargo run -p embclox-mkimage -- ...
      - run: .\scripts\hyperv-test.ps1 -Image $image
```

### Azure Native Boot

Running embclox directly on Azure (not nested in QEMU) requires
understanding what hardware Azure exposes vs. local Hyper-V.

**Hyper-V Gen1 emulated devices** (no drivers needed):

| Device | Emulated Type | Notes |
|--------|--------------|-------|
| Boot disk | IDE controller | Bootloader reads from IDE; kernel runs from RAM |
| DVD/CD | ATAPI (IDE) | Not needed |
| Legacy NIC | DEC 21140 (Tulip) | **Not available on Azure** |
| Keyboard/Mouse | PS/2 | Not needed (headless) |
| Video | S3 Trio 32/64 | Not needed (serial only) |
| COM port | 16550 UART | ✅ Serial output works |

**Critical difference: Azure vs local Hyper-V**

| | Local Hyper-V Gen1 | Azure Gen1 | Gen2 (any) |
|---|---|---|---|
| **Firmware** | BIOS ✅ | BIOS ✅ | UEFI only ❌ |
| **Boot disk** | IDE (emulated) ✅ | IDE (emulated) ✅ | SCSI only (needs storvsc) ❌ |
| **Serial** | COM1 → named pipe ✅ | COM1 → Azure Serial Console ✅ | **No COM ports** ❌ |
| **Legacy NIC** | ✅ Available (DEC Tulip) | ❌ **Not available** | ❌ None |
| **Synthetic NIC** | Optional (needs netvsc) | **Only option** (needs netvsc) | Only option (needs netvsc) |
| **Synthetic disk** | Optional (needs storvsc) | Optional (IDE suffices for boot) | **Required** (no IDE) |

### Why Gen2 Is Not Viable (Currently)

Gen2 VMs have three blockers:

1. **UEFI boot** — our bootloader v0.11 uses `create_bios_image`. We'd
   need `create_uefi_image` (the bootloader crate supports this, but
   our `embclox-mkimage` tool is not set up for it).
2. **SCSI boot** — no IDE controller exists in Gen2. The VM cannot boot
   without a storvsc driver, even though our kernel runs from RAM after
   boot. The bootloader itself needs to read the disk.
3. **No COM port** — Gen2 has no serial ports at all. Our entire serial
   output and test infrastructure depends on COM1 (port 0x3F8). We'd
   need a Hyper-V synthetic console (VMBus-based) or EFI console output.

**Gen1 is the right target.** It gives us BIOS boot (works today), IDE
disk (no driver needed for our RAM-only kernel), and COM1 serial
(existing infrastructure). The only missing piece is netvsc for
networking.

**Key insight**: Our kernel doesn't need a disk driver at all after boot.
The bootloader (BIOS, IDE) loads the kernel into RAM, then our kernel runs
entirely from memory. We only need:

1. **Serial** (COM1) — already implemented ✅
2. **Network** — requires a NIC driver

**For local Hyper-V**: We can use the Legacy NIC (DEC 21140 / Tulip), but
that's yet another driver to implement. Alternatively, implement netvsc.

**For Azure**: netvsc is the **only** option. Azure does not expose Legacy
NIC even on Gen1 VMs.

### Full Driver Matrix

| Target | Boot | Serial | Network | Disk |
|--------|------|--------|---------|------|
| **QEMU** | bootloader | ✅ 16550 | e1000 ✅ / virtio-net (future) | N/A (RAM) |
| **Local Hyper-V** | bootloader + IDE | ✅ COM1 pipe | netvsc (or Legacy Tulip) | N/A (RAM) |
| **Azure** | bootloader + IDE | ✅ Azure Serial Console | netvsc **required** | N/A (RAM) |
| **GCP** | bootloader | ✅ serial | virtio-net | N/A (RAM) |

### Azure Serial Console

Azure provides a Serial Console feature in the portal that connects to
COM1. Our existing `embclox_hal_x86::serial::Serial` (port 0x3F8) works
without modification — it's the same 16550 UART the Azure console expects.

Boot diagnostics must be enabled on the VM for serial console access:
```bash
az vm boot-diagnostics enable --name embclox-test --resource-group rg
```

### Mock Testing (No Hypervisor)

For unit-testing the VMBus ring buffer logic and RNDIS message
serialization without a real Hyper-V host, we can write `#[cfg(test)]`
tests in the `embclox-hyperv` crate that exercise the protocol parsing
and ring buffer algorithms against mock memory. This runs on any platform
including Linux CI.

## Implementation Phases

### Phase 0: Prerequisites

- Understand Hyper-V TLFS (Top Level Functional Specification)
- Set up local Hyper-V VM for testing with serial output
- Verify embclox bootloader works under Hyper-V

### Phase 1: Hypercall + SynIC Foundation

- MSR read/write wrappers
- Hypercall page allocation and setup
- SynIC initialization (message page, event page, SINT vectors)
- Verify: guest OS ID registered, SynIC enabled

### Phase 2: VMBus Transport

- Handle channel offers from host
- GPADL creation (share ring buffer memory with host)
- Channel open/close
- Ring buffer read/write primitives
- Verify: channel opened, can send/receive VMBus messages

### Phase 3: RNDIS + NetVSC

- RNDIS init handshake
- OID queries (MAC address, MTU)
- Packet send/receive (RNDIS_PACKET_MSG wrap/unwrap)
- Verify: ARP round-trip with Hyper-V virtual switch

### Phase 4: Embassy Integration

- `NetvscEmbassy` adapter implementing `embassy_net_driver::Driver`
- SynIC interrupt → `AtomicWaker` → executor wake
- TCP echo test

## Open Questions

1. **Bootloader compatibility**: Does `bootloader` v0.11 work under
   Hyper-V? Hyper-V Gen1 uses BIOS, Gen2 uses UEFI. Our current
   bootloader targets BIOS.

2. **ACPI dependency**: VMBus device discovery may require ACPI table
   parsing. Do we need an ACPI parser, or can we hardcode the well-known
   NetVSC GUID (`f8615163-df3e-46c5-913f-f2d2f965ed0e`)?

3. **OpenVMM reference**: Microsoft's OpenVMM (Rust, MIT-licensed) has
   VMBus code, but it's the **host** side. The guest-side protocol is
   the mirror image — useful as reference but not directly portable.

## OpenVMM Crate Analysis

Microsoft's [OpenVMM](https://github.com/microsoft/openvmm) (MIT-licensed)
has Rust VMBus crates, but **all are `std`-only user-mode code**:

| Crate | `no_std`? | What's There |
|-------|-----------|--------------|
| `vmbus_core` | ❌ | Protocol message structs (`OfferChannel`, `OpenChannel`, etc.) via `zerocopy` |
| `vmbus_ring` | ❌ | Ring buffer read/write, `PacketDescriptor`, control page layout |
| `vmbus_channel` | ❌ | Channel lifecycle, GPADL, async channel management |
| `vmbus_client` | ❌ | Full VMBus client state machine (user-mode, async/await) |

**Cannot use as dependencies** — they depend on `std`, `futures`, `mesh`
(async IPC), `guestmem` (host-side memory abstraction), `pal_async`.

**Valuable as reference** — the `protocol.rs` files contain exact wire
format struct definitions (MIT-licensed) that match the Hyper-V TLFS. We
could port these `zerocopy`-derived structs to `no_std` directly, saving
significant reverse-engineering effort vs. working from Linux C headers.

## References

- [Hyper-V TLFS (Top Level Functional Specification)](https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/reference/tlfs)
- [Linux hv_vmbus driver](https://github.com/torvalds/linux/blob/master/drivers/hv/vmbus_drv.c)
- [Linux netvsc driver](https://github.com/torvalds/linux/blob/master/drivers/net/hyperv/netvsc.c)
- [Linux RNDIS filter](https://github.com/torvalds/linux/blob/master/drivers/net/hyperv/rndis_filter.c)
- [OpenVMM (Microsoft, Rust)](https://github.com/microsoft/openvmm)
- [RNDIS specification](https://learn.microsoft.com/en-us/windows-hardware/drivers/network/overview-of-remote-ndis)
