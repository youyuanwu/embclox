# Unified kernel example

Boots whatever NIC is present (e1000, tulip, or NetVSC) via the
`embclox-driver` registry. Designed to replace per-NIC example
binaries with a single canonical reference.

See [docs/design/driver-model.md](../docs/design/driver-model.md).

## Boot flow

1. Limine hands off to `kmain`.
2. `embclox_hal_x86::init` brings up serial / heap / paging.
3. IDT, PIC disable, LAPIC enable, APIC timer started.
4. If running on Hyper-V (`embclox_hyperv::detect`), the SINT2 ISR
   is installed and `embclox_hyperv::init` brings up VMBus
   (non-fatal: misdetection demotes to `None` and the PCI loop
   still runs).
5. `DriverRegistry::new()` + `register_default_drivers()` register
   the three in-tree NIC families.
6. `embclox_driver::probe_all` walks PCI (and VMBus offers if
   available); first-successful-probe-wins per device with hard
   `PROBE_BUDGET` cap.
7. Lowest-priority `ProbedNic` (NetVSC=10, e1000=20, tulip=30) is
   wrapped in `DynNic` and handed to `embassy_net::Stack`.
8. TCP echo server on port 1234.

## Running

```bash
cmake --build build --target qemu-kernel              # e1000
cmake --build build --target qemu-kernel-tulip        # tulip
ctest -R kernel-echo --test-dir build                 # CI
```
