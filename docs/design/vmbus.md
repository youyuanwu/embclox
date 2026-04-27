# Design: VMBus Implementation

## Status

**VMBus core: complete.** The `embclox-hyperv` crate implements the full
VMBus stack from CPUID detection through ring buffer I/O. Verified on
Hyper-V Gen2 VMs.

**Synthvid: protocol complete, display black.** All synthvid messages
accepted by host, but VM Connect shows black. No hobby OS has achieved
Gen2 display — only Linux/FreeBSD have working synthvid (hyperv_fb).

**NetVSC: not yet implemented.** See `hyperv-netvsc.md` for the plan.

### Current networking paths

| Target | NIC | Driver | Status |
|--------|-----|--------|--------|
| QEMU | DEC 21143 (Tulip) | `embclox-tulip` | ✅ Working |
| QEMU | Intel 82540EM (e1000) | `embclox-e1000` | ✅ Working |
| Local Hyper-V Gen1 | DEC 21140 (Legacy NIC) | `embclox-tulip` | ✅ Working |
| Azure Gen1 | VMBus synthetic | netvsc (TODO) | ❌ No legacy NIC on PCI |
| Hyper-V Gen2 | VMBus synthetic | netvsc (TODO) | ❌ No legacy NIC |

## Architecture

```
┌──────────────────────────────────────────────────────┐
│  Device drivers: synthvid, netvsc (TODO), storvsc    │
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
│  │ ring (read/write with safety)       │            │
│  └──────────────────────────────────────┘            │
├──────────────────────────────────────────────────────┤
│  Hyper-V hypervisor (MSRs, hypercalls, SynIC)        │
└──────────────────────────────────────────────────────┘
```

## Crate modules (`embclox-hyperv`)

| Module | LOC | Purpose | Status |
|--------|-----|---------|--------|
| `detect.rs` | ~40 | CPUID 0x40000000 "Microsoft Hv" detection | ✅ |
| `msr.rs` | ~30 | rdmsr/wrmsr inline wrappers | ✅ |
| `hypercall.rs` | ~120 | Hypercall page (RX mapping), HvPostMessage, HvSignalEvent | ✅ |
| `synic.rs` | ~100 | SynIC init (SIMP, SIEFP, SINT2), message polling | ✅ |
| `vmbus.rs` | ~400 | Version negotiation (WIN10/WIN8_1), channel offers | ✅ |
| `channel.rs` | ~300 | Channel open/close, GPADL creation, send/recv | ✅ |
| `ring.rs` | ~250 | Ring buffer read/write with fences and wrap-around | ✅ |
| `guid.rs` | ~30 | Well-known device GUIDs (synthvid, netvsc, etc.) | ✅ |
| `synthvid.rs` | ~300 | Synthvid protocol (version, VRAM, resolution, dirty rect) | ✅ |

## Init sequence

```
CPUID 0x40000000 → detect "Microsoft Hv"
  ↓
MSR: Guest OS ID → Hypercall page (RX) → SynIC (SCONTROL, SIMP, SIEFP, SINT2)
  ↓
HvPostMessage: INITIATE_CONTACT (version WIN10 v4.0)
  ↓
Poll SIMP: VERSION_RESPONSE (accepted)
  ↓
HvPostMessage: REQUEST_OFFERS
  ↓
Poll SIMP: OFFERCHANNEL × N → collect offers by GUID
Poll SIMP: ALLOFFERS_DELIVERED
  ↓
Ready — open channels for synthvid, netvsc, etc.
```

## Channel lifecycle

```
Host                              Guest
  │                                  │
  │── ChannelOffer (GUID, relid) ──>│  Host offers device
  │                                  │
  │<── GPADL_HEADER (ring PFNs) ────│  Guest shares ring buffer
  │<── GPADL_BODY (more PFNs) ─────│  (if >26 pages)
  │── GPADL_CREATED (handle) ─────>│
  │                                  │
  │<── OPENCHANNEL (gpadl, vec) ────│  Guest opens channel
  │── OPENCHANNEL_RESULT ──────────>│
  │                                  │
  │<══ Ring buffer data flow ══════>│  Device-specific protocol
```

## Ring buffer format

Each channel has a send + receive ring in a shared GPADL allocation
(default 256 KB = 128 KB per direction).

```
┌─────────────────────┐  offset 0
│  Control page (4KB) │  write_index, read_index, feature_bits
├─────────────────────┤
│  Data buffer        │  circular byte buffer
└─────────────────────┘

Packet format in data buffer:
  [VmPacketDescriptor: 16 bytes]  type=6, offset, length, flags, txid
  [Payload: variable]             device-specific (synthvid/netvsc msg)
  [Padding: 0-7 bytes]            align to 8 bytes
  [Trailing indices: 8 bytes]     (write_idx << 32) | read_idx
```

Key invariants:
- Volatile access for shared indices (host writes `write_index`)
- `fence(Acquire)` after reading host index, `fence(Release)` before
  updating our index
- Wrap-around safe memcpy for circular buffer
- Bounds-check all host-sourced values before use

## Well-known device GUIDs

| Device | GUID | Status |
|--------|------|--------|
| Synthvid (video) | `DA0A7802-E377-4AAC-8E77-0558EB1073F8` | ✅ Implemented |
| NetVSC (network) | `F8615163-DF3E-46C5-913F-F2D2F965ED0E` | Next (see netvsc.md) |
| Keyboard | `F912AD6D-2B17-48EA-BD65-F927A61C7684` | — |
| Heartbeat | `57164F39-9115-4E78-AB55-382F3BD5422D` | — |

## Memory budget

| Allocation | Size | Purpose |
|------------|------|---------|
| Hypercall page | 4 KB | Host-filled code page (RX mapping) |
| SynIC pages (SIMP + SIEFP) | 8 KB | Message + event delivery |
| Monitor pages | 8 KB | Channel signaling optimization |
| HvPostMessage buffer | 4 KB | Outgoing control messages |
| Ring buffer (per channel) | 256 KB | Send + receive rings |
| VRAM (synthvid) | 3 MB | 1024×768×32bpp framebuffer |
| NVSP shared buffers (netvsc) | ~3 MB | 2 MB receive + 1 MB send |

Heap must be ≥8 MB to accommodate VMBus + networking.

## Gen2 display: known issue

The synthvid protocol completes without errors on Gen2, but the
display remains black. No hobby OS has achieved this.

Key findings:
- Gen2 has no COM port (0x3F8 not emulated) — no serial debug
- After ExitBootServices, UEFI's synthvid driver is unloaded → black
- PCI video device (1414:5353) exists with non-zero BAR0
- All synthvid messages accepted (no Hyper-V Event Viewer errors)
- Tried: PCI BAR VRAM, GOP framebuffer VRAM, heap VRAM — all black

## Testing

- **QEMU**: Cannot test VMBus (no Hyper-V emulation). CPUID detection
  skips VMBus init. Existing QEMU tests unaffected.
- **Local Hyper-V Gen1**: BIOS boot, COM1 serial via named pipe.
  Use `scripts/hyperv-tulip-test.ps1` as reference.
- **Local Hyper-V Gen2**: UEFI boot, no serial — crash-checkpoint
  debugging only.
- **Azure Gen1**: VHD boot via `tests/infra/main.bicep`, serial
  console via boot diagnostics.
