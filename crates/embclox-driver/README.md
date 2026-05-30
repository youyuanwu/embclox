# embclox-driver

Bus / driver / device abstraction for the embclox framework.

See [docs/design/driver-model.md](../../docs/design/driver-model.md)
for the design rationale.

## Shape

- `Bus` — enumerates devices on a transport (PCI, VMBus).
- `PciDriver` / `VmBusDriver` — match table + `probe()` per driver.
- `EmbcloxNic` — dyn-safe network device the registry returns.
- `DynNic` — adapter from `Box<dyn EmbcloxNic>` back to
  `embassy_net_driver::Driver`.
- `ProbeCtx` — capabilities passed into each `probe()`.
- `DriverRegistry` — owned `Vec`s of drivers, built in `kernel_main`,
  dropped after probing.
- `register_default_drivers` — registers the three in-tree NICs
  (e1000, tulip, NetVSC).

The crate is `no_std` and depends on the `alloc` crate (the registry
and probe results are heap-allocated; this is fine, embclox has a
`LockedHeap` global allocator).
