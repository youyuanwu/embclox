# Gap analysis: from prototype to ring-0 I/O framework

## Status: strategy

embclox today is a **driver-and-runtime prototype**: it boots on
QEMU and Hyper-V, exposes three NIC drivers (e1000, tulip, NetVSC)
behind embassy-net + smoltcp, and ships three single-driver example
kernels.

Where we want to go: a **framework for ring-0 embedded applications
that are network-and-disk-IO-heavy**. Concretely:

> An application author should be able to drop their async Rust code
> into an embclox example crate, link against our HAL + drivers +
> stack, build a single Limine-bootable image, and ship it as the
> firmware of a network/storage appliance. The framework should
> handle device discovery, drivers, the I/O stacks, scheduling, and
> fault containment so the application can focus on business logic.

This doc is a **gap analysis** between today's prototype and that
target. It is informed by the
[Rust kernel survey](../background/rust-kernel-survey.md) and the
deep-dive background docs on Redox, Moss, and Theseus. It is **not**
a plan; it identifies what is missing and references the prior
design work that addresses each gap.

## The target workload, made concrete

To make "I/O-heavy ring-0 application" concrete, here are three
representative use cases the framework should support:

1. **Network appliance.** TCP/HTTP load balancer or stateful
   firewall. Multi-Gbps line-rate, 64-byte to MTU packet sizes,
   thousands of concurrent connections, low jitter.
2. **Storage gateway.** iSCSI target or block-level dedupe cache
   sitting between a NIC and an NVMe drive. Sustained read/write
   throughput at storage device limits, multi-queue parallelism.
3. **Edge compute hub.** MQTT broker + on-device analytics on a
   small number of sensor streams. Low power, deterministic latency,
   long-running uptime (months).

Each scenario implies different concrete requirements, but they
share: **driver breadth, I/O stack maturity, scheduling discipline,
fault tolerance, and observability**.

## Gap inventory

Gaps below are categorised by subsystem. For each:

- **Today**: current state in embclox.
- **Needed**: what the target workload requires.
- **Severity**: 🔴 blocker · 🟠 major · 🟡 minor.
- **References**: prior design or background work that informs the
  approach.

### 1. Storage and disk I/O — 🔴

This is the biggest single gap. The target workload includes
storage scenarios; we have **zero storage support**.

| Layer | Today | Needed |
|-------|-------|--------|
| Disk driver | None | At least one of: virtio-blk, NVMe, AHCI. Probably **virtio-blk first** (works in QEMU + Hyper-V + Azure), **NVMe** for performance, **AHCI** if we want bare-metal SATA. |
| Block layer | None | A `BlockDevice` trait (read/write sectors, queue ops), buffer pool, completion routing. |
| Buffer cache | None | Page-cache equivalent — coalescing, write-back, eviction policy. |
| Filesystem | None | At minimum a read-only ramfs for boot artifacts. For real work: ext4 (read), FAT32, or our own crash-consistent log-structured FS for write workloads. |
| Async block I/O | None | `embedded-storage-async` traits + executor integration so disk I/O composes with network I/O in the same async runtime. |

References: Redox's `ahcid`/`nvmed`/`virtio-blkd` daemons, Moss's
ext2/3/4 driver, ArceOS's `axdriver` block trait. Theseus has only
`ata` PIO — useful as a worked example but too slow for real work.

**Severity: 🔴.** Without a block driver and FS, two of the three
target use cases (storage gateway, MQTT persistence) are unbuildable.

### 2. Scheduler and multitasking — 🔴

Today: one logical task running in `block_on_hlt`, plus whatever the
embassy executor multiplexes on top. No SMP, no preemption, no
priorities.

| Capability | Today | Needed |
|------------|-------|--------|
| Multi-task | embassy executor (single-CPU, cooperative) | Adequate for in-app concurrency. **Keep.** |
| SMP | No (Limine boots APs; we ignore them) | Bring up APs; assign per-CPU executors; cross-CPU work stealing or partition. |
| Preemption | No | At least preempt long-running tasks via APIC timer; needed for fairness in the load-balancer/firewall scenarios. |
| Priority / fairness | FIFO only | EEVDF or weighted-fair-queueing for I/O paths. Reference: Moss uses EEVDF. |
| Real-time guarantees | None | Optional. Edge-compute scenario benefits from tickless deadline-driven scheduling. |
| Per-CPU state | Per-static `AtomicWaker` | Need full per-CPU state framework (per-CPU stacks, per-CPU executors, per-CPU IRQ counters). Reference: Theseus `cls`/`cls_allocator`, Moss `per_cpu_private!`. |

References: Moss (single-task-is-future + EEVDF + per-CPU
runqueues), Theseus (3 swappable schedulers behind a trait),
Redox (sync `WaitQueue` + cooperative `context::switch`). The
Moss model is most directly applicable.

**Severity: 🔴.** Network appliance scenarios need to scale across
CPUs; storage gateway needs concurrent I/O queues. Single-CPU
embassy maxes out at one core's worth of work.

### 3. Driver model — 🟠

Currently we hardcode device discovery in each example's `main.rs`.

This gap is **already designed** in
[../design/driver-model.md](./driver-model.md): `Bus` /
`PciDriver` / `VmBusDriver` / `ProbeCtx` / static driver registry.

References: see that doc + the
[Rust kernel survey](../background/rust-kernel-survey.md) for
ArceOS / Tock as the most directly applicable production designs.

**Severity: 🟠.** Workable today by hardcoding, but blocks "single
binary across QEMU + Hyper-V + bare metal" goal.

### 4. Network stack maturity — 🟠

embassy-net + smoltcp is a strong baseline. What's missing:

| Feature | Today | Needed for target |
|---------|-------|-------------------|
| TCP/UDP/ICMP/IPv4 | ✅ via smoltcp | ✅ |
| IPv6 | smoltcp supports, untested | Test + enable in examples |
| TLS | None | rustls (no_std build), needed for HTTPS / MQTTS |
| DNS resolver | None | embassy-net has dns module; integrate |
| HTTP client | None for our examples | A no_std HTTP client (e.g. embedded-nal-async + reqwless) |
| HTTP server | None | A no_std HTTP server (axum-style is too heavy; picoserve is lighter) |
| Multi-NIC routing | First-found only | Per-route table; reference: Linux RIB |
| NAPI-style polling | Pure interrupt-driven | Hybrid: switch to polling under heavy load to reduce IRQ overhead. Critical for line-rate. |
| Zero-copy receive | smoltcp copies into per-socket buffers | Zero-copy path for high-pps use cases |
| Hardware offloads | None | TCP/IP checksum, LSO, RSS — required to hit modern line rates |

References: Moss (smoltcp + ringbuf, similar baseline), Redox
(smolnetd over scheme — same library, different IPC model).

**Severity: 🟠.** Today's stack runs TCP echo at QEMU speeds. Real
network appliance workloads need hardware offloads + NAPI-style
polling at minimum.

### 5. Fault containment and RAS — 🟠

| Concern | Today | Needed |
|---------|-------|--------|
| Panic policy | `panic = abort` → halt | `panic = unwind` + per-task `catch_unwind` so a single failed request doesn't take the box down. |
| Driver isolation | None — all drivers share kernel address space | Without an MMU split: at least sound `unsafe` audit boundaries; in the limit, Theseus-style cell isolation. |
| Memory pressure | Unbounded heap; OOM = halt | Per-task quotas; pressure feedback to back-pressure I/O. |
| Watchdog | None | Hardware or software watchdog with auto-reset. |
| Auto-restart | None | Theseus-style swap-in-known-good for crashed components. |
| Stack overflow detection | None | Guard pages on each per-task stack. Moss has it. |

References: Theseus `fault_crate_swap`, Moss stack-overflow
detection, Hubris static-everything-no-OOM.

**Severity: 🟠.** A storage gateway that takes down the kernel on
one bad checksum is not viable. Even the simplest version
(`panic = unwind` + per-request panic catching + auto-restart of the
failing task) closes most of the gap.

### 6. Memory management — 🟠

| Concern | Today | Needed |
|---------|-------|--------|
| Heap | `LockedHeap` (linked-list) | Per-CPU slab allocator for small objects; arena allocators for I/O buffers. |
| DMA buffers | `BootDmaAllocator` (bump) | Pool allocator with size classes; coherent vs streaming DMA distinction. |
| Page allocator | Limine-provided HHDM + `MemoryMapper` | A real frame allocator (buddy or bitmap) for runtime page allocation beyond MMIO mapping. |
| MMIO mapping | `MemoryMapper::map_mmio` | OK; needs page-attribute (cached/uncached/write-combining) discipline. |
| Per-CPU memory | None | Per-CPU heaps + per-CPU stacks for SMP. |
| Memory encryption | None | Optional — needed for confidential-computing deployments. |

References: Moss (slab + buddy + per-CPU object cache), Theseus
(multiple heaps, slabmalloc with safe/unsafe variants), ArceOS
(`axalloc`, `axmm_crates`).

**Severity: 🟠.** Workable today for small workloads, but
fragmentation and contention will bite at sustained throughput.

### 7. Application framework — 🟠

What does it mean to be "an embclox application"?

Today: an example crate is a `no_std` `no_main` binary that imports
HAL crates, defines its own `kmain`, registers ISRs, builds an
embassy-net stack, and runs an async loop. Every aspect of the
kernel is reachable; there is no API surface.

| Aspect | Today | Needed |
|--------|-------|--------|
| App entry point | Hand-rolled `kmain` | `#[embclox::main] async fn main()` macro that handles boot, device init, stack bring-up. |
| App config | Limine cmdline only | Persistent config file in flash / a config namespace; structured deserialisation. |
| App logging | `log!` macros to UART | Structured logging with severity levels + remote shipping. |
| App I/O | Direct embassy-net usage | A facade (e.g. `embclox::net::TcpListener`) that hides smoltcp details and provides familiar `tokio`-like API. |
| Multi-app | Not supported | Either: (a) Theseus-style cell namespaces, (b) separate kernels with VM-style isolation, or (c) explicitly single-app and document it. |
| Hot reload | Not supported | Optional. Theseus-style crate swap if we want it. |

References: Theseus app crates (`applications/swap`,
`applications/upd`), Moss user-space (Linux ABI is overkill for our
scope), embassy `#[embassy_executor::main]`.

**Severity: 🟠.** Workable today by writing the boot dance
manually, but a deterrent to adoption. The `embclox::main` macro
alone would 10× the perceived ease-of-use.

### 8. Architecture support — 🟡

Today: x86_64 only.

| Arch | Today | Needed for target |
|------|-------|-------------------|
| x86_64 | ✅ | ✅ |
| aarch64 | None | Many embedded use cases. Reference: Moss (aarch64-only), Theseus (in progress), rust-raspberrypi-OS-tutorials. |
| RISC-V | None | Emerging embedded; Theseus has it for some crates; rCore is full RISC-V. |
| Virtio | Net only (via NetVSC for Hyper-V; not generic virtio-net) | Generic virtio-pci would unlock all clouds — KVM, Firecracker, GCP. |

**Severity: 🟡.** x86_64 covers QEMU + Hyper-V + Azure already.
Other archs are growth, not blocker.

### 9. Observability — 🟡

| Concern | Today | Needed |
|---------|-------|--------|
| Logging | `log!` to UART | Structured (key-value) + ring buffer + remote shipping. |
| Metrics | None | Counter/gauge/histogram primitives + a Prometheus-style endpoint over HTTP. |
| Tracing | None | `tracing`-style spans, or at least a fixed-format binary trace. |
| Crash dumps | None | Save panic + stack + registers to flash before reboot. |
| ptrace / debugger | qemu + gdb only | OK for now; consider exposing live state over the network for production. |

References: Bottlerocket-style telemetry, Moss procfs, Theseus
serial port.

**Severity: 🟡.** Not blocking, but production deployments need it
within months of go-live.

### 10. Testing and CI — 🟡

| Concern | Today | Needed |
|---------|-------|--------|
| Unit tests | `embclox-async` host-tests; ctest + qemu integration | Expand host-testable surface; reference: Moss `libkernel`, Theseus `libkernel`. |
| Real-hardware CI | Hyper-V on Azure (manual) | Periodic automated runs on real ARM/x86 boards. |
| Network workload tests | Single TCP echo | iperf, h2load, packet-blaster runs at scale. |
| Storage workload tests | None (no storage) | fio-equivalent for our block layer once it exists. |
| Fuzzing | None | Network input fuzzing (smoltcp has its own; we should fuzz our drivers). |
| Formal verification | None | Aspirational only at our stage. |

**Severity: 🟡.** Quality grows in proportion to coverage. Right
now we're under-tested but not catastrophically so.

### 11. Configuration and provisioning — 🟡

| Concern | Today | Needed |
|---------|-------|--------|
| Build-time config | Cargo features per example | OK. |
| Runtime config | `cmdline=` Limine entries | Need: typed parsing, defaults, validation, persisted config. |
| Persistent storage | None | Tied to storage gap (#1). |
| Image management | Manual `make iso` | A1-2 step `embclox-bake my-app.toml` workflow. |
| Update mechanism | Reflash | Theseus-style OTA crate swap is overkill; A/B partition image update is reasonable. |

**Severity: 🟡.** Convenience, not capability.

### 12. Security — 🟡 → 🟠 if confidential workloads

| Concern | Today | Needed |
|---------|-------|--------|
| Privilege isolation | None — Ring 0 only | Either: accept (unikernel position), or add safe-language isolation (Theseus). |
| Secure boot | Limine handles boot, no chain-of-trust | Measured boot for production. |
| Cryptographic primitives | None in tree | Add via crates: `ring`, `aes-gcm`, `chacha20poly1305` — all have no_std builds. |
| Network firewalling | smoltcp accepts everything | Need a packet filter at minimum — reference: Linux netfilter / nftables shape. |
| Confidential computing | None | TDX / SEV-SNP support if we want cloud confidential VMs. Asterinas is exploring this in Rust. |

**Severity: depends on workload.** A network appliance needs
firewalling. A confidential storage workload needs measured boot +
TDX. A device on a private network needs neither.

## What we deliberately don't need (anti-goals)

To keep scope honest, things the survey shows other Rust kernels
have that **we should explicitly not build**:

- **POSIX / Linux ABI compatibility** (Moss, Maestro, Kerla,
  Asterinas). We're a framework for native-Rust ring-0 apps; the
  cost of Linux compat is enormous and the audience for
  "Linux-API-but-in-Rust" is well-served already.
- **Userspace** (Redox, Theseus). Adding ring 3 means syscalls,
  ABIs, and a process abstraction — undoes the entire ring-0
  efficiency story.
- **Microkernel IPC** (Redox, Hubris). Per-message overhead is
  exactly the cost we're trying to avoid for I/O-heavy workloads.
- **General-purpose desktop / GUI** (Aero, blog_os). We are not
  competing with hobby OSes; we're a framework, not an OS product.
- **Hot-loadable modules / cell swap** (Theseus). The complexity is
  enormous (`mod_mgmt` is ~5 kLOC of dynamic linker plus per-section
  dependency tracking) and the use case (live evolution) is rare
  for embedded appliances that can A/B flash.
- **Multi-arch from day one** (Tock supports 4 archs). Pick x86_64
  + aarch64; defer the rest.

Pruning these honestly is how we keep the framework small enough to
be approachable.

## Prioritisation

Ranking by expected impact × inverse cost. **Storage is deferred**
— the prototype focus is network appliance scenarios first.

### Tier 1 — required for basic credibility (next 1-2 quarters)

1. **Driver model** (gap #3). Implement
   [./driver-model.md](./driver-model.md) (with the SMP-forward
   notes called out in that doc). Cheapest, highest-ROI change:
   unblocks "single binary across QEMU + Hyper-V", crystallises
   the driver lifetime so subsequent SMP work has clean nouns, and
   forces every driver to satisfy `Send + Sync` on day one.
2. **Scheduler upgrade** (gap #2). Bring up APs, give each CPU an
   embassy executor, simple per-CPU FIFO or work-stealing. Do
   **not** build EEVDF on day one. The driver model from step 1
   ensures driver state is already SMP-safe.

(Storage — gap #1 — is **deferred** out of Tier 1.)

### Tier 2 — required for production deployments (after Tier 1)

3. **Fault containment** (gap #5): `panic = unwind` + per-task
   `catch_unwind` + watchdog.
4. **Application framework** (gap #7): `embclox::main` macro +
   typed config. Big UX win, low engineering cost.
5. **Network stack maturation** (gap #4): TLS, DNS, HTTP, then
   NAPI / hardware offloads.

### Tier 3 — required to scale (after production deployments exist)

6. **Memory management** (gap #6): per-CPU slab + buddy.
7. **Observability** (gap #9): metrics endpoint, structured logging.
8. **Architecture expansion** (gap #8): aarch64.
9. **Testing depth** (gap #10): real-HW CI, fuzzing.
10. **Storage stack** (gap #1) re-prioritised when a storage
    workload is on the roadmap.

### Always-on

11. **Configuration / provisioning** (gap #11): incremental UX
    improvements as the framework grows.
12. **Security** (gap #12): driven by specific deployment
    requirements.

### Why driver model before scheduler?

Both are blockers for the target. The order is **driver model
first** because:

- It is bounded scope and **already designed**
  ([driver-model.md](./driver-model.md)) — weeks of work, not
  months.
- It is a **demonstrability win**: "one binary boots on QEMU AND
  Hyper-V" is a concrete framework property; scheduler work is
  invisible until you measure throughput.
- It **crystallises interfaces** the scheduler then builds on
  (`Driver`, `ProbeCtx`, `NetDevice`). Without these nouns, every
  SMP discussion is hand-wavy about "the e1000 thing" vs "the
  tulip thing."
- **Low blast radius**: if the design is wrong we refactor a probe
  loop, not three months of per-CPU state migration.

The legitimate counterargument is "SMP changes the rules — drivers
need `Send + Sync`, IRQ allocation must be per-CPU, etc." That
risk is mitigated in [driver-model.md](./driver-model.md) by
**three SMP-forward design choices** (per-CPU vector allocator
shape, `Send + Sync` requirement on day one, `&'static dyn Driver`
registry) baked in from the start.

## What this doc is not

- **Not a roadmap.** No dates, no resourcing, no team assignments.
- **Not a design.** Each numbered gap will need its own design doc;
  several already exist (`driver-model.md`, `async-boot-init.md`,
  `hal-x86.md`).
- **Not a commitment.** Some Tier 3 items may never be needed if
  embclox stays narrow.

## Summary

embclox today is **about 25% of the way** to its stated target:

- ✅ Boot, paging, IDT/APIC/IOAPIC, MMIO, PCI scan
- ✅ Async runtime (block_on_hlt, embassy-net)
- ✅ Three NIC drivers + smoltcp TCP/IP
- ✅ Hyper-V VMBus + Azure deployment path
- ❌ Any storage support
- ❌ Multi-CPU scheduling
- ❌ Driver model (designed; not built)
- ❌ Fault containment beyond `panic = abort`
- ❌ Production-grade application API

Closing the driver-model and scheduler gaps (Tier 1, gaps #3 and
#2) would take embclox from "interesting prototype" to "minimum
viable ring-0 network framework." Storage is deliberately deferred
until a concrete storage workload is on the roadmap. Everything
else is iterative refinement once those foundations are in place.

The background research (Tock for no-alloc drivers, ArceOS for
modular composition, Moss for async-everywhere ergonomics, Theseus
for fault containment) gives us tested designs to reference for
each gap. The design work is largely about *which* of those patterns
to adopt for our specific positioning — embedded, ring 0, I/O-heavy
— rather than inventing from scratch.
