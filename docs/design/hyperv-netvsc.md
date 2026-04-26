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

The Hyper-V networking stack has **5 layers**, each of which must be implemented:

```
┌────────────────────────────────────────────────┐
│  embassy-net / smoltcp  (existing)             │
├────────────────────────────────────────────────┤
│  NetVSC driver  (embassy adapter)              │
├────────────────────────────────────────────────┤
│  RNDIS protocol  (frame encapsulation)         │
│  Send: RNDIS_PACKET → NVSP → VMBus ring        │
│  Recv: VMBus ring → NVSP → RNDIS → frame        │
├────────────────────────────────────────────────┤
│  NVSP  (Network VSP protocol)                   │
│  Version negotiation, shared buffer setup       │
├────────────────────────────────────────────────┤
│  VMBus transport  (ring buffers, channels)      │
│  Channel offer/open, GPADL, sendpacket/recvpkt  │
├────────────────────────────────────────────────┤
│  Hyper-V hypercall layer  (MSRs, SynIC)         │
│  CPUID detect, wrmsr, hypercall page, SynIC     │
├────────────────────────────────────────────────┤
│  Hardware  (Hyper-V hypervisor)                  │
└────────────────────────────────────────────────┘
```

Compare with e1000 (1 layer) and virtio-net via `virtio-drivers` (1 layer
of glue):

| | e1000 | virtio-net | hv_netvsc |
|---|---|---|---|
| Layers | 1 (MMIO regs) | 1 (wrap crate) | 5 (hypercall → VMBus → NVSP → RNDIS → NetVSC) |
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

0. Detect Hyper-V via CPUID leaf `0x40000000` ("Microsoft Hv" signature).
   If not present, return `Err(HypervisorNotPresent)` — MSR writes on
   non-Hyper-V platforms cause #GP(0) faults.
1. Write `HV_X64_MSR_GUEST_OS_ID` to identify as a guest
2. Allocate a page for hypercall code, write GPA to `HV_X64_MSR_HYPERCALL`.
   The page must be mapped with **execute** permissions (RX) — the HAL's
   `map_mmio` creates RW-only mappings, so a new `map_executable` or
   explicit page table flags will be needed.
3. Enable SynIC via `HV_X64_MSR_SCONTROL`
4. Allocate message + event pages, write GPAs to `SIMP`/`SIEFP`
5. Configure SINT vectors (map to IDT vectors for interrupt delivery).
   Vectors must not collide with existing IOAPIC vectors (32+ for APIC
   timer, 33 for e1000). Use a dedicated range (e.g., 48–63).

### Key Hypercalls

| Hypercall | Code | Purpose |
|-----------|------|---------|
| `HV_POST_MESSAGE` | `0x005c` | Send a message to the host |
| `HV_SIGNAL_EVENT` | `0x005d` | Signal a synthetic interrupt/event |

Hypercalls are issued via the hypercall page (not direct `vmcall`).

**Estimated LOC**: ~400 (MSR wrappers, CPUID detection, hypercall page
setup with RX mapping, SynIC init, error handling)

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

**Ring buffer invariants** (must enforce defensively):
- `0 <= read_index, write_index < buffer_size` — bounds-check all
  host-written indices before use to prevent UB
- Wrap-around: messages may span the buffer boundary; split reads/writes
  must handle the wrap point correctly
- Backpressure: when TX ring is full, return `WouldBlock` — do not spin
- ISR scope: the SynIC ISR must only call `AtomicWaker::wake()` — ring
  buffer reads happen in executor context to avoid data races on
  shared header fields (`read_index`, `write_index`)

### GPADL (Guest Physical Address Descriptor List)

GPADL shares guest physical pages with the host. The guest sends a list
of PFNs (Page Frame Numbers), the host maps them, and returns a handle
used in subsequent operations.

**GPADL lifecycle constraints**:
- Teardown ordering: close channel → revoke GPADL → free DMA. Rust
  `Drop` order (reverse field declaration) must enforce this.
- If the host rejects GPADL creation, the guest must free the allocated
  DMA memory and return an error (not panic).
- The current `DmaAllocator` returns a single contiguous `paddr`. GPADL
  requires per-page PFNs. Verify that heap allocations from
  `BootDmaAllocator` are physically contiguous under bootloader v0.11,
  or extend the trait to expose `page_pfns()`.

### Channel Offer Handling

The guest waits for the host to deliver channel offers via the SynIC
message page. **Timeout is mandatory**: use a bounded poll loop
(e.g., 5 seconds) to avoid hanging indefinitely if no offer arrives
(misconfigured VM, SynIC init failure). Log all received offer GUIDs
before filtering for the NetVSC GUID.

**Estimated LOC**: ~1000 (channel management, ring buffer read/write
with bounds checks and wrap-around, GPADL creation/teardown, message
parsing, timeout/error handling)

## Layer 3: NVSP (Network VSP Protocol)

NVSP sits between VMBus and RNDIS. It handles version negotiation and
shared buffer establishment — RNDIS messages cannot be exchanged without
prior NVSP setup.

### NVSP Init Sequence

1. Send `NVSP_MSG_TYPE_INIT` with supported version (typically v4 or v5)
2. Receive `NVSP_MSG_TYPE_INIT_COMPLETE` with accepted version
3. Send `NVSP_MSG1_TYPE_SEND_RECV_BUF` — establish shared receive buffer
   via GPADL (typically ~2 MiB)
4. Receive `NVSP_MSG1_TYPE_SEND_RECV_BUF_COMPLETE` — host confirms
   buffer sections
5. Send `NVSP_MSG1_TYPE_SEND_SEND_BUF` — establish shared send buffer
   via GPADL (typically ~1 MiB)
6. Receive `NVSP_MSG1_TYPE_SEND_SEND_BUF_COMPLETE`

### Data Path

RNDIS packets are wrapped in `NVSP_MSG1_TYPE_SEND_RNDIS_PKT` for TX
and delivered via `NVSP_MSG1_TYPE_SEND_RNDIS_PKT_COMPLETE` for RX.
The NVSP layer also handles transfer page ranges that reference
offsets within the shared receive buffer.

**Memory budget**: NVSP shared buffers consume ~3 MiB (2 MiB receive +
1 MiB send). With the current 4 MiB heap, this leaves ~1 MiB for
kernel, Embassy, smoltcp, and ring buffers. Buffer sizes are
negotiable — smaller buffers trade throughput for memory. Document
the trade-off and consider increasing heap to 8 MiB.

**Estimated LOC**: ~300 (version negotiation, buffer setup, RNDIS
message wrapping/unwrapping)

## Layer 4: RNDIS Protocol

NetVSC uses RNDIS (Remote NDIS) — Microsoft's protocol for encapsulating
network frames over an abstract transport.

### Message Types

**Required** (minimum viable):

| Message | Type Code | Direction | Purpose |
|---------|-----------|-----------|---------|
| `RNDIS_INITIALIZE_MSG` | `0x00000002` | Guest → Host | Init RNDIS session |
| `RNDIS_INITIALIZE_CMPLT` | `0x80000002` | Host → Guest | Init response |
| `RNDIS_QUERY_MSG` | `0x00000004` | Guest → Host | Query OIDs (MAC, etc.) |
| `RNDIS_QUERY_CMPLT` | `0x80000004` | Host → Guest | Query response |
| `RNDIS_SET_MSG` | `0x00000005` | Guest → Host | Set config (filters) |
| `RNDIS_SET_CMPLT` | `0x80000005` | Host → Guest | Set response |
| `RNDIS_PACKET_MSG` | `0x00000001` | Both | Data packet (Ethernet frame) |
| `RNDIS_KEEPALIVE_MSG` | `0x00000008` | Host → Guest | Keepalive (must respond) |
| `RNDIS_KEEPALIVE_CMPLT` | `0x80000008` | Guest → Host | Keepalive response |

**Handle but defer** (log and ignore):

| Message | Purpose |
|---------|---------|
| `RNDIS_INDICATE_STATUS_MSG` | Link status changes, media events |
| `RNDIS_RESET_MSG` | Host-initiated reset |

Each request/response is correlated by `request_id`. The guest must
track pending request IDs to match responses. RNDIS version is
typically 1.0 (`major=1, minor=0`).

### Data Flow

**TX**: Ethernet frame → wrap in `RNDIS_PACKET_MSG` header → write to
VMBus ring → signal host

**RX**: Host writes `RNDIS_PACKET_MSG` to VMBus ring → SynIC interrupt →
strip RNDIS header → Ethernet frame

### RNDIS Init Sequence

Each step can fail independently — version mismatch, timeout, unexpected
message type, or OID query failure. Each step must have a timeout and
return a descriptive error on failure.

1. Send `RNDIS_INITIALIZE_MSG` (version 1.0, max transfer size)
2. Receive `RNDIS_INITIALIZE_CMPLT` — check status field, verify version
3. Query `OID_802_3_PERMANENT_ADDRESS` → get MAC address
4. Query `OID_GEN_MAXIMUM_FRAME_SIZE` → get MTU
5. Set `OID_GEN_CURRENT_PACKET_FILTER` → enable receive
6. Ready to send/receive `RNDIS_PACKET_MSG`

**Estimated LOC**: ~800 (message types with full set, init handshake with
error handling, OID queries, keepalive, request ID correlation, packet
wrap/unwrap)

## Layer 5: NetVSC / Embassy Adapter

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
**~2500–3500 LOC** of new Rust code (including NVSP layer, error
handling, bounds checks, and CPUID/MSR infrastructure).

## Comparison: Effort vs. Alternatives

| Approach | New Code | Crate Reuse | Testing |
|----------|----------|-------------|---------|
| **e1000** (done) | 620 LOC | None needed | QEMU ✅ |
| **virtio-net** | ~200 LOC glue | `virtio-drivers` | QEMU ✅ |
| **hv_netvsc** | ~2500-3500 LOC | None available | Hyper-V only ❌ |

## Crate Structure

### Prerequisite: Extract `DmaAllocator` trait

The `DmaAllocator` and `DmaRegion` types are currently defined in
`embclox-e1000::dma`. Both `embclox-hyperv` and any future driver need
them, but depending on `embclox-e1000` from `embclox-hyperv` is
architecturally wrong (cross-driver dependency). **Move to
`embclox-core`** before implementation begins — it already holds the
`BootDmaAllocator` implementation and `MmioRegs`, so the trait belongs
alongside its primary consumer.

### Module Layout

```
crates/
├── embclox-hyperv/             (new — ~1700 LOC)
│   ├── src/
│   │   ├── lib.rs
│   │   ├── detect.rs           # CPUID 0x40000000 hypervisor detection
│   │   ├── hypercall.rs        # MSR wrappers, hypercall page (RX mapping)
│   │   ├── synic.rs            # SynIC init, SINT vectors
│   │   ├── vmbus.rs            # Channel lifecycle, GPADL, offer timeout
│   │   ├── ring_buffer.rs      # Ring buffer with bounds checks, wrap-around
│   │   ├── nvsp.rs             # NVSP version negotiation, shared buffers
│   │   └── message.rs          # VMBus message types
│   └── Cargo.toml
├── embclox-netvsc/             (new — ~1000 LOC)
│   ├── src/
│   │   ├── lib.rs
│   │   ├── rndis.rs            # RNDIS message types, keepalive, request ID
│   │   ├── device.rs           # NetVSC device (send/recv)
│   │   └── oid.rs              # OID constants + query helpers
│   └── Cargo.toml
├── embclox-core/
│   ├── src/
│   │   ├── dma.rs              # DmaAllocator trait, DmaRegion (moved from e1000)
│   │   ├── dma_alloc.rs        # BootDmaAllocator (existing)
│   │   ├── e1000_embassy.rs    (existing)
│   │   ├── virtio_embassy.rs   (future)
│   │   ├── netvsc_embassy.rs   (new — ~150 LOC)
│   │   └── ...
│   └── Cargo.toml
├── embclox-e1000/              (existing — re-export from embclox-core::dma)
└── embclox-hal-x86/            (existing — extend for SynIC, MSR, RX mapping)
```

Two separate crates because `embclox-hyperv` (VMBus transport) is reusable
for future Hyper-V devices (storvsc for disk, kvp for key-value pairs).

## Infrastructure Reuse

| Abstraction | Reusable? | Notes |
|-------------|-----------|-------|
| `DmaAllocator` trait | ✅ | **Move to `embclox-core`** from `embclox-e1000` first |
| `BootDmaAllocator` | ✅ | Same heap-based allocation |
| Embassy adapter pattern | ✅ | Same `Driver` trait impl |
| `AtomicWaker` | ✅ | SynIC interrupt → wake executor (ISR only wakes, no ring access) |
| PCI discovery | ❌ | VMBus uses ACPI/offers, not PCI |
| `MmioRegs` / `RegisterAccess` | ❌ | VMBus uses ring buffers, not MMIO |
| IOAPIC routing | ❌ | SynIC replaces IOAPIC (vectors must not collide) |
| `MemoryMapper::map_mmio` | ⚠️ | May need `map_executable` for hypercall page |

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
| **VM generation** | Target **Gen2** (UEFI boot). Gen1 hangs due to VBE. |
| **Secure Boot** | Must disable — our bootloader is not signed. |
| **Debug output** | No COM port, no display without synthvid VMBus driver. Use nested QEMU for now. |
| **Disk format** | VHDX preferred for Gen2. `qemu-img convert -f raw -O vhdx`. |
| **Network** | "Default Switch" provides NAT for ARP/DHCP tests. |
| **Platform** | Requires Windows with Hyper-V enabled — cannot run on Linux. |

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

### Gen1 Status Update (2026-04-26)

Gen1 VMs use BIOS boot, but `bootloader` v0.11's BIOS stage
unconditionally attempts VBE (VESA BIOS Extensions) framebuffer setup.
Hyper-V's virtual BIOS does not implement VBE, causing a hang before
the kernel starts. This is a known upstream issue (bootloader GitHub
issues #575, #222). Tested 2026-04-25.

**Gen1 is viable if VBE is bypassed.** ReactOS runs on Hyper-V Gen1
and Azure Gen1 using only legacy emulated devices (VGA + IDE + Tulip
NIC) — no VMBus needed. The approach:

1. Use VGA text mode (port I/O at 0x3C0-0x3DF, framebuffer at 0xA0000)
   instead of VBE graphics mode — works without any driver
2. Use DEC 21140 Tulip NIC (`0x1011:0x0009`) for networking — requires
   new driver, but the chip is well-documented and simpler than e1000
3. Use COM1 serial via named pipe for debug output — confirmed working
   on Gen1

**To pursue Gen1**: either patch bootloader to skip VBE, or use a
different bootloader (e.g., limine) that supports VGA text mode.

### Gen2 Status Update (2026-04-26)

Gen2 uses UEFI firmware, which `bootloader` v0.11 supports via
`create_uefi_image()`. UEFI boot works — kernel runs (confirmed via
crash-checkpoint debugging). However:

- **Display**: Synthvid VMBus protocol completes without errors (version
  negotiated, VRAM acknowledged, resolution set), but VM Connect shows
  black. No hobby OS has achieved Gen2 display — only Linux/FreeBSD
  have working synthvid drivers (hyperv_fb).
- **Serial**: Port 0x3F8 is NOT emulated on Gen2. `Set-VMComPort`
  configures a named pipe but the firmware doesn't provide a 16550 UART.
  VMBus synthetic serial is only accessible via Azure Serial Console.
- **Networking**: Requires VMBus netvsc (no legacy NIC on Gen2).

### ReactOS Comparison

ReactOS runs on Hyper-V Gen1 and Azure Gen1 using ONLY legacy emulated
hardware. It does NOT implement any VMBus drivers (no netvsc, synthvid,
or storvsc). Azure Gen1 support is experimental/demo quality.

| | ReactOS | embclox (current) |
|---|---|---|
| Gen1 display | VGA text mode ✅ | Bootloader VBE hang ❌ |
| Gen1 network | DEC 21140 Tulip ✅ | No Tulip driver ❌ |
| Gen1 serial | COM1 ✅ | COM1 ✅ (if VBE bypassed) |
| Gen2 display | Not supported ❌ | Protocol complete, display black ❌ |
| Gen2 network | Not supported ❌ | VMBus stack built, netvsc WIP |
| VMBus | Not implemented ❌ | Fully implemented ✅ |

### Recommended Path

**Gen1 for initial bring-up** (immediate visible results):
- Fix bootloader VBE issue → VGA text mode display
- Write DEC 21140 Tulip NIC driver → networking
- COM1 serial for debugging → unblocks all further work
- Works on both local Hyper-V and Azure Gen1

**Gen2 for production** (future, needs Linux ftrace comparison):
- VMBus + netvsc for networking (crate built)
- VMBus + synthvid for display (protocol WIP)
- SR-IOV for high-performance networking

### Hyper-V Test Script Changes

The `scripts/hyperv-boot-test.ps1` must be updated for Gen2:

```powershell
# Gen2 VM creation
New-VM -Name $VMName -Generation 2 -MemoryStartupBytes 256MB -NoVHD
Add-VMHardDiskDrive -VMName $VMName -Path $vhdPath
# Disable Secure Boot (unsigned bootloader)
Set-VMFirmware -VMName $VMName -EnableSecureBoot Off
# No COM port — capture output via VM Connect screenshot or
# framebuffer text visible in the VM Connect window
```

### Hyper-V Display Status

**Tested 2026-04-25**: Both Gen1 and Gen2 VMs boot (confirmed by CPU
activity and "Running" state), but display output is not visible in
VM Connect:

- **Gen1**: Bootloader hangs on VBE. Serial captures one bootloader
  line but kernel never starts. Known bootloader issue (#575, #222).
- **Gen2**: VM runs with CPU activity but VM Connect shows black
  screen — even the UEFI firmware's own output is not visible. This
  appears to be a VM Connect / synthetic video display issue, not a
  kernel problem. Enhanced Session Mode disabled, both VHD and VHDX
  formats tested.

**Other OSes** (Linux, FreeBSD) work on Gen2 because they include
the `hyperv_fb` VMBus synthetic video driver. The UEFI GOP
framebuffer reportedly persists after `ExitBootServices` on Hyper-V,
but VM Connect may require the synthvid VMBus protocol to actually
render to the display window.

**Conclusion**: Native Hyper-V display requires implementing the
synthvid VMBus protocol (`drivers/video/fbdev/hyperv_fb.c` in Linux,
~800 LOC). This is part of the VMBus infrastructure that must be
built for netvsc anyway. Until then, use **nested QEMU** for
development and testing.
|--------|------|-------------|---------|------|
| **QEMU** | bootloader BIOS | ✅ COM1 serial | e1000 ✅ / virtio-net (future) | N/A (RAM) |
| **Hyper-V Gen2** | bootloader UEFI | Framebuffer text | netvsc | N/A (RAM) |
| **Azure Gen2** | bootloader UEFI | Azure Serial Console* | netvsc | N/A (RAM) |
| **GCP** | bootloader | ✅ serial | virtio-net | N/A (RAM) |

\* Azure may provide serial console access even for Gen2 via its
diagnostics infrastructure — needs verification.

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

### Mock Testing (embclox-tests crate)

Pure-logic tests (ring buffer, RNDIS parsing, NVSP state machine) run
in a standard `embclox-tests` crate that uses the Rust `std` library.
This avoids `no_std`/`#[cfg(test)]` limitations and runs via regular
`cargo test` on any platform including Linux CI.

**Specific test coverage**:
- Ring buffer: write/read with wrap-around, full buffer backpressure,
  corrupt index detection, 8-byte alignment enforcement
- RNDIS: message serialization/deserialization round-trips, keepalive
  response generation, request ID correlation
- NVSP: version negotiation state machine, buffer setup message format
- VMBus: GPADL PFN list construction, channel offer GUID matching

The `embclox-hyperv` and `embclox-netvsc` crates should design their
ring buffer and protocol modules to operate on `&[u8]` slices, making
them testable from `embclox-tests` without any hardware or hypervisor.

### Gen2 VM Diagnostic

If a developer accidentally creates a Gen2 VM, the result is complete
silence — no boot, no serial, no error. The `hyperv-test.ps1` script
must validate `Generation -eq 1` and print a clear error if Gen2 is
detected. The doc should note the symptom: "no serial output from
Hyper-V VM → check VM generation (must be Gen1)."

## Implementation Phases

### Phase 0: Prerequisites (Go/No-Go Gate)

- Understand Hyper-V TLFS (Top Level Functional Specification)
- **UEFI boot**: Add `create_uefi_image` support to `embclox-mkimage`.
  Build UEFI image → convert to VHD → boot in Gen2 Hyper-V VM with
  Secure Boot disabled → verify output appears in VM Connect window.
  **This is a hard gate.**
- **Framebuffer text output**: Implement basic framebuffer text renderer
  (~200 LOC) using the bootloader-provided framebuffer. Needed because
  Gen2 has no COM port.
- Extract `DmaAllocator` trait from `embclox-e1000` to `embclox-core`
- Add `map_executable` (RX page mapping) to HAL if needed for
  hypercall page

### Phase 1: Hypercall + SynIC Foundation

- CPUID `0x40000000` hypervisor detection
- MSR read/write wrappers (via `x86_64` crate `Msr::read()/write()`)
- Hypercall page allocation and setup (with RX mapping)
- SynIC initialization (message page, event page, SINT vectors)
- Spike: do channel offers arrive without ACPI? If not, evaluate `acpi`
  crate for `no_std`.
- Verify: guest OS ID registered, SynIC enabled

### Phase 2: VMBus Transport

- Handle channel offers from host (with timeout)
- GPADL creation (share ring buffer memory with host, per-page PFNs)
- Channel open/close with cleanup on failure
- Ring buffer read/write with bounds checks and wrap-around handling
- Verify: channel opened, can send/receive VMBus messages

### Phase 3: NVSP + RNDIS + NetVSC

- NVSP version negotiation
- Shared receive/send buffer setup via GPADL
- RNDIS init handshake with error handling per step
- OID queries (MAC address, MTU)
- Packet send/receive (NVSP → RNDIS_PACKET_MSG wrap/unwrap)
- Keepalive response
- Verify: ARP round-trip with Hyper-V virtual switch

### Phase 4: Embassy Integration

- `NetvscEmbassy` adapter implementing `embassy_net_driver::Driver`
- SynIC interrupt → `AtomicWaker` → executor wake
- TCP echo test

## Open Questions

1. **Bootloader UEFI boot** (Phase 0): `bootloader` v0.11 supports
   `create_uefi_image()`. Need to add this to `embclox-mkimage`, build
   a UEFI image, and test on a Gen2 VM with Secure Boot disabled.
   Gen1 BIOS boot is blocked by VBE hang (bootloader #575, #222).

2. **ACPI dependency** (Phase 1 spike): VMBus channel offers may arrive
   via SynIC without ACPI, or ACPI may be required for VMBus connection
   address discovery. Spike on real Hyper-V during Phase 1. If ACPI is
   needed, evaluate the `acpi` crate (`no_std` compatible).

3. **Heap sizing**: NVSP shared buffers need ~3 MiB. Current heap is
   4 MiB. May need to increase to 8 MiB, or negotiate smaller NVSP
   buffers (trade throughput for memory).

4. **OpenVMM reference**: Microsoft's OpenVMM (Rust, MIT-licensed) has
   VMBus code, but it's the **host** side. The `protocol.rs` wire
   format structs are useful reference for porting to `no_std`.

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
