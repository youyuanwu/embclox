# Hyper-V NetVSC Driver

NetVSC is the synthetic NIC exposed to guests over VMBus. It's the only
network path on Azure (Gen1 PCI scan shows only chipset bridges and a
synthetic VGA — no NIC) and the production path on local Hyper-V Gen2.

VMBus channel: `F8615163-DF3E-46C5-913F-F2D2F965ED0E`

## Status: shipped

| Layer | Where | Verified |
|-------|-------|----------|
| NVSP — version negotiation, recv/send buffer GPADLs | `crates/embclox-hyperv/src/netvsc.rs` | NVSP v6.1 negotiated on local Hyper-V Gen1 + Azure Gen1 |
| RNDIS — init, MAC/MTU query, packet filter | `crates/embclox-hyperv/src/{nvsp_msg.rs,nvsp_types.rs}` | RNDIS v1.0, MAC/MTU read back on both |
| RNDIS_PACKET_MSG TX/RX | `netvsc.rs::transmit_with` / `recv_with` | Gratuitous ARP + DHCP DISCOVER → reply received |
| SynIC SINT2 → AtomicWaker | `examples-hyperv/src/main.rs::vmbus_isr` + `netvsc::NETVSC_WAKER` | IRQ count > 0 in test logs |
| `embassy_net_driver::Driver` impl | `crates/embclox-hyperv/src/netvsc_embassy.rs` | TCP echo @ port 1234 verified locally + on Azure |
| Cmdline-driven DHCP/static selection | `embclox_hal_x86::cmdline` + `examples-hyperv/limine.conf` | Both modes parse correctly |
| Azure Gen1 deployment | `tests/infra/{storage,vm}.bicep` + `cmake --build build --target hyperv-vhd` | TCP echo from public Internet to bare-metal kernel: `echo X \| nc <public-ip> 1234` returns `X` |

## Architecture

```
┌────────────────────────────────────────────────┐
│  embassy-net / smoltcp                         │
├────────────────────────────────────────────────┤
│  netvsc_embassy::NetvscEmbassy (Driver impl)   │
├────────────────────────────────────────────────┤
│  netvsc::NetvscDevice                          │
│   • RNDIS  (frame encapsulation)               │
│   • NVSP   (version, shared buffers)           │
├────────────────────────────────────────────────┤
│  VMBus channel (channel.rs, ring.rs)           │
├────────────────────────────────────────────────┤
│  Hypercalls + SynIC (hypercall.rs, synic.rs)   │
└────────────────────────────────────────────────┘
```

NetVSC lives inside `embclox-hyperv` because it shares VMBus internals.
The embassy adapter is a separate small module so it can be omitted in
contexts that don't use embassy-net.

## Buffer layout (per device)

| Allocation | Size | Notes |
|------------|------|-------|
| NVSP receive buffer | 2 MB | Shared via GPADL, host writes RX packets here |
| NVSP send buffer | 1 MB | Shared via GPADL, guest writes TX packets here |
| Ring buffer | 256 KB | Standard VMBus channel ring (128 KB × 2) |

Ring uses `VM_PKT_DATA_INBAND` with `send_buf_section_index` referencing
an offset in the send buffer (matches Linux's batched-TX path; the
GPA-direct path is not implemented because the section path is
sufficient).

## Network mode at boot

`examples-hyperv/limine.conf` exposes two boot menu entries that select
the embassy-net config via the kernel command line:

| Entry | cmdline | Used for |
|-------|---------|----------|
| static (default) | (empty) | local Hyper-V on `embclox-test` Internal vSwitch |
| DHCP | `net=dhcp` | Azure (host has a real DHCP server) |

Cmdline tokens parsed by [`embclox_hal_x86::cmdline`]:

| Token | Effect |
|-------|--------|
| `net=dhcp` | embassy-net DHCPv4 |
| `net=static` | embassy-net static IPv4 (uses `ip=`/`gw=` if present, else defaults) |
| `ip=A.B.C.D/N` | Override static IP+prefix |
| `gw=A.B.C.D` | Override static gateway |

Default static address is `192.168.234.50/24` (matches
`scripts/hyperv-setup-vswitch.ps1`). See
[`docs/dev/HyperV-Testing.md`](../dev/HyperV-Testing.md) for the rationale
behind the dedicated Internal vSwitch.

## Testing

| Environment | NIC | Network | Use |
|-------------|-----|---------|-----|
| QEMU SLIRP | Tulip (PCI) | DHCP | CI (smoltcp DHCP coverage, `ctest -R tulip-echo`) |
| Local Hyper-V Gen1 | NetVSC (VMBus) | static, embclox-test vSwitch | Manual: `scripts/hyperv-boot-test.ps1` |
| Azure Gen1 | NetVSC (VMBus) | DHCP | Manual: `tests/infra/{storage,vm}.bicep` (TCP echo verified end-to-end) |

## Future work

These are concrete next steps that would build directly on the shipped
implementation. None are blocking; the data path, embassy integration,
and TCP echo all work today on local Hyper-V, QEMU, and Azure.

### High-leverage

- **NDIS_INDICATE_STATUS handler.** Real link-state changes (cable
  unplug, MTU change, link speed change). The embassy adapter
  currently reports `LinkState::Up` unconditionally. Linux's
  `rndis_filter_receive_indicate_status` is the reference.

- **CI on Azure.** The bicep templates and `hyperv-vhd` cmake target
  are in place; a CI job that runs `az deployment group create`,
  probes TCP echo, and tears down would catch NetVSC regressions
  against a real production Hyper-V host (not local Default Switch).

### Performance

- **LAPIC timer ISR.** Today the executor poll loop spin-waits between
  `embassy_time` alarm wakeups (no APIC timer is wired). Adding an
  APIC timer interrupt that calls `embclox_hal_x86::time::on_timer_tick`
  would let the loop `hlt` between events for proper idle. Pure
  power/perf, not correctness.

- **NetVSC subchannels.** Linux uses `nvsp_5_send_indirect_table` to
  spread RX/TX across multiple channels for higher throughput. We use
  one queue and it works for TCP echo. Reference: Linux
  `drivers/net/hyperv/netvsc.c::netvsc_init_buf` and
  `rndis_filter.c::netvsc_set_queues`.

- **Hardware offloads (TCP/IP checksum, LSO).** Linux's
  `netvsc_drv.c::netvsc_xmit` adds per-packet info structs (PPI) for
  CSUM/LSO. We always set `PerPacketInfoLength = 0`. For wire-speed
  TCP this is the biggest single win on Hyper-V hosts that support it.

### Robustness / cleanup

- **Teardown.** Rust `Drop` should send REVOKE_SEND_BUF + REVOKE_RECV_BUF
  + close channel + free GPADLs in order. Today the device is created
  once and lives until kernel halt, so teardown is unexercised.

- **DHCP testing without Default Switch ICS.** Either run dnsmasq in
  WSL bound to `vEthernet (embclox-test)`, or rely on Azure for DHCP
  coverage. QEMU SLIRP already covers the smoltcp DHCP code path for
  CI, so this is a low priority.

- **Azure Gen2 / accelerated networking (SR-IOV).** Different code
  path entirely (PCI passthrough of a Mellanox/Microsoft NIC).
  Major new scope, not a NetVSC concern.

## References

### Files in this repo

- `crates/embclox-hyperv/src/netvsc.rs` — device, NVSP/RNDIS init, data path
- `crates/embclox-hyperv/src/netvsc_embassy.rs` — `embassy_net_driver::Driver` impl
- `crates/embclox-hyperv/src/{nvsp_msg.rs,nvsp_types.rs,ffi.rs}` — protocol parsers/builders + bindgen FFI
- `crates/embclox-hyperv/include/{nvspprotocol.h,rndis_msvm.h,VmbusPacketFormat.h}` — Microsoft mu_msvm headers (BSD-2-Clause-Patent)
- `crates/embclox-hyperv/include/README.md` — header provenance
- `examples-hyperv/src/main.rs` — Limine boot, IDT, embassy executor
- `examples-hyperv/limine.conf` — cmdline-selected boot menu entries
- `scripts/hyperv-{setup-vswitch,boot-test}.ps1` — local Hyper-V test harness
- `docs/dev/HyperV-Testing.md` — vSwitch setup + ICS pollution writeup
- `docs/design/vmbus.md` — underlying VMBus implementation

### External

- Linux: `drivers/net/hyperv/{netvsc.c, rndis_filter.c, netvsc_drv.c, hyperv_net.h}`
- FreeBSD: `sys/dev/hyperv/netvsc/if_hn.c`
- Microsoft mu_msvm UEFI VM firmware (header source): https://github.com/microsoft/mu_msvm

## Appendix: protocol reference

Kept here as a quick lookup; the actual constants and structs live in
`ffi.rs` (auto-generated via bindgen from the mu_msvm headers).

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

NVSP versions: v1 (WIN2008), v2 (WIN2008R2), v4 (WIN2012), v5 (WIN2012R2+),
v6.1 (WIN10+). Negotiation tries highest first and falls back.

### RNDIS message types

| Code | Name | Direction | Purpose |
|------|------|-----------|---------|
| `0x00000002` | `RNDIS_INITIALIZE_MSG` | G→H | Init session |
| `0x80000002` | `RNDIS_INITIALIZE_CMPLT` | H→G | Init response |
| `0x00000004` | `RNDIS_QUERY_MSG` | G→H | Query OIDs (MAC, MTU) |
| `0x80000004` | `RNDIS_QUERY_CMPLT` | H→G | Query response |
| `0x00000005` | `RNDIS_SET_MSG` | G→H | Set config (packet filter) |
| `0x80000005` | `RNDIS_SET_CMPLT` | H→G | Set response |
| `0x00000001` | `RNDIS_PACKET_MSG` | Both | Data packet (Ethernet frame) |
| `0x00000007` | `RNDIS_INDICATE_STATUS_MSG` | H→G | Link state change (not yet handled) |
| `0x00000008` | `RNDIS_KEEPALIVE_MSG` | H→G | Keepalive (we respond) |
| `0x80000008` | `RNDIS_KEEPALIVE_CMPLT` | G→H | Keepalive response |

### OIDs we issue

| OID | Code | Purpose |
|-----|------|---------|
| `OID_802_3_CURRENT_ADDRESS` | `0x01010102` | MAC address (preferred over PERMANENT) |
| `OID_GEN_MAXIMUM_FRAME_SIZE` | `0x00010106` | MTU |
| `OID_GEN_CURRENT_PACKET_FILTER` | `0x0001010E` | Enable receive (DIRECTED + MULTICAST + BROADCAST) |

### RNDIS packet wire format

Total header is 44 bytes (8-byte rndis_msg_hdr + 36-byte rndis_packet),
followed by the Ethernet frame at `data_offset`.

```c
struct rndis_packet_msg {  // exact layout, all little-endian
    u32 msg_type;          // = 1 (RNDIS_PACKET_MSG)
    u32 msg_len;           // header + data, total bytes
    u32 data_offset;       // = sizeof(rndis_packet) = 36, relative to data_offset field
    u32 data_len;          // Ethernet frame length
    u32 oob_data_offset;   // 0
    u32 oob_data_len;      // 0
    u32 num_oob_data_elements;  // 0
    u32 per_pkt_info_offset;    // = sizeof(rndis_packet) = 36 (Linux conformance)
    u32 per_pkt_info_len;       // 0 (no PPIs in our minimal driver)
    u32 vc_handle;         // 0
    u32 reserved;          // 0
    // followed by Ethernet frame at offset data_offset (relative to data_offset field)
};
```

Important: offsets are measured **from the `data_offset` field**, not
from the start of the message. See FreeBSD's `hn_rndis_pktmsg_offset`
helper for the canonical computation.
