# SMP via per-CPU executors (Option A)

## Status: shipped

APs now boot via the Limine MP request, bring up their own ISR +
APIC timer, and each runs its own embassy `Executor` alongside the
BSP. Tasks are pinned to the CPU that spawned them; there is no
migration. IRQs are routed to a specific CPU at probe time and that
CPU's executor sees the resulting wake.

This is the "Option A" path from the SMP architecture discussion:
the lightest-weight way to use additional cores while keeping the
existing driver shape intact.

Shipped across 8 commits on `dev` plus this design doc; runtime is
opt-in via the `smp=on` cmdline token (see
[examples-kernel/limine-smp.conf](../../examples-kernel/limine-smp.conf)).
The `kernel-echo-e1000-smp` ctest lane exercises the path
end-to-end under `qemu -smp 4`: each AP runs `ap_heartbeat_task`
on its own executor and the BSP asserts every AP's counter is
non-zero before serving the echo socket.

## Goals and non-goals

### Goals

- Boot APs via the Limine MP request; each AP enters Rust running
  per-CPU ISR + APIC timer setup, then drives its own embassy
  `Executor`. (Instantiating the executor itself is a kernel-side
  concern; the HAL provides the bring-up primitives and the
  canonical `run_executor` poll/hlt loop.)
- Per-CPU LAPIC handle is **not** needed (single global stash
  works for cross-CPU EOI; see [Per-CPU statics](#per-cpu-statics)).
  Per-CPU APIC timer drives `embassy-time` via per-CPU alarm slots.
- IRQ routing: each device IRQ is delivered to exactly one CPU
  (chosen at driver init), and that CPU's executor sees the
  resulting wake.
- Driver-facing APIs unchanged: `AtomicWaker`-based wakes keep
  working because the wake fires on the same CPU as the IRQ and
  the executor that owns the registered waker.
- Existing single-CPU kernel build keeps working unchanged. SMP is
  opt-in via the `smp=on` cmdline token; default behaviour leaves
  APs in Limine's spin loop.

### Non-goals

- **Task migration / work stealing.** Tasks never move between
  CPUs. If a workload needs cross-CPU dispatch, the future is
  responsible for sending a message (Option C, future work).
- **`Send` audit of the driver crates.** Per-CPU executors don't
  require `Send` futures; we keep using `Rc`-shaped state freely.
- **Preemption.** Each CPU stays cooperative within its own
  executor. APIC timer still fires per CPU to drive `embassy-time`.
- **`Sync` device state across CPUs.** A PCI device is owned by one
  CPU (the one its IRQ is routed to) and accessed only from that
  CPU's executor. Multi-queue NICs are a later step (Option C).
- **Multi-CPU probe.** Probe stays serialised on the BSP, matching
  the existing
  [driver-model.md](./driver-model.md#smp-forward-design-choices)
  decision.

## Rejected alternatives (recap)

The architecture discussion considered three shapes; only Option A
is in scope here. The other two are documented in
[../background/moss-kernel.md](../background/moss-kernel.md) and
recapped briefly:

| Option | Shape | Why not now |
|--------|-------|-------------|
| A (this doc) | N executors, tasks pinned to CPUs | Smallest delta from today; no `Send` audit; matches embassy's recommended SMP shape. |
| B | One global runqueue, tasks migrate on wake (Moss-style) | Requires `Send` everywhere across `.await`, IPI infrastructure, and per-CPU runqueue management. Big lift; payoff only when load imbalance matters. |
| C | A + opt-in cross-CPU mailbox | Natural follow-on once a workload demonstrates need. |

Option A leaves the door open for Option C: a `CpuMailbox<T>` can
be added later without disturbing the per-CPU executor model.

## Architecture

```
┌────────── shared, immutable after boot ──────────┐
│ IDT (one shared image, no per-CPU TSS today)     │
│ DriverRegistry / PCI scan results                │
│ Memory map / heap / HHDM                         │
│ embassy-time driver (single global, per-CPU slot)│
└──────────────────────────────────────────────────┘

per-CPU (read via GS_BASE):
┌─ CPU 0 (BSP) ──────┐  ┌─ CPU 1 ────────────┐  ┌─ CPU N ────────────┐
│ LocalApic          │  │ LocalApic          │  │ LocalApic          │
│ APIC timer ISR     │  │ APIC timer ISR     │  │ APIC timer ISR     │
│ embassy Executor   │  │ embassy Executor   │  │ embassy Executor   │
│ time alarm slots   │  │ time alarm slots   │  │ time alarm slots   │
│ run_executor loop  │  │ run_executor loop  │  │ run_executor loop  │
│ device IRQs:       │  │ device IRQs:       │  │ device IRQs:       │
│   - APIC timer 32  │  │   - APIC timer 32  │  │   - APIC timer 32  │
│   - e1000 vec 33   │  │   - VMBus SINT2 34 │  │   - (none today)   │
│   - spurious 39    │  │   - spurious 39    │  │   - spurious 39    │
└────────────────────┘  └────────────────────┘  └────────────────────┘
```

Wakes stay local: a device IRQ on CPU *k* runs the ISR on CPU *k*,
which calls `AtomicWaker::wake()` on a waker that was registered
by a future running inside CPU *k*'s executor. The executor's
`hlt`-loop returns from the halt on the IRQ, polls its task set,
and the future makes progress.

There is no cross-CPU wake path because every (driver, executor)
pair lives on the same CPU.

## Bringing up APs

Limine has a built-in MP (multi-processor) request that boots APs
into a callback we provide. The handshake:

1. BSP declares `MpRequest` alongside the other Limine requests.
   The macro in
   [crates/embclox-hal-x86/src/limine_boot.rs](../../crates/embclox-hal-x86/src/limine_boot.rs)
   places the request in `.requests` and exposes a `pub fn
   mp_response()` accessor on the generated module.
2. After `kmain` finishes single-threaded boot (heap, IDT, PCI
   scan, driver probe), it walks the MP response's `cpus()` slice.
   Each entry has the ACPI `id`, the hardware `lapic_id`, a
   `goto_address` field the bootloader is spinning on, and an
   `extra` atomic the kernel can use as a per-AP scratch word.
3. For each AP, BSP assigns a sequential `processor_id` (1..N,
   skipping the BSP), writes that to `cpu.extra` (release-ordered),
   then writes `cpu.goto_address = ap_entry`. Limine guarantees the
   `goto_address` store is release-ordered relative to the AP's
   first instruction fetch, so the AP sees the `extra` value.
4. Limine gives each AP its own stack via the bootloader; we use
   that as-is and never allocate a separate AP stack.
5. Each AP enters the kernel-supplied `ap_entry` thunk, which calls
   `smp::ap_init_from(cpu)` to rebuild its `ApInit` from the per-AP
   `extra` plus the shared TSC + LAPIC vaddr stashed by
   `smp::set_ap_init_params`, then calls `smp::ap_setup(init)` to
   finish per-CPU boot (slot + GS_BASE + IDT + LAPIC + APIC timer).

The public API lives in
[crates/embclox-hal-x86/src/smp.rs](../../crates/embclox-hal-x86/src/smp.rs):

- `ApInit { cpu_id, apic_id, tsc_per_us, lapic_vaddr }` — passed
  to the kernel's AP entry function.
- `set_ap_init_params(tsc_per_us, lapic_vaddr)` — BSP stashes the
  shared values into two atomics before bring-up.
- `bring_up_aps(mp, max_aps, thunk)` — walks `cpus()`, assigns
  processor_ids, writes `goto_address`.
- `ap_init_from(&Cpu) -> ApInit` — AP thunk reconstructs its init
  state from the Limine `Cpu` entry.
- `ap_setup(init)` — AP-side boot helper (`init_ap` +
  `idt::load_current_cpu` + LAPIC enable + periodic timer
  programming).
- `check_tsc_sync(bsp_tsc, tsc_per_us) -> i64` — optional AP
  self-check that `|tsc_ap - tsc_bsp| < 1 ms`.

`ap_entry` lives in the kernel binary (see
[examples-kernel/src/main.rs](../../examples-kernel/src/main.rs)):
a small thunk that calls `ap_init_from` + `ap_setup`, initialises
this AP's slot in the `AP_EXECUTORS: [StaticCell<Executor>;
MAX_CPUS - 1]` table, spawns `ap_heartbeat_task(processor_id)`,
and hands off to `runtime::run_executor`. Each AP then sits in the
same canonical poll/hlt loop the BSP uses.

## Per-CPU statics

A fixed-size table indexed by sequential `processor_id`, defined
in [crates/embclox-hal-x86/src/cpu_local.rs](../../crates/embclox-hal-x86/src/cpu_local.rs).
The currently-executing CPU's `processor_id` is held in `GS_BASE`
(kernel-mode `IA32_GS_BASE`), written once during `init_bsp` /
`init_ap` and read back as a plain `mov gs:0`.

- `MAX_CPUS = 8` — fixed, covers QEMU smoke tests, Hyper-V Gen1 /
  Azure standard SKUs, with headroom.
- `CpuLocal { cpu_id, apic_id }` — populated once per CPU. The
  field set is intentionally small: only the data the ISRs and the
  per-CPU IRQ routing actually need.
- `static CPU_LOCALS: [spin::Once<CpuLocal>; MAX_CPUS]` — first
  call wins; later `init_*` calls are no-ops.
- `init_bsp(apic_id)` / `init_ap(processor_id, apic_id)` populate
  the slot and write `GS_BASE`.
- `current_cpu_id()` / `current()` / `by_id(cpu)` read the slot.

The `LocalApic` handle is **not** stored in `CpuLocal`. The single
global `static mut LAPIC` in
[crates/embclox-hal-x86/src/runtime.rs](../../crates/embclox-hal-x86/src/runtime.rs)
is used only for `lapic_eoi()`, and LAPIC MMIO at `0xFEE000B0` is
per-CPU-physical: an EOI write to that VA from any CPU EOIs *that*
CPU's local APIC. No per-CPU LAPIC handle is needed.

## CpuId

Defined in
[crates/embclox-hal-x86/src/vector_alloc.rs](../../crates/embclox-hal-x86/src/vector_alloc.rs):
the `Bsp` variant is implicit `processor_id == 0`; the `Ap(u8)`
variant carries the sequential `processor_id` (1..MAX_CPUS), not
the LAPIC ID.

The LAPIC ID is hardware-assigned and possibly sparse, so it lives
in `CpuLocal::apic_id` instead of being embedded in `CpuId`. The
`CpuId::apic_id()` accessor resolves through `cpu_local::by_id` and
is what `IoApic::enable_irq` feeds the IOAPIC redirection entry.
Drivers treat `CpuId` as an opaque token and never see the LAPIC
ID directly.

## IRQ routing

Drivers route their PCI IRQ to a specific CPU at probe time via
the new `ProbeCtx::install_pci_isr_on(line, handler, cpu_id)`
entry point in
[crates/embclox-driver/src/driver.rs](../../crates/embclox-driver/src/driver.rs).
The existing `install_pci_isr(line, handler)` delegates to
`install_pci_isr_on(..., CpuId::Bsp)` so BSP-only drivers compile
unchanged.

- The IDT vector pool is a single global `VectorAllocator` (vectors
  33..47, 15 total), because the IDT is one shared structure across
  all CPUs. CPU placement is purely an IOAPIC routing decision and
  does not gate vector allocation. See
  [crates/embclox-hal-x86/src/vector_alloc.rs](../../crates/embclox-hal-x86/src/vector_alloc.rs).
- `VectorAllocator::allocate()` returns the bare `u8`; `ProbeCtx`
  stamps the `InstalledIsr` with the caller-supplied `cpu_id` so
  drivers can wire their per-CPU waker from the same `CpuId` the
  ISR will run on.
- The IOAPIC redirection entry's destination field is set to
  `cpu_id.apic_id()`, so the AP whose LAPIC has that ID receives
  the interrupt instead of the BSP.

Hyper-V VMBus is per-vCPU on the host side too — each CPU has its
own SCONTROL/SIMP/SIEFP/SINTx MSRs. The Phase 6 data-structure
refactor in
[crates/embclox-hyperv/src/isr.rs](../../crates/embclox-hyperv/src/isr.rs)
turned the ISR's SIMP/SIEFP page lookups into per-CPU arrays
indexed by `cpu_local::current_cpu_id()`. AP-side SynIC bring-up
(actually programming the AP's MSRs and routing offers) is future
work — see [VMBus per-CPU status](#vmbus-per-cpu-status) below.

## Executor and task placement

Each CPU runs its own embassy `Executor` instance. We **don't need
cross-CPU pender routing** because wakes are CPU-local by
construction: a future running on CPU *k* registers wakers in CPU
*k*'s state; an ISR on CPU *k* fires those wakers; the executor on
CPU *k* is the one polled in the loop that runs on CPU *k*. The
existing no-op `__pender` keeps working unchanged.

Flow per CPU:

- An ISR-driven wake sets the "ready" flag on this CPU's executor.
- The same IRQ that caused the wake also breaks this CPU out of
  `hlt` in its own `run_executor` loop
  ([crates/embclox-hal-x86/src/runtime.rs](../../crates/embclox-hal-x86/src/runtime.rs)).
- The next `executor.poll()` sees the ready flag and polls the
  task.

### No per-CPU executor registry in the HAL

The original design called for a `[Once<&'static Executor>;
MAX_CPUS]` lookup in the HAL. We dropped it: embassy's `Executor`
type contains `PhantomData<*mut ()>` and is therefore `!Sync`, so
storing `&'static Executor` in a shared static requires an
`unsafe impl Sync` wrapper for no benefit — the kernel owns each
executor's `StaticCell` at the call site and never needs a global
lookup table for it.

The example kernel keeps two separate statics: a single
`EXECUTOR: StaticCell<Executor>` for the BSP (running `net_task`
+ `echo_task`) and `AP_EXECUTORS: [StaticCell<Executor>; MAX_CPUS
- 1]` indexed by `processor_id - 1` for the APs. Each AP's
`ap_entry` initialises exactly its own slot.

Spawning a task on a CPU still requires a `Spawner` for that CPU's
executor. Cross-CPU spawn is intentionally awkward: the BSP can
hand each AP a closure to call at startup, but there's no global
`spawn(task)` that picks a CPU automatically. That stays out until
Option C.

## embassy-time per CPU

`embassy_time_driver::time_driver_impl!` registers exactly one
`Driver` for the whole program (no per-CPU choice; it's a global
by API). We keep the single global driver and shard its alarm slot
array by CPU. See
[crates/embclox-hal-x86/src/time.rs](../../crates/embclox-hal-x86/src/time.rs):
the single `[Option<Alarm>; 8]` became
`[Mutex<RefCell<[Option<Alarm>; 8]>>; MAX_CPUS]` indexed by
`cpu_local::current_cpu_id()`.

Routing works because tasks don't migrate:

- `schedule_wake(at, waker)` runs in the context of the calling
  task, which is pinned to one CPU. It reads
  `cpu_local::current_cpu_id()` and writes the alarm into that
  CPU's slot array.
- `on_timer_tick()` (called from each CPU's APIC timer ISR) reads
  `cpu_local::current_cpu_id()` and drains only that CPU's slots.
- The waker stored in the slot was registered by a task on the
  same CPU; firing it sets the ready flag on the same-CPU
  executor, which is exactly what we need.

`now()` stays global (TSC read).

Time itself (the monotonic clock) is global. Modern x86 guarantees
invariant TSC across cores and we rely on Limine + the hypervisor
to leave the APs' TSCs in sync with the BSP. Hyper-V Gen1 / Azure
expose the InvariantTsc enlightenment which guarantees this; QEMU
starts all vCPUs with TSC=0 at boot.
[`smp::check_tsc_sync`](../../crates/embclox-hal-x86/src/smp.rs)
is available as an AP self-check; the example kernel does not
call it yet.

## Driver impact

The three production drivers (e1000, tulip, NetVSC) need **no
source changes** to work under per-CPU executors. They use:

- `AtomicWaker` for IRQ→executor wake. The wake fires on the same
  CPU as the ISR and finds a waker registered by a future running
  in that CPU's executor.
- `embassy_net_driver::Driver` with `&mut self`. Single-CPU
  semantics still hold because each driver is owned by exactly one
  CPU.
- `dev.waker()` accessors (NetVSC). The per-channel waker table in
  [crates/embclox-hyperv/src/isr.rs](../../crates/embclox-hyperv/src/isr.rs)
  is now indexed by `cpu_local::current_cpu_id()` for the per-CPU
  SIMP/SIEFP slots; the per-channel relid table stays a single
  global array (relids are bus-wide, not per-CPU).

The only driver-facing change is the new
`install_pci_isr_on(line, handler, cpu_id)` API for drivers that
explicitly want to land on an AP.

## VMBus per-CPU status

Phase 6 was a **data-structure refactor only**:

- `isr::SIEFP_VADDR` / `SIMP_VADDR` became `[AtomicUsize; MAX_CPUS]`
  indexed by `processor_id`.
- `vmbus_isr` reads `cpu_local::current_cpu_id()` to pick its slot.
- `publish_siefp` / `publish_simp` take a `CpuId` parameter.
- `try_init` populates the BSP's slot (`CpuId::Bsp`).

What did **not** ship in phase 6 (needs Hyper-V test env):

- An `ap_setup_synic(cpu_id, dma)` helper that allocates per-AP
  SIMP/SIEFP pages and writes the AP's SCONTROL/SIMP/SIEFP/SINT2
  MSRs from inside the AP's context.
- NetVSC `NumSubChannels = N` path (currently we send 0 =
  single-queue; see `nvsp_request_single_queue` in
  [crates/embclox-hyperv/src/netvsc.rs](../../crates/embclox-hyperv/src/netvsc.rs)).
- Subchannel offer routing to APs.

Until those land, VMBus stays single-CPU (BSP only) at runtime,
which is the right behaviour for our current workloads. The data
shape is ready for AP-side bring-up to be added later without
touching `isr.rs` again.

## Single-queue NICs today (scaling cap)

All three production NIC drivers are **single-queue, single-CPU**:

| Driver | Queues | IRQ | Notes |
|--------|--------|-----|-------|
| e1000 ([crates/embclox-e1000](../../crates/embclox-e1000)) | 1 RX + 1 TX (256 desc each) | INTx via PCI line | Hardware supports up to 2 RX queues, unused. No MSI/MSI-X. |
| tulip ([crates/embclox-tulip](../../crates/embclox-tulip)) | 1 RX + 1 TX (16 desc each) | INTx via PCI line | DEC 21140/21143 is intrinsically single-queue. |
| NetVSC ([crates/embclox-hyperv/src/netvsc.rs](../../crates/embclox-hyperv/src/netvsc.rs)) | 1 primary VMBus channel (shared RX/TX) | SINT2 | We explicitly request `NumSubChannels=0` (single-queue). Protocol supports up to 8 subchannels for RSS. |

This caps any single-NIC workload at one CPU's worth of driver work
regardless of how many APs the SMP work brings online. The
Option-A win is shaped like:

1. **One NIC, N CPUs.** NIC stays on BSP; APs run application
   logic. Network driver throughput is unchanged; application
   throughput scales.
2. **Multiple NICs, N CPUs.** Each NIC pinned to a different CPU.
   Realistic line-rate path for, e.g., a dual-NIC firewall (NIC-0
   on CPU-0, NIC-1 on CPU-1). Works today with no driver changes.
3. **One NIC, RSS across N CPUs.** Requires per-driver multi-queue
   support; out of scope for this design.

Multi-queue support is a natural follow-on, ordered by likely value:

- **NetVSC subchannels first.** Protocol is already designed for
  it: send `NumSubChannels=N` instead of `0`, allocate N more
  `Channel`s, pin each to a different CPU's SINT2 waker. Each
  subchannel has its own ring-buffer pair. The Hyper-V VMBus
  per-CPU work in this design's Phase 6 is the substrate.
- **virtio-net next.** When the storage gap brings in virtio
  infrastructure ([gap-analysis.md](./gap-analysis.md) gap #1),
  virtio-net's native multiqueue support comes along.
- **e1000 stays single-queue.** 8254x RSS is anaemic and not worth
  the effort. A future e1000e/igb driver would be the place for
  real PCI-NIC multiqueue.

This is tracked under [gap-analysis.md](./gap-analysis.md) gap #4
("Hardware offloads") and gap #2 ("the returned NetDevice will
need either per-CPU sharding (RX/TX queues per core) or a
per-driver lock"). The current design intentionally defers that
work — SMP-Option-A is useful by itself for cases (1) and (2),
which cover most of the immediate workloads.

## Compatibility with the existing kernel

We have one example kernel binary today
([examples-kernel/src/main.rs](../../examples-kernel/src/main.rs)).
The e1000 / tulip / NetVSC / Hyper-V variants are all the same ELF
booted with different `limine-*.conf` cmdlines
([examples-kernel/limine.conf](../../examples-kernel/limine.conf),
[limine-hyperv.conf](../../examples-kernel/limine-hyperv.conf),
[limine-hyperv-tulip.conf](../../examples-kernel/limine-hyperv-tulip.conf),
[limine-azure.conf](../../examples-kernel/limine-azure.conf)).
The kernel inspects cmdline at boot and picks the right NIC path.

SMP support follows the same model:

- **Default behaviour is unchanged.** If no SMP cmdline arg is
  present, `kmain` skips `bring_up_aps`; the APs sit in their
  Limine-provided spin loop and never enter Rust. All existing
  ctest lanes pass with no code or config changes.
- **Opt-in via cmdline.** The `smp=on` and `cpus=N` tokens are
  parsed by `parse_smp` in
  [crates/embclox-hal-x86/src/cmdline.rs](../../crates/embclox-hal-x86/src/cmdline.rs)
  (same `whitespace-split` + `key=value` parser already used for
  `net=`, `ip=`, `gw=`, `nic=`). When `smp=on` is present, BSP
  calls `bring_up_aps` for up to `cpus=N` APs (capped at
  `MAX_CPUS`) and routes the kernel-supplied `ap_entry` to each.
  The AP entry function lives in
  [examples-kernel/src/main.rs](../../examples-kernel/src/main.rs)
  next to `kmain`.
- **No new kernel binary.** The SMP path is a runtime branch in the
  existing kernel, gated by cmdline. This matches how the NIC
  variants are selected today.

## Test plan (shipped)

Per the test harness shape
([docs/design/test-framework.md](./test-framework.md)):

1. **`cpu_local` unit suite** in
   [qemu-tests/unit/src/suites/cpu_local.rs](../../qemu-tests/unit/src/suites/cpu_local.rs).
   6 tests covering slot layout, idempotent `init_bsp`,
   `current_cpu_id()` after init, `CpuId::apic_id()` resolution,
   unpopulated-slot and out-of-range-slot behaviour.
2. **`parse_smp` unit tests** added to
   [crates/embclox-hal-x86/src/cmdline.rs](../../crates/embclox-hal-x86/src/cmdline.rs):
   6 tests covering default-disabled, `smp=on` (case-insensitive),
   `cpus=N`, malformed inputs.
3. **`kernel-echo-e1000-smp` ctest lane** in
   [examples-kernel/CMakeLists.txt](../../examples-kernel/CMakeLists.txt).
   Boots `build/kernel-smp.iso` (same ELF as `kernel.iso`, different
   limine cmdline: `smp=on cpus=4`) under `qemu -smp 4`. Each AP
   spawns `ap_heartbeat_task` on its own embassy executor; the
   task ticks `AP_COUNTERS[processor_id]` on every 10 ms
   `Timer::after_millis` expiry. The test passes when: (a) e1000
   TCP echo still works on BSP, and (b) the `SMP CHECK:
   ap_counters=[0, n, n, n, ...]` log line shows positive heartbeat
   counters for APs 1–3 (slot 0 stays 0 because BSP never runs
   `ap_entry`).
4. **`cargo build --target x86_64-unknown-none` + `cargo clippy -D
   warnings`** clean on the workspace.

Hyper-V lanes were intentionally not modified: VMBus stays
BSP-only at runtime, so existing `kernel-hyperv-netvsc` and
`kernel-hyperv-tulip` paths are unchanged.

Last verified run (4 lanes, 100% pass, phase 8 shipped):

```
kernel-echo-e1000      11.14s
kernel-echo-tulip      11.14s
kernel-echo-e1000-smp  11.15s
unit                    4.93s
```

Captured AP heartbeat snapshot from the SMP lane:

```
[INFO ] AP 2 alive (apic_id=2, tsc/us=2692)
[INFO ] AP 3 alive (apic_id=3, tsc/us=2692)
[INFO ] AP 1 alive (apic_id=1, tsc/us=2692)
[INFO ] SMP CHECK: ap_counters=[0, 40, 43, 42, 0, 0, 0, 0]
```

The ~40 ticks per AP cover the AP's full lifetime from
`ap_entry` start to the BSP sample point (boot finishing + the
100 ms BSP sleep before the dump), at one tick per 10 ms
`Timer::after_millis` cycle. The BSP slot is 0 by design.

## What shipped

Phases, in dependency order; each row is a commit on `dev`.

| Phase | Commit | Summary |
|-------|--------|---------|
| Plan | `4f11ca7` | This doc. |
| 1 | `fbe463a` | `CpuId::Ap(u8)` + `cpu_local` table + 6-test unit suite. |
| 2 | `0b4bd17` | Limine `MpRequest` in the `limine_boot_requests!` macro. |
| 3 | `12e5761` | `smp::bring_up_aps` + `ap_setup` + GS_BASE per-CPU + `idt::load_current_cpu`. |
| 4 | `2a6f7bb` | Per-CPU `embassy-time` alarm slots in the global driver. |
| 5 | `8dc0bb2` | `ProbeCtx::install_pci_isr_on(line, handler, cpu_id)`. |
| 7 | `05859b7` | `smp=on cpus=N` cmdline + `kernel-echo-e1000-smp` ctest lane. |
| 6 | `c2946dc` | Per-CPU SIEFP/SIMP slot table in the VMBus ISR. |
| 8 | _(this change)_ | APs run their own embassy executor + `ap_heartbeat_task` instead of a halt loop. |

Phases 1–5 were mechanical and had no driver-facing API churn.
Phase 6 affects only `embclox-hyperv` (data-shape only; runtime
behaviour unchanged). Phase 7 wired up the cmdline opt-in and the
new ctest lane. Phase 8 swapped the AP body from a halt loop to a
real per-CPU executor running an embassy task — the substrate is
now in place for APs to do meaningful async work.

## Decisions

Decisions taken to keep the design concrete; revisit only if
implementation exposes a problem.

- **AP stack: Limine-provided.** `cpus[i].stack` from the SMP
  response. No heap allocation, no `static` array carving. We can
  switch to per-CPU `static` stacks later if we need guard pages.
- **`MAX_CPUS = 8`.** Hardcoded constant. Covers QEMU smoke tests
  (`-smp 4`), Hyper-V Gen1 / Azure standard SKUs, and leaves
  headroom. Bigger boxes can bump the constant.
- **GS_BASE for per-CPU pointer.** Standard x86_64 kernel-mode
  convention. Uncontroversial because we have no userspace and
  therefore never need `swapgs`.
- **Embassy `__pender`: no-op (unchanged).** Wakes are CPU-local
  by construction; no routing layer needed.
- **No per-CPU TSS.** We don't use IST today. Add per-CPU TSS the
  day we want a double-fault IST stack, not now.
- **TSC: rely on hypervisor sync.** Hyper-V's `InvariantTsc`
  enlightenment and QEMU's same-start guarantee already give us
  what we need. Phase 3 adds an early-boot AP self-check that
  asserts `|tsc_ap - tsc_bsp| < 1 ms` worth of ticks; if a target
  fails it, we revisit (per-CPU TSC offset is the fallback).
- **Cmdline tokens: `smp=on cpus=N`.** Two independent tokens so
  `smp=on` alone defaults to "use all reported APs up to
  MAX_CPUS".

## References

- [moss-kernel.md](../background/moss-kernel.md) — full
  architecture survey and the "Option A/B/C" framing.
- [driver-model.md](./driver-model.md) — `SMP-forward design
  choices` section that pre-shaped `CpuId` / `InstalledIsr` /
  registry borrowing.
- [interrupt-driven-mode.md](./interrupt-driven-mode.md) — current
  single-CPU IRQ + APIC timer wiring.
- [gap-analysis.md](./gap-analysis.md) gap #2 (scheduler /
  multitasking) — broader strategic context.
- [Limine SMP protocol](https://github.com/limine-bootloader/limine/blob/trunk/PROTOCOL.md#smp-feature)
- [Embassy multi-executor docs](https://embassy.dev/book/#_multiple_executors).
