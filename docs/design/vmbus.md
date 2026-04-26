# Design: VMBus Implementation

## Motivation

Native Hyper-V support requires VMBus — the paravirtual bus that all
synthetic devices (network, video, storage) communicate over. VMBus is
the foundation: without it, no Hyper-V device works. We implement
VMBus first, then synthvid (video) for display output, then netvsc
(network) for networking.

## Implementation Order

```
1. embclox-hyperv   (VMBus core)      → can talk to Hyper-V
2. synthvid         (VMBus video)     → can see output on screen
3. netvsc + NVSP    (VMBus network)   → can send/receive packets
```

Synthvid before netvsc because display output enables debugging
everything that follows.

## Architecture

```
┌──────────────────────────────────────────────────────┐
│  Consumers: synthvid, netvsc, storvsc (future)       │
├──────────────────────────────────────────────────────┤
│  embclox-hyperv  (VMBus core crate)                  │
│  ┌─────────┐ ┌──────────┐ ┌────────────┐            │
│  │ detect  │ │ hypercall│ │   synic    │            │
│  │ CPUID   │ │ page +   │ │ SIMP/SIEFP│            │
│  │ check   │ │ HvPost   │ │ SINT      │            │
│  └─────────┘ └──────────┘ └────────────┘            │
│  ┌──────────────────────────────────────┐            │
│  │ vmbus (connect, offers, channels)   │            │
│  └──────────────────────────────────────┘            │
│  ┌──────────────────────────────────────┐            │
│  │ ring_buffer (read/write with safety)│            │
│  └──────────────────────────────────────┘            │
├──────────────────────────────────────────────────────┤
│  Hyper-V hypervisor (MSRs, hypercalls, SynIC)        │
└──────────────────────────────────────────────────────┘
```

## Boot-to-Framebuffer Path

The minimum sequence from kernel entry to visible display:

```
CPUID 0x40000000 → detect "Microsoft Hv"
  ↓
MSR: Guest OS ID → Hypercall page → SynIC (SCONTROL, SIMP, SIEFP, SINT2)
  ↓
HvPostMessage: INITIATE_CONTACT (version WIN10)
  ↓
Poll SIMP: VERSION_RESPONSE (success)
  ↓
HvPostMessage: REQUEST_OFFERS
  ↓
Poll SIMP: OFFERCHANNEL × N → match synthvid GUID
Poll SIMP: ALLOFFERS_DELIVERED
  ↓
Allocate ring buffer (256KB) → HvPostMessage: GPADL_HEADER (+GPADL_BODY if >26 PFNs)
  ↓
Poll SIMP: GPADL_CREATED
  ↓
HvPostMessage: OPENCHANNEL
  ↓
Poll SIMP: OPENCHANNEL_RESULT
  ↓
Ring send: SYNTHVID_VERSION_REQUEST (v3.5)
Ring recv: SYNTHVID_VERSION_RESPONSE
  ↓
Allocate VRAM (4MB) → Ring send: SYNTHVID_VRAM_LOCATION (phys addr)
Ring recv: SYNTHVID_VRAM_LOCATION_ACK
  ↓
Ring send: SYNTHVID_SITUATION_UPDATE (1024×768, 32bpp)
  ↓
✅ Framebuffer is live — write pixels to VRAM allocation
```

## Phase 1: Hyper-V Detection + MSR Init

### CPUID Detection

```rust
fn detect_hyperv() -> bool {
    let r = cpuid(0x40000000);
    r.ebx == 0x7263694D &&  // "Micr"
    r.ecx == 0x666F736F &&  // "osof"
    r.edx == 0x76482074     // "t Hv"
}
```

Also check `CPUID(0x40000003).EAX` bit 5 for SynIC support.

### MSR Sequence

| Step | MSR | Value | Purpose |
|------|-----|-------|---------|
| 1 | `0x40000000` (Guest OS ID) | `0x8100_xxxx_xxxx_xxxx` | Identify as open-source OS |
| 2 | `0x40000001` (Hypercall) | `phys_page \| 1` | Enable hypercall page |
| 3 | `0x40000080` (SCONTROL) | `0x1` | Enable SynIC |
| 4 | `0x40000083` (SIMP) | `phys_page \| 1` | SynIC message page |
| 5 | `0x40000082` (SIEFP) | `phys_page \| 1` | SynIC event flags page |
| 6 | `0x40000092` (SINT2) | `vector \| 0x10000` | Auto-EOI, IDT vector for VMBus |

### Hypercall Page

A 4KB page allocated with **execute** permissions. The hypervisor
fills it with code. Guest calls into it with:
- RCX = hypercall code (e.g., `0x005C` for HvPostMessage)
- RDX = physical address of input parameters
- R8 = physical address of output parameters (if any)

Returns status in RAX.

### Memory Requirements

| Allocation | Size | Permissions | Purpose |
|------------|------|-------------|---------|
| Hypercall page | 4 KB | RX (execute!) | Hypercall code injected by hypervisor |
| SynIC message page | 4 KB | RW | Incoming VMBus messages (16 slots × 256 bytes) |
| SynIC event flags page | 4 KB | RW | Event notification flags |
| HvPostMessage input | 256 bytes | RW | Outgoing message buffer (page-aligned) |

**HAL changes needed**:
- Verify whether `map_mmio` already sets execute permissions (RWX) or
  only RW. If RW-only, add `map_executable(phys, size)` for the
  hypercall page. If already RWX, document that no change is needed.
- Add `poll_with_timeout(deadline, check_fn)` utility for bounded
  polling of SIMP message slots.

**Message pending draining**: After reading a message from the SIMP
slot, check the `message_pending` flag in the message header. If set,
write `MSR_EOM` (End of Message) to signal the hypervisor to deliver
the next queued message. Without this, messages can be lost.

**Estimated LOC**: ~300

## Phase 2: VMBus Connection

### Message Flow

All VMBus control messages go through `HvPostMessage` (hypercall `0x005C`)
to connection ID 1, message type 1. Responses arrive on the SynIC
message page (SIMP).

```rust
// Send a VMBus channel message
fn vmbus_post_message(msg: &[u8]) {
    let input = HvPostMessageInput {
        connection_id: 1,          // VMBUS_MESSAGE_CONNECTION_ID
        message_type: 1,           // Channel message
        payload_size: msg.len(),
        payload: msg,
    };
    hypercall(HV_POST_MESSAGE, &input);
}

// Poll for response on SIMP
fn vmbus_poll_message() -> Option<&VmbusMessage> {
    let slot = &simp_page.messages[VMBUS_SINT];
    if slot.header.message_type != 0 {
        let msg = &slot.payload;
        // Process message...
        // Write 0 to message_type to acknowledge
        slot.header.message_type = 0;
        // Write MSR_EOM to signal end-of-message
        wrmsr(MSR_EOM, 0);
        Some(msg)
    } else {
        None
    }
}
```

### INITIATE_CONTACT

```rust
#[repr(C)]
struct VmbusInitiateContact {
    header: VmbusChannelMsgHeader,  // type = 14
    version: u32,                   // 0x00040000 (WIN10)
    target_vcpu: u32,               // 0
    interrupt_page_or_target_info: u64,
    parent_to_child_monitor: u64,   // monitor page GPA (allocate 4KB)
    child_to_parent_monitor: u64,   // monitor page GPA (allocate 4KB)
}
```

**Version fallback**: Try `[WIN10=0x00040000, WIN8_1=0x00030003, WIN8=0x00020004]`
in order. If all rejected, return `Err(HvError::NoSupportedVersion)`.

Response: `VERSION_RESPONSE` (type 15) with `version_supported: bool`.
Poll with `poll_with_timeout(5_seconds)`.

### HvPostMessage Error Handling

`HvPostMessage` returns a status code in RAX. Handle:
- `HV_STATUS_SUCCESS` (0): continue
- `HV_STATUS_INSUFFICIENT_BUFFERS` (0x13): retry after short delay
  (transient, common during GPADL body bursts)
- Other errors: return `Err(HvError::HypercallFailed(status))`

```rust
fn hv_post_message(msg: &[u8]) -> Result<(), HvError> {
    for _ in 0..10 {
        let status = hypercall(HV_POST_MESSAGE, &input);
        match status {
            0 => return Ok(()),
            0x13 => { /* InsufficientBuffers — retry */ },
            _ => return Err(HvError::HypercallFailed(status)),
        }
    }
    Err(HvError::HypercallRetryExhausted)
}
```

### REQUEST_OFFERS → OFFERCHANNEL

After version handshake, send `REQUEST_OFFERS` (type 3, empty payload).
The host responds with one `OFFERCHANNEL` (type 1) per device, then
`ALLOFFERS_DELIVERED` (type 4).

Each offer contains a device GUID. Match against known GUIDs:
- Synthvid: `{DA0A7802-E377-4AAC-8E77-0558EB1073F8}`
- NetVSC: `{F8615163-DF3E-46C5-913F-F2D2F965ED0E}`

**Estimated LOC**: ~400

## Phase 3: Channel Open (GPADL + Ring Buffer)

### GPADL Creation

GPADL shares guest physical memory with the host. For a ring buffer:

1. Allocate memory (e.g., 256 KB = 64 pages)
2. Build PFN list by **translating each page individually** via page
   table walk (`translate_addr`). Do NOT assume physical contiguity —
   the heap allocator makes no such guarantee.
3. Send `GPADL_HEADER` (type 8) with channel ID, GPADL ID, PFN list
4. If PFN list exceeds single message (~26 PFNs fit in one 256-byte
   message), send additional `GPADL_BODY` (type 9) messages with
   sequential `msgnumber` values
5. Receive `GPADL_CREATED` (type 10) with status
6. On failure: free the allocated memory and return error

```rust
fn build_pfn_list(vaddr: usize, size: usize, mapper: &OffsetPageTable) -> Vec<u64> {
    let num_pages = size / 4096;
    (0..num_pages)
        .map(|i| {
            let va = VirtAddr::new((vaddr + i * 4096) as u64);
            mapper.translate_addr(va).unwrap().as_u64() >> 12
        })
        .collect()
}
```

### Ring Buffer Structure

Two ring buffers per channel (send + receive), sharing the GPADL
allocation (256 KB total = 128 KB send + 128 KB receive):

```
┌─────────────────────────┐  offset 0
│  Send Ring (128 KB)     │
│  ┌───────────────────┐  │
│  │ Control page      │  │  write_index, read_index, interrupt_mask
│  ├───────────────────┤  │
│  │ Data buffer       │  │  circular byte buffer
│  └───────────────────┘  │
├─────────────────────────┤  offset = 128 KB
│  Receive Ring (128 KB)  │
│  ┌───────────────────┐  │
│  │ Control page      │  │
│  ├───────────────────┤  │
│  │ Data buffer       │  │
│  └───────────────────┘  │
└─────────────────────────┘
```

**Ring buffer correctness requirements**:

- **Volatile access**: `read_index` and `write_index` are shared with
  the host. Use `read_volatile`/`write_volatile` for all accesses.
- **Memory ordering**: `fence(Ordering::Acquire)` after reading the
  host's index, `fence(Ordering::Release)` before updating our index.
  Same pattern as the e1000 driver's descriptor rings.
- **Wrap-around**: Packets may span the buffer boundary. `ring_write`
  and `ring_read` must handle split copies (write first part to end of
  buffer, second part from start).
- **Bounds checking**: Validate all host-sourced values (`write_index`,
  `VmpacketDescriptor.length8`, `.data_offset8`) before use. Out-of-range
  values → return `Err(RingError::Corrupt)`, do not access memory.
- **8-byte alignment**: All packets are padded to 8-byte boundaries.

### Channel Open

```rust
#[repr(C)]
struct VmbusOpenChannel {
    header: VmbusChannelMsgHeader,  // type = 5
    channel_id: u32,
    open_id: u32,
    ring_buffer_gpadl: u32,
    target_vcpu: u32,
    downstream_ring_offset: u32,    // offset to receive ring in GPADL
    // user_data for the device (e.g., synthvid pipe mode)
}
```

Response: `OPENCHANNEL_RESULT` (type 6) with status.

**Estimated LOC**: ~500 (ring buffer ~250, GPADL ~150, channel ~100)

## Phase 4: Synthvid Protocol

After channel is open, communicate via ring buffer (not HvPostMessage).

### Message Types

| Code | Name | Direction | Purpose |
|------|------|-----------|---------|
| 1 | `VERSION_REQUEST` | Guest → Host | Negotiate synthvid version |
| 2 | `VERSION_RESPONSE` | Host → Guest | Accepted version |
| 3 | `VRAM_LOCATION` | Guest → Host | Tell host where VRAM is |
| 4 | `VRAM_LOCATION_ACK` | Host → Guest | Host confirms VRAM mapping |
| 5 | `SITUATION_UPDATE` | Guest → Host | Set resolution + depth |
| 10 | `DIRT` | Guest → Host | Dirty rectangle (trigger redraw) |

> **Note**: Synthvid version encoding is `(minor << 16) | major`, e.g.,
> v3.5 = `0x00050003`. This is the opposite of VMBus version encoding.

### Init Sequence

1. **Version negotiate**: Send `VERSION_REQUEST(0x00050003)` (v3.5 = WIN10).
   Receive `VERSION_RESPONSE` with timeout. If rejected, retry with
   `0x00020003` (v3.2 = WIN8). If both rejected, return error.

2. **VRAM location**: Allocate framebuffer memory (1024×768×32 = 3 MB).
   VRAM does **not** need DMA-coherent memory — any physical memory the
   host can map is sufficient. Send `VRAM_LOCATION` with physical address
   + generation counter. Receive `VRAM_LOCATION_ACK` with timeout.

3. **Set resolution**: Send `SITUATION_UPDATE` with desired width,
   height, bpp (1024×768×32). No response expected.

4. **Dirty rect**: Send `DIRT` with full-screen rectangle to trigger
   initial display. Subsequent updates send only changed regions.

5. **Headless VMs**: If no synthvid device is offered (e.g., Azure
   headless), skip display init gracefully — log a message and continue.
   The kernel still functions, just without visual output.

### Ring Buffer Packet Format

Synthvid messages are sent as VMBus in-band data packets:

```rust
#[repr(C)]
struct VmbusPacketDescriptor {
    packet_type: u16,    // VM_PKT_DATA_INBAND = 6
    data_offset8: u16,   // offset to data / 8
    length8: u16,        // total packet length / 8
    flags: u16,          // COMPLETION_REQUESTED = 1
    transaction_id: u64, // unique ID for request/response matching
}
// followed by payload (synthvid message)
// followed by padding to 8-byte alignment
```

**Estimated LOC**: ~300

## Crate Structure

```
crates/embclox-hyperv/
├── src/
│   ├── lib.rs          # Public API: init() → VmBus, VmBus::open_channel()
│   ├── detect.rs       # CPUID 0x40000000 detection (~40 LOC)
│   ├── msr.rs          # MSR read/write wrappers (~30 LOC)
│   ├── hypercall.rs    # Hypercall page setup + HvPostMessage (~120 LOC)
│   ├── synic.rs        # SynIC init: SIMP, SIEFP, SINT (~100 LOC)
│   ├── vmbus.rs        # Connect, offers, channel open/close (~400 LOC)
│   ├── ring.rs         # Ring buffer read/write (~250 LOC)
│   └── gpadl.rs        # GPADL creation (~100 LOC)
└── Cargo.toml

crates/embclox-synthvid/        (or module in examples-hyperv initially)
├── src/
│   ├── lib.rs          # Synthvid device: version, vram, resolution (~300 LOC)
│   └── protocol.rs     # Message types and constants
└── Cargo.toml
```

**Total estimate**: ~1340 LOC for VMBus core + synthvid.

## Public API

```rust
// Detection
pub fn detect_hyperv() -> Option<HvFeatures>;

// Init (must be called on Hyper-V only — CPUID-gated)
pub fn init(dma: &impl DmaAllocator) -> Result<VmBus, HvError>;

// VmBus handle
impl VmBus {
    pub fn offers(&self) -> &[ChannelOffer];
    pub fn find_offer(&self, guid: &Guid) -> Option<&ChannelOffer>;
    pub fn open_channel(&mut self, offer: &ChannelOffer, ring_size: usize)
        -> Result<Channel, HvError>;
}

// Channel handle — supports split for concurrent send/recv (needed by netvsc)
impl Channel {
    pub fn split(&mut self) -> (&mut SendRing, &mut RecvRing);
}

impl SendRing {
    pub fn send(&mut self, data: &[u8]) -> Result<(), RingError>;
    pub fn has_space(&self, len: usize) -> bool;
}

impl RecvRing {
    pub fn recv(&mut self, buf: &mut [u8]) -> Result<usize, RingError>;
    pub fn poll_recv(&self) -> bool;
}

// Cleanup (explicit, mirrors e1000 Drop pattern)
impl Drop for Channel {
    fn drop(&mut self) {
        // Close channel → teardown GPADL → free ring buffer
    }
}

impl Drop for VmBus {
    fn drop(&mut self) {
        // Unload VMBus → disable SynIC → clear MSRs
    }
}
```

**CPUID guard**: All MSR/hypercall operations are behind the CPUID
check. On QEMU (no Hyper-V), `detect_hyperv()` returns `None` and
no VMBus code executes. Existing QEMU tests are unaffected.

## Memory Budget

| Allocation | Size | When |
|------------|------|------|
| Hypercall page | 4 KB | Phase 1 |
| SynIC pages (SIMP + SIEFP) | 8 KB | Phase 1 |
| Monitor pages (parent↔child) | 8 KB | Phase 2 |
| HvPostMessage buffer | 4 KB | Phase 2 |
| Ring buffer (synthvid, 128KB×2) | 256 KB | Phase 3 |
| VRAM (1024×768×32bpp) | 3 MB | Phase 4 |
| **Total** | **~3.3 MB** | |

**Decision**: Increase heap from 4 MB to **8 MB** to accommodate VMBus
allocations alongside the kernel, Embassy, and smoltcp. Cap initial
resolution at 1024×768×32bpp (3 MB VRAM). Higher resolutions (1080p =
8 MB VRAM) would need a separate allocation region or further heap
increase.

VRAM does **not** need DMA-coherent memory (no device DMA involved —
the host maps guest physical pages via GPADL). A simple heap allocation
with page-table walk for PFNs is sufficient.

## Implementation Phases

### Phase 1: Detect + MSR + Hypercall (~300 LOC)

- CPUID detection with feature check
- Guest OS ID, hypercall page, SynIC setup
- `HvPostMessage` working
- **Verify**: Send `INITIATE_CONTACT`, receive `VERSION_RESPONSE`
- **Test**: Run on Hyper-V Gen2, check VM doesn't crash (kernel runs)

### Phase 2: VMBus Connect + Offers (~400 LOC)

- Version negotiation
- Request and enumerate channel offers
- Log all device GUIDs via serial (QEMU) or store for later display
- **Verify**: See synthvid + netvsc GUIDs in offers

### Phase 3: GPADL + Channel Open (~350 LOC)

- Ring buffer allocation and GPADL creation
- Channel open for synthvid
- Ring buffer send/receive
- **Verify**: Synthvid channel open succeeds

### Phase 4: Synthvid (~300 LOC)

- Version negotiation (v3.5 → fallback v3.2)
- VRAM allocation and location message
- Resolution set + dirty rectangle
- **Verify**: Text appears on VM Connect display 🎉

## Testing Strategy

- **QEMU**: Cannot test VMBus (no Hyper-V). Use CPUID detection to
  skip VMBus init. Existing QEMU tests unaffected.
- **Hyper-V Gen2**: Crash-checkpoint debugging (ud2 at specific init
  stages) since Gen2 has no serial output. All stages 1-9 verified.
- **Unit tests** (`embclox-tests`): Ring buffer logic, message
  serialization, GPADL PFN list construction. Uses `std`.

## Implementation Status

### Completed (verified on real Hyper-V Gen2)
- [x] CPUID detection + Guest OS ID MSR
- [x] Hypercall page (alloc, map executable, HvPostMessage, HvSignalEvent)
- [x] SynIC (SIMP, SIEFP, SINT2)
- [x] VMBus version negotiation (WIN10 v4.0 accepted)
- [x] Channel offer enumeration (synthvid, netvsc GUIDs found)
- [x] GPADL creation (per-page PFN translation)
- [x] Channel open (ring buffer, pipe mode)
- [x] Ring buffer send/recv with volatile access + memory fences
- [x] Synthvid version negotiation (v3.5 accepted)
- [x] VRAM location acknowledged by host
- [x] Heap increased to 8 MB
- [x] `map_code` for executable page mappings
- [x] `translate_addr` for page table walks
- [x] 64-bit PCI BAR support

### Not yet working
- [ ] Synthvid display output (screen remains black)

### Gen2 display: known issue

No hobby OS has achieved display output on Hyper-V Gen2. Only Linux
and FreeBSD have working synthvid drivers (hyperv_fb). Our VMBus stack
completes the full synthvid protocol without errors, but VM Connect
shows black.

Key findings from debugging:
- Gen2 has no COM port; port 0x3F8 is not emulated (no serial output)
- `Set-VMComPort` configures a named pipe but the UEFI firmware does
  not emulate a 16550 UART — PuTTY connects but receives nothing
- VM Connect works (confirmed with empty Gen2 VM showing UEFI screen)
- After ExitBootServices, the UEFI firmware's synthvid driver is
  unloaded and the display goes black — expected behavior
- PCI video device (0x1414:0x5353) exists with non-zero BAR0
- All synthvid messages accepted (no Hyper-V Event Viewer errors)
- HvSignalEvent fast hypercall + monitor page signaling both tried
- PCI BAR VRAM, GOP framebuffer VRAM, and heap VRAM all tried

Next steps for Gen2 display:
1. Boot Linux in same Gen2 VM, capture synthvid messages via ftrace
2. Compare byte-for-byte with our implementation
3. Alternatively, pursue Gen1 VGA path (like ReactOS — simpler, proven)

## Prerequisites

- [x] Verify `map_mmio` page permissions (RW vs RWX) for hypercall page
      → Added `map_code` with PRESENT|WRITABLE (no NO_CACHE, no NO_EXECUTE)
- [x] Increase heap from 4 MB to 8 MB in `embclox-hal-x86::heap`
- [ ] Extract `DmaAllocator` trait from `embclox-e1000` to shared crate
      (deferred — circular dependency; embclox-hyperv deps on embclox-e1000 directly)

## Performance Path: SR-IOV (Future)

VMBus netvsc is the **baseline** networking path — it works on all
Hyper-V/Azure VMs. For high-performance networking, Azure offers
**Accelerated Networking** which exposes an SR-IOV Virtual Function
(Mellanox ConnectX VF) directly into the VM via PCI passthrough.

```
Baseline path:               High-performance path:
App → netvsc → VMBus →       App → DPDK/driver → Mellanox VF → NIC
  Host → NIC                      (direct PCI, bypasses VMBus)
```

DPDK and SR-IOV bypass VMBus entirely — the VF appears as a standard
PCI device with its own BAR and interrupt. VMBus is **not in the hot
path** for high-performance networking.

For embclox, this means a future Mellanox `mlx5` VF driver (or DPDK
poll-mode driver) could deliver near-native line rate. The netvsc
VMBus path remains as the universal fallback and control plane. The
SR-IOV VF is only available on specific Azure VM sizes (e.g., Dv3,
Ev3, Fsv2) with Accelerated Networking enabled.

## Gen1 vs Gen2: Hardware Comparison

Gen1 emulates legacy PC hardware. Gen2 uses only synthetic (VMBus)
devices. No hobby OS has achieved display on Gen2 — only Linux and
FreeBSD have working synthvid drivers.

### Emulated hardware (Gen1 only)

| Device | Emulation | PCI ID | Driver needed |
|--------|-----------|--------|---------------|
| Display | Standard VGA | N/A (ISA) | VGA port I/O at 0x3C0-0x3DF, framebuffer at 0xA0000 |
| Network | DEC 21140 "Tulip" | `0x1011:0x0009` | Tulip driver (NOT e1000) |
| Storage | IDE/ATA | N/A (ISA) | Standard IDE driver |
| Serial | 16550 UART | N/A (ISA) | Port 0x3F8 (COM1) — works with named pipe |

### Synthetic hardware (Gen1 + Gen2, requires VMBus)

| Device | VMBus GUID | Protocol |
|--------|------------|----------|
| Network (netvsc) | `F8615163-DF3E-46C5-...` | NVSP over VMBus ring buffer |
| Video (synthvid) | `DA0A7802-E377-4AAC-...` | Synthvid over VMBus ring buffer |
| Storage (storvsc) | `BA6163D9-04A1-4D29-...` | SCSI over VMBus ring buffer |

### ReactOS approach (Gen1)

ReactOS runs on Hyper-V Gen1 and Azure Gen1 using ONLY legacy emulated
devices — no VMBus drivers. It uses VGA for display and DEC 21140
(Tulip) for networking. This is slow but works without any Hyper-V
specific code.

ReactOS does NOT support Gen2 (no UEFI boot, no synthvid, no netvsc).
It also does not implement any VMBus drivers. Azure Gen1 support is
experimental/demo only.

### Recommended path for embclox

**Gen1 for initial bring-up:**
- VGA text mode for display (immediate, no driver needed)
- DEC 21140 Tulip for networking (new driver, well-documented chip)
- COM1 serial via named pipe for debug output
- Avoids bootloader VBE hang (use text mode instead of graphics)

**Gen2 / Azure for production:**
- VMBus + netvsc for networking (embclox-hyperv crate, partially built)
- VMBus + synthvid for display (protocol complete, display WIP)
- SR-IOV for high-performance networking (future)

### Gen2 serial port reality

- `Set-VMComPort` succeeds on Gen2 (configures named pipe)
- BUT the UEFI firmware does NOT emulate a 16550 UART at 0x3F8
- PuTTY connects to the pipe but receives nothing
- The only serial on Gen2 is the VMBus synthetic serial device
  (GUID `3BEE6146-20A3-4A86-...`), accessible only via Azure
  Serial Console — not available on local Hyper-V
