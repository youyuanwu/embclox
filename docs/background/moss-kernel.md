# Moss kernel: drivers and async I/O model

## Status: background research

A study of [moss](https://github.com/hexagonal-sun/moss-kernel), an
experimental AArch64 monolithic kernel written in Rust that aims for
binary compatibility with Linux userspace (currently runs Arch Linux
ash, BusyBox, coreutils, strace).

Moss is **the inverse of Redox** in two key ways: it is monolithic
not microkernel, and it makes Rust `async`/`await` the **universal
concurrency primitive across the entire kernel** — every non-trivial
syscall is an `async fn`, every sleep point is `.await`. This makes
it the most relevant point of comparison for embclox, which also
uses async-Rust as its only concurrency primitive but inside a
single-CPU bare-metal context.

Companion to [redox-drivers.md](./redox-drivers.md) and
[redox-io-model.md](./redox-io-model.md). Read those first for
context on the microkernel-side of the design space.

## Sources

- moss README and `etc/syscalls_linux_aarch64.md`
- `hexagonal-sun/moss-kernel` repo, primary paths:
  `src/sched/{mod,waker,uspc_ret,sched_task}.rs`,
  `src/drivers/{init,mod}.rs`, `src/drivers/uart/{mod,pl011}.rs`,
  `src/process/sleep.rs`, `libkernel/src/sync/{spinlock,waker_set}.rs`

## TL;DR

Moss's design choices, point by point:

| Topic | Moss | embclox | Redox |
|-------|------|---------|-------|
| Kernel topology | Monolithic | Monolithic | Microkernel |
| Userspace ABI | Linux-compatible (105 syscalls) | None (kernel-only) | Custom (schemes via relibc) |
| Concurrency primitive | `Future` + `Waker` everywhere | `Future` + `Waker` everywhere | `WaitQueue<T>` + `context::switch` |
| Tasks are... | `Pin<Box<dyn Future>>` per process | One async task in `block_on_hlt` | Synchronous contexts |
| Scheduler | EEVDF, preemptive, SMP | None (single CPU) | Custom, preemptive |
| Spinlock-over-sleep prevention | Compile-time (`!Send` guards) | N/A (no scheduler) | Runtime (lock-tier system) |
| Driver discovery | FDT via `PlatformBus` + linker-section initcalls | Hardcoded in each example | `pcid` daemon + `config.toml` |
| Driver match table | `DeviceMatchType::FdtCompatible` strings | `vendor:device` literals | TOML files, runtime-loaded |

The closest sentence-summary: **Moss is what Linux would look like
if its core were rewritten in Rust with futures as the kernel
concurrency primitive instead of kthreads + wait_queue + scheduler
hooks.**

## High-level architecture

Moss is a full POSIX-style monolithic kernel:

```
Linux userspace ELF (Arch ash, BusyBox, …)
         │ aarch64 SVC
         ▼
┌─────────────────────────────────────────────┐
│ Exception vector → spawn_kernel_work(...)   │
│   ├─ scheduler picks a task                 │
│   ├─ polls task's `kern_work` future        │ ← async fn syscalls live here
│   ├─ polls task's `signal_work` future      │
│   └─ if Pending: sleep, pick another task   │
│   if Ready: restore user context, ERET      │
├─────────────────────────────────────────────┤
│ VFS / fs (ext4, fat32, tmpfs, devfs)        │
│ Drivers (uart, virtio, rtc, …)              │
│ Memory (CoW, slab, buddy, MMU mgmt)         │
│ Sched (EEVDF + per-CPU runqueues + IPI)     │
└─────────────────────────────────────────────┘
```

Module map:

| Path | Role |
|------|------|
| `src/main.rs` | Boot entry |
| `src/arch/` | AArch64-specific vectors, MMU, context switch |
| `src/sched/` | EEVDF scheduler, waker, syscall dispatcher |
| `src/process/` | `Task`, `ProcessGroup`, signals, ptrace, fork/exec |
| `src/drivers/` | Driver framework + concrete drivers |
| `src/fs/` | VFS, ext4, fat32, tmpfs, devfs, procfs |
| `src/interrupts/` | GIC, per-CPU IPI messaging |
| `src/net/` | Smoltcp-based stack (in progress) |
| `src/sync/` | Per-CPU statics |
| `libkernel/` | Architecturally-decoupled utilities, host-testable |

`libkernel` is host-testable (validated via 230+ tests running on
x86 against AArch64 page-table parsing logic, etc.) — same pattern
as embclox's `embclox-async` crate.

## The async-everywhere kernel core

The defining feature. Almost every kernel function that can sleep is
an `async fn`. Concretely:

```rust
// src/process/sleep.rs — Moss's entire nanosleep syscall
pub async fn sys_nanosleep(rqtp: TUA<TimeSpec>, rmtp: TUA<TimeSpec>) -> Result<usize> {
    let timespec: Duration = TimeSpec::copy_from_user(rqtp).await?.into();
    let started_at = now().unwrap();

    match sleep(timespec).interruptable().await {
        InterruptResult::Interrupted => {
            if !rmtp.is_null() {
                let elapsed = now().unwrap() - started_at;
                copy_to_user(rmtp, (timespec - elapsed).into()).await?;
            }
            Err(KernelError::Interrupted)
        }
        InterruptResult::Uninterrupted(()) => Ok(0),
    }
}
```

Three things to notice:

1. **`copy_from_user` is async.** Reading from userspace memory may
   fault and require swapping in a page; that's a sleep point, so
   the function returns a future.
2. **`.interruptable()` combinator.** Wraps any future to make it
   signal-aware — returns `InterruptResult::Interrupted` if a signal
   arrives during the wait, `Uninterrupted(value)` if the inner
   future completed normally. This solves the classic Linux problem
   of "sprinkle `signal_pending()` checks everywhere" in two lines.
3. **No mutex, no spinlock, no condition variable.** The whole
   syscall is just async function composition. Scheduling cooperation
   is implicit.

### Tasks ARE futures

The key implementation detail (`src/sched/sched_task/mod.rs` and
`src/sched/uspc_ret.rs`):

```rust
// each task carries a Pin<Box<dyn Future>> for its in-flight kernel work
ctx.task_mut().ctx.put_kernel_work(Box::pin(fut));

// when returning to userspace, dispatch_userspace_task polls it:
match kern_work.as_mut().poll(&mut Context::from_waker(&current_work_waker())) {
    Poll::Ready(()) => state = State::ProcessKernelWork,    // syscall done
    Poll::Pending  => {
        ctx.task_mut().ctx.put_kernel_work(kern_work);      // park future on task
        if try_sleep_current() {
            state = State::PickNewTask;                      // sleep, pick another
        } else {
            state = State::ProcessKernelWork;                // woken concurrently
        }
    }
}
```

Each task has both a userspace context and a kernel-side
`Pin<Box<dyn Future>>`. When a syscall starts, `spawn_kernel_work`
pushes the future onto the task. The scheduler-return-path polls
it. If `Pending`, the task sleeps; the scheduler picks another
runnable one. When the future's waker fires
(`insert_work_cross_cpu`), the task is re-enqueued, eventually
re-scheduled, and the future is polled again from where it left off.

This is the **embassy/Tokio "task-is-a-future" model applied at OS
kernel level**.

### The Waker

`src/sched/waker.rs` implements `RawWaker` over `Arc<Work>`:

```rust
unsafe fn wake_waker_no_consume(data: *const ()) {
    let data: *const Work = data.cast();
    Arc::increment_strong_count(data);
    let work = Arc::from_raw(data);
    match work.state.wake() {
        WakerAction::Enqueue => insert_work_cross_cpu(work),
        WakerAction::PreventedSleep | WakerAction::None => {}
    }
}
```

When any code calls `.wake()` on a task's waker, the task gets
re-inserted into a runqueue (possibly on a different CPU via IPI,
since SMP). The next scheduler tick picks it up.

The `WakerAction::PreventedSleep` enum variant is interesting: it
handles the race where a wake happens *between* "future returns
Pending" and "task transitions to PendingSleep" — the wake is
recorded, and the impending sleep is cancelled.

## Compile-time spinlock-over-sleep prevention

A famous class of kernel bugs in Linux is "sleeping with a spinlock
held" — it deadlocks because the holder may be migrated off-CPU
while another thread spins forever waiting. Linux relies on
runtime checks (`might_sleep()` macro + lockdep) to catch these.

Moss prevents them at **compile time** via Rust's `Send` rules.
From `libkernel/src/sync/spinlock.rs`:

```rust
pub struct SpinLockIrqGuard<'a, T: ?Sized + 'a, CPU: CpuOps> {
    lock: &'a SpinLockIrq<T, CPU>,
    irq_flags: usize,
    _marker: PhantomData<*const ()>, // !Send
}
```

The `_marker: PhantomData<*const ()>` makes the guard `!Send`. Now
consider trying to hold this guard across an `.await`:

```rust
let guard = some_spinlock.lock_save_irq();
some_future.await;            // ❌ compile error
guard.do_something();
```

The `await` may resume the future on a different CPU (the scheduler
is SMP, and tasks-as-futures may migrate). For the future state
machine to be valid across cores, every value live across the
`.await` must be `Send`. The guard is not. **Compile error.**

This is one of the cleanest "leverage Rust's type system to prevent
a kernel bug class" demonstrations in the wild. It works because
Rust async desugaring stores all live values across `.await` points
in the future struct, which must itself implement `Send` to be
passed to `insert_work_cross_cpu`.

## WakerSet: the universal "waiting room"

`libkernel/src/sync/waker_set.rs` provides the building block for
all driver-level async waits:

```rust
pub struct WakerSet<T = ()> {
    waiters: BTreeMap<u64, (Waker, T)>,
    next_id: u64,
}

impl WakerSet<T> {
    pub fn register_with_data(&mut self, waker: &Waker, data: T) -> u64 { ... }
    pub fn wake_one(&mut self) -> bool { ... }
    pub fn wake_all(&mut self) { ... }
    pub fn wake_if(&mut self, predicate: impl Fn(&T) -> bool) -> bool { ... }
}

// Generic "wait until the predicate matches" future
pub fn wait_until<C, T, F, G, R>(
    lock: Arc<SpinLockIrq<T, C>>,
    get_waker_set: G,                    // extract WakerSet from state
    predicate: F,                         // returns Some(R) when ready
) -> WaitUntil<C, T, F, G, R>
```

The pattern is:

```
Driver state:  Mutex<{ ring_buffer: Ring, wakers: WakerSet }>

Reader future: wait_until(state, |s| &mut s.wakers, |s| s.ring.try_pop())
                 ├─ locks state
                 ├─ if ring has data → Poll::Ready(byte)
                 └─ else register waker in s.wakers, return Poll::Pending

IRQ handler:   locks state
                 ├─ pushes received bytes into ring
                 └─ state.wakers.wake_one()
                       └─ task gets re-enqueued → scheduled → polled → reads byte
```

Critically, `WaitUntil` has a `Drop` impl that removes the waker
from the set if the future is dropped before completing — so dropping
an `.interruptable()` future on signal cancels the wait cleanly.
This is the futures equivalent of Redox's `WaitCondition::notify`
plus explicit cleanup in the `EINTR` path.

## Driver model

Linux-shaped, but more compact:

### Linker-section `initcalls`

`src/drivers/init.rs`:

```rust
#[macro_export]
macro_rules! kernel_driver {
    ($init_func:expr) => {
        paste::paste! {
            #[unsafe(no_mangle)]
            #[unsafe(link_section = ".driver_inits")]
            #[used(linker)]
            static [<DRIVER_INIT_ $init_func>]: $crate::drivers::init::InitFunc = $init_func;
        }
    };
}

pub unsafe fn run_initcalls() {
    extern "C" {
        static __driver_inits_start: u8;
        static __driver_inits_end: u8;
    }
    let mut current = &__driver_inits_start as *const _ as *const InitFunc;
    let end = &__driver_inits_end as *const _ as *const InitFunc;
    while current < end {
        (*current)(&mut PLATFORM_BUS.lock_save_irq(), &mut DM.lock_save_irq());
        current = current.add(1);
    }
}
```

Each driver does:

```rust
// src/drivers/uart/pl011.rs
pub fn pl011_init(bus: &mut PlatformBus, _dm: &mut DriverManager) -> Result<()> {
    bus.register_platform_driver(
        DeviceMatchType::FdtCompatible("arm,pl011"),
        Box::new(pl011_probe),
    );
    Ok(())
}

kernel_driver!(pl011_init);
```

This is a near-direct port of Linux's `module_init()` macro, which
similarly emits `static __initcall_<name>__init` into a linker
section walked at boot. Moss reuses Rust's `link_section` attribute
plus a `paste`-generated unique symbol.

### `PlatformBus` + FDT probing

`PlatformBus` keeps a `BTreeMap<DeviceMatchType, Vec<ProbeFn>>`. The
match key is a string from the FDT (Flattened Device Tree)
`compatible` property:

```rust
pub fn probe_device(&self, dm: &mut DriverManager, descr: DeviceDescriptor)
    -> Result<Option<Arc<dyn Driver>>>
{
    let matcher = match &descr {
        DeviceDescriptor::Fdt(node, _) => {
            node.compatible().and_then(|compats| {
                for compat in compats {
                    let match_type = DeviceMatchType::FdtCompatible(compat.ok()?);
                    if self.probers.contains_key(&match_type) {
                        return Some(match_type);
                    }
                }
                None
            })
        }
    };
    // try each registered probe_fn until one claims
    ...
}
```

Compared to Linux's `of_match_table` + `platform_driver_register`,
this is essentially the same pattern, simpler because there is only
one bus type today (FDT) — extending to PCI later just means adding
`DeviceDescriptor::Pci(...)` and `DeviceMatchType::PciId(v, d)`.

### Driver trait hierarchy

```rust
pub trait Driver: Send + Sync + Any {
    fn name(&self) -> &'static str;
    fn as_interrupt_manager(self: Arc<Self>) -> Option<Arc<InterruptManager>> { None }
    fn as_filesystem_driver(self: Arc<Self>) -> Option<Arc<dyn FilesystemDriver>> { None }
}

pub trait CharDriver: Send + Sync + 'static {
    fn get_device(&self, minor: u64) -> Option<Arc<dyn OpenableDevice>>;
}

pub trait OpenableDevice: Send + Sync {
    fn open(&self, args: OpenFlags) -> Result<Arc<OpenFile>>;
}
```

`Driver` is the base trait; `as_*` methods are downcasts (Linux
`container_of` equivalent). `CharDriver` registers under a major
number; `get_device(minor)` returns the openable device. Files
opened from `/dev/ttyS0` end up calling `UartCharDev::get_device(0)
→ UartInstance::open(...) → OpenFile { TtyAdapter }`.

## Anatomy of a syscall: read from a UART

End-to-end, with `read(0, buf, 1)` on `/dev/ttyS0`:

```
1. ELF userspace:    SVC #0 with x8=63 (read)
2. Exception vector: stash UserCtx; spawn_kernel_work(ctx, async { sys_read(...).await })
3. Scheduler:        dispatch_userspace_task polls kern_work
                        ├─ sys_read finds the OpenFile (TtyAdapter for ttyS0)
                        ├─ TtyAdapter::read → tty_buffer_read_async(buf).await
                        │     └─ wait_until(tty_state, |s| &mut s.input_wakers,
                        │                   |s| s.input.try_pop())
                        │           ├─ locks tty state
                        │           ├─ no data → register waker, return Pending
                        │           └─ Future::poll returns Pending
                        ├─ try_sleep_current() → state = PendingSleep
                        └─ schedule() picks another task; this one sleeps

(time passes)

4. UART hardware:    receive interrupt → GIC → exception vector
5. PL011 IRQ handler: drain_uart_rx(&mut buf) → for each byte: tty.push_byte(b)
                        └─ TtyInputHandler::push_byte
                             ├─ lock tty_state
                             ├─ s.input.push(byte)
                             └─ s.input_wakers.wake_one()
                                  └─ insert_work_cross_cpu(task)

6. Scheduler:        next dispatch_userspace_task picks the task
                        ├─ polls kern_work again
                        │     └─ wait_until re-checks predicate → s.input.try_pop()
                        │           returns Some(byte) → Poll::Ready(byte)
                        │     └─ TtyAdapter::read returns Ok(1)
                        ├─ kern_work returns Poll::Ready(()) → ReturnToUserspace
                        └─ restore_user_ctx(frame)
7. ERET to userspace: read returns 1, *buf = byte
```

Same shape as Redox's packet wait (both have ~6 wakeups), but
**every step inside the kernel is a future poll, not a context
switch**. The scheduler is the executor.

## Comparison to embclox

| Aspect | Moss | embclox |
|--------|------|---------|
| Async runtime | Custom executor in scheduler (`uspc_ret.rs`) | `block_on_hlt` (single-future) + embassy-net |
| Tasks | Many `Pin<Box<dyn Future>>` per process | One logical task running in `block_on_hlt` |
| Wakeup primitive | `WakerSet` + `wait_until` + `Waker` from `Arc<Work>` | `AtomicWaker` (embassy) + IRQ re-poll |
| Sleep primitive | Scheduler picks another task | `hlt` instruction |
| Driver registry | Linker-section `.driver_inits` walked at boot | None today; per-example main.rs |
| Driver match | FDT `compatible` strings | Hardcoded `vendor:device` constants |
| Spinlock-over-sleep prevention | `!Send` guard, compile-time | Same model would work — we use `embassy_sync::Mutex` (also `!Send`-friendly) |
| Sleep-over-IRQ-handler prevention | N/A — IRQ handlers are sync `fn handle_irq(&self, ...)` | N/A — same |
| Multi-core | Yes, EEVDF, IPI-based work migration | No (single CPU) |
| Linux ABI | Yes (105 syscalls) | No (kernel-only) |
| Userspace | Yes (Arch Linux ELFs) | No |

## Ideas worth borrowing

1. **Linker-section initcalls.** `kernel_driver!(my_init)` putting a
   static into `.driver_inits` and a tiny boot routine walking the
   range is the cleanest in-kernel "static plugin registry" pattern
   I've seen. It cleanly addresses the "where does the driver
   registry live" question from
   [../design/driver-model.md](../design/driver-model.md), and
   sidesteps the `inventory` crate's portability concerns. We could
   add this to embclox at any time.

2. **`!Send` lock guards as compile-time assert.** embclox already
   uses `embassy_sync::Mutex` whose guard is `!Send` — but this is
   accidental rather than deliberate. Documenting it (and maybe
   adding a doc-comment on the lock primitives) makes the
   compile-time guarantee discoverable.

3. **`.interruptable()` combinator pattern.** Wrapping any future
   to gain "may be cancelled by signal" semantics is generalisable
   to "may be cancelled by `embassy_time::Timer`" (i.e., a timeout
   combinator that doesn't allocate). embclox could publish a
   similar tiny utility.

4. **Generic `wait_until(state, get_waker_set, predicate)`.** A
   reusable "wait for a predicate over locked state" future. We
   currently re-implement this shape per driver (`wait_for_match` in
   synic, `WaitForPacket` in channel.rs, embassy-net's polling).
   A single generic version in `embclox-async` would dedup ~3 places.

5. **`libkernel`-style host-testable utility crate.** Moss runs 230+
   tests on x86 host against AArch64 logic (page tables, address
   types, ring buffers). embclox's `embclox-async` is the same idea
   at miniature scale; expanding it to other utility code (DMA
   helpers, MMIO regions) would let us catch more bugs without
   QEMU.

6. **EEVDF as default.** If we ever add multi-task scheduling, EEVDF
   is now Linux's default (since 6.6). Moss demonstrates it works
   well with futures-as-tasks at small scale, which is closer to
   what embclox would face than Linux's heavyweight kthread model.

## What does not apply

- **Multi-task scheduling.** embclox runs one logical task on one
  CPU; no scheduler exists, so all the EEVDF / per-CPU-runqueue /
  IPI infrastructure is moot.
- **Linux ABI compatibility.** We have no userspace and no plans to
  emulate one.
- **FDT-based device discovery.** Limine on x86 doesn't expose FDT;
  we'd use ACPI/MCFG/PCI scan instead. The pattern still applies —
  `DeviceDescriptor::Pci(bdf, ids)` is a natural extension.
- **Process memory + signals + ptrace.** No userspace, no signals,
  no need for the `signal_work` future on each task.

## Comparison summary: Moss / Redox / embclox

The three kernels span the design space of "where does Rust async
fit in an OS":

| | Userspace | Kernel | Concurrency primitive |
|--|-----------|--------|----------------------|
| Redox | Yes (POSIX-shaped) | Microkernel, no async | sync `WaitQueue<T>` + `context::switch`, drivers in userspace can use async |
| Moss | Yes (Linux-compatible) | Monolithic, async-everywhere | `Future` + `Waker` is THE concurrency primitive at all kernel layers |
| embclox | None (kernel-only) | Monolithic, single-task | `Future` + `Waker` because there is no scheduler |

Redox and embclox sit at opposite ends of "how much OS substrate is
there to support async?":

- Redox has a full microkernel + userspace; it doesn't need async in
  the kernel because the scheduler IS the concurrency model.
- embclox has neither a scheduler nor multiple address spaces; async
  is the only way to do anything concurrent.

Moss is the **interesting middle ground**: full monolithic kernel
infrastructure (scheduler, processes, MMU, signals, Linux ABI) AND
async-Rust as the kernel-internal concurrency primitive. The fact
that this combination works — and produces noticeably cleaner code
than C kernels do for the same operations — is strong evidence that
the embclox approach (use async at the only-task level) generalises
cleanly upward.

## Takeaway

Of the three kernels reviewed, **Moss is the most architecturally
similar to embclox in its async philosophy** — same `Future`/`Waker`
substrate, same compile-time `!Send` guard idea, same generic
`wait_until` pattern. The differences (multi-CPU scheduling, Linux
ABI, FDT discovery, processes) are orthogonal to the
async-everywhere choice and could in principle be added on top of
the existing embclox primitives without rethinking them.

The single most directly applicable pattern is the **linker-section
initcall driver registry** — it is small, portable, and slots
naturally into the proposed driver-model design without requiring
any of Moss's other infrastructure.
