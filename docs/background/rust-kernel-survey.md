# Rust kernels: a curated survey

## Status: background research

A curated survey of notable Rust-based kernels on GitHub, ordered by
relevance to embclox's design questions: how to structure drivers,
where async fits, how to scale a single-image kernel without losing
clarity.

This doc complements [redox-drivers.md](./redox-drivers.md),
[redox-io-model.md](./redox-io-model.md), and
[moss-kernel.md](./moss-kernel.md), which deep-dive into three
specific kernels. The survey here is breadth-first: short
descriptions per project, opinionated relevance notes, no source
inspection.

Star counts are snapshots from when this doc was written and will
drift. Use them as rough popularity signals only.

## How this is organised

- **Most relevant to embclox** — kernels whose design choices speak
  directly to driver-model, async, single-image, or no-alloc
  questions we have considered.
- **Production / serious applications** — kernels actually used or
  targeting real deployment.
- **Tutorial / educational** — reference designs, not kernels per
  se but foundational for anyone building in this space.
- **Special-interest** — notable for niche reasons.

Each entry: name (★), one-line description, relevance note.

## Most relevant to embclox

### [theseus-os/Theseus](https://github.com/theseus-os/Theseus) — 3.2k★

Research kernel from Rice/Yale exploring **intralingual design** —
every kernel "module" is a Rust crate, hot-swappable at runtime via
the language's `dyn`/trait-object machinery. No traditional process
abstraction; uses single-address-space + language-enforced isolation.

*Relevance:* Strongest position on "use Rust types as the OS
abstraction." Highly relevant if embclox ever wants hot-reload or
fault containment without an MMU. Conceptually adjacent to
embclox's "everything is a Rust crate" structure but pushed to its
logical conclusion.

### [asterinas/asterinas](https://github.com/asterinas/asterinas) — 4.4k★

Production-targeting Linux-compatible kernel from a Chinese research
consortium. Novel **framekernel** architecture: a tiny `unsafe`
nucleus (~13 kLOC) provides safe abstractions (capabilities, frames)
on top of which the rest of the kernel runs as **safe-Rust services**.

*Relevance:* A third design point beyond monolithic vs microkernel.
Real production ambitions. Useful if we ever want to draw a sharp
"here is the unsafe nucleus, everything else is safe" line in
embclox.

### [arceos-org/arceos](https://github.com/arceos-org/arceos) — 765★

Modular OS framework from Tsinghua. Compose your kernel from feature
crates (`axhal`, `axalloc`, `axnet`, `axdriver`). The same component
set can build a unikernel, a monolithic kernel, **or a hypervisor** —
depending on which crates you select and link.

*Relevance:* Closest existing match to embclox's per-crate
philosophy. Directly addresses "kernel as Cargo workspace." Sister
projects: `starry-next` (monolithic on ArceOS),
`axvisor`/`arm_vcpu` (hypervisor on ArceOS),
`page_table_multiarch` (reusable page-table crate). The
component-reuse-across-OS-shapes idea is something embclox could
borrow at scale.

### [oxidecomputer/hubris](https://github.com/oxidecomputer/hubris) — 3.5k★

Oxide Computer's microkernel, **shipping in real hardware**. Deeply
embedded, message-passing, **no dynamic allocation in the kernel
ever**. All tasks declared statically at build time via `app.toml`.

*Relevance:* The most extreme answer to "what if everything is
declared statically?" Compelling foil for embclox's ad-hoc but
heap-using approach. Useful if we ever care about strict
determinism or running in environments where heap is forbidden.

### [nuta/kerla](https://github.com/nuta/kerla) — 3.5k★

Linux-ABI-compatible kernel; predecessor design idea to moss with
async-Rust syscall implementations on a monolithic core.

*Relevance:* Less actively developed than moss now but has wider
feature coverage (TCP/IP works, networking is up). Useful read if
comparing two takes on "Linux compat in async Rust."

### [maestro-os/maestro](https://github.com/maestro-os/maestro) — 3.2k★

Linux-compat kernel from scratch. Sync-style (no async kernel core),
aims for full Linux ABI in clean Rust. Long-running solo project
with surprisingly broad coverage.

*Relevance:* Counterpoint to moss for "do you really need
async-everywhere?" Demonstrates the alternative path stays viable.

## Production / serious applications

### [Andy-Python-Programmer/aero](https://github.com/Andy-Python-Programmer/aero) — 1.2k★

Modern x86_64 monolithic kernel. UEFI, 5-level paging, SMP, runs
DOOM. Closest in intent to "what if Linux were rebuilt clean in Rust
today" — minus moss's async core, plus more hardware coverage.

### [hermit-os/kernel](https://github.com/hermit-os/kernel) — 1.4k★ (with [hermit-rs](https://github.com/hermit-os/hermit-rs) — 1.9k★)

Rust unikernel for HPC + cloud. Library OS model — the application
links against the kernel, runs on KVM/Firecracker. Production users
in HPC.

*Relevance:* The "unikernel design point" — tighter than embclox
but similar single-image structure. Worth reading for how they do
single-address-space Rust at production scale.

### [tock/tock](https://github.com/tock/tock) — 6.3k★

Secure embedded OS for microcontrollers. Production-deployed (Google
Titan, OpenSK). Isolation via MPU, not MMU. Capsule architecture
for drivers — drivers are `'static` trait objects, not
`alloc`-allocated.

*Relevance:* **Very relevant to embclox's "no-alloc-friendly driver
model" question.** Tock has solved exactly this in production. Their
capsule + grant abstraction is unique and battle-tested.

### [redox-os/redox](https://github.com/redox-os/redox) — 16k★

Already covered in [redox-drivers.md](./redox-drivers.md) and
[redox-io-model.md](./redox-io-model.md). Microkernel, drivers as
userspace daemons, scheme abstraction. Most mature Rust-OS project
overall.

### [bottlerocket-os/bottlerocket](https://github.com/bottlerocket-os/bottlerocket) — 9.6k★

Container host OS from AWS. Linux kernel + Rust userland. **Not a
Rust kernel** — listed here only to note it shows up in searches
and to clarify it isn't relevant for kernel-design questions.

## Tutorial / educational

These are not kernels themselves but reference designs every Rust-OS
project ends up reading.

### [phil-opp/blog_os](https://github.com/phil-opp/blog_os) — 17k★

The canonical "writing an OS in Rust" tutorial, x86_64. Most Rust-OS
projects (including embclox in its early form) trace some lineage
here.

### [rust-embedded/rust-raspberrypi-OS-tutorials](https://github.com/rust-embedded/rust-raspberrypi-OS-tutorials) — 15k★

aarch64 embedded equivalent of blog_os. Same pedagogical depth, ARM
side.

### [rcore-os/rCore-Tutorial-v3](https://github.com/rcore-os/rCore-Tutorial-v3) — 2k★ (with [book](https://github.com/rcore-os/rCore-Tutorial-Book-v3) — 1.4k★)

RISC-V from-scratch tutorial. Parent project of ArceOS — many of
its modular ideas originated here.

### [chyyuu/os_kernel_lab](https://github.com/chyyuu/os_kernel_lab) — 4k★

OS kernel labs based on Rust + RISC-V/x86. Course material; widely
used in Chinese universities.

## Special-interest

### [obhq/obliteration](https://github.com/obhq/obliteration) — 791★

PS4 kernel reimplementation in Rust. Reverse-engineering project,
not directly applicable to embclox but interesting precedent for
"clone an existing complex kernel ABI in Rust."

### [nebulet/nebulet](https://github.com/nebulet/nebulet) — 2.3k★ (archived)

WebAssembly "userspace" running in Ring 0. Abandoned, but the design
idea — use language-VM isolation instead of MMU isolation — is
intellectually adjacent to Theseus's intralingual position.

## Recommendation: what to read next

If you can only deep-dive three of these for embclox's purposes:

1. **Tock** — the gold standard for "drivers without an allocator in
   production." Their capsule + grant abstractions are unique and
   battle-tested. Most directly applicable to embclox's `no_std`
   reality.

2. **ArceOS** — most directly addresses the "kernel as composable
   Cargo workspace" question that mirrors embclox's structure.
   Worth understanding their feature-crate decomposition before
   we add too many new component crates of our own.

3. **Hubris** — most extreme answer to "what if we statically
   declare everything?" Useful as a foil to consider what
   flexibility we *do* want — and a sanity check that the no-alloc
   kernel approach is viable in shipping hardware.

Theseus is the most intellectually interesting (intralingual design
is a genuinely novel position), but its ideas are research-grade
rather than directly applicable today.

## Bigger picture

The current Rust-kernel landscape spans roughly four design axes:

| Axis | One end | Other end | Examples |
|------|---------|-----------|----------|
| Topology | Microkernel | Monolithic | Redox / Hubris ↔ moss / Maestro / Aero |
| Concurrency | Sync (`WaitQueue`) | Async (`Future`) | Redox / Maestro ↔ moss / Theseus |
| Allocation | `alloc`-using | Strictly static | Most ↔ Hubris / Tock |
| Composition | Monolithic crate | Cargo workspace of crates | Older ↔ ArceOS / embclox |

embclox currently sits at: **monolithic, async, alloc-using,
workspace-of-crates** — the same quadrant as moss, with ArceOS as
the closest peer on the workspace-decomposition dimension.

Knowing where the field is helps decide which decisions are
"settled" (use Cargo workspaces; expose drivers as trait objects)
versus "open" (sync vs async core; alloc vs static; how strict to
be about isolation).
