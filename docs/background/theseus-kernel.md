# Theseus OS: intralingual single-address-space kernel

## Status: background research

A study of [Theseus](https://github.com/theseus-os/Theseus), an
academic Rust kernel from Rice/Yale that pursues an extreme
position: a single address space, a single privilege level, and
language-enforced isolation between **159 kernel crates** ("cells")
that can be hot-swapped at runtime via dynamic linking of `.o`
files.

Of all the kernels in the
[survey](./rust-kernel-survey.md), Theseus is the most
intellectually radical and the most aggressively decomposed. It
asks: **what if the unit of an OS module was a Rust crate, and the
kernel was its dynamic linker?**

Companion to [redox-drivers.md](./redox-drivers.md),
[redox-io-model.md](./redox-io-model.md),
[moss-kernel.md](./moss-kernel.md), and the broader
[rust-kernel-survey.md](./rust-kernel-survey.md). Read those for
context on conventional and async-everywhere designs respectively;
this doc covers the most unconventional design point.

## Sources

- The Theseus book: <https://theseus-os.github.io/Theseus/book/>
- Academic papers list: <https://theseus-os.github.io/Theseus/book/misc/papers_presentations.html>
  (notably the OSDI '20 paper "Theseus: an Experiment in Operating
  System Structure and State Management")
- `theseus-os/Theseus` repo, paths: `kernel/` (159 crates),
  `kernel/{nano_core,mod_mgmt,crate_swap,crate_metadata,task,scheduler,
  scheduler_round_robin,scheduler_epoch,scheduler_priority,
  context_switch_avx,fault_crate_swap,simd_personality}/`
- Rendered design pages:
  `book/design/{design,idea,source_code_organization,booting}.html`,
  `book/subsystems/task.html`

## TL;DR

Theseus's distinguishing choices, point by point:

| Topic | Theseus | Conventional |
|-------|---------|--------------|
| Address spaces | **One**, shared by everything | One per process |
| Privilege levels | **One** (Ring 0 only) | Two (kernel + user) |
| Isolation mechanism | Rust type system + safety + compiler | MMU + privilege rings |
| Module unit | A **Rust crate** (= one `.o` file) | A header file / kobject / .ko module |
| Module loader | `mod_mgmt` — full ELF dynamic linker in-kernel | linker at build time / `insmod` |
| Dependency tracking | **Per-section** via `.rela.*` relocations | Per-module via headers |
| Live evolution | Replace any cell at runtime via `crate_swap` | reboot |
| Fault recovery | Restart the failed cell, not the kernel | kernel panic = reboot |
| Schedulers | Three (round-robin, priority, epoch) — swappable | One, hard-wired |
| Userspace | Same SAS, just safe-Rust crates | Separate address space + ABI |

The single-sentence summary: **Theseus is the dynamic linker as the
kernel — every OS subsystem is a Rust crate that can be loaded,
linked, swapped, and unloaded at runtime, with isolation enforced by
the compiler rather than the MMU.**

## The PIE / PHIS principle

Theseus's foundational thesis (book section "Theseus Ideas and
Inspiration"):

> The hardware should only be responsible for **performance** and
> **efficiency**. **Isolation** should be the responsibility of
> software alone.

Abbreviated: **PHIS — Performance in Hardware, Isolation in
Software.**

This is a direct response to:
- Speculative-execution exploits (Meltdown, Spectre) showing that
  hardware isolation has been weaker than assumed.
- Privilege-mode and address-space switches being expensive on
  modern hardware.
- Rust offering compile-time guarantees that previously required
  managed runtimes (Java, .NET) with their attendant overhead.

The conclusion: drop hardware isolation entirely (no Ring 0 vs
Ring 3, no per-process page tables); rely on Rust safety + the
linker's per-section visibility to enforce isolation; keep the MMU
purely for memory **management** (allocation, fragmentation
avoidance), not protection.

## Cells = crates

The unit of OS modularity in Theseus is the **cell**, with a
deliberate biological analogy:

| Biological cell | Theseus cell |
|-----------------|---------------|
| Cell membrane | Public interface (Rust `pub` visibility) |
| Selective permeability | Symbol export visibility |
| Mitosis (split) | Live refactoring of one crate into multiple |
| Cell motility (replace) | Crate swap for fault recovery |

The cell takes three forms across the lifecycle:

1. **Implementation time** — a Rust **crate** with `Cargo.toml` +
   source.
2. **Build time** — compiled to a single ELF `.o` file (configured
   via `crate-type = ["rlib"]` plus per-cell linker flags so each
   crate stays a separate object).
3. **Runtime** — a `LoadedCrate` struct in `mod_mgmt`, owning the
   memory regions for its sections + metadata about cross-section
   dependencies.

This means **the kernel image is fundamentally a collection of
separately-compiled, separately-loadable object files** — not a
single linked vmlinux blob.

### Per-section dependency tracking

The novelty of `mod_mgmt` (`kernel/mod_mgmt/`) is that it tracks
dependencies at the **section** level (every individual function and
data item), not the crate level:

```rust
// kernel/crate_metadata
pub struct LoadedCrate {
    pub crate_name: StrRef,
    pub object_file: FileRef,
    pub sections: BTreeMap<Shndx, StrongSectionRef>,
    ...
}

pub struct LoadedSection {
    pub typ: SectionType,         // Text | Rodata | Data | Bss | ...
    pub mapped_pages: ...,         // owned MappedPages backing this section
    pub inner: Mutex<LoadedSectionInner>,
}

pub struct LoadedSectionInner {
    pub sections_i_depend_on:    Vec<StrongDependency>,    // outgoing
    pub sections_dependent_on_me: Vec<WeakDependent>,       // incoming
    pub internal_dependencies:   Vec<InternalDependency>,
}
```

When `mod_mgmt` loads a `.o` file, it walks the `.rela.*`
relocation entries and records that "section X in crate A depends
on symbol Y in crate B". This produces a precise, fine-grained
dependency graph — much sharper than Linux's per-`module` deps from
`MODULE_AUTHOR()` macros.

The key payoff: **swapping a crate at runtime is feasible** because
the loader knows exactly which other sections to re-link and can
prove (via the dep graph) that no live state is left dangling.

## CrateNamespace: multiple OS personalities

A `CrateNamespace` is a bag of `LoadedCrate`s linked against each
other. Multiple namespaces can coexist:

```rust
pub struct CrateNamespace {
    name: String,
    dir: NamespaceDir,            // backing folder of .o files
    crate_tree: Mutex<...>,       // loaded LoadedCrates
    symbol_map: Mutex<SymbolMap>, // exported symbol → section ref
    recursive_namespace: Option<Arc<CrateNamespace>>,  // fallback chain
}
```

Tasks can be spawned into specific namespaces. This effectively
gives Theseus **process-like isolation without processes**:

- Two tasks in different namespaces see different versions of
  shared crates (e.g., one running the SSE context-switch crate, one
  the AVX one — see `simd_personality`).
- A namespace can be torn down without affecting others.
- Symbol resolution falls back via `recursive_namespace` to a base,
  shared namespace.

This is the closest Theseus comes to a "process" abstraction: a
namespace is "the set of code my task can call." It's looser than a
Unix process (no memory protection between namespaces) but achieves
the same goal of "different applications see different OS variants
on the same machine" — at a fraction of the cost.

## The filesystem: special and central, but mostly swappable

The VFS in Theseus is **functionally central** to the whole
live-evolution story, because `mod_mgmt` loads cells by reading
`.o` files **from the VFS**. So the FS has to exist before
`mod_mgmt` can do anything. This creates a chicken-and-egg: the FS
implementation crates (`fs_node`, `vfs_node`, `memfs`, `root`)
themselves must be statically linked into the irreducible
`nano_core`. Everything *else* in Theseus boots via "load this
crate from a `.o` file in the VFS"; the FS itself does not.

The other special property is the root directory:

```rust
// kernel/root/src/lib.rs
lazy_static! {
    pub static ref ROOT: (String, DirRef) = {
        let root_dir = RootDirectory { children: BTreeMap::new() };
        (ROOT_DIRECTORY_NAME.to_string(), Arc::new(Mutex::new(root_dir)) as DirRef)
    };
}

pub fn get_root() -> &'static DirRef { &ROOT.1 }
```

`ROOT` is a `lazy_static` `Arc` referenced by everything that uses
paths. **You cannot swap the `root` crate** because doing so would
invalidate every `DirRef` (`Arc<Mutex<dyn Directory>>`) throughout
the system that points into it. `RootDirectory::remove` even
explicitly refuses:

```rust
fn remove(&mut self, node: &FileOrDir) -> Option<FileOrDir> {
    if let FileOrDir::Dir(dir) = node {
        if Arc::ptr_eq(dir, get_root()) {
            error!("Ignoring attempt to remove the root directory");
            return None;
        }
    }
    ...
}
```

Otherwise, FS components are as swappable as any other cell:

| Component | Swappable? | Why |
|-----------|-----------|-----|
| `root` crate | **No** | Singleton `Arc` referenced everywhere |
| `fs_node` (trait crate) | Effectively no | Trait definitions; swapping orphans all `dyn Directory` implementors |
| `memfs` (in-memory FS impl) | Yes | Just a regular cell |
| `vfs_node` (default node impl) | Yes | Implementation crate, not a singleton |
| `task_fs` (synthetic FS exposing tasks) | Yes | Built on top of the above |
| Files / directories | Yes | That's just `Directory::insert/remove` |
| Subdirectory contents | Yes | Can be remounted by inserting a new `DirRef` |

### `CrateNamespace` = directory in the VFS

The very interesting bit: each `CrateNamespace` is **backed by its
own directory in the VFS** (`NamespaceDir` in
`mod_mgmt/src/lib.rs`). Conceptually:

```
/                       (the singleton root from `root` crate)
├── namespaces/
│   ├── _applications/  (NamespaceDir for app cells)
│   │   ├── shell.o
│   │   ├── ls.o
│   │   └── ...
│   ├── _kernel/        (NamespaceDir for standard kernel cells)
│   │   ├── memory.o
│   │   ├── scheduler_round_robin.o
│   │   └── ...159 more...
│   └── _simd/          (NamespaceDir for the SIMD personality)
│       └── context_switch_avx.o
├── tasks/              (synthetic — task_fs)
└── ...
```

So **adding a new `CrateNamespace` is essentially `mkdir` +
populating it with `.o` files**. This is how `simd_personality`
works — it's a separate directory whose `context_switch.o` resolves
to the AVX-saving variant instead of the regular one.

The corollary: if you wanted to ship a "patched" namespace that
overrides one cell with a fixed version, you'd literally do it as
`mkdir /namespaces/_patched/ && cp fixed_driver.o /namespaces/_patched/`,
then spawn affected tasks into the new namespace. Live patching is
filesystem-rooted by construction.

### Comparison to other kernels

| Aspect | Theseus FS | Linux FS | Redox FS |
|--------|-----------|----------|----------|
| Privileged? | No special privilege (no rings) | Yes (kernel-mode) | No (`redoxfs` is a userspace daemon) |
| Pluggable backend | Yes (`memfs`, `task_fs`, `heapfile`; FAT/ext4 not yet ported) | VFS dispatch table | Daemon implements `file:` scheme |
| Root singleton | Yes (`lazy_static! ROOT`) | Yes (`init_task->fs->root`) | No (root is scheme registration) |
| FS used to load drivers? | **Yes** (cells = `.o` files in FS) | Yes (kernel modules from disk) | Yes (driver binaries from disk) |

Theseus's FS is **not semantically privileged** — it's just data
structures in the shared address space — but it is **functionally
central** because the entire dynamic-loading story flows through
it. The result is a nice symmetry: if cells are the unit of code
modularity, the VFS is the namespace those units live in.

## Boot flow

Despite the radical structure, the boot is staged:

```
GRUB/Limine
   ▼
nano_core  (~minimum static kernel — only this is fully linked at
            build time as a `staticlib` .a)
            │  initialises:
            │   - early_printer, logger, exceptions_early
            │   - serial_port_basic
            │   - memory_initialization (paging, frame allocator)
            │   - early_tls (per-CPU TLS slot)
            │   - mod_mgmt (the dynamic linker)
            │  loads from boot modules:
            │   - all the .o files for the rest of the kernel
            │  hands off to:
            ▼
captain (orchestrator)
            │  calls into the loaded crates to:
            │   - init APIC, IO-APIC, IDT
            │   - bring up other CPUs (multicore_bringup, ap_start)
            │   - init scheduler
            │   - spawn first_application
            ▼
running multitasking system
```

The `nano_core` is intentionally minimal (~the smallest set of
crates the bootloader can jump to and load the rest from). Its
`Cargo.toml` declares only ~15 dependencies; everything else
materialises as `mod_mgmt` walks the boot modules CPIO archive.

## The 159 kernel crates

Selected highlights from `kernel/`:

| Category | Crates |
|----------|--------|
| Boot | `nano_core`, `captain`, `boot_info`, `multicore_bringup`, `ap_start`, `bootloader_modules` |
| Memory | `memory`, `frame_allocator`, `page_allocator`, `page_table_entry`, `pte_flags`, `heap`, `multiple_heaps`, `slabmalloc`, `slabmalloc_safe`, `slabmalloc_unsafe`, `block_allocator` |
| CPU | `cpu`, `cls`, `cls_allocator`, `gdt`, `tss`, `apic`, `ioapic`, `pic`, `tlb_shootdown` |
| Interrupts | `interrupts`, `exceptions_early`, `exceptions_full`, `interrupt_controller`, `gic` |
| Tasking | `task`, `task_struct`, `spawn`, `scheduler`, `scheduler_round_robin`, `scheduler_priority`, `scheduler_epoch`, `preemption`, `idle` |
| Context switch | `context_switch`, `context_switch_regular`, `context_switch_sse`, `context_switch_avx`, `single_simd_task_optimization`, `simd_personality` |
| Sync | `wait_queue`, `wait_condition`, `wait_guard`, `sync_block`, `sync_channel`, `sync_preemption`, `unified_channel`, `simple_ipc`, `rendezvous`, `waker`, `waker_generic` |
| Storage / FS | `fs_node`, `vfs_node`, `memfs`, `task_fs`, `path`, `storage_device`, `storage_manager`, `block_cache`, `ata` |
| Net | `net`, `nic_buffers`, `nic_initialization`, `nic_queues`, `physical_nic`, `virtual_nic`, `intel_ethernet`, `mlx_ethernet`, `e1000`, `ixgbe`, `mlx5`, `http_client`, `ota_update_client` |
| Async runtime | `dreadnought` (async executor), `sleep`, `waker`, `waker_generic` |
| Display | `framebuffer`, `framebuffer_compositor`, `framebuffer_drawer`, `framebuffer_printer`, `compositor`, `displayable`, `font`, `shapes`, `color`, `vga_buffer`, `window`, `window_inner`, `window_manager`, `console`, `tty`, `text_terminal`, `libterm` |
| Input | `keyboard`, `mouse`, `ps2` |
| Loader / live evolution | `mod_mgmt`, `crate_metadata`, `crate_metadata_serde`, `crate_name_utils`, `crate_swap`, `fault_crate_swap`, `local_storage_initializer` |
| Fault tolerance | `catch_unwind`, `unwind`, `external_unwind_info`, `panic_entry`, `panic_wrapper`, `stack_trace`, `stack_trace_frame_pointers`, `fault_log`, `signal_handler` |
| WASI | `wasi_interpreter` |
| Other | `acpi`, `pci`, `iommu`, `device_manager`, `random`, `rtc`, `time`, `tsc`, `pit_clock`, `pit_clock_basic`, `pmu_x86`, `app_io`, `environment`, `state_store`, `kernel_config`, `arm_boards`, `serial_port`, `serial_port_basic`, `uart_pl011`, `early_printer`, `early_tls`, `event_types`, `io`, `logger`, `no_drop`, `page_attribute_table`, `panic_entry`, `root`, `simd_test`, `stack`, `sync_block`, `test_thread_local`, `thread_local_macro`, `libtest`, `heapfile`, `debug_info`, `deferred_interrupt_tasks`, `first_application`, `generic_timer_aarch64`, `memory_aarch64`, `memory_x86_64`, `memory_structs` |

A few patterns to notice:

- **Three swappable schedulers** (`scheduler_round_robin`,
  `scheduler_priority`, `scheduler_epoch`) all behind the same
  `scheduler` trait crate. You select one at boot, or even later
  via `crate_swap`.
- **Three context-switch impls** (`context_switch_{regular,sse,avx}`)
  — gives `simd_personality` the ability to keep most tasks on the
  cheap regular context switch while letting tasks that opt in to
  SIMD pay the AVX-save cost only when needed.
- **Three slab allocators** (`slabmalloc_safe`,
  `slabmalloc_unsafe`, plus the wrapper `slabmalloc`) — to compare
  performance against memory-safety overhead.
- **Multiple heaps** (`heap`, `multiple_heaps`) — for per-CPU and
  per-namespace allocation.
- **`fault_crate_swap`** — automatically replace a crate that
  panicked with a known-good version, then re-run the failed task.

### Drivers and filesystems: an intentional asymmetry

What hardware does Theseus actually drive, and what filesystems can
it mount?

**Network — well-developed.** Three NIC drivers, all hardware-real:

| Driver | Hardware | Notes |
|--------|----------|-------|
| `e1000` | Intel 825xx Gigabit (QEMU default) | Standard hobbyist starter NIC |
| `ixgbe` | Intel 10 Gigabit (82599-class) | Server-class NIC |
| `mlx5` | **Mellanox ConnectX-5** | Enterprise-grade 25/40/100 GbE — surprising for a research OS |

The TCP/IP stack is **smoltcp 0.10** wrapped by the `net` crate —
the same library embclox uses via `embassy-net`. Above the stack,
`http_client` and `ota_update_client` ride on it.

**Disk — rudimentary.** One driver:

| Driver | Hardware | Notes |
|--------|----------|-------|
| `ata` | IDE/PATA via I/O ports 0x1F0/0x170 | Legacy PIO; no DMA, no AHCI/NVMe/virtio-blk |

**On-disk filesystems — none.** All four FS implementations are
in-memory:

| FS | Backed by | Use |
|----|-----------|-----|
| `memfs` | Heap (`Vec<u8>`) | General RAM-backed files |
| `vfs_node` | Heap | Default node implementation |
| `task_fs` | Live `Task` structs | Synthetic — `/tasks/<id>/...` exposes task metadata |
| `heapfile` | Heap | Special-purpose heap-backed file |

Verified: zero hits for `fat32`, `ext2`, `ext4`, `iso9660` in any
`Cargo.toml` across the repo.

**Why the asymmetry?** It tracks the research thesis precisely. The
showcase application of Theseus is **OTA update via cell swap**:
fetch a new `.o` over the network, hand it to `crate_swap`, replace
the running cell. So:

- **Network is mandatory** → 3 NIC drivers + smoltcp + HTTP +
  `ota_update_client`.
- **Disk is incidental** → ATA exists for completeness but isn't
  on the critical path. Cells are loaded from multiboot modules
  (GRUB/Limine puts them in RAM at boot), and runtime updates come
  over the network rather than from disk.
- **No real on-disk FS** because persistent storage isn't needed
  to demonstrate the live-evolution thesis.

The driver portfolio is the clearest signal of what kind of project
Theseus actually is: a vehicle for research on intralingual design
and live evolution, not a general-purpose OS chasing hardware
breadth. The code that exists is the code the thesis demands.

## Tasks: green threads + native threads

From `book/subsystems/task.html`:

> Tasks in Theseus are effectively a combination of the concept of
> language-level green threads and OS-level native threads.

Because there is no separate process abstraction (there's nothing
separate to switch to), a "task" is simply a unit of execution
scheduled onto a CPU. Tasks can be spawned with closures (just like
`std::thread::spawn`) but they run in kernel mode in the global
address space.

The `Task` struct is **deliberately minimal** — scheduler-related
state lives in the scheduler crate, not in `Task`. This is the
state-spilling philosophy from the OSDI '20 paper: keep state with
the subsystem that operates on it, not in monolithic god-structs.

Compare to Linux's `task_struct` (~10 KB, hundreds of fields, every
subsystem stuffs its bookkeeping in there) — Theseus's `Task` is
tiny because every subsystem keeps its own per-task data in its own
crate.

```rust
// kernel/task — abridged
pub struct Task {
    pub id: usize,
    pub name: String,
    pub runstate: AtomicCell<RunState>,    // {Initing, Runnable, Blocked, Exited, Reaped}
    pub running_on_cpu: AtomicCell<...>,
    pub inner: IrqSafeMutex<TaskInner>,    // mutable parts behind a lock
    pub namespace: Arc<CrateNamespace>,    // which OS variant this task sees
    pub app_crate: Option<...>,            // if this is an app, the loaded crate
    ...
}

pub struct TaskInner {
    pub saved_sp: VirtualAddress,          // where to resume on next switch
    pub kstack: Stack,
    pub env: Arc<Mutex<Environment>>,
    pub task_local_data: TaskLocalData,
    ...
}
```

The `TaskRef` newtype (= `Arc<Task>` with restricted public API) is
threaded throughout the system — it's how foreign crates manipulate
tasks without seeing the raw `Task` fields directly.

## Live evolution and fault recovery

The unique-to-Theseus capability: replace a crate at runtime.

`crate_swap` (`kernel/crate_swap/`) does the heavy lifting:

1. Load the new `.o` file into a temporary `CrateNamespace`.
2. **Copy `.data` and `.bss` sections** from old to new crate (the
   cheap form of state migration — works only if layouts match).
3. Find every section in the existing crate, look up its dependents
   from the per-section dep graph.
4. **Re-link every dependent** to the corresponding section in the
   new crate (atomically, with TLB shootdowns coordinated by
   `tlb_shootdown`).
5. **Optionally invoke caller-provided `StateTransferFunction`s**
   that read state from the old namespace and populate the new one
   (for incompatible swaps where step 2's byte-copy isn't enough).
6. Mark the old crate's memory free.

The split between step 2 (automatic) and step 5 (manual) is the
honest tradeoff Theseus makes: the **linker mechanics are solid**,
but **semantic correctness of any given swap is the caller's
responsibility**. The crate's docstring is candid about this:

> # Warning: Correctness not guaranteed
> This function currently makes no attempt to guarantee correct
> operation after a crate is swapped. ... It will most likely error
> out, but this responsibility is currently left to the caller.

So a "compatible" swap (bug-fix release with same layout) is a
one-liner; an "incompatible" swap (round-robin → priority scheduler)
requires a migration function that knows how to rewrite the runqueue.

### Who actually triggers a swap?

Three callers exist (`grep`-verified across the repo); there is
**no autonomous in-kernel policy** that swaps things on its own:

| Caller | Trigger | Use case |
|--------|---------|----------|
| `applications/swap` | User types it at the shell | Interactive live evolution |
| `applications/upd` | User runs `upd apply` | OTA update from remote server |
| `kernel/fault_crate_swap` | A task panics, unwinder fires | Reactive panic recovery |

**`swap`** is a regular Theseus application (lives in
`applications/`). Its CLI:

```
swap (OLD1, NEW1 [, true | false]) [(OLD2, NEW2 [, true | false])]...
        -t scheduler::migrate_runqueue
```

Parses tuples, looks up the new `.o` file in the namespace
directory, builds a `SwapRequestList`, and calls
`crate_swap::swap_crates(...)`. To switch schedulers at runtime you
would literally type:

```
swap (scheduler_round_robin, scheduler_priority) \
     -t scheduler_priority::migrate_from_round_robin
```

**`upd`** is a second application implementing the OTA workflow:

| Subcommand | Action |
|------------|--------|
| `upd list` / `list-diff` | Talk to update server, see what builds are available |
| `upd download <build>` | Fetch changed `.o` files into a new namespace dir |
| `upd apply <base_dir>` | Read the diff manifest, build `SwapRequestList`, call `swap_crates` |

This is the OTA path the README markets as the showcase application
of Theseus — fetch updates from a remote server over the network
(via `e1000`/`ixgbe`/`mlx5` + smoltcp + `http_client` +
`ota_update_client`), then apply them to the running kernel.

**`fault_crate_swap`** is the only non-interactive caller. The
unwinder (`unwind` + `catch_unwind` + `panic_wrapper`) catches a
panic, identifies which crate caused it, calls `swap_crates` to
install a known-good replacement, and re-runs the failed task.
**The kernel never crashes** — only individual cells do, and they
get rebuilt in place.

### Why this organisation matters

The fact that `swap` and `upd` are **regular applications calling a
kernel API directly** is only possible because Theseus has no
kernel/userspace divide. In Linux equivalent terms, it's as if
`apt upgrade` could directly patch `vmlinux` while the kernel was
running. Same property that makes `simd_personality` possible: an
"application" is just code in the same address space that can
invoke any public function in any cell.

This is the cleanest in-the-wild realisation of "kernel as a system
of replaceable parts" — Linux can do this only via kdump + reboot;
microkernels can restart user-space servers but not kernel
components; Theseus restarts kernel components themselves, on a
trigger from a regular shell command.

## Comparison to embclox and the others

| Aspect | Theseus | embclox | Moss | Redox |
|--------|---------|---------|------|-------|
| Address spaces | 1 (SAS+SPL) | 1 (no userspace) | many (per-process) | many (per-process) |
| Privilege rings | 1 (Ring 0 only) | 1 (Ring 0 only, no userspace) | 2 (kernel + user) | 2 (kernel + user) |
| Isolation source | Rust types + linker visibility | Rust types only | MMU + Rust types | MMU + Rust types |
| Crate count in kernel | **159** | 7 | ~50 (rough est.) | similar (microkernel) |
| Static or dynamic linking | **Dynamic** (in-kernel ELF loader) | Static | Static | Static |
| Live cell swap | Yes (`crate_swap`) | No | No | Restart daemons |
| Fault recovery granularity | Single cell | Reboot | Single process | Single daemon |
| User-facing API | Theseus libtheseus + WASI | None | Linux ABI | POSIX-via-relibc |

embclox shares the **single-address-space, single-privilege-level**
property with Theseus — simply by virtue of being kernel-only. But
embclox has **none** of Theseus's dynamic loading or live-evolution
machinery; our 7 crates link statically into one Limine kernel
image.

## Ideas worth borrowing

The most likely-applicable Theseus patterns for embclox:

1. **Per-section dependency tracking via `.rela.*` parsing.** Even
   without dynamic loading, an offline pass that builds the
   per-symbol dep graph for the kernel image would let us flag
   "this driver crate accidentally pulls in `alloc`" or "removing
   crate X would break Y, Z". A debug-only auditing tool, not a
   loader.

2. **Per-subsystem state, not god-structs.** `Task` is tiny because
   schedulers, fs, etc. each keep their own per-task data. embclox
   has no `Task` yet, but if we ever add one (e.g., for multi-task
   examples), the discipline is "each crate owns its own per-task
   data, don't centralise."

3. **Multiple swappable schedulers behind one trait.** Even without
   runtime swap, this is the cleanest pattern for letting future
   examples choose between cooperative and preemptive scheduling
   without forking embclox itself.

4. **The `simd_personality` namespace pattern.** Apply only the
   expensive feature to tasks that opt in, by routing them through
   a different `CrateNamespace`. embclox has no equivalent today,
   but if we ever add SIMD or expensive vector contexts, the
   per-task-namespace-of-context-switch idea is directly portable.

5. **`catch_unwind` + auto-restart.** Theseus's per-task panic
   catching (vs. Linux's "panic = reboot") is achievable in any
   `panic = unwind` Rust kernel. We currently `panic = abort`; the
   Theseus model is a real alternative if we want long-running
   embclox demos to survive driver bugs.

6. **`nano_core` as the irreducible static minimum.** Even if we
   never ship dynamic loading, the discipline of "what is the
   absolute minimum that must be statically linked" is good kernel
   hygiene. embclox today statically links everything; an exercise
   to identify what could in principle be deferred might surface
   useful boundaries.

## What does not apply

- **Dynamic ELF loading at runtime.** Requires `mod_mgmt` (~5 kLoC
  of dynamic linker), an in-kernel filesystem to find `.o` files,
  TLB-shootdown coordination across CPUs. Pure complexity, no
  benefit at embclox's scale.

- **Live crate swap.** Requires both the loader above AND careful
  state migration (any data owned by the old crate must be
  forwarded to the new one). This is the kind of feature that pays
  off only if you genuinely need 5-nines uptime.

- **Multiple schedulers / `simd_personality` style multi-namespace.**
  embclox has no scheduler at all today.

- **Userspace via `libtheseus` / WASI.** No userspace plans.

- **PIE / PHIS as a user-facing claim.** We have no userspace, so
  "Isolation in Software via Rust" is automatically true and
  trivially uninteresting. Theseus's claim is significant because
  it applies to **applications**; ours is significant only to
  driver-vs-driver isolation, which matters less at our scale.

## Takeaway

Theseus is **the most architecturally radical Rust kernel in the
field**, and reading it is the best way to understand the upper
bound of what "use the type system as the OS" can achieve. The
biological-cell metaphor is not just rhetorical; it captures real
properties (per-cell membranes = symbol visibility; cell motility =
runtime swap; mitosis = crate refactoring).

For embclox, the directly applicable inheritance is small but real:
**state-locality discipline (each crate owns its own per-task data)
+ per-section dep awareness (even just as an offline auditing tool)
+ panic-isolation-via-unwinding (if we ever want long-running
demos)**. The rest — dynamic loading, multi-namespace personalities,
live evolution — is research-grade machinery whose payoff requires
a vastly more ambitious project than embclox.

The most important takeaway is **negative**: Theseus demonstrates
that the "single address space + safe Rust" design is internally
coherent and has shipped working code with novel capabilities (live
swap of running drivers!). embclox already lives in the same
quadrant of the design space — single AS, single PL, safe-Rust
modules — but with seven crates and static linking. The path from
embclox's current shape to a Theseus-like target is **gradual
addition of dependency-graph machinery**, not a paradigm shift.
Theseus tells us where the road leads if we keep walking it.
