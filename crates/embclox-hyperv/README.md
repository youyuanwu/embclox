# embclox-hyperv

Hyper-V VMBus + NetVSC driver for bare-metal `no_std` x86_64 kernels.
Generic over `embclox_dma::DmaAllocator` and
`embclox_hal_x86::memory::MemoryMapper`.

## What this provides

| Module | Purpose |
|--------|---------|
| `detect` | CPUID Hyper-V detection (returns `None` on QEMU/bare metal) |
| `hypercall` | Hypercall page setup + `HvPostMessage` / `HvSignalEvent` |
| `synic` | SynIC SIMP/SIEFP setup + SINT2 ISR plumbing + `wait_for_match` future |
| `vmbus` | INITIATE_CONTACT + REQUEST_OFFERS handshake; channel offer enumeration |
| `channel` | GPADL alloc, OPENCHANNEL, ring-buffer send/recv, `WaitForPacket` future |
| `netvsc` | NVSP version negotiation + recv/send buffer GPADLs |
| `netvsc` (RNDIS) | RNDIS_INITIALIZE + MAC/MTU OID queries + packet filter |
| `netvsc_embassy` | `embassy_net_driver::Driver` impl wrapping `NetvscDevice` |
| `synthvid` | Synthetic graphics device (currently unused) |

## API surface

```rust
let mut vmbus = embclox_hyperv::init(&dma, &mut memory)?;
for offer in vmbus.offers() { ... }

let netvsc = embclox_hyperv::netvsc::NetvscDevice::init(
    &mut vmbus, &dma, &memory)?;

// Hand to embassy:
let driver = embclox_hyperv::netvsc_embassy::NetvscEmbassy::new(netvsc);
let (stack, runner) = embassy_net::new(driver, config, resources, seed);
```

Caller is responsible for:
- Mapping the LAPIC + starting the APIC periodic timer
  (`embclox_hal_x86::runtime::start_apic_timer`)
- Installing `vmbus_isr` at `msr::VMBUS_VECTOR` (= 34) **before**
  `embclox_hyperv::init` runs — the SINT2 IRQ is what wakes the
  internal `block_on_hlt` runner from `hlt` while waiting for host
  responses

See `examples-kernel/src/main.rs` for the canonical setup order.

## Boot-time waits

VMBus / NetVSC / Synthvid init no longer busy-spin. Sync entry
points (`init`, `NetvscDevice::init`, `recv_with_timeout`) drive
async cores via `embclox_hal_x86::runtime::block_on_hlt` so the CPU
sleeps in `hlt` between SINT2 IRQs.

## Bindgen FFI

Protocol headers under `include/` are sourced from Microsoft's
[mu_msvm](https://github.com/microsoft/mu_msvm) UEFI VM firmware
(BSD-2-Clause-Patent). `build.rs` runs bindgen at compile time;
generated bindings live in `src/ffi.rs`. Building requires `bindgen`
and `libclang-dev` on the host.

See `include/README.md` for protocol-header provenance.
