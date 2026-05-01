# Driver model: bus / driver / device abstraction

## Status: proposal

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
    type Device;
    fn enumerate(&mut self) -> &[Self::Device];
}
```

The associated `Device` type is intentionally bus-specific — a
`PciDevice` carries `(bus, dev, func, vendor_id, device_id, BARs…)`
while a `VmBusDevice` carries `(class_guid, instance_guid,
ChannelHandle)`. Forcing a single `Device` enum would either lose
information or require every driver to downcast.

### `DeviceInfo`

A small "what is this" trait every bus device implements so the
registry can log uniformly without caring about the bus:

```rust
pub trait DeviceInfo {
    fn bus_name(&self) -> &'static str;       // "pci" | "vmbus"
    fn id(&self) -> heapless::String<32>;     // "0000:00:03.0" | "f8615163-…"
    fn class(&self) -> DeviceClass;            // Net | Storage | Display | Other
}
```

### `Driver`

A driver is a singleton describing what it can claim and how to bring
it up. Each bus has its own driver trait because `probe()` needs the
bus's concrete device type:

```rust
pub trait PciDriver: Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, dev: &PciDevice) -> bool;
    fn probe(&self, dev: PciDevice, ctx: &mut ProbeCtx) -> Result<Box<dyn NetDevice>, ProbeError>;
}

pub trait VmBusDriver: Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, dev: &VmBusDevice) -> bool;
    fn probe(&self, dev: VmBusDevice, ctx: &mut ProbeCtx) -> Result<Box<dyn NetDevice>, ProbeError>;
}
```

`ProbeCtx` carries the per-driver capabilities the probe might need:
the DMA allocator, the `MemoryMapper`, the `IoApic` handle (to wire
its IRQ), and a callback to register an ISR vector. This avoids
making each driver `unsafe` reach into globals.

### `NetDevice`

We already have `embassy_net_driver::Driver` impls for all three NICs
(`E1000Embassy`, `TulipEmbassy`, `NetvscEmbassy`). The abstraction
just collects them behind `Box<dyn embassy_net_driver::Driver>` —
nothing new.

### Registry

No allocator at link time and no `inventory` crate (it depends on
`ctor`/`linkme`-style tricks that are platform-fragile in `no_std`).
Instead, a plain inherent constructor builds the driver list:

```rust
pub fn driver_registry() -> DriverRegistry {
    DriverRegistry {
        pci: &[
            &embclox_e1000::driver::E1000_DRIVER,
            &embclox_tulip::driver::TULIP_DRIVER,
        ],
        vmbus: &[
            &embclox_hyperv::driver::NETVSC_DRIVER,
        ],
    }
}
```

Each driver crate exposes a `pub static FOO_DRIVER: FooDriver = …`.
Adding a fourth NIC requires editing this one function — explicit
and grep-able, which is appropriate at this scale.

## Boot flow

```text
kernel_main(boot_info)
  ├─ embclox_hal_x86::init(...)         // existing — heap, paging, PCI handle
  ├─ idt::init() + pic::disable()
  ├─ map LAPIC + IOAPIC                 // existing
  ├─ start_apic_timer(...)              // existing
  │
  ├─ ProbeCtx { dma, memory, ioapic, irq_alloc }
  │
  ├─ for each PciDriver in registry:
  │     for each PciDevice in pci.enumerate():
  │         if drv.matches(dev): nics.push(drv.probe(dev, ctx)?)
  │
  ├─ if hyper-v hypervisor present:    // CPUID "Microsoft Hv"
  │     vmbus = VmBus::init(ctx)?
  │     for each VmBusDriver in registry:
  │         for each VmBusDevice in vmbus.enumerate():
  │             if drv.matches(dev): nics.push(drv.probe(dev, ctx)?)
  │
  ├─ pick primary NIC (see Multi-NIC)
  ├─ embassy_net::Stack::new(primary, …)
  └─ runtime::run_executor(...)
```

All existing setup before the registry loop is unchanged. The new
work is the loops and the trait impls (`E1000_DRIVER`,
`TULIP_DRIVER`, `NETVSC_DRIVER`) that wrap each crate's existing
`new`/`init` constructor.

## Bus detection (Hyper-V)

VMBus initialisation is expensive and crashes on bare hardware, so we
gate it on the Hyper-V CPUID leaves:

```rust
fn is_hyperv() -> bool {
    let cpuid = raw_cpuid::CpuId::new();
    cpuid.get_hypervisor_info()
        .map(|h| h.identify() == raw_cpuid::HypervisorInfo::HyperV)
        .unwrap_or(false)
}
```

Linux does the same (`hv_is_hyperv_initialized()` in
`drivers/hv/hv_common.c`) and refuses to probe VMBus drivers
otherwise.

## ISR registration

Each driver's `probe()` may need to install an interrupt handler.
Today examples do this inline with `idt::set_handler(33, my_isr)` and
`ioapic.enable_irq(line, 33, 0)` at known vectors. With multiple
drivers we need allocation:

```rust
pub struct ProbeCtx<'a> {
    pub dma: &'a dyn DmaAllocator,
    pub memory: &'a mut MemoryMapper,
    pub ioapic: &'a mut IoApic,
    pub irq_alloc: &'a mut VectorAllocator,   // hands out 33..=47
}

impl ProbeCtx<'_> {
    pub fn install_pci_isr(
        &mut self,
        line: u8,                              // PCI interrupt line (config 0x3C)
        handler: extern "x86-interrupt" fn(InterruptStackFrame),
    ) -> Result<u8, ProbeError> {
        let vec = self.irq_alloc.allocate()?;
        unsafe { idt::set_handler(vec, handler); }
        self.ioapic.enable_irq(line, vec, 0);
        Ok(vec)
    }
}
```

The `extern "x86-interrupt"` handler still has to be a static `fn`
(no closures) because IDT entries are raw function pointers. Each
driver therefore exposes a static `extern "x86-interrupt" fn` and a
static `Waker` slot it pokes — same pattern as today's
`e1000_handler` + `NET_WAKER`.

Hyper-V is exempt: its single SINT2 ISR is already installed by
`examples-hyperv` before VMBus init, and all NetVSC channels share
that one vector.

## Multi-NIC

If both an e1000 and a tulip card are present, we probe both — they
both succeed and produce a `Box<dyn embassy_net_driver::Driver>`
each. But embassy-net's `Stack` is single-driver:

```rust
impl<D: Driver, const N: usize> Stack<D, N> { ... }
```

So we pick **one primary NIC** for the stack and log the others as
"detected but unused":

```rust
let primary = nics.iter().min_by_key(|n| n.priority()).unwrap();
```

Priority is a small per-driver constant (e.g. `NetvscDriver` = 10,
`E1000Driver` = 20, `TulipDriver` = 30) — lower wins. The Hyper-V
synthetic NIC always wins when present because that's the one with
working DMA in a Hyper-V guest.

True multi-homed routing (one stack per NIC, route policy table) is
deferred — it is a much larger change touching the IP stack and is
not needed for any current example.

## Lifecycle

Drivers do not implement `Drop`-driven teardown. Once probed, they
live for the program's lifetime — same as today. This keeps the
ownership story simple (`nics: Vec<Box<dyn …>>` lives in `kmain`'s
stack frame) and matches the bare-metal "we never shut down" model.

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
- `examples-e1000` / `examples-tulip` / `examples-hyperv`. They stay
  as small, single-driver references that newcomers can read in one
  sitting.

## Migration path

Three phases, each independently shippable:

1. **Define traits + extract constructors.** Add `crates/embclox-driver`
   with `Bus`, `PciDriver`, `VmBusDriver`, `ProbeCtx`,
   `VectorAllocator`. No examples change. Each driver crate gains a
   `driver` module exposing `pub static FOO_DRIVER: FooDriver`.

2. **Add `examples-kernel`** that uses the registry. Existing
   `examples-*` crates are untouched. CI grows one more `qemu` test:
   boot `examples-kernel` with `-device e1000`, then with
   `-device tulip`, expect both to print the matching driver name and
   a TCP echo.

3. **(Optional) Retire single-driver examples.** Only worth doing if
   the unified binary becomes the canonical reference. We probably
   keep them as documentation regardless.

## Trade-offs

| Cost | Mitigation |
|------|------------|
| ~600 LoC of new abstraction | One-time; pays back at the third NIC. |
| Per-driver ISRs still need static hooks (Rust IDT limitation) | Same as today. |
| Driver registry edited by hand | Explicit > clever at this scale. `inventory`-style auto-registration adds linker fragility. |
| `Box<dyn …>` requires `alloc` | We already have `LockedHeap` in HAL; one `Box` per NIC is trivial. |
| Probe order is registry order, not deterministic by topology | Document that the first match wins; matches Linux behaviour for `pci_register_driver`. |

## Comparison to Linux

| Concept | Linux | embclox proposal |
|---------|-------|------------------|
| Device tree | `struct device` in `/sys/devices` | `DeviceInfo` trait + per-bus device structs |
| Bus | `struct bus_type` (PCI, USB, VMBus, …) | `Bus` trait, one impl per transport |
| Driver registration | `module_pci_driver()` macro → linker section | `pub static` in driver crate, listed in `driver_registry()` |
| Match tables | `MODULE_DEVICE_TABLE(pci, …)` | `Driver::matches(&dev) -> bool` (code, not data — gives flexibility for class-code matches) |
| Probe | `drv->probe(dev)` | `Driver::probe(dev, &mut ProbeCtx)` |
| net subsystem | `register_netdev()` → `struct net_device` | `Box<dyn embassy_net_driver::Driver>` returned from probe |
| Hot-plug | uevent, `kobject_uevent()` | not supported |
| Module loading | `insmod`, `request_module()` | not supported (everything statically linked) |

The proposal is intentionally a strict subset: same shapes, none of
the dynamic loading, hot-plug, or sysfs surface area. Those are what
make Linux's driver model expensive; cutting them keeps the cost
proportional to what embclox actually needs.

## When to revisit

Adopt this when **any** of the following becomes true:

- Three or more drivers per bus (e.g. virtio-net joins the PCI list).
- We want a "live image" that boots unmodified on QEMU and Hyper-V
  for distribution / Azure marketplace experiments.
- A driver needs to be optional at runtime (e.g. graphics support
  only on Gen2 UEFI).

Until then, the per-example layout is the right answer.
