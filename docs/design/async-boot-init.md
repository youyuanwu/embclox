# Async boot-time init with `block_on_hlt`

## Status: shipped

VMBus / NetVSC / Synthvid initialization on Hyper-V used to busy-spin
waiting for host responses (10+ separate spin loops across
`embclox-hyperv`). All such waits are now interrupt-driven: a tiny
custom one-future runner halts the CPU between polls and the SINT2
ISR wakes it when the host writes a response.

The runtime data path (after `run_executor`) is unchanged — this is
purely about boot-phase init.

## How it works

Three pieces:

| Piece | Where | Role |
|-------|-------|------|
| `block_on_with(fut, park)` | `embclox-async` | Pure-`core` 12-line one-future runner. Polls the future; on `Pending`, calls `park()`. Repeat until `Ready`. |
| `block_on_hlt(fut)` | `embclox-hal-x86::runtime` | Thin wrapper that supplies `sti; hlt` as the park function. |
| `wait_for_match(synic, deadline, matcher)` + `WaitForPacket` | `embclox-hyperv::{synic,channel}` | Futures that poll the SIMP slot or a channel ring buffer; `Pending` until the matcher succeeds or `embassy_time::Instant` deadline passes. |

The CPU sleeps in `hlt` until **any** interrupt fires — the SINT2 ISR
(host wrote a VMBus message), the APIC periodic timer, or any device
IRQ. Then `block_on_hlt` re-polls the future, which re-checks the
SIMP / ring. No waker plumbing is needed because we re-poll on every
IRQ unconditionally.

### Why a no-op waker

`block_on_with` installs a no-op `Waker`. This is intentional:

- We have no executor to dispatch a "wake" callback, so a real waker
  would do nothing useful.
- We re-poll on every IRQ wakeup anyway, so the contract is "the
  caller arranges for some IRQ to fire when the future might become
  ready." Currently that's the SINT2 ISR (for host messages) plus
  the 1 ms APIC periodic timer (defensive — bounds the worst-case
  re-poll latency to 1 ms even if the relevant IRQ misses).

### Caller contract

Anyone using `block_on_hlt` must ensure:

1. **APIC periodic timer is running** (`runtime::start_apic_timer`)
   so deadlines fire even if no host event arrives.
2. **The relevant device ISR is installed** *before* the first
   request that might generate a response. For VMBus: install
   `vmbus_isr` at IDT vector 34 before the first `post_message` call.
3. **The future never blocks itself** — it must return `Pending` when
   not ready and let `block_on_hlt` perform the halt. Don't call
   `hlt` directly from inside the future.

`examples-kernel/src/main.rs::kmain` shows the canonical setup order:
TSC calibration → LAPIC enable → `start_apic_timer` → install
`vmbus_isr` → call `embclox_hyperv::init` (which uses `block_on_hlt`
internally).

## Sites converted

All in `crates/embclox-hyperv/src/`:

| Site | Implementation |
|------|----------------|
| `vmbus.rs::try_version` | `try_version_async` + `block_on_hlt` |
| `vmbus.rs::request_offers` | `request_offers_async` + `block_on_hlt`; matcher mutates a `Vec` |
| `channel.rs::create_gpadl_msg` | `wait_for_match` + `block_on_hlt` |
| `channel.rs::open_channel_msg` | `wait_for_match` + `block_on_hlt` |
| `channel.rs::recv_with_timeout` | `wait_for_packet_async` + `block_on_hlt`; signature changed `spin_iters: u64` → `timeout: embassy_time::Duration` |
| `netvsc.rs::recv_rndis_response` | `recv_rndis_response_async` + `block_on_hlt` (uses `embassy_futures::yield_now()` between `poll_channel` calls) |
| `netvsc.rs::send_rndis_control` (section-free wait) | `wait_for_send_section_async` + `block_on_hlt` |
| `synthvid.rs::drain_recv` | short-deadline poll + `yield_now` under `block_on_hlt` |

### Sites intentionally NOT converted

| Site | Reason |
|------|--------|
| `hypercall.rs::HvPostMessage` retry-on-InsufficientBuffers | Microsecond-scale CPU-instruction wait, not a host event. `hlt` (~µs latency + 1 ms APIC tick rounding) would be slower than spinning. |

## Adding a new async wait

The common pattern, when the host responds to a request you posted:

```rust
// 1. Post the request (sync hypercall).
hcall.post_message(connection_id, msg_type, &msg_bytes)?;

// 2. Wait for the response with a deadline.
let deadline = embassy_time::Instant::now() + Duration::from_secs(5);
let result = embclox_hal_x86::runtime::block_on_hlt(
    crate::synic::wait_for_match(synic, deadline, |payload| {
        // Return Some(value) when matched, None to discard and keep waiting.
        if payload.len() < N { return None; }
        let msgtype = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        if msgtype != EXPECTED_TYPE { return None; }
        Some(parse_response(payload))
    }),
)?;
```

For channel ring buffer waits, use `Channel::recv_with_timeout` (or
its async core `wait_for_packet_async` if you need to chain it with
other futures).

### `wait_for_match` discard semantics

Every message visible in the SIMP slot is passed to `matcher`
**exactly once** then **acked unconditionally** — including when
`matcher` returns `None`. Acking clears the slot and (if
`MessagePending` is set) signals `EOM` so the host delivers the next
message; the discarded message is gone forever.

In practice this is safe because VMBus init is request/response
sequenced — the host shouldn't send unrelated messages mid-step,
and the SIMP only has one slot per SINT. But a `matcher` that
expects to see *several* message types in one wait (the
`request_offers` pattern: OFFERCHANNEL × N + ALLOFFERS_DELIVERED)
must recognise every type it cares about. Returning `None` for a
message you actually wanted is a silent data loss bug.

Discarded messages are logged at `trace!` so unexpected host traffic
can be diagnosed without flooding success-path logs.

#### Comparison to Linux

Linux's `drivers/hv/{connection,channel_mgmt}.c` uses a different
model:

- The ISR (well, its deferred procedure `vmbus_on_msg_dpc`) drains
  every SIMP message unconditionally and dispatches via a
  per-msgtype table (`channel_message_table[CHANNELMSG_COUNT]`).
- Request/response messages (`VERSION_RESPONSE`, `GPADL_CREATED`,
  `OPENCHANNEL_RESULT`) walk a list of pending requests
  (`vmbus_connection.chn_msg_list`) keyed by `(msgtype, child_relid,
  gpadl_id, openid)`, then `complete()` the matching `waitevent`.
  Waiters do `wait_for_completion(&msginfo->waitevent)` and sleep
  until the ISR signals them.
- If no waiter matches, the response is **silently dropped** (no
  log, no warning) — the for_each_entry loop just doesn't find a
  hit.
- Unsolicited messages (`OFFERCHANNEL`, `RESCIND_CHANNELOFFER`,
  `ALLOFFERS_DELIVERED`) have dedicated handlers that always run
  regardless of waiter state.

We achieve the same effect with much less code because we exploit
two simplifications Linux can't make:

1. **Single in-flight wait.** Linux supports many concurrent
   requests across many channels; their list-walk and ID-keyed
   matching is essential. We sequence init synchronously so there's
   only ever one `wait_for_match` parked at a time. The matcher
   closure replaces both the dispatch table and the list walk.
2. **No background dispatch needed during init.** Linux's ISR runs
   constantly, even when no one is waiting, because user-space
   processes can connect to running channels at any time. Our boot
   path runs to completion before any user code exists.

Trade-off: we'd need to add a separate background dispatch loop if
we ever wanted to handle unsolicited host events that arrive
*outside* any pending wait — e.g. channel rescind during init. Our
current matchers would silently discard those. Today none of the
init steps care about such events; after init, the embassy executor
+ NETVSC_WAKER cover all asynchronous events on the data plane.

## Why not full embassy-executor for boot?

- **Circular dependency.** The full executor wants to spawn tasks,
  which need a `Driver` impl, which needs an initialized device,
  which means init must complete first. Workable with an "init task"
  + signal but adds significant scaffolding for a one-shot operation.
- **Per-task `StaticCell` boilerplate** for state allocation.
- **Spawn tokens + `embassy_executor::task` macros** generate code
  for features (multi-task scheduling) we don't use here.

`block_on_hlt` is 12 lines, no allocation, no codegen, no executor
state machine. It hits the exact problem we have: "run one async fn
to completion while letting the CPU sleep between events." Once init
is done we hand off to the real embassy executor as before.

## Why not just `enable_and_hlt` in the existing spin sites?

Considered. Pros: no async restructure. Cons:

- Each spin site keeps its bespoke timeout-by-iteration-count logic
  (hard to read; iteration counts depend on CPU speed).
- Protocol parsing in `vmbus.rs`/`netvsc.rs` stays interleaved with
  control flow; the async version reads as straight-line code.
- `embassy_time::Instant`-based timeouts are wall-clock accurate.
- Async lets us compose `select(wait_x(), Timer::after(y))` if we
  want richer combinators in the future.

The async restructure is more code change but yields code that's
*shorter and clearer* than the spin loops it replaces.

## Concurrency notes

Single-core x86. Relevant races:

| Race | Resolution |
|------|------------|
| ISR fires after our last poll but before `hlt` | `enable_and_hlt` is the atomic `sti; hlt` instruction sequence — IRQ is delivered exactly between, can't be lost. |
| Multiple SIMP messages queued | `synic.ack_message()` already drains via `wrmsr(EOM)`. |
| Timeout fires while a real response also lands | Future returns whichever its poll observes first; both observed correctly. |

## Testing

| What | How |
|------|-----|
| `block_on_with` semantics (Ready, Pending, park-count) | `cargo test -p embclox-async` (4 host-side unit tests) |
| End-to-end VMBus + NetVSC init under `block_on_hlt` | `ctest --test-dir build` (5/5 must pass; `hyperv-boot` exercises Limine + VMBus stub on QEMU) |
| Real Hyper-V end-to-end | `scripts/hyperv-boot-test.ps1` reports TCP echo @ 1234 VERIFIED |
| No unexpected discarded messages | Inspect serial log for `wait_for_match: discarding` traces |

## References

- `crates/embclox-async/src/lib.rs` — `block_on_with` + tests
- `crates/embclox-hal-x86/src/runtime.rs` — `block_on_hlt` wrapper
- `crates/embclox-hyperv/src/synic.rs` — SIMP mechanism + `wait_for_match`
- `crates/embclox-hyperv/src/channel.rs` — `WaitForPacket` future
- `crates/embclox-hyperv/src/{vmbus,netvsc,synthvid}.rs` — call sites
- [`embassy_futures::block_on`](https://docs.rs/embassy-futures/0.1.2/embassy_futures/fn.block_on.html) — reference impl we adapted
- Hyper-V TLFS §10 — SynIC SIMP/SIEFP/SINT semantics
- [`docs/design/hyperv-netvsc.md`](./hyperv-netvsc.md) — NetVSC architecture
- [`docs/design/vmbus.md`](./vmbus.md) — VMBus channel protocol
