# SMP via per-CPU executors (Option A)

## Status: planning

Bring up application processors (APs) so embclox can run on more
than one core. Each CPU gets its own embassy executor and its own
async runtime; tasks are pinned to a CPU at spawn time and never
migrate. IRQs are routed to a specific CPU and that CPU's executor
wakes its local tasks.

This is the "Option A" path from the SMP architecture discussion:
the lightest-weight way to use additional cores while keeping the
existing driver shape intact.

## Goals and non-goals

### Goals

- Boot APs via the Limine SMP request; each AP enters Rust running
  its own `run_executor` loop.
- One embassy executor per CPU, owning its own task set. No task
  migration.
- Per-CPU LAPIC handle, per-CPU APIC timer driving `embassy-time`.
- IRQ routing: each device IRQ is delivered to exactly one CPU
  (chosen at driver init), and that CPU's executor sees the
  resulting wake.
- Driver-facing APIs unchanged: `AtomicWaker`-based wakes keep
  working because the wake fires on the same CPU as the IRQ and
  the executor that owns the registered waker.
- Existing single-CPU example kernels keep working with zero source
  changes (build-time gate or runtime "1 CPU detected" path).

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

Limine has a built-in SMP request that boots APs into a callback
we provide. The handshake is:

1. BSP declares `SmpRequest` alongside the other Limine requests
   (extend the `limine_boot_requests!` macro in
   [crates/embclox-hal-x86/src/limine_boot.rs](../../crates/embclox-hal-x86/src/limine_boot.rs)).
2. After `kmain` has finished single-threaded boot (heap, IDT, PCI
   scan, driver probe), it walks the SMP response's `cpus[]` array.
   For each entry it gets a sequential `processor_id` (Limine's own
   0..N numbering) and an `lapic_id` (hardware-assigned, possibly
   sparse).
3. For each AP, BSP allocates a per-CPU `CpuLocal` block and stores
   it in `CPU_LOCALS[processor_id]`. The block is populated *before*
   the AP is told to start; Limine guarantees the AP's `goto_address`
   store is release-ordered relative to the AP's first instruction
   fetch, so the AP sees the populated block.
4. BSP writes a `goto_address` that points at `ap_entry`. Limine
   also gives each AP its own stack via `cpus[i].stack`; we use
   that as-is (no separate stack allocation needed).
5. Each AP enters `ap_entry`, loads `GS_BASE` with the address of
   its `CpuLocal` block (looked up by `processor_id` passed via
   `cpus[i].extra_argument`), enables its LAPIC + APIC timer, then
   calls `run_executor` on its own executor.

```rust
// new public API in embclox-hal-x86::smp
pub struct ApInit {
    pub cpu_id: CpuId,        // CpuId::Ap(processor_id)
    pub apic_id: u32,         // from Limine cpus[i].lapic_id
    pub tsc_per_us: u64,      // BSP-calibrated, copied to AP
}

pub fn bring_up_aps(
    smp_response: &SmpResponse,
    on_ap_ready: extern "C" fn(ApInit) -> !,
);
```

`on_ap_ready` is supplied by the kernel binary (so the kernel
chooses what tasks to spawn on each AP). The HAL gives APs a
canonical setup helper:

```rust
pub fn ap_setup(init: ApInit) -> Peripherals { ... }
// loads GS_BASE, installs IDT pointer, enables LAPIC, starts APIC timer.
// returns the per-CPU peripherals
```

## Per-CPU statics

x86 convention is to point `GS_BASE` (set via
`wrmsr IA32_KERNEL_GS_BASE`) at a per-CPU data block. embclox today
has no per-CPU data because there's only one CPU; this work
introduces a fixed-size table indexed by sequential `processor_id`:

```rust
// embclox-hal-x86::cpu_local
pub const MAX_CPUS: usize = 8;   // covers QEMU smoke tests, Hyper-V, Azure

#[repr(C)]
pub struct CpuLocal {
    pub cpu_id: CpuId,
    pub apic_id: u32,
    pub lapic: LocalApic,
    pub executor: &'static Executor,
}

static CPU_LOCALS: [OnceCell<CpuLocal>; MAX_CPUS] = [const { OnceCell::new() }; MAX_CPUS];

pub fn current() -> &'static CpuLocal { ... }   // reads GS_BASE
pub fn current_cpu_id() -> CpuId { current().cpu_id }
pub fn by_id(cpu: CpuId) -> Option<&'static CpuLocal> { ... }
```

The block is populated by BSP during `bring_up_aps` (BSP also gets
one, at `CPU_LOCALS[0]`). Each AP loads its own block's address
into `GS_BASE` in `ap_setup`. `MAX_CPUS = 8` is hardcoded; the
additional cost is 8 `CpuLocal` slots in BSS (a few hundred bytes
total) and we can grow it if a target VM exposes more vCPUs.

Modules that currently rely on `static mut LAPIC: Option<...>` in
[crates/embclox-hal-x86/src/runtime.rs](../../crates/embclox-hal-x86/src/runtime.rs)
become `cpu_local::current().lapic` accessors. The APIC timer ISR
and the `lapic_eoi()` helper change shape but not semantics.

## CpuId becomes real

[crates/embclox-hal-x86/src/vector_alloc.rs](../../crates/embclox-hal-x86/src/vector_alloc.rs)
already anticipates this. `CpuId::Ap` carries Limine's sequential
`processor_id` (0..MAX_CPUS), not the LAPIC ID:

```rust
pub enum CpuId {
    Bsp,        // == processor_id 0
    Ap(u8),     // processor_id 1..MAX_CPUS
}
```

The LAPIC ID is hardware-assigned and possibly sparse, so it's
stored alongside in `CpuLocal::apic_id` rather than embedded in
`CpuId`. `cpu_local::by_id(cpu).apic_id` is what `IoApic::enable_irq`
feeds the IOAPIC redirection entry. Drivers handle `CpuId` as an
opaque token and never need to look at the LAPIC ID directly.

## IRQ routing

Each device picks one CPU at probe time:

```rust
// inside a driver's probe()
let isr = ctx.install_pci_isr_on(line, handler, cpu)?;
// ctx.install_pci_isr(...) keeps existing BSP-default behaviour
```

The current single-CPU `install_pci_isr` defaults to `CpuId::Bsp`;
the SMP path adds `install_pci_isr_on(line, handler, cpu_id)` that
allocates the vector from *that CPU's* VectorAllocator and writes
the IOAPIC redirection entry pointing at `cpu_id.apic_id()`. The
returned `InstalledIsr` already carries `cpu_id` so the driver's
waker setup naturally lives on the same CPU.

Hyper-V VMBus is per-vCPU on the host side too — each CPU has its
own SIMP/SIEFP pages. SMP support for VMBus is therefore "set up
per-CPU SIMP/SIEFP and route SINT2 per-CPU" rather than "broadcast
one SINT2 vector to all CPUs". `embclox-hyperv::try_init` is
already structured around this; the SMP work extends it to install
per-CPU ISRs and publish per-CPU page pointers.

## Executor and task placement

Each CPU has its own `static Executor`, stored in a `StaticCell`
slot in the same `[T; MAX_CPUS]` shape as `CPU_LOCALS`:

```rust
static EXECUTORS: [StaticCell<Executor>; MAX_CPUS] = [const { StaticCell::new() }; MAX_CPUS];
```

The `Executor::new(context_ptr)` constructor takes a context
pointer that embassy passes back to the `__pender` callback when a
task becomes ready. We **don't need cross-CPU pender routing**
because wakes are already CPU-local by construction (a future
running on CPU *k* registers wakers in CPU *k*'s state; an ISR on
CPU *k* fires those wakers; the executor on CPU *k* is the one
polled in the loop that runs on CPU *k*). The existing no-op
`__pender` keeps working:

- An ISR-driven wake sets the "ready" flag on CPU *k*'s executor.
- The same IRQ that caused the wake also breaks CPU *k* out of
  `hlt` in its own `run_executor` loop.
- The next `executor.poll()` sees the ready flag and polls the
  task.

This is exactly how the single-CPU build works today; the only
change is that there are N independent instances of the loop
instead of one. `context_ptr` is passed (set to
`cpu_local::current() as *const _ as *mut ()`) for future use, but
nothing routes by it today.

The kernel chooses what to spawn on each CPU:

```rust
// BSP, inside kmain after probe
spawner.spawn(net_stack_task()).unwrap();
spawner.spawn(net_runner_task()).unwrap();

// AP entry — kernel provides this; same binary, different code path
extern "C" fn on_ap_ready(init: ApInit) -> ! {
    let peripherals = hal::ap_setup(init);
    let executor = hal::cpu_local::current().executor;
    let spawner = executor.spawner();
    spawner.spawn(worker_task()).unwrap();
    hal::runtime::run_executor(executor)
}
```

Spawning a task on a CPU requires a `Spawner` for that CPU's
executor. Cross-CPU spawn is intentionally awkward: the BSP can
hand each AP a closure to call at startup, but there's no global
`spawn(task)` that picks a CPU automatically. That stays out until
Option C.

## embassy-time per CPU

`embassy_time_driver::time_driver_impl!` registers exactly one
`Driver` for the whole program (see
[crates/embclox-hal-x86/src/time.rs](../../crates/embclox-hal-x86/src/time.rs)).
We keep the single global driver but shard its alarm slots by CPU.
The existing flat `[Option<Alarm>; 8]` becomes per-CPU:

```rust
struct ApicTimeDriver {
    tsc_per_us: AtomicU64,
    alarms: [Mutex<RefCell<[Option<Alarm>; MAX_ALARMS]>>; MAX_CPUS],
}
```

Routing works because futures don't migrate:

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
starts all vCPUs with TSC=0 at boot. Phase 3 of the implementation
adds a sanity check: each AP reads TSC at startup and asserts the
delta against the BSP's reference is `< 1 ms` worth of ticks.

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
  becomes per-CPU once VMBus is SMP-aware; until then it stays on
  the BSP.

The only driver-facing change is the new `install_pci_isr_on(...,
cpu_id)` API for drivers that explicitly want to land on an AP.

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
  ctest lanes keep passing with no code or config changes.
- **Opt-in via cmdline.** Add `smp=on` and `cpus=N` tokens to
  [crates/embclox-hal-x86/src/cmdline.rs](../../crates/embclox-hal-x86/src/cmdline.rs)
  (same `whitespace-split` + `key=value` parser already used for
  `net=`, `ip=`, `gw=`, `nic=`). When `smp=on` is present, BSP
  calls `bring_up_aps` for up to `cpus=N` APs (capped at
  `MAX_CPUS`) and routes a kernel-supplied `on_ap_ready` to each.
  The AP entry function lives in `examples-kernel/src/main.rs`
  next to `kmain`.
- **No new kernel binary.** The SMP path is a runtime branch in the
  existing kernel, gated by cmdline. This matches how the NIC
  variants are selected today.

## Test plan

Per the current test harness shape
([docs/design/test-framework.md](./test-framework.md)) and the
existing cmdline-driven variant pattern:

1. **Unit (qemu-tests/unit).** Add a suite that boots
   `unit-tests.iso` with `-smp 4`, verifies all 4 CPUs entered
   Rust, each ran their own APIC timer at least once, and that
   `cpu_local::current_cpu_id()` returns a distinct value per CPU.
   This requires `unit-iso` to set the `smp=on` cmdline.
2. **New ctest lane `kernel-echo-e1000-smp`.** Same kernel ELF as
   the existing `kernel-echo-e1000` lane, booted via a new
   `limine-smp.conf` that adds `smp=on cpus=4` to the cmdline. QEMU
   is invoked with `-smp 4`. NIC IRQ stays on BSP; APs run a
   per-CPU idle counter task. After echo traffic completes, the
   kernel dumps per-CPU counters over debug-out; the ctest harness
   asserts all four advanced.
3. **kernel-hyperv-\* (manual).** Existing Hyper-V lanes already
   support cmdline selection; bring the SMP variants up by editing
   `limine-hyperv.conf` to add `smp=on` and provisioning the VM
   with 2+ vCPUs. Confirm boot + DHCP still pass and that the
   second vCPU's APIC timer fires.
4. **`cargo build --target x86_64-unknown-none` + `cargo clippy -D
   warnings`** continue to pass on the single workspace.

The CMake changes are small: a new `limine-smp.conf` and a new
ctest lane in [examples-kernel/CMakeLists.txt](../../examples-kernel/CMakeLists.txt)
following the existing `kernel-echo-e1000` pattern.

## Implementation outline

Phases, in dependency order:

1. **`CpuId` and per-CPU statics.** Promote `CpuId` to a real enum
   with `Ap(u32)`. Add `embclox-hal-x86::cpu_local` module
   (GS_BASE-backed `current()`). Adapt existing single-CPU users.
2. **Limine SMP request.** Extend `limine_boot_requests!`. Add the
   SMP response field to `LimineBootInfo`.
3. **AP bring-up.** Add `bring_up_aps` + `ap_setup`. Each AP gets
   its own LAPIC handle and APIC timer; per-CPU `CpuLocal` block
   populated.
4. **Per-CPU executor support.** `embassy-time` driver becomes
   per-CPU; APIC-timer ISR advances local slots. `run_executor`
   uses `cpu_local::current().executor`.
5. **Per-CPU `VectorAllocator`.** `ProbeCtx` keeps its existing
   `install_pci_isr(line, handler)` returning a BSP `InstalledIsr`;
   add `install_pci_isr_on(line, handler, cpu_id)` that hits the
   target CPU's allocator. IOAPIC routing uses
   `CpuId::apic_id()`.
6. **VMBus per-CPU.** When the target CPU has a Hyper-V-aware
   driver, allocate per-CPU SIMP/SIEFP pages, install SINT2 per
   CPU, publish per-CPU vaddrs. Channel wakers move to a
   `(cpu_id, relid)` table.
7. **SMP cmdline + ctest lane.** Add `smp=on cpus=N` cmdline
   handling in `examples-kernel/src/main.rs`, a new
   `limine-smp.conf`, and a `kernel-echo-e1000-smp` ctest lane that
   boots the existing kernel ELF under `qemu -smp 4`.

Phases 1–5 are mechanical and have no driver-facing API churn.
Phase 6 only affects `embclox-hyperv`. Phase 7 wires up the new
`limine-smp.conf` and ctest lane.

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
