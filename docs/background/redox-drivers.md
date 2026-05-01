# Redox OS: drivers and devices

## Status: background research

A study of how [Redox OS](https://gitlab.redox-os.org/redox-os) — a
microkernel written in Rust — manages drivers and device discovery.
Written to inform embclox's own driver-model design (see
[../design/driver-model.md](../design/driver-model.md)) by examining a
mature Rust OS that has solved many of the same problems.

This is not a proposal. It is a reference for "what does the field
look like" so design decisions can be made with eyes open.

## Sources

- [Redox book — Drivers](https://doc.redox-os.org/book/drivers.html)
- [Redox book — Schemes](https://doc.redox-os.org/book/schemes.html)
- `redox-os/base` repo, paths `drivers/pcid/`, `drivers/pcid-spawner/`,
  `drivers/net/e1000d/`, `drivers/net/driver-network/`
- Redox book sections on system design, kernel, user space

Code references in this document are paths within
`gitlab.redox-os.org/redox-os/base` unless noted.

## High-level architecture

Redox is a **microkernel** OS. The kernel provides only:

- Memory management and paging
- Process/thread scheduling
- A scheme registration syscall surface
- A handful of "kernel schemes" for things that genuinely cannot be
  pushed out (`memory:physical`, `irq:`, `event:`)

Everything else — including all device drivers, the filesystem, the
network stack, even the framebuffer — runs as **userspace daemons**.

A driver crashing therefore takes down one process, not the kernel.
The trade-off is per-IPC-hop overhead on every device interaction
(a packet flowing in/out of `tcp:` traverses NIC → e1000d → smolnetd
→ application, four address spaces).

## The scheme abstraction

Redox extends "everything is a file" to "everything is a URI". A
**scheme** is a named service that handles file-like operations
(`open`, `read`, `write`, `close`, `fmap`, …) on a namespace it owns.

Examples:

| Scheme | Provider | What lives there |
|--------|----------|------------------|
| `file:` | `redoxfs` (daemon) | The on-disk filesystem |
| `tcp:` | `smolnetd` (daemon) | TCP sockets — `open("tcp:127.0.0.1/3000")` returns a connected fd |
| `network.0000:00:03.0_e1000:` | `e1000d` (daemon) | Raw frames in/out of one NIC |
| `memory:physical` | kernel | MMIO mapping by physical address |
| `irq:` | kernel | IRQ events delivered as readable fds |
| `pci:` | `pcid` (daemon) | Per-function PCI config space + BARs |

A scheme is the unit of service location. The kernel routes file
operations on `/scheme/<name>/<path>` to whichever process registered
`<name>`. Drivers are scheme providers.

## The driver pipeline

Three cooperating processes turn "PCI device exists" into "driver is
running":

```text
pcid                   →  pcid-spawner            →  e1000d (one per NIC)
(walks PCI tree,          (matches IDs against        (talks to its
 exposes /scheme/pci/*)    config.toml,                hardware,
                           spawns drivers)             registers
                                                       /scheme/network.*)
```

### `pcid` — the PCI bus daemon

`drivers/pcid/src/main.rs` walks the PCIe configuration space at
boot, builds a `BTreeMap<PciAddress, Func>` of every function, parses
each device's BARs (32-bit / 64-bit / I/O), capability list (MSI,
MSI-X, PCIe, …), and option ROM, and exposes the result as the
`pci:` scheme.

Crucially, `pcid` is **already in userspace** — it is one of the
first daemons started by `bootstrap`. It uses the `pci_types` crate
plus a thin `Pcie` config-access wrapper over `memory:physical` to
read MMCONFIG (PCIe extended config space).

### `pcid-spawner` — the device→driver matchmaker

A separate, smaller daemon (`drivers/pcid-spawner/src/main.rs`) does
the matching. It:

1. Reads driver `config.toml` files from a known directory.
2. Iterates `/scheme/pci/*` (every device `pcid` discovered).
3. For each device, finds the first config entry whose match table
   includes its vendor/device ID, then `Command::new(program).spawn()`
   passing the PCI function handle as a file descriptor in the
   environment variable `PCID_CLIENT_CHANNEL`.
4. Calls `handle.enable_device()` (sets bus-master + memory + I/O in
   the command register) before spawning.

This is a textbook microkernel pattern: **policy** (which driver
claims which device) lives in userspace data files, separate from
**mechanism** (how config space is read).

### Declarative match tables

Each driver ships a `config.toml` next to its `Cargo.toml`. The
e1000d entry is the canonical small example:

```toml
# drivers/net/e1000d/config.toml
[[drivers]]
name = "E1000 NIC"
class = 0x02
ids = { 0x8086 = [0x1004, 0x100e, 0x100f, 0x109a, 0x1503] }
command = ["e1000d"]
```

`pcid-spawner` reads this; the driver binary itself never declares
its match IDs — it only handles the device once spawned. This means
adding a new device ID is a pure data change: edit the TOML, no
recompile of the spawner.

This design is closer to Linux's `MODULE_DEVICE_TABLE` macro (which
emits a separate ELF section parsed by `depmod`) than to Linux's
`pci_register_driver()` API where the driver registers a runtime
table. Both encode match tables as data; Redox just keeps the data
external to the binary.

## Anatomy of an e1000d daemon

The whole `main.rs` is ~80 lines (`drivers/net/e1000d/src/main.rs`).
The pattern generalises across nearly every Redox driver:

```rust
fn daemon(daemon: daemon::Daemon, mut pcid_handle: PciFunctionHandle) -> ! {
    let pci_config = pcid_handle.config();
    let irq = pci_config.func.legacy_interrupt_line.expect(...);
    let mut irq_file = irq.irq_handle("e1000d");

    // Map BAR0 — pcid_handle internally calls /scheme/memory/physical
    let address = unsafe { pcid_handle.map_bar(0) }.ptr.as_ptr() as usize;

    // Construct the device + register a scheme for it
    let mut scheme = NetworkScheme::new(
        move || unsafe { device::Intel8254x::new(address)... },
        daemon,
        format!("network.{name}"),
    );

    // Two event sources: IRQ fd and scheme fd
    let event_queue = EventQueue::<Source>::new()...;
    event_queue.subscribe(irq_file.as_raw_fd() as usize, Source::Irq, READ)?;
    event_queue.subscribe(scheme.event_handle().raw(), Source::Scheme, READ)?;

    libredox::call::setrens(0, 0).expect("e1000d: enter null namespace");

    for event in event_queue {
        match event.user_data {
            Source::Irq => {
                let mut irq = [0; 8];
                irq_file.read(&mut irq)?;
                if unsafe { scheme.adapter().irq() } {
                    irq_file.write(&mut irq)?;        // ack
                    scheme.tick()?;                     // process rings
                }
            }
            Source::Scheme => scheme.tick()?,           // smolnetd asked for a packet
        }
    }
}
```

Five things to note:

1. **The driver is a normal Unix process.** `std::io::Read/Write`,
   `Command`, environment variables — all available. No `no_std`, no
   `#[global_allocator]` gymnastics.
2. **IRQs are file descriptors.** `read(irq_file)` blocks until the
   kernel signals the IRQ; `write(irq_file)` acks it. This makes
   `select`/`epoll`-style I/O multiplexing work uniformly across
   IRQs and other file descriptors.
3. **MMIO is `mmap` of `memory:physical`.** The driver gets a raw
   pointer it can dereference; the kernel set up the page tables and
   trusts the userspace daemon to behave (it's still ring 3, so no
   privileged instructions, but it has direct memory access to the
   device's MMIO region).
4. **The driver registers its own scheme.** `NetworkScheme::new(…,
   "network.0000:00:03.0_e1000")` is how `smolnetd` finds the NIC.
   Naming is hierarchical and includes the PCI address, so
   multiple NICs of the same model are unambiguous.
5. **`setrens(0, 0)` enters a null namespace.** Once the driver has
   opened its IRQ fd, BAR mapping, and scheme registration, it
   drops access to the rest of the filesystem — minimum-privilege
   sandboxing, free.

## Network stack handoff

Above e1000d sits `smolnetd` (Redox's network stack daemon, built on
the smoltcp library). At startup it scans for `network.*` schemes,
opens each as both reader and writer, and binds them to smoltcp
interfaces. It then registers `tcp:`, `udp:`, `ip:`, `icmp:`,
`netcfg:` schemes for applications.

A TCP packet path:

```text
wire → e1000 NIC → IRQ → e1000d reads RX ring → frame written to
network.0000:00:03.0_e1000 → smolnetd reads it → smoltcp processes →
TCP segment delivered to fd held by the application
```

Four context switches per packet. Redox accepts this cost; the
isolation makes it possible to upgrade a NIC driver without
rebooting and to crash-restart a misbehaving one.

## Library reuse across drivers

Redox factors out shared logic as ordinary Rust crates:

| Crate | Purpose |
|-------|---------|
| `drivers/common` | Logging setup, file/directory helpers |
| `drivers/executor` | Async runtime for drivers — futures + IRQ-driven re-poll, no separate reactor thread |
| `drivers/net/driver-network` | `NetworkScheme` — turns a `(read_packet, write_packet)` pair into an `/scheme/network.*` provider |
| `drivers/storage/driver-block` | Equivalent for block devices |
| `drivers/virtio-core` | Shared VirtIO transport (config, queue, doorbell) |
| `pcid` (as a library) | Provides `PciFunctionHandle` to driver crates |

This is the same pattern as embclox's `embclox-core`,
`embclox-async`, `embclox-hal-x86` split — shared substrate as
crates, driver-specific glue per crate.

## Comparison to embclox

| Concern | Redox | embclox |
|---------|-------|---------|
| Kernel topology | Microkernel; drivers in ring 3 | Monolithic; everything in ring 0 |
| Driver isolation | Process boundary, MMU-enforced | None — all drivers share address space |
| IPC | Scheme calls (`read`/`write`/`fmap` over fd) | Direct Rust function calls |
| Device discovery | `pcid` daemon walks PCIe, exposes `/scheme/pci/*` | `embclox_hal_x86::pci::PciBus` scans inline at boot |
| Match tables | External `config.toml` per driver, read by `pcid-spawner` | Compile-time `vendor`+`device` IDs in each example's main |
| Driver lifecycle | Spawned on demand, restartable, sandboxed via `setrens` | Static — initialised once in `kmain`, lives forever |
| IRQ delivery | File descriptor on `/scheme/irq`, blocks in `read()` | Hardware IDT entry → ISR → `Waker` → `block_on_hlt` re-polls |
| MMIO mapping | `mmap` over `/scheme/memory/physical` | `MemoryMapper::map_mmio` direct page-table edit |
| Network stack | `smolnetd` (separate process) consumes `network.*` schemes | `embassy-net` (in-kernel) consumes `embassy_net_driver::Driver` impls |
| Driver language requirements | Full `std`, `alloc`, threads | `no_std`, bounded `alloc` only |
| Crash recovery | Restart the daemon | Reboot the machine |

The two systems make **opposite trade-offs** on the isolation /
performance axis. Redox optimises for crash containment and dynamic
reconfiguration; embclox optimises for low-overhead direct hardware
access and pedagogical simplicity.

## Ideas worth borrowing

1. **Declarative match tables outside the driver binary.** Even in a
   monolithic kernel we can put `[[drivers]]` entries in a TOML or
   `inventory!`-style static and have the driver registry consume
   them, instead of hard-coding ID lists in `examples-*/src/main.rs`.
   Keeps "what hardware does this driver claim" close to the driver,
   not in the boot code.

2. **`(IRQ-source, request-source)` event loop shape.** e1000d's
   `EventQueue<Source::{Irq, Scheme}>` pattern is how you idiomatically
   structure an interrupt-driven driver in async Rust. embclox already
   uses an isomorphic version (`NET_WAKER` + smoltcp's "stack wants to
   send" path) and the resemblance is reassuring — it suggests the
   shape is converged-upon, not accidental.

3. **Bus daemon vs driver daemon split.** Even without processes,
   keeping bus enumeration (`PciBus`) separate from driver matching
   (a driver registry) and from per-driver lifetime (one struct per
   probed device) is clean. The proposed
   [driver-model](../design/driver-model.md) follows this layering.

4. **Naming devices by topology, not type.** Redox's
   `network.0000:00:03.0_e1000` carries the bus address. If we ever
   support multiple instances of the same NIC, including the PCI
   `(bus, dev, func)` triple in the device name avoids ambiguity.

5. **A shared `driver-network` library.** We have
   `embclox_core::{e1000_embassy, tulip_embassy}` and
   `embclox_hyperv::netvsc_embassy` — three near-identical
   `embassy_net_driver::Driver` impls. A shared abstraction (similar
   to Redox's `NetworkScheme`) could collapse them.

## What does not apply

- **Process isolation as a security boundary.** We have no MMU
  separation between drivers, so a buggy driver corrupts kernel
  memory. This is a fundamental architectural choice; embclox is a
  teaching kernel + demo platform, not a security-isolated OS.

- **Hot-reloadable drivers.** Redox can `kill -9` a driver and
  `pcid-spawner` re-spawns it. We statically link everything; there
  is no equivalent mechanism and adding one would require either a
  module loader or an in-kernel restart-marshalling story.

- **Schemes as the universal IPC.** Schemes presuppose a ring-3
  syscall surface. We have no such surface; all "IPC" between
  drivers and the network stack is direct trait calls.

- **`setrens` privilege drop.** Without processes there is nothing to
  drop privilege from.

## Takeaway

Redox demonstrates that a clean, scalable driver model in Rust is
achievable, but the cleanliness depends heavily on the microkernel
substrate — process isolation, fd-based IRQs, and the scheme syscall
surface are doing most of the work. embclox should pick the patterns
that are substrate-independent (declarative match tables, event-loop
shape, bus/driver/device layering) and leave the substrate-dependent
ones (process spawning, scheme IPC, sandboxing) alone.
