# Hyper-V on Redox: `steffengy/hyperv-redox-drivers`

## Status: background research

A study of a **toy/POC Hyper-V Gen 2 driver bundle** for Redox OS,
maintained by [steffengy](https://github.com/steffengy) as a fork of
the upstream redox-os/drivers tree. The implementation is interesting
because it covers roughly the same scope as embclox's NetVSC + VMBus
work — synthetic keyboard, mouse, NetVSC NIC over VMBus — but does it
as **userspace daemons inside a microkernel** instead of as bare-metal
crates inside our monolithic kernel.

Written to compare design choices and steal good ideas. Not a
proposal.

- Repo: <https://github.com/steffengy/hyperv-redox-drivers>
- Driver tree: <https://github.com/steffengy/hyperv-redox-drivers/tree/master/hyperv>
- Reference commit: `d164798` ("dump implementation here...", 2 years
  old, single-commit fork ahead of upstream)
- License: MIT (matches upstream redox-os/drivers)
- ~10 Rust source files, ~2 kLOC total in [`hyperv/src/`](https://github.com/steffengy/hyperv-redox-drivers/tree/master/hyperv/src)

For embclox's own NetVSC design and the underlying VMBus implementation,
see [../design/hyperv-netvsc.md](../design/hyperv-netvsc.md) and
[../design/vmbus.md](../design/vmbus.md). For Redox's broader
driver/scheme model see
[redox-drivers.md](./redox-drivers.md) and
[redox-io-model.md](./redox-io-model.md).

## What's in the bundle

[`hyperv/src/`](https://github.com/steffengy/hyperv-redox-drivers/tree/master/hyperv/src)
contains a single daemon binary that hosts every VMBus device the
author cared about:

| File | Role |
|------|------|
| `main.rs` | Daemon entry; opens hypercall + IRQ scheme fds, sets `HV_GUEST_OS_ID`, calibrates the synthetic timer (`STIMER0`), enumerates channel offers, spawns per-device futures. |
| `msr.rs` | Thin wrapper that opens MSR scheme fds (`hv_simp`, `hv_siefp`, `hv_sint2`, `hv_scontrol`, `hv_eom`, `hv_eoi`, `hv_hypercall`, `hv_guest_os_id`, …) and provides typed `.get()` / `.set(u64)` accessors. |
| `vmbus.rs` | SynIC setup, channel offer enumeration, `OPENCHANNEL`, GPADL plumbing, the SIEFP scan + tick dispatcher. |
| `ring.rs` | Producer + Consumer halves of the VMBus ring buffer, `send_vmpacket` (inband), `send_vmpacket_gpa_direct`. |
| `gpadl.rs` | `map_gpadl` builder that fragments PFN lists across `GpadlHeader` + repeated `GpadlBody` messages. |
| `notify.rs` | Single-receiver `NotifySender` / `NotifyReceiver` primitive used to deliver IRQs and channel-ready edges to async tasks. |
| `keyboard.rs`, `mouse.rs` | Synthetic HID device drivers (one channel each). |
| `netvsc.rs` | NetVSC: NVSP negotiation, RNDIS init, MAC query, RX/TX. ~400 LOC. |
| `timer.rs` | `STIMER0` (synthetic timer) setup for scheduling. |

Kernel patches are required (`kernel-src.patch`, `kernel-recipe.patch`):
the stock Redox kernel doesn't expose MSR RDWR, a non-PIT timesource,
or VMBus interrupt delivery. The author's README is explicit that this
is a POC, not a production driver.

## Architecture comparison

```
embclox                                  steffengy/hyperv-redox-drivers
──────────                               ──────────────────────────────
bare-metal kernel binary                 userspace daemon (PID N)
                                         ────────────────────────
NetvscEmbassy → NetvscDevice             driver_network::NetworkScheme
       ↓                                       ↓ (file ops via Redox)
Channel (ring.rs)                        RingBuffer<Producer/Consumer>
       ↓                                       ↓
synic::SynIC                             /scheme/hyperv/hypercall
       ↓                                  /scheme/irq/cpu-00/N
hypercall (vmcall MSR via wrmsr)         /scheme/event/ (epoll-ish)
       ↓
direct CR3 + ISR
```

Both implementations talk the same VMBus + NVSP + RNDIS wire
protocols; the difference is **how each side of the
guest/host boundary is reached from Rust**. embclox writes MSRs and
walks page tables directly; the Redox port goes through kernel
schemes that the patched kernel exposes.

## VMBus init — same dance, different plumbing

The Redox driver's `VmBus::init` ([vmbus.rs](https://github.com/steffengy/hyperv-redox-drivers/blob/master/hyperv/src/vmbus.rs))
performs the same steps embclox does in
[`embclox-hyperv/src/{msr,synic,vmbus}.rs`](../../crates/embclox-hyperv/src):

1. Allocate a 4 KiB DMA page each for SIMP, SIEFP, two monitor pages.
2. Program the corresponding MSRs (`hv_simp`, `hv_siefp`,
   `hv_sint2`, `hv_scontrol`) to publish the page addresses and
   enable SynIC.
3. Hypercall `InitiateContact` (with `VMBUS_VERSION_WIN10_V5_2 = 5.2`).
4. Hypercall `RequestOffers`; collect every `OfferChannel` until
   `AllOffersDelivered`.

Differences:

- **Version requested.** Redox driver asks for `WIN10_V5_2` (5.2);
  embclox asks for `WIN10_V5_3` (5.3) — both negotiate down
  gracefully on older hosts.
- **VMBus message SINT.** Both pick SINT2. Both rely on the host
  signalling on the same SINT for SIMP messages *and* for per-channel
  data via SIEFP — exactly the layout documented in our
  [/memories/repo/hyperv-vmbus.md](/memories/repo/hyperv-vmbus.md).
- **MSR access path.** embclox writes MSRs directly with `wrmsr`
  from supervisor mode. Redox driver opens `/scheme/msr/...` file
  descriptors and `.read()` / `.write()` them — every MSR access is
  a userspace → kernel round-trip.
- **Hypercall path.** Redox uses two scheme fds:
  `/scheme/hyperv/hypercall` (general) and
  `/scheme/hyperv/fast_hypercall8` (the 8-byte fast variant used for
  `SignalEvent`). The kernel patch translates writes on those fds
  into actual `vmcall`s.

## GPADL — virtually identical

[`gpadl.rs`](https://github.com/steffengy/hyperv-redox-drivers/blob/master/hyperv/src/gpadl.rs)
mirrors the embclox `channel::create_gpadl` flow byte-for-byte:

- A monotonically-increasing `NEXT_GPADL_HANDLE` (atomic u32) hands
  out fresh handles.
- A `GpadlHeader` message carries as many PFNs as fit in one VMBus
  message payload (HV message payload is 30 × 8 = 240 bytes minus
  header overhead → ~28 pages per header).
- Overflow pages go in repeated `GpadlBody` messages numbered 0, 1,
  …, until the PFN list is exhausted.
- Wait for `GpadlCreated` with matching `(channel_id, handle)` to
  complete the future.

Worth borrowing if we ever want async-first GPADL setup: the Redox
version is a clean `pub async fn map_gpadl(&self, …) -> u32` returning
the handle, with the SIMP message ack delivered through a one-shot
`futures::channel::oneshot`.

## Ring buffer

[`ring.rs`](https://github.com/steffengy/hyperv-redox-drivers/blob/master/hyperv/src/ring.rs)
implements the same `HvRingBuffer` layout (4 KiB control header
followed by N data pages, paired producer + consumer halves) that we
have in
[`embclox-hyperv/src/ring.rs`](../../crates/embclox-hyperv/src/ring.rs).
A few details worth contrasting:

- **No mmap circular trick.** The author flags this in a comment:

  > HINT: We can optimize performance by using circular memory
  > mapped pages — But keep it simple for now. Couldnt get that to
  > work with redox mmaps EPERM?

  Both implementations therefore copy on wraparound. The Redox
  version takes a `Cow<[u8]>` and only does the
  `Cow::Owned(iter().chain(iter())...)` copy when the packet spans
  the wrap point. embclox does the same conceptual thing inside
  `recv_packet` / `recv_packet_raw`.

- **Uncommitted read index.** `RingBuffer<Consumer>` exposes
  `next() -> Option<Cow<[u8]>>` that returns a borrow without
  updating the on-ring `read_index`. The caller commits later via
  `flush()`. This lets the dispatcher process a packet zero-copy
  before releasing the slot to the host. embclox's
  `recv_packet_raw` copies into the caller's buffer first, then
  updates the index immediately — simpler, but pays an extra
  memcpy per packet.

- **Two TX shapes.** Producer offers `send_vmpacket` (inband,
  payload inline in the ring) and `send_vmpacket_gpa_direct`
  (descriptor + GPA range list + inline header in the ring; large
  payload bytes stay where they are in DMA memory and the host reads
  them via the GPAs). The latter is what NetVSC uses for RNDIS
  control + data because the RNDIS message + packet sit in already-
  pinned DMA-allocated `Dma<RndisInitializeRequest>`.

  embclox currently uses inband-only via the **send buffer GPADL**
  (a separate 1 MiB region the host has already mapped). The GPA-
  direct path is the alternative Linux netvsc has but we documented
  as "not implemented because the section path is sufficient".

- **Flow-control feature bit.** Both implementations set
  `feature_bits = 1` (HV_RING_FLOW_CONTROL) on both halves. The
  flush path honours `pending_send_sz`: if the host has indicated it
  wants to be re-signalled when space frees up, we issue the
  `SignalEvent` hypercall after advancing `read_index`.

## SIEFP scan — same algorithm

`VmBus::tick` in [vmbus.rs](https://github.com/steffengy/hyperv-redox-drivers/blob/master/hyperv/src/vmbus.rs)
is the moral equivalent of embclox's `vmbus_isr`:

```rust
let siefp_page = &mut *(self.siefp_page.as_mut_ptr()
    as *mut SynIcEventFlags).wrapping_add(VMBUS_MESSAGE_SINT as usize);
for (i, flag) in (*flags).iter().enumerate() {
    let target_element = &*(flag as *const u64 as *const AtomicU64);
    for bit in 0..64 {
        if target_element.fetch_and(!(1<<bit), Ordering::SeqCst) & (1<<bit) != 0 {
            let notified_channel = 64 * i as u32 + bit as u32;
            // …notify per-channel waiter…
        }
    }
}
self.msr.hv_eoi.set(0).unwrap();
```

Exactly the same idea as our 32-word scan in
[`crates/embclox-hyperv/src/isr.rs::vmbus_isr`](../../crates/embclox-hyperv/src/isr.rs):
walk the per-SINT slot of the SIEFP, atomically clear any set bit, and
notify the per-channel listener.

Differences:

- **Atomic clear vs non-atomic.** Redox uses `fetch_and(!(1<<bit),
  SeqCst)` per bit. embclox uses a non-atomic read-zero-word-then-
  write because we're single-CPU in interrupt context. The Redox
  approach is necessary because their tick runs in a userspace
  thread alongside other tasks — the kernel still delivers the
  interrupt edge but the actual clear happens in userland.
- **One waker per channel.** Redox keeps a `HashMap<u32,
  VecDeque<VmBusChQueueEntry>>` of pending one-shot senders and
  long-lived subscribers per `child_relid`. NetVSC subscribes its
  own relid and drives a single `JoinedFut` that wakes on either
  vmbus events or scheme events. embclox has one global
  `NETVSC_WAKER: AtomicWaker` because we only run one synthetic NIC
  driver at a time; the Redox shape generalises better to multi-
  channel guests.
- **EOI.** Redox writes `hv_eoi` (MSR 0x40000070) after every tick.
  embclox SynIC SINT vectors use auto-EOI per
  [`docs/design/hyperv-netvsc.md`](../design/hyperv-netvsc.md), so
  no MSR write is needed.

## NetVSC — same protocol, shorter code

[`netvsc.rs`](https://github.com/steffengy/hyperv-redox-drivers/blob/master/hyperv/src/netvsc.rs)
is ~400 LOC vs embclox's ~990 LOC. The reductions come from:

- **One NVSP version only.** The Redox driver hardcodes
  `NVSP_PROTOCOL_VERSION_61` (`0x60001`) for both `min` and `max` in
  the `Init` message. If the host doesn't speak v6.1 the
  `assert_eq!(resp.status & 1, 1, "Not successful")` panics. embclox
  walks `NEGOTIATE_ORDER = [V61, V6, V5, V4, V2, V1]` and falls back.

- **One NDIS version path.** The Redox driver sends
  `NvspV2NdisConfigMessage` (MTU + capabilities) and
  `V1SendNdisVersion` (NDIS 6.30) unconditionally. embclox bucket-
  selects NDIS 6.30 vs 6.1 based on NVSP version.

- **No send buffer, GPA-direct only.** `send_buffer_section_offset =
  u32::max_value()` (the sentinel meaning "I'm not using the send
  buffer") is hardcoded for every TX. The RNDIS message + Ethernet
  frame are passed as GPA ranges via `send_vmpacket_gpa_direct`. The
  send buffer GPADL is still allocated and registered, but
  immediately ignored. embclox does the opposite: send buffer
  section index 0 is always used; GPA-direct path is unimplemented.

- **No xfer-page completion-bookkeeping subtleties.** RX is
  `handle_send_packet`: reads
  `xfer.transfer_pageset_id == NETVSC_RECEIVE_BUFFER_ID`, asserts
  `range_count == 1`, dereferences the single range, dispatches
  RNDIS by type. No subchannel request, no SIEFP-related single-
  queue acknowledgement (Redox is on a 2-year-old host that
  predates the post-2026-05 behaviour we wrestled with).

- **Two OIDs queried, no SET.** Only
  `OID_GEN_MAXIMUM_FRAME_SIZE` and `OID_802_3_PERMANENT_ADDRESS`.
  No `OID_GEN_CURRENT_PACKET_FILTER` set after init — RX still
  works, suggesting the older host defaults to
  DIRECTED+MULTICAST+BROADCAST without the explicit set. embclox
  sets the filter explicitly (Linux does too) for safety on newer
  hosts.

- **No keepalive handling, no link-state handling, no teardown.**
  The driver runs forever, only `RndisMessageType::{Init,
  InitComplete, Query, QueryComplete, Packet}` are matched. The
  `match` arms not covered fall through with `x => ()`.

- **VecDeque queues for embassy-net equivalent.** The driver
  exposes a `driver_network::NetworkAdapter` trait impl
  (`read_packet`, `write_packet`, `mac_address`,
  `available_for_read`) backed by `VecDeque<Vec<u8>>` for RX and
  TX. The Redox `smolnetd` daemon polls this scheme over IPC. We
  use embassy-net's poll-driven driver trait directly with a
  zero-copy `RxToken`/`TxToken` pair.

What we *do* and they don't:

- Single-queue NVSPv5+ `SUBCHANNEL` acknowledgement (Linux
  conformance for newer hosts — see
  [/memories/repo/hyperv-vmbus.md](/memories/repo/hyperv-vmbus.md))
- Multi-version NVSP fallback
- Hyper-V-style `HV_LINUX_VENDOR_ID` guest-OS-ID encoding
- Explicit packet-filter `OID_GEN_CURRENT_PACKET_FILTER` SET
- `nvsp_send_ndis_version` per-NVSP-version selection
- Cmdline-driven DHCP vs static address selection
- TCP-echo verified on Azure Gen1, local Hyper-V Gen1, and QEMU

What they *do* and we don't:

- GPA-direct TX path (the more flexible of the two; pays off when
  the data already lives in DMA-pinned memory)
- Per-channel `HashMap` of waiters scoped by `child_relid` (would
  make adding more synthetic devices cleaner than our single
  `NETVSC_WAKER`)
- Async `map_gpadl(&self, …) -> u32` returning the handle from a
  future (we have a synchronous spin loop in
  `channel::create_gpadl`)
- Keyboard + mouse drivers wired up — useful reference if we ever
  want a graphical Hyper-V image (the synthvid path is already
  partly implemented in [`embclox-hyperv/src/synthvid.rs`](../../crates/embclox-hyperv/src/synthvid.rs))

## Author's stated limitations

From [`hyperv/README.md`](https://github.com/steffengy/hyperv-redox-drivers/blob/master/hyperv/README.md):

- Single-CPU only (no per-vCPU SynIC state).
- Ping times 2–200 ms ("no idea if that's redox in general or this
  driver").
- Hard host requirements: VMBus 5.2, NVSP 6.1 exactly.
- Daemon never exits cleanly; teardown unimplemented.
- Lots of `mem::forget` to keep DMA buffers alive past their nominal
  scope.

The "not implemented" list in the README is illustrative of the full
synthetic-device surface a Hyper-V guest *could* implement:
SCSI, IDE, shutdown integration, heartbeat, KVP (key-value pair),
dynamic memory, VSS (backup/restore), synthetic video, synthetic FC,
file copy, NetworkDirect, PCIE passthrough. We have the same gap.

## Lessons we could apply

In rough order of ROI to embclox:

1. **GPA-direct TX path.** Adding `Channel::send_with_gpa_ranges`
   would let NetVSC stop double-copying the RNDIS message into the
   send buffer. The Redox `send_vmpacket_gpa_direct` is ~40 LOC and
   maps cleanly onto our existing
   [`channel::send_with_page_buffer`](../../crates/embclox-hyperv/src/channel.rs)
   (which already exists but is unused by NetVSC). Could go
   alongside the [single in-flight TX][known-issues] known issue.

2. ~~Per-channel waiter table.~~ **Done.** The previous
   `NETVSC_WAKER: AtomicWaker` global has been replaced with a
   `[AtomicWaker; 64]` indexed by `child_relid % 64` in
   [`crates/embclox-hyperv/src/isr.rs`](../../crates/embclox-hyperv/src/isr.rs).
   The [SIEFP-clearing
   ISR](../design/hyperv-netvsc.md#siefp-event-flag-clearing--required-for-host→guest-rx)
   wakes only the relevant device task. A second waker
   ([`isr::SIMP_WAKER`](../../crates/embclox-hyperv/src/isr.rs)) is
   woken on SIMP message arrivals so init-path control futures
   sleep on `hlt` until the host responds instead of polling on
   the 1 ms APIC tick.

3. **Async `create_gpadl`.** Our current `channel::create_gpadl`
   uses a `recv_with_timeout` spin loop driven by `block_on_hlt`
   for the `GpadlCreated` response. Reframing as a future +
   one-shot sender (as Redox does) would compose better with the
   [async boot init](../design/async-boot-init.md) work-in-progress.

4. **`Cow`-returning consumer ring `next()`.** Borrow-on-no-wrap,
   copy-on-wrap. Saves a memcpy per RX packet on the common
   non-wrap path. Lives in
   [`embclox-hyperv/src/ring.rs::recv_packet_raw`](../../crates/embclox-hyperv/src/ring.rs).

5. **Skip the send buffer entirely.** If GPA-direct works for us
   as it does for Redox, we could drop the NVSP send-buffer GPADL
   altogether (a 1 MiB DMA allocation + one GPADL + one
   `SEND_SEND_BUF` round-trip during init). Linux keeps it because
   batched TX through the send buffer is faster per-packet for
   small frames; we don't batch.

What we should **not** copy:

- Hardcoding NVSP 6.1 / asserting status on init: works only against
  one host vintage.
- Skipping `OID_GEN_CURRENT_PACKET_FILTER`: works on the author's
  host but is explicitly required by Linux's reference driver for
  consistent RX behaviour across NDIS versions.
- The "panic on every error" style: pushed past POC quickly, but
  unfit for production-quality kernel code.
- The kernel patches the author needed for Redox (MSR scheme,
  STIMER0 IRQ, non-PIT timesource) are equivalent to functionality
  we already have in
  [`embclox-hal-x86`](../../crates/embclox-hal-x86); no porting
  work needed.

## References

- Repo: <https://github.com/steffengy/hyperv-redox-drivers>
- Hyper-V crate: <https://github.com/steffengy/hyperv-redox-drivers/tree/master/hyperv>
- Upstream Redox drivers tree: <https://gitlab.redox-os.org/redox-os/drivers>
- embclox NetVSC design: [../design/hyperv-netvsc.md](../design/hyperv-netvsc.md)
- embclox VMBus design: [../design/vmbus.md](../design/vmbus.md)
- Redox driver model: [./redox-drivers.md](./redox-drivers.md)
- Redox I/O model: [./redox-io-model.md](./redox-io-model.md)

[known-issues]: ../design/hyperv-netvsc.md#known-issues
