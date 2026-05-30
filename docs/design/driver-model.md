# Driver model: bus / driver / device abstraction

## Status: implemented (May 2026)

Phases 1 and 2 of the migration path below are shipped in the tree:
[`crates/embclox-driver`](../../crates/embclox-driver/) implements the
registry; [`examples-kernel`](../../examples-kernel/) is the unified
binary; the existing per-NIC `examples-*` crates remain as one-driver
references. Verified end-to-end on QEMU (e1000 + tulip ctest lanes)
and on real Hyper-V Gen1 (NetVSC probe + RNDIS + TCP echo). Phase 3
(retire the per-NIC examples) is deliberately left open — see the
bottom of this doc.

Today each `examples-*` crate hard-codes one driver: PCI scan for
`0x8086:0x100E`, or `0x1011:0x0009`, or `embclox_hyperv::init` for
NetVSC. This is clear pedagogically but does not scale beyond a
handful of drivers and forbids a single binary that boots on multiple
platforms.

This doc sketches a Linux-shaped abstraction — `Bus` discovers
devices, `Driver`s declare match tables, a registry runs probe — sized
for embclox's actual constraints (no global allocator in the HAL,
fewer than ten drivers, no hot-plug, single network stack instance).

It is a **design only**. Adopting it is a separate decision; the
current per-example layout is fine if the driver count stays small.

## Goals

- One binary boots on QEMU (e1000 or tulip) **and** Hyper-V (NetVSC)
  with no compile-time switch — the runtime picks whichever NIC is
  present.
- Adding a fourth NIC (virtio-net, RTL8139, …) is one new file plus
  one registry entry — no edits to the boot path.
- Drivers stay loosely coupled to the IP stack: each driver produces
  an `embassy_net_driver::Driver`, the example owns the embassy-net
  `Stack`.

## Non-goals

- Hot-plug. Devices are discovered once at boot.
- Module loading. Everything is statically linked into the kernel
  binary (we are not building a `.ko` equivalent).
- Multi-stack networking. Only one `embassy_net::Stack` is brought up
  even if multiple NICs are present (see "Multi-NIC" below).
- Replacing the per-example crates wholesale. They remain as small,
  focused references; the unified binary is an additional
  `examples-kernel` crate.

## Shape

Four traits and one registry. None require an allocator beyond what
`embclox-hal-x86` already pulls in for `BootDmaAllocator`.

### `Bus`

A bus enumerates devices it knows about. Implementations: `PciBus`
(wraps the existing scanner in `embclox-hal-x86::pci`) and `VmBus`
(wraps `embclox_hyperv::init` + the offer table).

```rust
pub trait Bus {
    type Device: Clone;
    /// Returns the cached enumeration. The bus enumerates once at construction;
    /// a separate `rescan(&mut self)` may be added later if hot-plug arrives.
    fn enumerate(&self) -> &[Self::Device];
}
```

The associated `Device` type is intentionally bus-specific — a
`PciDevice` carries `(bus, dev, func, vendor_id, device_id, BARs…)`
while a `VmBusDevice` carries `(class_guid, instance_guid,
ChannelHandle)`. Forcing a single `Device` enum would either lose
information or require every driver to downcast.

Both device types must be `Clone` (the boot flow consumes the
enumeration via `.iter().cloned()`). `PciDevice` in
[crates/embclox-hal-x86/src/pci.rs](../../crates/embclox-hal-x86/src/pci.rs)
derives `Clone + Copy + Debug` and carries `class` / `subclass`
(shipped in Phase 1 \u2014 see "Migration path" below).

### `DeviceInfo` — deferred

An earlier draft proposed a `DeviceInfo` trait
(`bus_name()` / `id()` / `class()`) to give the registry a
bus-agnostic log line. With no caller in the current boot flow it
is YAGNI; the per-bus probe loops log via `drv.name()` and the
concrete device's `{:?}`. Add `DeviceInfo` only when a second
consumer appears (e.g. a `/proc/devices`-style introspection
endpoint).

### `Driver`

A driver describes what it can claim and how to bring it up. Each
bus has its own driver trait because `probe()` needs the bus's
concrete device type:

```rust
pub trait PciDriver: Send + Sync {
    fn name(&self) -> &'static str;
    fn priority(&self) -> u8;                            // lower wins; see Multi-NIC
    fn matches(&self, dev: &PciDevice) -> bool;
    fn probe(&self, dev: PciDevice, ctx: &mut ProbeCtx) -> Result<Box<dyn EmbcloxNic>, ProbeError>;
}

pub trait VmBusDriver: Send + Sync {
    fn name(&self) -> &'static str;
    fn priority(&self) -> u8;
    fn matches(&self, dev: &VmBusDevice) -> bool;
    fn probe(&self, dev: VmBusDevice, ctx: &mut ProbeCtx) -> Result<Box<dyn EmbcloxNic>, ProbeError>;
}
```

`Send + Sync` is required from day one (see "SMP-forward design
choices" below) but **not `'static`** — drivers are owned by the
registry as `Box<dyn PciDriver>`, so their lifetime is bounded by
the registry's. This lets driver constructors take config or hold
non-`const` state.

`PciDevice` is a *descriptor* (bus/dev/func/vendor/device IDs only
— see `crates/embclox-hal-x86/src/pci.rs`); a driver's `probe()`
needs the live `&PciBus` to call `read_bar()`,
`enable_bus_mastering()`, and `read_config(0x3C)` for the IRQ line.
`ProbeCtx` therefore carries `pci: &PciBus` (see "ISR registration"
below for the full struct).

`ProbeCtx` carries the per-driver capabilities the probe might need:
the bus handle, the DMA allocator, the `MemoryMapper`, the `IoApic`
handle (to wire its IRQ), and a callback to register an ISR vector.
This avoids making each driver `unsafe` reach into globals.

### `EmbcloxNic` (dyn-safe network device)

`embassy_net_driver::Driver` has GAT-typed RX/TX tokens
(`type RxToken<'a> where Self: 'a`) and is **explicitly not
dyn-compatible** per its upstream docs. We therefore cannot use
`Box<dyn embassy_net_driver::Driver>` directly as the registry's
return type.

The registry instead returns a local, dyn-safe wrapper trait that
threads packet access through a `&mut dyn FnMut(&mut [u8])`
callback — the standard pattern for erasing GATs:

```rust
pub trait EmbcloxNic: Send {
    /// Returns true if a packet was delivered (and `f` invoked).
    fn poll_rx(&mut self, cx: &mut Context<'_>, f: &mut dyn FnMut(&mut [u8])) -> bool;

    /// Returns true if `len` bytes were enqueued (and `f` invoked).
    fn poll_tx(&mut self, cx: &mut Context<'_>, len: usize, f: &mut dyn FnMut(&mut [u8])) -> bool;

    fn link_state(&mut self, cx: &mut Context<'_>) -> LinkState;
    fn capabilities(&self) -> Capabilities;
    fn hardware_address(&self) -> HardwareAddress;
}
```

A single adapter newtype bridges `Box<dyn EmbcloxNic>` back to the
GAT trait that `embassy_net::Stack` requires:

```rust
pub struct DynNic(pub Box<dyn EmbcloxNic>);

impl embassy_net_driver::Driver for DynNic {
    type RxToken<'a> = DynRxToken<'a> where Self: 'a;
    type TxToken<'a> = DynTxToken<'a> where Self: 'a;
    // receive/transmit construct tokens that, when consumed, call
    // EmbcloxNic::poll_rx / poll_tx with the user's closure as the
    // `&mut dyn FnMut(&mut [u8])` argument.
    // ...
}
```

Each existing per-NIC adapter (`E1000Embassy`, `TulipEmbassy<D>`,
`NetvscEmbassy`) keeps its current `embassy_net_driver::Driver` impl
— a thin `impl EmbcloxNic for E1000Embassy { ... }` block per crate
forwards `poll_rx` to the existing `recv_with` / `transmit_with`
path. No rewrite of the data-plane code.

**Cost.** One extra indirect call per packet boundary
(through `Box<dyn EmbcloxNic>`) plus the `&mut dyn FnMut` indirection
on the closure. This is on top of the inherent vtable cost discussed
in **T-1**; budget ~30–80 ns/packet total versus a monomorphised
impl. Acceptable for Tier-1 (control-plane echo / small-MTU
workloads); revisit when Tier-2 NAPI batching lands — the batching
amortises the indirection across hundreds of packets per IRQ, same
as the Linux mitigation playbook described in T-1.

**Why this shape vs. alternatives.** Two other shapes were
considered (see also **T-1**):

- **`enum Nic { E1000(..), Tulip(..), Netvsc(..) }`** —
  monomorphised data plane, no vtable. Rejected because every new
  in-tree NIC requires editing a central enum **and** loses the
  "driver crate is self-contained" property (the enum can't live in
  a per-driver crate without circular deps).
- **`embassy-net-driver-channel` adapter** — concrete `Device<MTU>`
  struct, no `dyn` anywhere. Rejected because it forces one extra
  packet copy through the channel buffer and pins the kernel to a
  single compile-time MTU, both of which we'd rather not pay until
  measurements demand it.

The `EmbcloxNic` wrapper preserves the registry's runtime-dispatch
shape, costs only the indirect call (no copy), and stays compatible
with all three driver crates as-is.

### Registry

The registry is an **owned value** built in `kernel_main`,
populated via `&mut self` registration methods, then borrowed
shared (`&self`) during the probe loop. No global state; no
`'static` constraint on driver instances — the registry owns its
drivers as `Box<dyn PciDriver>`.

```rust
pub struct DriverRegistry {
    pci:   Vec<Box<dyn PciDriver>>,
    vmbus: Vec<Box<dyn VmBusDriver>>,
}

impl DriverRegistry {
    pub fn new() -> Self { ... }
    /// Convenience: boxes the driver internally so callers write
    /// `registry.register_pci(E1000Driver)` instead of `Box::new(...)`.
    pub fn register_pci(&mut self, drv: impl PciDriver + 'static) { ... }
    pub fn register_vmbus(&mut self, drv: impl VmBusDriver + 'static) { ... }
    pub fn pci(&self)   -> &[Box<dyn PciDriver>];
    pub fn vmbus(&self) -> &[Box<dyn VmBusDriver>];
}
```

Each driver crate exposes its driver type (typically a marker
struct) and optionally a constructor. The application builds the
registry at boot:

```rust
// in kernel_main:
let mut registry = DriverRegistry::new();
embclox_driver::register_default_drivers(&mut registry);
// app can also register a custom driver here:
// registry.register_pci(MyCustomDriver::new(custom_config));

let nics = probe_all(&registry, &mut probe_ctx, &pci, hyperv_vmbus.as_ref());
```

`probe_all` returns `Vec<ProbedNic>` (never `Err`) — individual
probe failures are logged and skipped per the boot flow below; the
caller cannot recover any way other than "pick a primary or panic",
so there is no `?` on this line. Its full definition is the body of
the "Boot flow" pseudocode below.

Where `register_default_drivers` is just:

```rust
// crates/embclox-driver/src/defaults.rs
pub fn register_default_drivers(registry: &mut DriverRegistry) {
    registry.register_pci(embclox_e1000::driver::E1000Driver);
    registry.register_pci(embclox_tulip::driver::TulipDriver);
    registry.register_vmbus(embclox_hyperv::driver::NetvscDriver);
}
```

The crate is `embclox-driver` (singular, matching the existing
`embclox-{role}` convention). The helper module name is also
`embclox_driver::register_default_drivers` — no plural variant
exists. Forgetting to add a new driver here is a runtime defect
mitigated by CI (see "Decided trade-offs / T-3").

Design notes:

- **No global state.** Registration concurrency is enforced by the
  `&mut self` borrow; probe-loop concurrency is enforced by the
  `&self` borrow. Rust's borrow checker gives us the same
  guarantees a `RwLock` would, at zero runtime cost.
- **`Box<dyn>` not `&'static dyn`** — drivers are owned by the
  registry. Cleaner ownership, no `OnceLock` needed for drivers
  that need runtime construction (config-driven match tables,
  feature-negotiation state, etc.). For our 3 zero-sized driver
  structs the cost is 3 tiny `Box::new` allocations — negligible.
- **`Vec` requires the heap.** That is fine — embclox already has
  a `LockedHeap` global allocator.
- **Forgetting to register a driver** is a runtime defect (no NICs
  found). To mitigate, the Tier-1 implementation provides a
  `register_default_drivers(&mut registry)` helper covering the
  in-tree drivers; applications that want a custom set call
  individual `register_pci()`/`register_vmbus()` instead.
- **No init-order question.** The registry is built when needed
  and dropped when probing is done. No `lazy_static!`, no
  `OnceLock`, no "is the static initialized yet" check.
- **Testable.** Each test can build its own registry with whatever
  set of drivers it cares about, including ad-hoc mock drivers.

### Dynamic loading roadmap

The registry shape supports three progressive levels of
"dynamic":

| Level | Description | Status |
|-------|-------------|--------|
| **L1** | Runtime registration of statically-compiled drivers | **Tier 1 target.** Drivers are linked into the kernel binary at build time; `register_pci()` calls run at boot from `kernel_main`. |
| **L2** | Cargo-feature-driven inclusion | Future. Build-time flag selects which driver crates are even compiled in. No registry change. |
| **L3** | True runtime loading (ELF dlopen) | Out of scope. Would require a Theseus-style `mod_mgmt` (~5 kLOC of dynamic linker) **plus** a static `OnceLock<DriverRegistry>` (or equivalent) so dlopened module init functions can find the registry without a parameter. Not planned. |

The same `register_pci()` API serves all three levels, so any
later move to L2 (or L3 if we ever want it) is **additive**, not
breaking. L3 specifically may want to promote the registry to a
static at that time; until then, the owned-value form is cleaner
and avoids a mutex.

We explicitly anti-goal `inventory`/`linkme`-style auto-registration
via linker sections. Those crates work fine in `no_std`, but the
hidden-side-effect registration model conflicts with L1's
explicit-call discipline and would force a different shape than
the future L2 wants. Better to be explicit at every level.

## SMP-forward design choices

Even though the initial implementation targets the current
single-CPU runtime, three small choices in this design head off
the rework that would otherwise be required when the scheduler
grows multi-CPU support
([gap-analysis.md](./gap-analysis.md) Tier 1, gap #2).

### 1. `ProbeCtx::install_pci_isr` returns `(vector, cpu_id)`

```rust
pub struct InstalledIsr {
    pub vector: u8,
    pub cpu_id: CpuId,    // BSP only on single-CPU systems
}

impl ProbeCtx<'_> {
    pub fn install_pci_isr(
        &mut self,
        line: u8,
        handler: extern "x86-interrupt" fn(InterruptStackFrame),
    ) -> Result<InstalledIsr, ProbeError> { ... }
}
```

On the current single-CPU runtime, `cpu_id` is always
`CpuId::BSP` and the implementation is trivial. When SMP arrives,
the `VectorAllocator` becomes per-CPU and the same API returns the
real `(vector, cpu_id)` pair — no driver code changes. Drivers that
need to ack an IRQ from the right ISR context can already plumb
the CPU ID through their static state.

### 2. `Driver: Send + Sync` from day one

```rust
pub trait PciDriver: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, dev: &PciDevice) -> bool;
    fn probe(&self, dev: PciDevice, ctx: &mut ProbeCtx) -> Result<...>;
}
```

The `Send + Sync` requirement is currently free (we have one
thread, so nothing is shared) but it forbids drivers from baking
in non-`Sync` state like raw `Cell<T>` or `RefCell<T>`. The
compile error happens at driver implementation time — much
cheaper than catching it during the SMP migration. embedded-hal
and embassy already enforce this discipline; we just inherit it.

**Control plane vs data plane.** `Send + Sync` on the *control
plane* (`matches()`, `probe()`) is cheap because those run during
boot. The *data plane* (`embassy_net_driver::Driver::transmit` /
`receive` taking `&mut self`) has a separate lock-or-shard story
that this design does not yet specify. Today there is one CPU and
one stack so no concurrent `&mut self` exists; under SMP the
returned `NetDevice` will need either per-CPU sharding (RX/TX
queues per core) or a per-driver lock (`~20–500 ns/packet`
contention). That decision is part of the scheduler work
([gap-analysis.md](./gap-analysis.md) gap #2), not this design.

### 3. Registry concurrency via borrow rules, not a lock

```rust
pub struct DriverRegistry {
    pci:   Vec<Box<dyn PciDriver>>,
    vmbus: Vec<Box<dyn VmBusDriver>>,
}
```

The registry is built and populated under `&mut DriverRegistry`
during boot, then borrowed shared (`&DriverRegistry`) for the
probe loop. **Rust's borrow checker enforces the same invariant
a `RwLock` would** — exclusive write or shared read — at zero
runtime cost.

**SMP caveat: probe is BSP-only for now.** `ProbeCtx` holds three
`&mut` borrows (`MemoryMapper`, `IoApic`, `VectorAllocator`) of
system singletons that two CPUs cannot acquire concurrently.
Under SMP, probe stays serialised on the BSP — the registry is
read-shared but `ProbeCtx` access is exclusive. The SMP migration
will not need to change driver-facing APIs for this; it does not
need to make `ProbeCtx` itself shareable. The "no API changes"
claim above applies to the `Bus` / `Driver` / `ProbeCtx::install_*`
shapes; it does not promise SMP probe parallelism.

The combined effect: when SMP work begins, the driver model needs
**no API changes** — only the `VectorAllocator` implementation
becomes per-CPU. Drivers that compiled clean on single-CPU stay
compiling clean.

## Boot flow

```text
kernel_main(boot_info)
  ├─ embclox_hal_x86::init(...)         // existing — heap, paging, PCI handle
  ├─ idt::init() + pic::disable()
  ├─ map LAPIC + IOAPIC                 // existing
  ├─ start_apic_timer(...)              // existing
  │
  ├─ if is_hyperv():                    // CPUID + feature-bit gate (see "Bus detection")
  │     install_sint2_isr();            // MUST happen before VmBus::init
  │     vmbus = embclox_hyperv::init(&dma, &mut memory)
  │             .ok();                  // None on failure → skip vmbus probe, keep PCI
  │
  ├─ let mut registry = DriverRegistry::new();
  ├─ register_default_drivers(&mut registry);
  │   // app may also call registry.register_pci(MY_CUSTOM_DRIVER) here
  │
  ├─ ProbeCtx { dma, memory, ioapic, irq_alloc, pci }
  │
  ├─ // Device-major loop with claim tracking and partial-failure tolerance.
  ├─ // Iteration order: device-major + first-successful-probe-wins per device.
  ├─ // This matches Linux's pci_register_driver semantics.
  ├─ let mut claimed: BTreeSet<(u8,u8,u8)> = BTreeSet::new();
  ├─ let mut nics:    Vec<ProbedNic>       = Vec::new();
  ├─ for dev in pci.enumerate().iter().cloned():
  │     if claimed.contains(&(dev.bus, dev.dev, dev.func)) { continue; }
  │     if nics.len() >= PROBE_BUDGET { break; }   // hard cap; see Probe-then-discard cost
  │     for drv in registry.pci():                 // drv: &Box<dyn PciDriver>
  │         if drv.matches(&dev) {
  │         if drv.matches(&dev) {
  │             match drv.probe(dev, &mut ctx) {
  │                 Ok(nic) => {
  │                     claimed.insert((dev.bus, dev.dev, dev.func));
  │                     nics.push(ProbedNic::new(nic, drv.priority(), drv.name()));
  │                     break;          // first successful probe wins for this device
  │                 }
  │                 Err(e) => log::warn!("probe {} on {:?} failed: {}", drv.name(), dev, e),
  │             }
  │         }
  │
  ├─ if let Some(vmbus) = &vmbus:
  │     for offer in vmbus.enumerate().iter().cloned():
  │         for drv in registry.vmbus():
  │             if drv.matches(&offer) {
  │                 match drv.probe(offer, &mut ctx) {
  │                     Ok(nic) => { nics.push(ProbedNic::new(nic, drv.priority(), drv.name())); break; }
  │                     Err(e) => log::warn!("vmbus probe {} failed: {}", drv.name(), e),
  │                 }
  │             }
  │
  ├─ drop(registry);                    // probing done; registry no longer needed
  │
  ├─ // Multi-NIC selection — see Multi-NIC section
  ├─ if nics.is_empty() {
  │     panic!("no recognised NIC; enumerated devices: {:?}", pci.enumerate());
  │ }
  ├─ for n in &nics { log::info!("nic {} priority={}", n.name, n.priority); }
  ├─ let primary = nics.into_iter().min_by_key(|n| n.priority).unwrap();
  ├─ embassy_net::Stack::new(DynNic(primary.driver), …)
  └─ runtime::run_executor(...)
```

`PROBE_BUDGET` is a small compile-time constant (Tier-1 default: 4)
that caps the number of NICs the registry will probe before
bailing. Without it, a hostile or misconfigured PCI topology with
many matching cards could exhaust `BootDmaAllocator` (~1 MiB per
probed NIC; see "Probe-then-discard cost").

Three changes from a naive sketch worth highlighting:

1. **Device-major loop with claim tracking + first-successful-probe-wins per device** — matches Linux's `pci_register_driver` semantics. Two drivers whose `matches()` overlap (which we explicitly support for class-code matches) cannot both probe the same device and clobber each other's IOAPIC routing. The `claimed: BTreeSet<(bus,dev,func)>` makes the "first match wins" property the rest of the doc claims actually true.

2. **`match` instead of `?` in the probe call** — a transient `ProbeError` from one driver logs and continues; the kernel boots with the remaining NICs. The previous `?` propagation made any single probe failure abort boot with half-initialised hardware (mapped BARs, allocated vectors) never rolled back. See "Lifecycle" for the cleanup story.

3. **SINT2 ISR installed before `VmBus::init`** — moved from `examples-hyperv` (which doesn't exist in `examples-kernel`) into the boot flow itself. `embclox_hyperv::init` failures are now non-fatal (`.ok()` → `None` → skip vmbus probe loop) so a misdetected hypervisor doesn't kill PCI NICs.

All existing setup before `DriverRegistry::new()` is unchanged.
The new work is the registry construction, the registration calls,
the probe loops with claim tracking, the SINT2 install in the
Hyper-V branch, and the trait impls (`E1000Driver`, `TulipDriver`,
`NetvscDriver`) that wrap each crate's existing `new`/`init`
constructor.

## Bus detection (Hyper-V)

VMBus initialisation is expensive and crashes on bare hardware, so we
gate it on the Hyper-V CPUID leaves **plus the synthetic-feature
bits** — vendor-string alone false-positives under
`qemu -cpu host,+hypervisor,hv_synic,hv_vendor_id=...`.

[crates/embclox-hyperv/src/detect.rs](../../crates/embclox-hyperv/src/detect.rs)
already implements this check today:

```rust
use embclox_hyperv::detect;

fn is_hyperv() -> bool {
    matches!(detect::detect(), Some(f) if f.has_synic && f.has_hypercall)
}
```

`detect::detect()` returns `Some(HvFeatures { has_synic,
has_hypercall, .. })` only when CPUID leaf `0x4000_0000` reports
the Hyper-V vendor string; the boot flow tightens that to require
the two feature bits `embclox_hyperv::init` actually depends on.
Linux's `hv_is_hyperv_initialized()` (`drivers/hv/hv_common.c`)
does the same.

VMBus init failure is **non-fatal**: the boot flow uses `.ok()` to
demote `Err` to `None`, the vmbus probe loop is skipped, and the
PCI NICs from the previous loop are still usable. KVM with
HV-enlightenments (which advertises HyperV in vendor-string but
implements only a subset of the synthetic surface) thus boots
cleanly on its PCI virtio-net.

Linux does the same (`hv_is_hyperv_initialized()` in
`drivers/hv/hv_common.c`).

## ISR registration

Each driver's `probe()` may need to install an interrupt handler.
Today examples do this inline with `idt::set_handler(33, my_isr)` and
`ioapic.enable_irq(line, 33, 0)` at known vectors. With multiple
drivers we need allocation:

```rust
pub struct ProbeCtx<'a> {
    pub dma:       &'a dyn DmaAllocator,
    pub memory:    &'a mut MemoryMapper,
    pub ioapic:    &'a mut IoApic,
    pub irq_alloc: &'a mut VectorAllocator,
    pub pci:       &'a PciBus,             // for read_bar, enable_bus_mastering, read_config(0x3C)
}

impl ProbeCtx<'_> {
    /// Install an INTx-line ISR. Returns the allocated vector and the CPU it's pinned to
    /// (always BSP today; per-CPU when SMP lands — see "SMP-forward design choices §1").
    /// MSI/MSI-X drivers should use a future `install_pci_msix()` variant; see "Interrupt model scope".
    pub fn install_pci_isr(
        &mut self,
        line: u8,                              // PCI interrupt line (config 0x3C)
        handler: extern "x86-interrupt" fn(InterruptStackFrame),
    ) -> Result<InstalledIsr, ProbeError> {
        if line == 0xFF { return Err(ProbeError::InvalidIrqLine(line)); }
        if line as usize >= self.ioapic.max_entries() { return Err(ProbeError::InvalidIrqLine(line)); }
        let isr = self.irq_alloc.allocate()?;   // returns InstalledIsr { vector, cpu_id }
        unsafe { idt::set_handler(isr.vector, handler); }
        self.ioapic.enable_irq(line, isr.vector, isr.cpu_id.apic_id());
        Ok(isr)
    }
}
```

`install_pci_isr` returns the `InstalledIsr { vector, cpu_id }`
struct from "SMP-forward design choices §1" — that is the
authoritative signature; this section's earlier `Result<u8>` sketch
has been removed. There is one binding signature.

### `ProbeError`

A single error enum lives in `embclox-driver` and is returned by
both `ProbeCtx::install_pci_isr` and `PciDriver::probe` /
`VmBusDriver::probe`:

```rust
#[derive(Debug)]
pub enum ProbeError {
    /// PCI config 0x3C reported 0xFF (no IRQ wired) or a pin index
    /// outside the IOAPIC's redirection table.
    InvalidIrqLine(u8),
    /// `VectorAllocator` ran out of free IDT vectors.
    NoFreeVector,
    /// `MemoryMapper::map_mmio` failed (out of HHDM space, etc.).
    MmioMap,
    /// `BootDmaAllocator` allocation failed (heap exhausted).
    DmaAlloc,
    /// Driver-specific failure (link timeout, EEPROM CRC, VMBus
    /// channel-open NAK, ...). Carries a `&'static str` so each
    /// driver can give a one-line reason without dragging in
    /// `alloc::string::String` here.
    Driver(&'static str),
}
```

Drivers are free to map their internal errors to `Driver(...)` via
a small `From` impl. Only `InvalidIrqLine`, `NoFreeVector`,
`MmioMap`, and `DmaAlloc` are produced by `ProbeCtx` itself.

### Vector reservations

The `VectorAllocator` does **not** start with the full `33..=47`
range. It must reserve vectors already claimed elsewhere in the
kernel:

| Vector | Owner | Notes |
|--------|-------|-------|
| 32 | APIC timer | `runtime::start_apic_timer` |
| 34 | `embclox_hyperv::msr::VMBUS_VECTOR` | SINT2 IDT entry; reserved when `is_hyperv()` returns true, regardless of whether `embclox_hyperv::init` later succeeds. Cheap (one slot of 12) and keeps the reservation invariant simple. |
| 39 | spurious | `runtime::start_apic_timer` |
| 33, 35–38, 40–47 | available for `install_pci_isr` | 12 vectors total |

`VectorAllocator::new()` consumes a `&[u8]` exclusion list at
construction so the reservations are explicit and grep-able. A
second PCI driver cannot accidentally receive vector 34 and overwrite
the SynIC SINT2 IDT slot.

The `extern "x86-interrupt"` handler must still be a static `fn`
(no closures) because IDT entries are raw function pointers. Each
driver therefore exposes a static handler and **driver-private
static state** referenced from inside it. **Caveat:** with this
pattern the same driver type cannot service two device instances
without per-driver-instance vector demultiplexing — see
"Multi-instance limitation" below. For now the design assumes one
instance per driver kind.

### Interrupt model scope

The current shape is **legacy INTx only**: 12 IOAPIC pin-routed
vectors per system, no per-CPU affinity beyond BSP, no MSI/MSI-X.
This is sufficient for Tier-1 (the single-NIC examples + a unified
`examples-kernel` against one PCI device).

The Tier-2 NAPI / hardware-offload / multi-queue work
([gap-analysis.md](./gap-analysis.md) §4) requires MSI-X with
per-queue vectors steered to per-CPU executors. That work will add
`install_pci_msix(count: u8, affinity_hint: AffinityHint) -> Vec<InstalledIsr>`
**without changing** `install_pci_isr`'s signature — `InstalledIsr`
already carries the per-vector `cpu_id` placement needed.

### Multi-instance limitation

Static `extern "x86-interrupt" fn` handlers cap each driver at one
device instance per boot. Two e1000 cards present → second `probe()`
overwrites the first's static `REGS_BASE` → ISR services only the
second card → first card's IRQ never deasserts → IOAPIC re-asserts
in a tight loop → kernel livelocks. **Mitigation today**: probe
loop's `break` after first successful probe means second e1000 is
silently ignored (logged as "duplicate driver match: ignoring"); the
"detected but unused" log line still appears. **Future fix**: ISR
shim per-vector with `[Option<DeviceCtx>; 16]` indexed by vector
offset — out of scope for Tier-1.

## Multi-NIC

If both an e1000 and a tulip card are present, we probe both — they
both succeed and each produces a `Box<dyn EmbcloxNic>`.
But embassy-net's `Stack` is monomorphised on a single driver type:

```rust
impl<D: Driver, const N: usize> Stack<D, N> { ... }
```

So we pick **one primary NIC** for the stack. To preserve driver
identity (priority, name, log line) past the type-erased
`Box<dyn EmbcloxNic>`, the probe loop wraps each result in
`ProbedNic`:

```rust
pub struct ProbedNic {
    pub driver:   Box<dyn EmbcloxNic>,
    pub priority: u8,                      // lower wins
    pub name:     &'static str,            // "e1000", "tulip", "netvsc"
}

impl ProbedNic {
    pub fn new(driver: Box<dyn EmbcloxNic>, priority: u8, name: &'static str) -> Self {
        Self { driver, priority, name }
    }
}
```

Selection has an explicit empty-list policy:

```rust
let primary = match nics.into_iter().min_by_key(|n| n.priority) {
    Some(p) => p,
    None => panic!("no recognised NIC; enumerated PCI: {:?}, hyperv: {}",
                   pci.enumerate(), is_hyperv()),
};
```

Priority is supplied by the `PciDriver`/`VmBusDriver` (e.g.
`fn priority(&self) -> u8 { 20 }`) — each driver carries its own
constant. Recommended: `NetvscDriver` = 10, `E1000Driver` = 20,
`TulipDriver` = 30 — lower wins. Hyper-V synthetic NIC always wins
when present because that's the one with working DMA in a Hyper-V
guest.

**No-NIC policy: explicit panic with diagnostic** (see also
"Decided trade-offs / T-2"). The panic message enumerates what was
found so the boot log is actionable. A degraded-boot mode can be
added later when an application framework with a no-network code
path exists.

True multi-homed routing (one stack per NIC, route policy table) is
deferred — it is a much larger change touching the IP stack and is
not needed for any current example.

### Probe-then-discard cost

The current Multi-NIC strategy probes every matching device, then
discards all but the primary. Each discarded NIC has paid one
`BootDmaAllocator` allocation (RX+TX rings ≈ 1 MiB), one BAR
mapping, one IRQ vector (1/12 of the budget), and one driver state
struct. `BootDmaAllocator` is bump-style with no path to free.

This is **fine for Tier-1** (one PCI NIC + optional NetVSC at
runtime; ≤2 MiB waste). For Tier-2 deployments running 4+ NICs,
the boot flow should be restructured: build a candidate list of
`(driver, device, priority)` tuples *before* probing, sort by
priority, then call `probe()` only on the winner. Recorded as
follow-up work; not a Tier-1 blocker.

## Bootloader

**Limine is the framework's standard bootloader.** This is a
framework-level commitment, not a per-example choice. The
standardisation work (formerly "Phase 0") **is already complete in
the tree** as of the current commit:

- [`embclox_hal_x86::init`](../../crates/embclox-hal-x86/src/lib.rs)
  is Limine-native (`init(boot_info: LimineBootInfo<'_>, config:
  Config) -> Peripherals`); no `bootloader_api` constructor exists.
- [examples-e1000/CMakeLists.txt](../../examples-e1000/CMakeLists.txt),
  [examples-tulip/CMakeLists.txt](../../examples-tulip/CMakeLists.txt),
  and [examples-hyperv/CMakeLists.txt](../../examples-hyperv/CMakeLists.txt)
  all build Limine BIOS+UEFI ISOs through the same pipeline.
- The workspace has no remaining `bootloader_api` references.

Why Limine is the standard:

- **Hyper-V Gen1 deployment requires it.** `bootloader_api` does
  not produce Gen1-compatible images; `examples-hyperv`
  established the working Limine path for Hyper-V/Azure.
- **One bootloader, one HAL init path.** Two protocols would mean
  two initialisers, two `BootInfo` shapes, two memory maps to
  merge with `MemoryMapper`. Standardising removed a long-tail
  source of divergence between QEMU and bare-metal/cloud.
- **HHDM model fits.** Limine's higher-half direct map is what
  `MemoryMapper::map_mmio` builds on.
- **Active maintenance + Rust-friendly.** The Limine protocol is
  documented, stable, and the `limine` crate is `no_std`-clean.

### Outstanding doc cleanup

The code migration is done; the docs are not. Before merging the
driver-model work, sweep:

1. **`embclox-hal-x86` README** — mention Limine as the only
   supported boot protocol.
2. **The embclox-examples skill** (`/home/user1/code/embclox/.github/skills/embclox-examples/SKILL.md`)
   — same.
3. **Any in-tree doc that still says "bootloader_api or Limine"**
   — grep and update.

These are doc-only; they don't gate Phase 1.

### Why a single boot protocol is non-negotiable

A `bootloader_api`-vs-Limine fork in `embclox_hal_x86` would have
meant:

- Two `BootInfo` shapes propagating through every HAL caller.
- Two memory-map merge implementations.
- Twice the CI surface (every driver tested twice).
- A standing question on every PR ("which boot protocol does this
  affect?").

The framework's positioning as "ring-0 I/O appliances on QEMU AND
cloud" makes Limine the only viable convergent answer — which is
why the migration was completed before this design landed.

## Decided trade-offs

Four design choices flagged during review require explicit
recording so future readers can see what was considered.

### T-1: Data-plane driver hand-off — `Box<dyn EmbcloxNic>` (local dyn-safe wrapper)

`embassy_net_driver::Driver` has GAT-typed `RxToken<'a>` / `TxToken<'a>`
and is **not dyn-compatible** (upstream docs). The registry needs a
uniform return type for runtime dispatch, so we wrap each driver in
a local dyn-safe trait `EmbcloxNic` (see "`EmbcloxNic`" above) and
bridge back to `embassy_net::Stack` via a `DynNic` newtype adapter.

Three shapes were considered:

- **`Box<dyn EmbcloxNic>` (chosen).** Preserves the registry's
  runtime-dispatch shape with **zero packet copies**. Driver crates
  add one small `impl EmbcloxNic` block on top of their existing
  `embassy_net_driver::Driver` impl — no rewrite. Per-packet cost:
  ~30–80 ns from the `Box<dyn EmbcloxNic>` vtable + the
  `&mut dyn FnMut` closure indirection used to erase the token GATs.
- **`enum Nic { E1000(..), Tulip(..), Netvsc(..) }`.** Monomorphised,
  no vtable. Rejected: every new in-tree NIC edits a central enum,
  and the enum can't live in any single driver crate without
  circular deps. Loses the "each driver crate is self-contained"
  property the registry was built around.
- **`embassy-net-driver-channel` adapter.** Concrete `Device<MTU>`,
  no `dyn` anywhere. Rejected: forces one extra packet copy through
  the channel buffer and pins the kernel to a single compile-time
  MTU. Worth revisiting only if measurements show `Box<dyn>` is the
  bottleneck (it won't be at Tier-1 packet rates).

**Context: indirection on the data plane is universal, not
Rust-specific.** Linux's network drivers register a
`struct net_device_ops` with function pointers (`ndo_start_xmit`,
etc.) that every TX path dereferences:
`dev->netdev_ops->ndo_start_xmit(skb, dev)`. That's the same
indirect call our `Box<dyn EmbcloxNic>` produces — same machine
code, same hardware cost. C just doesn't call its struct-of-fn-
pointers a "vtable." So **the chosen design is Linux-equivalent**,
not degraded.

**Decision.** Keep `Box<dyn EmbcloxNic>` + `DynNic` adapter, and
follow the Linux mitigation playbook when the data plane actually
demands it:

1. **Tier-2 NAPI batching** — drain RX rings in batches per IRQ so
   the indirect call is amortised over hundreds of packets. This
   alone closes most of the gap to monomorphised dispatch.
2. **Hot-path devirtualisation** — Rust equivalent of Linux's
   `INDIRECT_CALL_WRAPPER`: a small `match` on the concrete driver
   type at the dispatch site for the common case, branch predictor
   handles the rest.
3. **Bypass the trait dispatch entirely for line-rate** — Linux uses
   XDP/eBPF; embclox would build an equivalent in-tree fast path that
   reads the device's RX ring directly without going through the
   embassy-net stack at all. Comparable to DPDK's bypass model.

The enum-monomorphisation shape is the **DPDK-equivalent design**
— used when you need the absolute last 50 ns/packet and have
decided to forgo pluggable drivers entirely. That's a different
workload class than Tier-2 even at its most aggressive; reach for
it only if measurements show the Linux mitigation playbook isn't
enough. Add a `cargo asm --release` check on `Stack::poll` before
freezing the trait surface to confirm the cost matches the estimate.

### T-2: No-NIC boot policy — explicit panic with diagnostic

When no driver matches any device, two reasonable behaviours
exist:

- **Explicit panic** (chosen for Tier-1). `panic!("no recognised
  NIC; enumerated PCI: {:?}, hyperv: {}", ...)` produces an
  actionable boot log instead of a generic `unwrap` trace. Forces
  visibility of the misconfiguration.
- **Degraded boot.** Continue without networking, surface the
  error via serial console / watchdog. Useful when the
  application has a no-network code path.

**Decision.** Ship the explicit panic now. Revisit when an
application framework with a no-network code path exists
([gap-analysis.md](./gap-analysis.md) gap #7). The worst outcome
(silent halt) is what we must avoid; either improvement is
acceptable.

### T-3: Driver registration — explicit helper + CI enforcement

`inventory`/`linkme`-style auto-registration would compile-time
enforce that every driver crate gets registered. The doc instead
chose explicit `register_default_drivers()` calls. Risk: a
contributor adds a new driver crate and forgets to edit the
helper, shipping a release where the new driver silently doesn't
load.

**Decision.** Keep the explicit helper but add CI enforcement: a
`qemu` lane that boots `examples-kernel` against every supported
`-device` and asserts each driver's `name()` appears in the boot
log. PR template item links any `crates/embclox-*` add to a
`register_default_drivers` edit. Revisit `linkme` if the in-tree
driver count exceeds ~5.

### T-4: Bootloader — Limine (framework standard, migration complete)

Limine is the embclox framework's standard bootloader. The HAL and
all three `examples-*` crates already build through the Limine
pipeline; see the "Bootloader" section above for the rationale and
the small remaining doc-cleanup list. **This trade-off is recorded
for history; it does not gate Phase 1.**

## Lifecycle

Drivers do not implement `Drop`-driven teardown. Once probed, they
live for the program's lifetime — same as today. This keeps the
ownership story simple (`nics: Vec<ProbedNic>` lives in `kmain`'s
stack frame) and matches the bare-metal "we never shut down" model.

**Probe-and-discard contract.** The `DriverRegistry` and the
`Box<dyn PciDriver>` instances inside it are dropped after probing
completes. All driver state required after probe **must be moved
into the returned `Box<dyn EmbcloxNic>`** during `probe()`, or
written to the driver's own static slots before `probe()` returns.
The `PciDriver`/`VmBusDriver` trait object itself becomes invalid
after `drop(registry)`. This rule is enforced by
`probe(&self, ...)` taking `&self` rather than `self` by value
(drivers cannot move state out of themselves into the `EmbcloxNic`);
implementations that hold per-device state in the driver struct
must duplicate it into the returned device or use a static slot.
This caveat ties into the multi-instance limitation in "ISR
registration" — the static-slot pattern is also why each driver
caps at one instance for now.

If we later add suspend/resume or driver unload, this trait will
grow a `remove()` method, but nothing in current examples needs it.

## What this does not change

- The driver crates themselves. `embclox-e1000`, `embclox-tulip`, and
  `embclox-hyperv` keep their existing public API
  (`E1000Device::new`, `TulipDevice::new`, `embclox_hyperv::init`).
  The `Driver` impls are thin wrappers added at the **top** of each
  crate, not a rewrite.
- The embassy-net `Driver` impls. They stay where they are.
- Per-driver ISRs. They remain `static extern "x86-interrupt" fn`.
- `examples-e1000` / `examples-tulip` / `examples-hyperv` continue
  to exist as small, single-driver references that newcomers can
  read in one sitting. (All three already use Limine — see
  "Bootloader".)

## Migration path

Three phases. **Phases 1 and 2 are shipped**; Phase 3 is optional and
open. Limine standardisation (formerly "Phase 0") was already in the
tree before this work began — see "Bootloader".

### Phase 1 — HAL prerequisites + driver-model crate — ✅ shipped

Three HAL additions plus the new crate:

1. **`PciBus::enumerate() -> Vec<PciDevice>`** —
   [crates/embclox-hal-x86/src/pci.rs](../../crates/embclox-hal-x86/src/pci.rs).
   Walks bus 0 dev 0..32 with multi-function fan-out; `PciDevice`
   also gained `class` / `subclass` so class-code matches work.
2. **`#[derive(Clone, Copy, Debug)]` on `PciDevice`** — same file.
3. **`vector_alloc` module** —
   [crates/embclox-hal-x86/src/vector_alloc.rs](../../crates/embclox-hal-x86/src/vector_alloc.rs).
   `CpuId::Bsp` (single-variant today), `InstalledIsr { vector,
   cpu_id }`, `VectorAllocator::{new, allocate, free_count}` with
   the explicit `&[u8]` reservation list.
4. **[`crates/embclox-driver`](../../crates/embclox-driver/)** —
   `Bus`, `PciDriver`, `VmBusDriver`, `EmbcloxNic`, `DynNic`,
   `ProbeCtx`, `DriverRegistry`, `ProbeError`, `ProbedNic`,
   `probe_all`, `PROBE_BUDGET`, `register_default_drivers`. The
   three in-tree NICs each got a small `drivers/{e1000,tulip,netvsc}.rs`
   wrapper inside this crate (marker type + `EmbcloxNic` impl + static
   ISR + private static slot), so the existing per-NIC crates were
   not touched.

### Phase 2 — `examples-kernel` — ✅ shipped

[`examples-kernel/src/main.rs`](../../examples-kernel/src/main.rs)
is the unified binary. `kmain` brings up the HAL, optionally
initialises Hyper-V VMBus (non-fatal), constructs a
`VectorAllocator` with the right reservations, registers all default
drivers, runs `probe_all`, picks the lowest-priority `ProbedNic`,
wraps it in `DynNic`, and hands it to `embassy_net::Stack` + a TCP
echo task on port 1234.

CI / image artefacts:

- `kernel-image` — generic ISO (`net=static ip=10.0.2.15/24 gw=10.0.2.2`,
  QEMU SLIRP defaults).
- `kernel-hyperv-image` — ISO with the dedicated `embclox-test` vSwitch
  IPs baked in (`192.168.234.50/24`), consumed by
  `scripts/hyperv-boot-test.ps1`.
- `kernel-vhd` — Azure Gen1 page-blob VHD with `net=dhcp`.
- ctest lanes `kernel-echo-e1000` and `kernel-echo-tulip` boot the same
  ISO against different QEMU `-device` flags and verify a TCP echo
  round-trip.
- Verified manually on real Hyper-V Gen1: Boot / VMBus / NetVSC PASSED,
  TCP echo VERIFIED via `scripts/hyperv-boot-test.ps1`.

The Hyper-V `scripts/hyperv-boot-test.ps1` default `-Iso` and the
`tests/infra/` Azure recipes now point at the unified `kernel.*`
artefacts; the per-NIC `examples-hyperv` paths remain available as
minimal one-driver references.

### Phase 3 — (Optional) Retire single-driver examples — open

The unified `examples-kernel` is the canonical reference going
forward. The per-NIC `examples-e1000` / `examples-tulip` /
`examples-hyperv` crates still exist as small, one-driver references
that newcomers can read in one sitting; their ctest lanes also
provide a per-driver regression target.

#### When to execute

Retire them only when either trigger fires:

- **Drift signal.** One of the per-NIC binaries fails to compile or
  boot because somebody touched the HAL and didn't update them.
  That's the maintenance-cost-exceeds-value point the design
  anticipates.
- **Pedagogical value subsumed.** Readers stop reaching for
  `examples-e1000/src/main.rs` because the per-driver wrapper at
  `crates/embclox-driver/src/drivers/e1000.rs` (~60 lines) is now
  the obvious one-driver read.

Until one of those happens, **leave them in place** — they cost
~1000 LoC + 9 CMake targets + 4 ctest lanes + 3 CI clippy lanes,
which is small relative to the regression coverage they buy.

#### Inventory

| Crate | LoC main.rs | CMake targets | ctest lanes | Owns also |
|-------|------------:|---------------|-------------|-----------|
| `examples-e1000` | ~250 | `e1000-{build,image}`, `qemu-e1000`, **`unit-test-{build,image}`** | `e1000-echo`, **`unit`** | the shared unit-test ISO pipeline |
| `examples-tulip` | ~280 | `tulip-{build,image,vhd}`, `qemu-tulip` | `tulip-boot`, `tulip-echo` | private `tulip_embassy.rs`, `LimineDmaAllocator` |
| `examples-hyperv` | ~500 | `hyperv-{build,image,vhd}`, `qemu-hyperv` | `hyperv-boot` | `limine-azure.conf`, Azure infra references (already redirected to `kernel.vhd`) |

#### Substeps, ordered by dependency + risk

**3a. Move unit-tests out of `examples-e1000/CMakeLists.txt`** —
prerequisite, zero-risk, can land any time. The shared
`unit-test-build` / `unit-test-image` / `unit` ctest lane currently
lives inside `examples-e1000/CMakeLists.txt`. Move it verbatim into
`qemu-tests/unit/CMakeLists.txt` and `add_subdirectory(qemu-tests/unit)`
from the root.

**3b. Decide what to do with `hyperv-boot`.** It's a QEMU UEFI smoke
that log-matches `HYPERV BOOT PASSED`. The kernel binary emits the
same marker. Two options:

- Drop it entirely — `kernel-echo-{e1000,tulip}` are a strict
  superset (boot + stack + TCP round-trip), so the coverage is
  already there. **Recommended.**
- Add a `kernel-boot` lane mirroring it — only worth doing if we
  want a fast (<15s) smoke that doesn't pay the embassy-stack tax.

**3c. Retire `examples-hyperv`** — biggest payoff first (~500 LoC,
most likely to drift because the inline SINT2 ISR / VMBus-init code
in its `main.rs` is duplicated by the kernel).

1. Delete `examples-hyperv/` directory.
2. `Cargo.toml`: remove from `members`.
3. Root `CMakeLists.txt`: remove `add_subdirectory(examples-hyperv)`;
   remove `hyperv-image` / `hyperv-vhd` from `images` aggregate;
   remove the `examples-hyperv` clippy line from `check`.
4. `.github/workflows/ci.yml`: remove the "Clippy hyperv example"
   lane.
5. Apply 3b's decision to `hyperv-boot`.
6. `tests/infra/README.md`: collapse the "Which VHD?" table back to
   two rows (`kernel.vhd`, `tulip.vhd`); drop "one-driver reference"
   language for the Azure path.
7. `scripts/hyperv-boot-test.ps1`: remove the "you can also pass
   `-Iso build/hyperv.iso`" escape hatch from the header.

**3d. Retire `examples-tulip`.** Same shape. Extras:

- `scripts/hyperv-tulip-test.ps1` is a tulip-on-Hyper-V manual
  harness. Either delete it (rare combo) or repoint it at
  `kernel-hyperv.iso` if there's still a use case.
- `examples-tulip/src/tulip_embassy.rs` is duplicated work that
  the registry already owns; deleting the example deletes it.

**3e. Retire `examples-e1000`** — last. Depends on 3a. Same shape.

#### What we keep regardless

- `crates/embclox-{e1000,tulip,hyperv}` — the device-driver
  implementations; only the binary wrappers retire.
- `crates/embclox-driver/src/drivers/{e1000,tulip,netvsc}.rs` —
  the registry-aware per-NIC wrappers; they become the canonical
  one-driver read.
- `scripts/hyperv-boot-test.ps1` and the `tests/infra/` Azure
  pipeline — already point at the unified `kernel.*` artefacts.

#### Risks

1. **Lost per-driver test isolation.** After retirement, the only
   way to test "NetVSC works in isolation" is to manually disable
   e1000/tulip registration in the kernel binary. Mitigation: if a
   regression demands it, add a "minimal" kernel-test crate that
   registers only one driver.
2. **Pedagogical value.** A ~250-line one-driver `main.rs` is
   easier to read in one sitting than `examples-kernel/src/main.rs`.
   Mitigation: `crates/embclox-driver/src/drivers/e1000.rs` (~60
   lines, just the driver wrapper) becomes the one-driver read.
3. **Real-Hyper-V coverage.** Drops from "explicit hyperv-only
   binary built every commit" to "Hyper-V tested only via the
   unified kernel". Already true after the Phase 2 redirect; this
   just makes it formal.

#### Recommended order if the trigger fires

3a + 3b first (zero-risk), then 3c (most LoC + most drift-prone),
then 3d, then 3e. Each substep is independently shippable; no need
to bundle.

## Trade-offs

| Cost | Mitigation |
|------|------------|
| ~600 LoC of new abstraction | One-time; pays back at the third NIC. |
| Per-driver ISRs still need static hooks; one device per driver kind | Documented in "ISR registration / Multi-instance limitation"; second match silently dropped with log line. Per-vector shim is post-Tier-1 work. |
| Driver registry built at runtime via explicit `register_pci()` calls | Explicit > clever; CI lane asserts every driver `name()` appears in the boot log (see "Decided trade-offs / T-3"). |
| `Vec<Box<dyn PciDriver>>` requires `alloc` (one Box per driver) | We already have `LockedHeap`; 3 tiny boxes for 3 zero-sized driver types is negligible. The registry is dropped after probing. |
| Probe-and-discard wastes per-NIC DMA / IRQ for non-primary NICs | Bounded for Tier-1 (≤1 secondary NIC); restructure to "select before probe" before Tier-2 multi-NIC scaling. See "Multi-NIC / Probe-then-discard cost". |
| Probe order is registration order; first successful probe wins per device | Boot flow uses device-major iteration with a `claimed` set — matches Linux's `pci_register_driver` semantics. |

## Comparison to Linux

| Concept | Linux | embclox proposal |
|---------|-------|------------------|
| Device tree | `struct device` in `/sys/devices` | `DeviceInfo` trait + per-bus device structs |
| Bus | `struct bus_type` (PCI, USB, VMBus, …) | `Bus` trait, one impl per transport |
| Driver registration | `module_pci_driver()` macro → linker section | Driver crate exports a marker type (e.g. `pub struct E1000Driver;`); `kernel_main` calls `registry.register_pci(Box::new(E1000Driver))` |
| Match tables | `MODULE_DEVICE_TABLE(pci, …)` | `Driver::matches(&dev) -> bool` (code, not data — gives flexibility for class-code matches) |
| Probe | `drv->probe(dev)` | `Driver::probe(dev, &mut ProbeCtx)` |
| net subsystem | `register_netdev()` → `struct net_device` | `Box<dyn EmbcloxNic>` returned from probe; `DynNic` adapter bridges to `embassy_net::Stack` |
| Hot-plug | uevent, `kobject_uevent()` | not supported |
| Module loading | `insmod`, `request_module()` | L1 only (statically linked, runtime-registered). L2/L3 are future, see "Dynamic loading roadmap". |

The proposal is intentionally a strict subset: same shapes as
Linux, none of the hot-plug or sysfs surface area, and only the
runtime-registration form of dynamic loading (L1 in the
"Dynamic loading roadmap" above). Those omissions are what make
Linux's driver model expensive; cutting them keeps the cost
proportional to what embclox actually needs.

## When to revisit

The driver model itself is now Tier 1 in
[gap-analysis.md](./gap-analysis.md) — adopt **as soon as the
scheduler design starts** so subsequent SMP work has clean
abstractions to reason about.

The L2 / L3 dynamic-loading levels become worth revisiting when
**any** of the following becomes true:

- Embclox supports more than ~5 in-tree driver crates and users
  want to opt out of the ones they don't need (→ L2 Cargo
  features).
- A use case demands runtime delivery of new drivers (e.g., a
  field-deployed appliance receiving an OTA update that adds a
  new NIC family without a full firmware reflash) (→ L3).

Neither is in scope today.
