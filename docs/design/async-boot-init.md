# Async boot-time init with `block_on_hlt`

## Status: design

The Hyper-V example's boot-phase initialization (VMBus channel
negotiation, NetVSC NVSP/RNDIS handshake, OID queries, and Synthvid
init) currently uses synchronous spin loops to wait for host
responses. This document describes how to replace those spins with
proper interrupt-driven `hlt` sleep using a tiny custom `block_on`
helper instead of standing up the full embassy executor for boot.

The runtime data path is unaffected; this design only covers the
one-shot init code that runs before `run_executor` is entered.

## Background

### Current behaviour

Every host→guest control-plane response involves:

1. Guest sends a request (hypercall `post_message` or ring-buffer
   write + `signal_event`).
2. Host computes the response and writes it to the SIMP slot
   (control plane) or appends to the channel ring buffer (data
   plane).
3. **Host triggers SINT2** — a synthetic interrupt mapped to IDT
   vector 34.
4. Guest spins polling the SIMP / ring buffer until the message is
   visible.

The host signals via interrupt (step 3) but the guest currently
ignores the IRQ during boot because:

- `sti` is deferred to `run_executor`, so SINT2 is masked.
- Even if it weren't, the ISR (`vmbus_isr`) only calls
  `NETVSC_WAKER.wake()` — there is no equivalent waker for the
  control plane.

So the spin loops are reading memory the host has already written.
The cost isn't waiting for the host — it's burning CPU until the
loop body comes back to re-check.

### Sites involved

All in `crates/embclox-hyperv/src/`:

| File / function | Phase | What it polls |
|-----------------|-------|---------------|
| `vmbus.rs::version_request` | boot | SIMP slot 2 (VERSION_RESPONSE) |
| `vmbus.rs::request_offers` | boot | SIMP slot 2 (OFFERCHANNEL × N + ALLOFFERS_DELIVERED) |
| `channel.rs::create_gpadl_msg` | boot | SIMP slot 2 (GPADL_CREATED) |
| `channel.rs::open_channel_msg` | boot | SIMP slot 2 (OPENCHANNEL_RESULT) |
| `channel.rs::recv_with_timeout` | boot | channel ring buffer |
| `netvsc.rs::recv_rndis_response` | boot | channel ring buffer (via `poll_channel`) |
| `netvsc.rs` NVSP/RNDIS init waits | boot | channel ring buffer |
| `synthvid.rs` init waits | boot | synthvid channel ring buffer |
| `netvsc.rs::transmit` (sync wrapper) | runtime back-compat | TX section free flag |

The `transmit` back-compat wrapper at `netvsc.rs:894` is the only
non-init spin site; it is kept for the `send_gratuitous_arp` path
and could be retired by moving that caller onto the embassy
`transmit_with` API.

`hypercall.rs::HvSignalEvent` waits on a CPU-instruction completion
(`AX` register), not a host event — it is *not* in scope.

## Design

### `block_on_hlt`: a one-future runner with idle sleep

A 12-line helper, lives in `embclox-hal-x86::runtime`:

```rust
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

static VTABLE: RawWakerVTable =
    RawWakerVTable::new(|_| RawWaker::new(core::ptr::null(), &VTABLE),
                        |_| {}, |_| {}, |_| {});

/// Run a future to completion, halting the CPU between polls.
///
/// Unlike `embassy_futures::block_on`, this issues `sti; hlt` between
/// polls so the CPU sleeps until any interrupt (SynIC SINT, APIC
/// timer, NIC IRQ) wakes it. Suitable for synchronous boot phases
/// where you want a real `Future` API but cannot run the full
/// embassy executor yet.
///
/// The supplied no-op waker is intentional: we rely on the next
/// interrupt to wake the CPU, not the future's waker plumbing.
/// If a future needs precise wake semantics, switch to a real
/// executor (e.g. embassy-executor) instead.
pub fn block_on_hlt<F: Future>(mut fut: F) -> F::Output {
    let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
    let raw = RawWaker::new(core::ptr::null(), &VTABLE);
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(r) = fut.as_mut().poll(&mut cx) {
            return r;
        }
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}
```

Key properties:

- **Single future, no scheduler.** No task arena, no `StaticCell`,
  no spawn tokens. Just polls one future to completion.
- **No-op waker.** The future doesn't need to register anything
  meaningful. We rely on the IRQ (SINT2 / APIC timer / device) to
  return from `hlt`; the next iteration re-polls.
- **CPU sleeps when idle.** Same effect as the runtime executor
  loop, just without task scheduling.

### Async init: surfaces become `async fn`

Each spin site is replaced with an `async fn` that polls the same
memory but returns `Pending` instead of spinning. Skeleton:

```rust
// Before:
for _ in 0..5_000_000u64 {
    if let Some(payload) = synic.poll_message() {
        if matches!(payload, ...) { return Ok(...); }
        synic.ack_message();
    }
    for _ in 0..1000 { core::hint::spin_loop(); }
}
Err(HvError::Timeout)

// After:
WaitForSynicMessage::new(synic, |payload| matches!(payload, ...))
    .with_timeout_us(5_000_000)
    .await?
```

`WaitForSynicMessage` is a `Future` whose `poll` calls
`synic.poll_message()`. If the matching message is present it
returns `Ready`; otherwise it returns `Pending` (and the no-op
waker means we rely on `hlt` + the next IRQ). Timeout is enforced
against `embassy_time::Instant::now()` (which works pre-executor
because our `ApicTimeDriver` is just a `_rdtsc()` reader).

### Boot path

`examples-hyperv/src/main.rs::kmain` becomes:

```rust
embclox_hal_x86::idt::init();
embclox_hal_x86::pic::disable();

// Map LAPIC + start APIC timer FIRST so block_on_hlt has a timer
// IRQ to break out of hlt on (otherwise a future returning Pending
// with no pending IRQ would deadlock).
let lapic_vaddr = memory.map_mmio(LAPIC_PHYS_BASE, 0x1000).vaddr();
let mut lapic = LocalApic::new(lapic_vaddr);
lapic.enable();
runtime::start_apic_timer(lapic, tsc_per_us, 1_000);

// Register the SINT2 ISR (already done in current code, just earlier).
unsafe { idt::set_handler(VMBUS_VECTOR, vmbus_isr); }

// All init now runs as one async fn under block_on_hlt:
let netvsc = runtime::block_on_hlt(async {
    let mut vmbus = embclox_hyperv::init_async(&dma, &mut memory).await?;
    NetvscDevice::init_async(&mut vmbus, &dma, &memory).await
})?;

run_embassy(netvsc, memory);  // unchanged
```

The APIC timer must start *before* `block_on_hlt` so:

1. The `WithTimeout` wrapper has a working time source
   (`embassy_time::Instant::now()` already works — TSC-based — but
   timeout *firing* relies on `on_timer_tick()` advancing the alarm
   table, which runs from the APIC timer ISR).
2. There's always at least one IRQ that can break us out of `hlt`,
   even if SINT2 misses (defensive).

### Concurrency

Single-core x86. The relevant races:

| Race | Resolution |
|------|------------|
| ISR fires after our last poll but before `hlt` | `enable_and_hlt` is the atomic `sti; hlt` instruction sequence — IRQ is delivered exactly between the two so it can't be lost |
| Multiple SIMP messages queued | Existing `synic.ack_message()` already drains via `wrmsr(EOM)` |
| Timeout fires while a real response also lands | Future returns whichever it polls first; both are observed as success/timeout based on order, no corruption |
| `block_on_hlt` interrupts disabled at entry | `enable_and_hlt` always re-enables, so even if the caller had cli'd, we proceed safely |

### What stays the same

- The runtime data path (`run_embassy` → `run_executor` →
  embassy-net) is unchanged.
- `NETVSC_WAKER` and `vmbus_isr` are unchanged.
- All the SynIC + hypercall + ring-buffer plumbing is unchanged —
  only the *waiting* code (the polls) is restructured.
- The `transmit` back-compat wrapper at `netvsc.rs:894` can be
  retired separately by porting the gratuitous-ARP caller to the
  embassy `transmit_with` API. Out of scope here.

## Rollout plan

Phased so each step is independently verifiable on the existing
ctest + Hyper-V test suite.

### Phase 1: `block_on_hlt` helper

- Add `block_on_hlt` to `embclox-hal-x86::runtime`.
- Unit test: `block_on_hlt(async { 42 })` returns 42 (host-side
  test, doesn't need an APIC timer because the future is `Ready`
  on first poll).
- No example changes yet.

### Phase 2: One pilot site — `version_request`

- Add `WaitForSynicMessage` future + `WithTimeout` wrapper in
  `embclox-hyperv`.
- Convert `vmbus.rs::version_request` to `async fn`.
- In `examples-hyperv`, wrap the existing `embclox_hyperv::init`
  call site so it ends up calling
  `block_on_hlt(version_request_async(...))` internally.
- Verify: `hyperv-boot` ctest passes; `scripts/hyperv-boot-test.ps1`
  TCP echo still works.

### Phase 3: Convert remaining VMBus init

- `request_offers`, `create_gpadl_msg`, `open_channel_msg` → async.
- `embclox_hyperv::init` becomes `init_async`; old sync wrapper kept
  as a thin `block_on_hlt(init_async(...))` for callers that haven't
  migrated.

### Phase 4: Convert NetVSC init

- `recv_with_timeout` → async on the channel ring buffer.
- `recv_rndis_response`, NVSP init waits, RNDIS init waits → async.
- `NetvscDevice::init` → `init_async`.

### Phase 5: Convert Synthvid init

- Same pattern. Lower priority since synthvid only matters for
  graphical output (not currently exercised).

### Phase 6: Cleanup

- Remove the synchronous `init` wrappers if no callers remain.
- Retire `netvsc.rs::transmit` if `send_gratuitous_arp` is moved to
  the embassy path.

## Verification

| Phase | Required green |
|-------|----------------|
| 1 | `cargo test -p embclox-hal-x86` |
| 2-5 | `ctest --test-dir build` (unit, integration, tulip-{boot,echo}, hyperv-boot) all pass |
| 2-5 | `scripts/hyperv-boot-test.ps1` reports TCP echo VERIFIED on real Hyper-V Gen1 |
| Final | Optional: Azure redeploy + `nc <public-ip> 1234` echo test |

CPU savings are not directly measured by ctest. Manual check via
host-side `top` on the QEMU process or Azure portal CPU graph would
show idle CPU drop from 100% to ~0% during init (measured in
milliseconds, so total saving is small but the behaviour is correct).

## Why not embassy-executor for boot?

Considered and rejected:

- **Circular dependency.** The full embassy executor wants to spawn
  tasks, which means the `Driver` impl needs the initialized device,
  which means init needs to complete first. Workable with a separate
  "init task" + signal but adds significant scaffolding.
- **`StaticCell` per task** for state allocation — boilerplate.
- **Spawn tokens** + `embassy_executor::task` macros — codegen for
  features we don't use here (multiple concurrent tasks with
  scheduling).

`block_on_hlt` is ~12 lines, no allocation, no codegen, no executor
state machine. It hits the exact problem we have: "run one async
init function to completion while letting the CPU sleep between
events." Once init is done we hand off to the real embassy executor.

## Why not just `enable_and_hlt` in the existing spin sites?

Considered as "option A" before settling on this design. Pros: no
async restructure, even smaller change. Cons:

- Each spin site still has its own bespoke timeout-by-iteration-count
  logic, which is hard to read and easy to get wrong (we already saw
  the time-driver double-static bug in the recent past).
- Spin loops in `vmbus.rs` and `netvsc.rs` interleave protocol
  parsing with control flow; converting them to `async fn` makes the
  protocol logic easier to follow as straight-line code.
- `embassy_time::Instant`-based timeouts are wall-clock accurate;
  iteration counts depend on CPU speed.
- Async lets us compose patterns like "wait for X with a Y ms
  timeout" via `select(wait_x(), Timer::after_millis(y))`.

The async restructure is more code change but yields code that's
*shorter and clearer* than the spin loops it replaces, not just
better-behaved.

## References

- `crates/embclox-hyperv/src/synic.rs` — SIMP/SIEFP/SINT mechanism
- `crates/embclox-hyperv/src/vmbus.rs` — control-plane spin sites
- `crates/embclox-hyperv/src/channel.rs` — control + data spin sites
- `crates/embclox-hyperv/src/netvsc.rs` — RNDIS init spin sites
- `crates/embclox-hal-x86/src/runtime.rs` — where `block_on_hlt`
  lives once implemented
- `embassy_futures::block_on` — reference implementation we adapt
  (https://docs.rs/embassy-futures/0.1.2/embassy_futures/fn.block_on.html)
- Hyper-V TLFS §10 (Synthetic Interrupt Controller) — SIMP/SIEFP/SINT
  semantics
- `docs/design/hyperv-netvsc.md` — overall NetVSC architecture
- `docs/design/vmbus.md` — VMBus channel protocol
