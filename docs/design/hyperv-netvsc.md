# Design: Hyper-V NetVSC Driver

## Motivation

Azure Gen1 VMs do not expose a legacy DEC 21140 NIC on the PCI bus —
the only network path is VMBus synthetic NIC (netvsc). The Tulip driver
works on local Hyper-V Gen1, but for Azure and Gen2 VMs, netvsc is
required.

PCI scan on Azure Gen1 shows only:

| Slot | Vendor:Device | Description |
|------|---------------|-------------|
| 00:00.0 | 8086:7192 | Intel 440BX PCI bridge |
| 07:00.0 | 8086:7110 | Intel PIIX4 ISA bridge |
| 08:00.0 | 1414:5353 | Hyper-V Synthetic VGA |

No NIC on PCI. Networking requires VMBus channel `F8615163-DF3E-46C5-913F-F2D2F965ED0E`.

## What's already implemented

The `embclox-hyperv` crate provides the complete VMBus stack (see
`vmbus.md`):

- ✅ CPUID detection, MSR setup, hypercall page
- ✅ SynIC message/event delivery
- ✅ VMBus version negotiation, channel offer enumeration
- ✅ Channel open/close with GPADL and ring buffers
- ✅ Ring buffer send/recv with fences and wrap-around
- ✅ Synthvid as a reference device driver implementation

The netvsc GUID is already recognized in `guid.rs`. Opening the
netvsc channel uses the same `VmBus::open_channel()` as synthvid.

## Protocol stack

```
┌────────────────────────────────────────────────┐
│  embassy-net / smoltcp  (existing)             │
├────────────────────────────────────────────────┤
│  NetVSC Embassy adapter  (Driver trait impl)   │
├────────────────────────────────────────────────┤
│  RNDIS  (frame encapsulation)                  │
│  Ethernet frame ↔ RNDIS_PACKET_MSG             │
├────────────────────────────────────────────────┤
│  NVSP  (Network VSP Protocol)                  │
│  Version negotiation, shared buffer setup      │
├────────────────────────────────────────────────┤
│  VMBus channel  (ring buffers)  ← EXISTING     │
├────────────────────────────────────────────────┤
│  Hyper-V hypercalls + SynIC     ← EXISTING     │
└────────────────────────────────────────────────┘
```

Three new layers to implement: NVSP, RNDIS, and the Embassy adapter.

## Layer 1: NVSP (Network VSP Protocol)

NVSP negotiates the network protocol version and establishes shared
memory buffers before any packets can flow. It sits between VMBus
ring buffers and RNDIS.

### Init sequence

1. Send `NVSP_MSG_TYPE_INIT` (version 5 or 4)
2. Recv `NVSP_MSG_TYPE_INIT_COMPLETE` (accepted version)
3. Send `NVSP_MSG1_TYPE_SEND_RECV_BUF` — GPADL for receive buffer (~2 MB)
4. Recv `NVSP_MSG1_TYPE_SEND_RECV_BUF_COMPLETE` — host confirms sections
5. Send `NVSP_MSG1_TYPE_SEND_SEND_BUF` — GPADL for send buffer (~1 MB)
6. Recv `NVSP_MSG1_TYPE_SEND_SEND_BUF_COMPLETE`

### Data path

- **TX**: RNDIS packet → `NVSP_MSG1_TYPE_SEND_RNDIS_PKT` → VMBus ring
- **RX**: VMBus ring → `NVSP_MSG1_TYPE_SEND_RNDIS_PKT_COMPLETE` → RNDIS

NVSP wraps RNDIS messages with transfer page ranges that reference
offsets within the shared receive/send buffers.

### Message format

Following synthvid's pattern (pipe_hdr + protocol_hdr + body):

```
VMBus ring packet:
  [VmPacketDescriptor: 16 bytes]    standard VMBus header
  [NvspMessage: variable]           NVSP header + body
```

### Estimated LOC: ~300

## Layer 2: RNDIS (Remote NDIS)

RNDIS encapsulates Ethernet frames over the NVSP transport. It also
handles device initialization (version, MAC address, packet filters).

### Required message types

| Message | Code | Direction | Purpose |
|---------|------|-----------|---------|
| `RNDIS_INITIALIZE_MSG` | 0x00000002 | G→H | Init session |
| `RNDIS_INITIALIZE_CMPLT` | 0x80000002 | H→G | Init response |
| `RNDIS_QUERY_MSG` | 0x00000004 | G→H | Query OIDs (MAC, MTU) |
| `RNDIS_QUERY_CMPLT` | 0x80000004 | H→G | Query response |
| `RNDIS_SET_MSG` | 0x00000005 | G→H | Set config (packet filter) |
| `RNDIS_SET_CMPLT` | 0x80000005 | H→G | Set response |
| `RNDIS_PACKET_MSG` | 0x00000001 | Both | Data packet (Ethernet frame) |
| `RNDIS_KEEPALIVE_MSG` | 0x00000008 | H→G | Keepalive (must respond) |
| `RNDIS_KEEPALIVE_CMPLT` | 0x80000008 | G→H | Keepalive response |

### Init sequence

1. Send `RNDIS_INITIALIZE_MSG` (version 1.0, max transfer size)
2. Recv `RNDIS_INITIALIZE_CMPLT` — check status, verify version
3. Query `OID_802_3_PERMANENT_ADDRESS` → MAC address
4. Query `OID_GEN_MAXIMUM_FRAME_SIZE` → MTU
5. Set `OID_GEN_CURRENT_PACKET_FILTER` → enable receive
6. Ready for `RNDIS_PACKET_MSG` send/recv

### Data flow

**TX**: Ethernet frame → prepend `RNDIS_PACKET_MSG` header (44 bytes)
→ write to NVSP send buffer → signal host

**RX**: Host writes `RNDIS_PACKET_MSG` to receive buffer → SynIC
interrupt → strip 44-byte header → Ethernet frame

Each request/response correlated by `request_id`.

### Estimated LOC: ~500

## Layer 3: Embassy adapter

Thin `embassy_net_driver::Driver` implementation wrapping the RNDIS
device. Same pattern as `tulip_embassy.rs`:

```rust
impl Driver for NetvscEmbassy {
    fn receive(&mut self, cx: &mut Context) -> Option<(RxToken, TxToken)>;
    fn transmit(&mut self, cx: &mut Context) -> Option<TxToken>;
    fn link_state(&mut self, cx: &mut Context) -> LinkState;
    fn capabilities(&self) -> Capabilities;
    fn hardware_address(&self) -> HardwareAddress;
}
```

Static `NETVSC_WAKER: AtomicWaker` for SynIC interrupt → executor wake.

### Estimated LOC: ~150

## Implementation plan

All testing on local Hyper-V Gen1 first (COM1 serial for debug),
then Azure Gen1 via `tests/infra/main.bicep`.

### Phase 1: NVSP channel setup (~300 LOC)

1. Open netvsc VMBus channel (same as synthvid)
2. NVSP version negotiation (try v5, fall back to v4)
3. Allocate and register receive/send buffers via GPADL
4. **Verify**: NVSP init completes without error (serial log)

### Phase 2: RNDIS init (~300 LOC)

1. RNDIS version negotiation (v1.0)
2. Query MAC address and MTU via OID
3. Set packet filter to enable receive
4. **Verify**: MAC address read from host, filter set

### Phase 3: Packet send/recv (~200 LOC)

1. TX: wrap Ethernet frame in RNDIS_PACKET_MSG, write to ring
2. RX: parse RNDIS_PACKET_MSG from ring, extract frame
3. Keepalive response handler
4. **Verify**: send gratuitous ARP, receive DHCP response

### Phase 4: Embassy integration + TCP echo (~150 LOC)

1. Embassy adapter with NETVSC_WAKER
2. Wire to embassy-net stack with DHCP
3. TCP echo server on port 1234
4. **Verify**: TCP echo from host on local Hyper-V, then Azure

### Total estimated: ~1000 LOC new code

(VMBus infrastructure is already done — ~1500 LOC in `embclox-hyperv`)

## Crate structure

```
crates/embclox-hyperv/src/
  ├── (existing VMBus core modules)
  ├── nvsp.rs              NEW — NVSP protocol messages + init
  ├── rndis.rs             NEW — RNDIS messages, OID queries
  └── netvsc.rs            NEW — NetVSC device (send/recv frames)

crates/embclox-core/src/
  └── netvsc_embassy.rs    NEW — Embassy Driver adapter
```

NetVSC lives inside `embclox-hyperv` (not a separate crate) because it
shares VMBus channel/ring internals. The Embassy adapter goes in
`embclox-core` alongside `e1000_embassy.rs` and `tulip_embassy.rs`.

## Memory budget

| Allocation | Size | Notes |
|------------|------|-------|
| NVSP receive buffer | 2 MB | Shared via GPADL, host writes RX packets here |
| NVSP send buffer | 1 MB | Shared via GPADL, guest writes TX packets here |
| Ring buffer | 256 KB | Standard VMBus channel ring (128 KB × 2) |
| **Total** | ~3.3 MB | On top of existing VMBus overhead (~280 KB) |

With 8 MB heap (already configured for VMBus), ~4.4 MB free for
kernel + Embassy + smoltcp. Sufficient for a TCP echo server.

Buffer sizes are negotiable — can reduce to 1 MB recv + 512 KB send
if memory is tight, at the cost of throughput.

## Testing strategy

| Environment | Boot | Serial | NIC | Use |
|-------------|------|--------|-----|-----|
| QEMU | Limine BIOS | COM1 stdio | Tulip (PCI) | CI + dev (no VMBus) |
| Local Hyper-V Gen1 | Limine BIOS (ISO) | COM1 named pipe | Synthetic (VMBus) | Primary netvsc dev |
| Azure Gen1 | Limine BIOS (VHD) | Serial console | Synthetic (VMBus) | Cloud validation |

**Key**: local Hyper-V Gen1 VMs can have both a legacy NIC (Tulip)
and a synthetic NIC. During development, keep the legacy NIC as
fallback while bringing up netvsc. The kernel can detect which NICs
are available (PCI scan for Tulip, VMBus offer for netvsc) and use
whichever is present.

## Linux source reference

| File | LOC | Layer |
|------|-----|-------|
| `drivers/net/hyperv/netvsc.c` | ~1600 | NetVSC + NVSP |
| `drivers/net/hyperv/rndis_filter.c` | ~1200 | RNDIS protocol |
| `drivers/net/hyperv/netvsc_drv.c` | ~2400 | Linux netdev glue |

Not all of this applies — Linux abstractions (workqueues, netdev,
NAPI, SKBs) don't exist in bare-metal. Our estimate of ~1000 LOC
reflects the minimal viable path.

## Protocol reference

### NVSP message types

| Code | Name | Direction | Purpose |
|------|------|-----------|---------|
| 1 | `NVSP_MSG_TYPE_INIT` | G→H | Version negotiation |
| 2 | `NVSP_MSG_TYPE_INIT_COMPLETE` | H→G | Version response |
| 100 | `NVSP_MSG1_TYPE_SEND_RECV_BUF` | G→H | Register receive buffer GPADL |
| 101 | `NVSP_MSG1_TYPE_SEND_RECV_BUF_COMPLETE` | H→G | Confirm receive buffer |
| 102 | `NVSP_MSG1_TYPE_REVOKE_RECV_BUF` | G→H | Teardown receive buffer |
| 103 | `NVSP_MSG1_TYPE_SEND_SEND_BUF` | G→H | Register send buffer GPADL |
| 104 | `NVSP_MSG1_TYPE_SEND_SEND_BUF_COMPLETE` | H→G | Confirm send buffer |
| 105 | `NVSP_MSG1_TYPE_REVOKE_SEND_BUF` | G→H | Teardown send buffer |
| 107 | `NVSP_MSG1_TYPE_SEND_RNDIS_PKT` | G→H | Data packet (RNDIS wrapped) |
| 108 | `NVSP_MSG1_TYPE_SEND_RNDIS_PKT_COMPLETE` | H→G | Data packet completion |

NVSP versions: v1 (WIN2008), v2 (WIN2008R2), v4 (WIN2012), v5 (WIN2012R2+).
Start with v5, fall back to v4.

### NVSP shared buffer mechanism

NVSP uses two large shared buffers (separate from the VMBus ring buffer):

- **Receive buffer** (~2 MB): host writes incoming packets here as
  offsets within the buffer. The `SEND_RNDIS_PKT_COMPLETE` message
  includes transfer page ranges `(offset, length)` referencing this
  buffer.
- **Send buffer** (~1 MB): guest writes outgoing packets here. The
  `SEND_RNDIS_PKT` message references the offset.

Both buffers are shared via GPADL (same mechanism as ring buffers).
The GPADL handle is sent in the `SEND_RECV_BUF` / `SEND_SEND_BUF`
messages.

### RNDIS message format

```c
// Common header for all RNDIS messages
struct rndis_msg_hdr {
    uint32_t msg_type;       // RNDIS_*_MSG constant
    uint32_t msg_len;        // Total message length including header
};

// RNDIS_INITIALIZE_MSG (0x00000002)
struct rndis_initialize_msg {
    rndis_msg_hdr hdr;
    uint32_t request_id;
    uint32_t major_version;  // 1
    uint32_t minor_version;  // 0
    uint32_t max_xfer_size;  // e.g., 0x4000 (16 KB)
};

// RNDIS_QUERY_MSG (0x00000004)
struct rndis_query_msg {
    rndis_msg_hdr hdr;
    uint32_t request_id;
    uint32_t oid;            // OID_802_3_PERMANENT_ADDRESS, etc.
    uint32_t info_buf_len;   // 0 for most queries
    uint32_t info_buf_offset;// Offset from start of request_id
    uint32_t device_vc_handle; // 0
};

// RNDIS_SET_MSG (0x00000005)
struct rndis_set_msg {
    rndis_msg_hdr hdr;
    uint32_t request_id;
    uint32_t oid;
    uint32_t info_buf_len;
    uint32_t info_buf_offset;
    uint32_t device_vc_handle;
    // followed by info buffer data
};

// RNDIS_PACKET_MSG (0x00000001) — data packets
struct rndis_packet_msg {
    rndis_msg_hdr hdr;       // msg_type = 1, msg_len = hdr + data
    uint32_t data_offset;    // Offset from start of data_offset field
    uint32_t data_len;       // Ethernet frame length
    uint32_t oob_data_offset;
    uint32_t oob_data_len;
    uint32_t num_oob_data_elements;
    uint32_t per_pkt_info_offset;
    uint32_t per_pkt_info_len;
    uint32_t vc_handle;      // 0
    uint32_t reserved;       // 0
    // followed by Ethernet frame at data_offset
};
// RNDIS_PACKET_MSG header size: 44 bytes
```

### OID constants

| OID | Code | Purpose | Response |
|-----|------|---------|----------|
| `OID_802_3_PERMANENT_ADDRESS` | 0x01010101 | MAC address | 6 bytes |
| `OID_GEN_MAXIMUM_FRAME_SIZE` | 0x00010106 | MTU | 4 bytes (u32) |
| `OID_GEN_CURRENT_PACKET_FILTER` | 0x0001010E | Enable receive | Set to NDIS_PACKET_TYPE_ALL_LOCAL (0x80) |
| `OID_GEN_LINK_SPEED` | 0x00010107 | Link speed | 4 bytes (100bps units) |

### GPADL for shared buffers

GPADL creation for NVSP shared buffers follows the same pattern as
VMBus ring buffers (already implemented in `channel.rs`):

1. Allocate physically contiguous memory (or translate per-page PFNs)
2. Send `GPADL_HEADER` with PFN list via HvPostMessage
3. Send `GPADL_BODY` for remaining PFNs if >26 pages
4. Receive `GPADL_CREATED` with handle
5. Use handle in `NVSP_MSG1_TYPE_SEND_RECV_BUF`

The key difference: NVSP shared buffers are larger (~2 MB = 512 pages)
so GPADL_BODY messages are required (ring buffers are 256 KB = 64 pages,
which often fit in a single GPADL_HEADER).

### Teardown sequence

Proper teardown ordering (enforced via Rust Drop):

1. Revoke send buffer (`NVSP_MSG1_TYPE_REVOKE_SEND_BUF`)
2. Revoke receive buffer (`NVSP_MSG1_TYPE_REVOKE_RECV_BUF`)
3. Close VMBus channel
4. Teardown GPADL handles
5. Free DMA allocations
