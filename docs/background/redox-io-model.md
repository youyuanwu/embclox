# Redox OS: I/O model

## Status: background research

A study of how Redox OS handles I/O — the kernel-side syscall and
yield mechanism, the daemon-side scheme protocol, the IRQ pipeline,
and how all three combine to wait for a single network packet.

Companion to [redox-drivers.md](./redox-drivers.md). That doc covers
*who* the players are (pcid, e1000d, smolnetd); this one covers
*how they talk* and *how they sleep*.

## Sources

- Redox book sections: Communication, Scheme Operation, Event Scheme,
  An Example, Kernel
- `redox-os/kernel` repo, paths `src/event.rs`, `src/scheme/user.rs`,
  `src/scheme/irq.rs`, `src/scheme/event.rs`
- `redox-os/base` repo, `drivers/executor/src/lib.rs`,
  `drivers/net/e1000d/src/main.rs`

## TL;DR

Redox has three distinct "queue" mechanisms that work together:

| Layer | Shape | What it carries |
|-------|-------|-----------------|
| Userspace event scheme | epoll-like (level-triggered fd readiness) | "fd N is ready, go non-blocking-read it" |
| Kernel ↔ scheme daemon protocol (v2) | io_uring-like (SQE/CQE rings) | Forwarded syscalls (open/read/write/…) and their results |
| Driver-internal hardware queues | NVMe-style SQ/CQ | Per-command submission/completion to the device itself |

Application code uses POSIX `read`/`write` (via relibc). The kernel
internally translates those into SQEs, suspends the process, and
resumes it when the daemon posts a matching CQE.

There is **no user-facing io_uring API today**. Userspace
applications wait via blocking syscalls or `epoll`-style event scheme
multiplexing.

## Layer 1: the user-facing API (POSIX)

Application code calls relibc, which looks like glibc:

```c
int fd = open("/scheme/tcp/127.0.0.1/3000", O_RDWR);
char buf[1500];
ssize_t n = read(fd, buf, sizeof(buf));   // blocks until data arrives
```

For non-blocking + multi-fd waiting, use the event scheme:

```rust
// open scheme fd in non-blocking mode
let mut tcp = OpenOptions::new()
    .read(true).write(true)
    .custom_flags(O_NONBLOCK as i32)
    .open("/scheme/tcp/127.0.0.1/3000")?;

let event_file = File::open("/scheme/event")?;
event_file.write(&Event { id: tcp.as_raw_fd(), flags: EVENT_READ, ... })?;

// blocks here
let mut buf = [0u8; 64];
event_file.read(&mut buf)?;

// guaranteed not to block — event scheme said "ready"
let n = tcp.read(&mut data)?;
```

The event scheme is the **epoll equivalent**. There is no
`epoll_create`/`epoll_ctl`/`epoll_wait` triplet; instead, opening
`/scheme/event` *is* `epoll_create`, writing `Event` records *is*
`epoll_ctl(EPOLL_CTL_ADD)`, and reading the event fd *is*
`epoll_wait`. The book itself draws this parallel
(`event-scheme.html`).

## Layer 2: the kernel ↔ daemon protocol (SQE/CQE)

What happens *behind* a `read("/scheme/tcp/...")`?

The kernel doesn't have a TCP stack. It needs to forward the read to
`smolnetd`. The mechanism is the **scheme socket** (described in
`src/scheme/user.rs`) which since v2 has an io_uring-shaped wire
protocol:

```
Application                 Kernel                      Daemon (smolnetd)

read(tcp_fd, buf, 1500)
                            sys_read(tcp_fd, ...)
                            ├─ wraps args into Sqe
                            │  { opcode: Read, tag, args }
                            ├─ enqueues Sqe on
                            │  smolnetd's todo: WaitQueue<Sqe>
                            ├─ marks current
                            │  context as Blocked
                            ├─ event::trigger(scheme_fd, READ)
                            │  → wakes smolnetd's reader
                            └─ context::switch()  ← THE YIELD
                                                        read(scheme_socket)
                                                        ├─ pulls Sqe out of todo
                                                        ├─ does the work
                                                        │  (or queues the request
                                                        │   if no data yet)
                                                        └─ write(scheme_socket, &Cqe)
                            ┌─ ParsedCqe::parse_cqe
                            ├─ unblocks waiting context
                            │  via WaitQueue
                            ├─ delivers CQE result to
                            │  the syscall return path
                            └─ context::switch back
syscall returns n
buf now contains data
```

Concretely from `src/scheme/user.rs`:

```rust
fn call_inner(&self, fds, sqe, ..., token) -> Result<Response> {
    // disable preemption so the next two steps are atomic
    let mut preempt = PreemptGuard::new(&context::current(), token);

    // mark caller blocked
    context::current().write(token).block("UserInner::call");

    // record the pending state slot for this tag
    states[sqe.tag as usize] = State::Waiting { context, fds, ... };

    // enqueue the SQE on the daemon's todo queue
    self.todo.send(sqe, token);

    // wake any daemon thread reading the scheme socket
    event::trigger(self.root_id, self.scheme_id, EVENT_READ, token);

    loop {
        context::switch(token);    // ← yield to scheduler
        // when we resume here, a CQE arrived (or signal woke us)
        ...
    }
}
```

So the kernel's "I/O yield" is a literal `context::switch()` call
inside the syscall handler. The current task is marked Blocked, the
scheduler picks another runnable context, and the original caller
sleeps until either:

- the daemon posts a CQE matching its `tag` (normal case), or
- a signal (`SIGKILL`) interrupts the wait (returns `EINTR`).

The SQE/CQE format is defined in `syscall::schemev2::{Sqe, Cqe}` and
covers all the file ops a scheme might handle: `Read`, `Write`,
`Open`, `Close`, `Fstat`, `Mmap`, `SendFd`, `RecvFd`, `Fevent`, etc.

This is **io_uring-shaped — but for IPC, not user-facing async I/O**.
Applications never see SQE/CQE; the kernel synthesises them on the
caller's behalf. Userspace `read`/`write` remains synchronous.

### Why io_uring shape?

Two reasons that `read(scheme_fd, &Packet)` blocking-call style
(used in older Redox versions) is being phased out:

- Per-message round-trips are expensive when a daemon handles many
  concurrent requests. Batching SQE/CQE in rings lets a daemon read
  N submissions in one syscall and write N completions in one
  syscall.
- Out-of-order completion. With tags, the daemon can hold onto a
  half-finished request (waiting for hardware) while serving newer
  ones; the kernel correlates the eventual CQE back to the right
  blocked context via the tag.

## Layer 3: the IRQ pipeline (`irq:` scheme)

How does an actual hardware IRQ become a wakeup for `e1000d`?

`src/scheme/irq.rs` registers an `IrqScheme`. A driver opens
`/scheme/irq/<n>` to get an "IRQ handle" fd. When the kernel's
interrupt handler fires for vector `32 + n`, it calls:

```rust
// src/scheme/irq.rs
pub fn irq_trigger(irq: u8, token: &mut CleanLockToken) {
    COUNTS.lock()[irq as usize] += 1;
    let fds: SmallVec<...> = {
        // find every fd handle currently registered for this IRQ
        HANDLES.read(token.token()).iter()
            .filter_map(|(fd, h)| Some((fd, h.as_irq_handle()?)))
            .filter(|&(_, (_, hi))| hi == irq)
            .map(|(f, _)| *f).collect()
    };
    for fd in fds {
        event::trigger(GlobalSchemes::Irq.scheme_id(), fd, EVENT_READ, token);
    }
}
```

The IRQ ISR runs in kernel mode, increments a counter, and calls
`event::trigger()` which scans the registry of event-queue
subscriptions and wakes any context blocked on a queue that
subscribed to this fd's READ readiness. Per `src/event.rs`, the
event queue's `WaitQueue<Event>` then wakes the daemon's thread.

So the chain is:

```
NIC raises IRQ → x86 interrupt vector → kernel ISR → irq_trigger(n)
  → event::trigger(irq_scheme_id, fd, READ) → wakes smolnetd or e1000d
  thread blocked in read("/scheme/event")
```

The driver acks the IRQ by `write(irq_fd, &count)` — that tells the
kernel "I handled it, mask is no longer needed". This is also the
hand-off where the kernel can re-enable the line for legacy IRQs.

### Why fd-based IRQs?

Because every wait primitive in the system is the event scheme,
making IRQs file descriptors lets a driver use **one** event loop
that uniformly handles:

- the IRQ fd (hardware demands attention)
- the scheme fd (smolnetd is asking for a packet)
- a timer fd (timeout expired)
- another driver's scheme fd (chained operation)

Compare e1000d's main loop (`drivers/net/e1000d/src/main.rs`):

```rust
let event_queue = EventQueue::<Source>::new()?;
event_queue.subscribe(irq_file.as_raw_fd(), Source::Irq, READ)?;
event_queue.subscribe(scheme.event_handle().raw(), Source::Scheme, READ)?;

for event in event_queue {
    match event.user_data {
        Source::Irq    => { irq_file.read(&mut [0;8])?;
                            scheme.adapter().irq();           // process RX/TX rings
                            irq_file.write(&[0;8])?;
                            scheme.tick(); }
        Source::Scheme => scheme.tick(),                       // smolnetd asked for a frame
    }
}
```

One blocking read on the event queue, two unrelated wake sources,
zero polling.

## End-to-end: waiting for a TCP packet

Combining all three layers, here is what happens when an application
calls `read(tcp_fd, buf, 1500)` and the next inbound segment hasn't
arrived yet:

```
0. Application:    syscall read(tcp_fd) ────────────────────────┐
                                                                 │
1. Kernel:         build SQE { Read, tag=42 }                    │
                   smolnetd.todo.send(sqe)                       │
                   event::trigger(scheme_id, READ)               │
                   context::block("UserInner::call")             │
                   context::switch()    ← caller goes to sleep ──┘

2. Smolnetd:       read("/scheme/event") returns
                   → reads scheme socket → gets SQE 42
                   → smoltcp has no segment buffered for that socket
                   → does NOT send a CQE yet; records "tag 42 wants
                     bytes from socket S, deliver when ready"
                   → goes back to read("/scheme/event")  ←─ blocked

3. (time passes)

4. NIC:            inbound Ethernet frame arrives
                   → NIC raises IRQ vector 33

5. Kernel ISR:     irq_trigger(1)
                   → event::trigger(irq_scheme_id, e1000d_irq_fd, READ)
                   → wakes e1000d thread blocked on read("/scheme/event")

6. e1000d:         event_queue.next() yields Source::Irq
                   → adapter.irq()    (ack, drain RX ring)
                   → frame written to its NetworkScheme buffer
                   → fevent triggered on its scheme fd

7. Kernel:         the fevent in step 6 wakes smolnetd because
                   smolnetd subscribed to network.0000:00:03.0_e1000

8. Smolnetd:       event_queue.next() yields the network scheme fd
                   → reads the frame
                   → smoltcp processes Ethernet → IP → TCP
                   → finds buffered segment matches tag-42's socket
                   → sends CQE { tag=42, result: 60 bytes copied }

9. Kernel:         user.rs sees CQE 42 → looks up State::Waiting
                   → unblocks the original caller's context
                   → schedules it
                   → context::switch() back to it

10. Application:   syscall read returns with 60 bytes
```

Six wakeups, four address spaces touched, two scheduler invocations.
Redox accepts this cost for the isolation guarantees; on a
microbenchmark it's noticeably slower than Linux but it cannot
crash the kernel from the network stack.

## The driver async executor

For drivers that talk to **command-queue hardware** (NVMe, virtio,
xHCI), Redox provides `drivers/executor` — a single-threaded async
executor purpose-built for the pattern:

```rust
pub trait Hardware: Sized {
    type CmdId; type CqId; type SqId;
    type Sqe; type Cqe; type Iv;
    type GlobalCtxt;

    fn try_submit(ctx, sq, success, fail) -> Option<(CqId, CmdId)>;
    fn poll_cqes(ctx, handle: impl FnMut(CqId, Cqe));

    fn mask_vector(ctx, iv);
    fn unmask_vector(ctx, iv);
    ...
}

pub struct LocalExecutor<Hw: Hardware> {
    queue: RawEventQueue,
    irq_handle: File,
    awaiting_submission: HashMap<SqId, VecDeque<FutIdx>>,
    awaiting_completion: HashMap<CqId, HashMap<CmdId, (FutIdx, ...)>>,
    external_event:      HashMap<EventUserData, (FutIdx, ...)>,
    ...
}
```

A futures-based executor that combines:

- `await` on hardware command completion (`awaiting_completion`),
  unblocked by the IRQ handler that drains CQE rings.
- `await` on a submission queue slot when the SQ is full.
- `await` on any other event-scheme fd via `register_external_event`.

The same shape as `tokio` / embassy, but specifically scheduled
around device SQ/CQ rings. There is no separate reactor thread —
the executor *is* the reactor, polling CQEs every time any wakeup
occurs (`poll_cqes(ctx, |cq, cqe| ...)`).

This is the closest Redox gets to a "user-facing io_uring": it is an
IPC and hardware-scheduling pattern available **inside drivers**.
Application code in Redox has no io_uring equivalent today.

## Kernel "yield" — what does it actually do?

The phrase "kernel yield" maps to `context::switch(token)`. From the
read-syscall path's perspective:

1. Mark the current `Context` as `Blocked` (sets the Status field).
2. Push the request onto the daemon's `WaitQueue<Sqe>`.
3. `context::switch()` — the scheduler runs, picks any other Runnable
   context, and starts executing it via the architecture-specific
   `arch::switch_to(prev, next)`.
4. Eventually `wake_up(blocked_ctx)` flips the Status back to
   Runnable, and the scheduler picks it; control returns *inside*
   step 3 — `context::switch` returns to the original caller.
5. The caller checks the State slot for the matching tag, finds
   `State::Responded(...)`, and returns the result.

There is no preemption-driven I/O completion ("when the IRQ fires
mid-syscall, return immediately"). The blocking syscall blocks on a
scheduler primitive; nothing returns until the scheduler explicitly
switches the context back in.

This is a **classic cooperative-blocking design**: the scheduler is
preemptive (timer interrupts can switch contexts), but I/O yields
are explicit calls from the syscall implementation. There is no
async-syscall fast path that returns `EAGAIN`-then-completion; for
that, callers use the event scheme.

## Does the kernel use Rust async?

**No.** Verified by inspection — the `redox-os/kernel` source
contains zero matches for `async fn`, `.await`, `Future`,
`core::future`, `Waker`, or `core::task`. All blocking and waking
flows through the synchronous primitives in `src/sync/`:

```rust
// src/sync/wait_queue.rs
pub struct WaitQueue<T> {
    incoming: Mutex<L3, VecDeque<T>>,
    outgoing: Mutex<L2, VecDeque<T>>,
    pub condition: WaitCondition,
}

// src/sync/wait_condition.rs
pub struct WaitCondition {
    contexts: Mutex<L3, Vec<Weak<ContextLock>>>,   // blocked contexts
}
```

`context::switch` / `context::block` / `context::current` together
appear ~111 times across the kernel — they *are* the kernel's
universal yield primitive. The flow is the same as Linux/BSD:

1. Caller marks itself `Status::Blocked` (`src/context/context.rs:38`).
2. Pushes a `Weak<ContextLock>` onto a `WaitCondition`.
3. `context::switch()` — scheduler picks another runnable context.
4. Wakeup fires → `WaitCondition::notify` walks `contexts`, calls
   `unblock()` (sets `Status::Runnable`); scheduler eventually picks
   it again.

### Why no async?

Async/await is most useful when you want many concurrent activities
multiplexed onto one OS thread, with the runtime controlling polling.
The Redox kernel already has cheap concurrency via its own `Context`
abstraction, and **the scheduler is the executor**. A kernel `async
fn` would be redundant — every `.await` would translate to "block
this context and switch", which is what `context::switch()` already
does directly with no indirection.

The driver-side `drivers/executor` (userspace) **does** use
`Future`/`Waker` — but there it pays off because one userspace
process needs to multiplex many in-flight hardware commands without
spawning OS threads.

### Contrast with embclox

embclox uses `Future`/`Waker` extensively (`embassy-net`,
`block_on_hlt`, `wait_for_match`) precisely because it has **no
scheduler and no contexts** — there is one logical thread of control
on one CPU, and async-Rust gives us the only concurrency primitive
available in that environment.

The trade-off summary:

|  | Concurrency primitive | Why |
|--|----------------------|-----|
| Redox kernel | `Context` + `WaitQueue` + `context::switch` | Scheduler exists; async would be a wrapper around what's already there |
| Redox driver daemons (userspace) | `Future` + `LocalExecutor` (drivers/executor) | One process, many in-flight HW commands, no thread spawning |
| embclox | `Future` + `block_on_hlt` / `embassy_net` | No scheduler, no contexts — async is the *only* available concurrency primitive |

So: Redox doesn't need async in the kernel because it has a
scheduler; embclox needs async everywhere because it doesn't.

## Comparison: epoll, io_uring, kqueue, Redox

| System | Readiness API | Submission API | Hardware-IRQ → wake |
|--------|---------------|----------------|---------------------|
| Linux (classic) | `epoll_wait` | blocking `read`/`write` syscall | wakes context blocked on socket wait_queue |
| Linux (io_uring) | `io_uring_enter` cqe wait | shared SQ ring in mmap'd memory | NAPI softirq → completion in CQ |
| FreeBSD | `kqueue`/`kevent` | blocking syscall | similar to Linux |
| Redox | event scheme (epoll-shape) | blocking syscall (kernel-internal SQE/CQE) | `irq_trigger` → `event::trigger` → wake event queue reader |

Redox uses both epoll-shape (user-facing) **and** io_uring-shape
(kernel↔daemon-facing) patterns simultaneously. They serve different
purposes: epoll for user-controllable readiness multiplexing,
io_uring-style for batched IPC between the kernel and scheme daemons.

## Comparison to embclox

| Concern | Redox | embclox |
|---------|-------|---------|
| User/kernel boundary | Hard (ring 0 / ring 3, MMU-enforced) | None (everything ring 0) |
| Equivalent of "syscall" | `int 0x80` / `syscall` instruction | direct Rust function call |
| Equivalent of "epoll" | `/scheme/event` + `WaitQueue<Event>` | `embassy_net::Stack` polled in `run_executor` |
| Equivalent of "io_uring" | SQE/CQE in scheme socket | none — direct calls |
| Blocking I/O wait | `context::switch()` after `Status::Blocked` | `block_on_hlt` → `sti; hlt` |
| Yield primitive | `context::switch()` from syscall handler | `Future::poll → Pending; hlt` |
| Wake from IRQ | ISR → `event::trigger` → `WaitQueue::wake` → scheduler picks ctx | ISR → `Waker::wake_by_ref` → `block_on_hlt` re-polls |
| IRQ delivery | fd-based (`/scheme/irq/N`) | direct IDT entry → static `extern "x86-interrupt" fn` |
| Scheduler | Preemptive (timer-driven), per-context Status | Cooperative (one async task or `block_on_hlt`) |
| Multi-source wait | `EventQueue<Source>` with subscribed fds | `embassy::select!` macro across futures |

embclox's `block_on_hlt` is a degenerate single-context version of
Redox's `context::switch` from the I/O wait path:

- Redox: many runnable contexts, one yields, scheduler picks another
  runnable one; the IRQ handler enqueues a wake that eventually
  unblocks the original.
- embclox: one logical "thread" (the executor), it `hlt`s when no
  future is ready; any IRQ wakes the CPU and re-polls.

The async-Rust shape is the same on both (`Future::poll`,
`Waker::wake`); what differs is what happens between polls — Redox
runs other contexts, embclox just halts the one CPU.

## Ideas worth borrowing

1. **`(Irq, Request)` event-loop shape.** e1000d's match-arm pattern
   on a single event source is the cleanest way to express
   interrupt-driven driver loops; embclox already independently
   converged on the same shape with `NET_WAKER` + smoltcp polling
   via `embassy_net::Stack`.

2. **Tag-based request correlation.** Redox's SQE/CQE `tag` field
   handles out-of-order completion cleanly. If embclox ever needs
   multiple in-flight VMBus requests (currently sequential), the
   same tag pattern would generalise the `wait_for_match` futures
   beyond single-shot waits.

3. **Explicit Blocked status + wake.** Redox marks `Context::Status =
   Blocked` before yielding, then a sibling thread / IRQ flips it
   back to Runnable. Even in our single-context world this is what
   `Future::poll → Pending` semantically encodes; making the state
   transition explicit (a debug counter, perhaps) would aid
   debugging missed wakes.

## What does not apply

- **User-facing io_uring.** We have no userspace; the kernel↔daemon
  IPC ring is irrelevant.
- **`context::switch()` between userspace tasks.** No userspace.
- **fd-based IRQ delivery.** Without a syscall surface there is no
  fd to deliver to; embassy `Waker` + atomic pending bit is the
  direct equivalent.
- **The whole event-scheme namespace.** Without multiple address
  spaces, `embassy::select!` over a fixed set of futures covers
  every case `/scheme/event` does.

## Takeaway

Redox's I/O design is a layered application of the same async-wait
primitive — `WaitQueue<T>` + `context::switch` — at three levels:
between application and daemon (epoll-shaped event scheme), between
kernel and daemon (io_uring-shaped SQE/CQE protocol), and between
hardware and driver (executor + IRQ fd). The architecture is
elaborate because the microkernel demands it; the **core async-wait
primitive** is identical to what embclox uses with `block_on_hlt`
and `embassy_net`'s waker plumbing, just wrapped in many more layers
of process and fd indirection.

The takeaway for embclox: we already use the right primitive
(future-poll + park on IRQ). We don't need to grow the layers; we
need to be careful that — as drivers multiply — we keep the
`(IRQ, request)` event-loop shape uniform across drivers, the way
Redox does.
