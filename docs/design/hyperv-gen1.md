# Design: Hyper-V Gen1 Integration (ReactOS Path)

## Motivation

Hyper-V Gen2 VMs expose only synthetic (VMBus) devices. While the
`embclox-hyperv` crate implements VMBus through synthvid protocol
negotiation, display output remains black — a problem no hobby OS has
solved (only Linux and FreeBSD have working synthvid drivers).

Hyper-V Gen1 VMs emulate legacy PC hardware. ReactOS demonstrates that
a simple OS can run on Gen1 using only legacy devices — VGA, DEC 21140
Tulip NIC, IDE, COM1 — with zero VMBus code. This is the fastest path
to a working Hyper-V demo with visible output and networking.

## Strategy

```
Phase 1: QEMU testing   Limine UEFI + Tulip NIC + serial
                         → full CI validation before real hardware

Phase 2: Hyper-V Gen1   Same image boots on Gen1 VM
                         → VGA text + COM1 named pipe + Tulip

Future:  Gen2 / Azure   VMBus + synthvid + netvsc
                         → production path (embclox-hyperv crate ready)
```

All development and testing is done on QEMU first. QEMU emulates the
DEC 21143 Tulip (`-net nic,model=tulip`), so the Tulip driver can be
fully validated in CI before deploying to Hyper-V.

We use Limine in UEFI mode (not legacy BIOS). This gives a consistent
boot path across QEMU (OVMF) and Hyper-V Gen1 (UEFI firmware). Gen1
supports UEFI boot.

## Gen1 Hardware

Hyper-V Gen1 emulates a legacy PC. These devices are available:

| Device      | Emulation        | PCI ID / Address   | Interface     |
|-------------|------------------|--------------------|---------------|
| Display     | Standard VGA     | ISA, 0xB8000       | Port I/O + memory |
| Network     | DEC 21140 Tulip  | `0x1011:0x0009`    | PCI MMIO      |
| Storage     | IDE/ATA          | ISA, 0x1F0/0x170   | Port I/O      |
| Serial      | 16550 UART       | ISA, 0x3F8 (COM1)  | Port I/O      |
| Keyboard    | i8042 PS/2       | ISA, 0x60/0x64     | Port I/O      |

Gen1 also offers synthetic VMBus devices (netvsc, storvsc) alongside
legacy devices, but the legacy devices work without any Hyper-V
specific code.

> **Azure note**: Azure Gen1 VMs do NOT expose the legacy Tulip NIC.
> PCI scan shows only chipset bridges and a Hyper-V synthetic VGA device:
>
> | Slot | Vendor:Device | Description |
> |------|---------------|-------------|
> | 00:00.0 | 8086:7192 | Intel 440BX PCI bridge |
> | 07:00.0 | 8086:7110 | Intel PIIX4 ISA bridge |
> | 08:00.0 | 1414:5353 | Hyper-V Synthetic VGA |
>
> The NIC on Azure is a VMBus synthetic device (not on PCI bus).
> The Tulip driver works on **local Hyper-V Gen1 only**. Azure
> networking requires the VMBus netvsc stack.

## Architecture

```
┌─────────────────────────────────────────────────┐
│  Application (TCP echo on port 1234)            │
├─────────────────────────────────────────────────┤
│  embassy-net (IP/ARP/TCP via smoltcp)           │
├─────────────────────────────────────────────────┤
│  embclox-core  (Tulip Embassy adapter)          │
├───────────────┬─────────────────────────────────┤
│  embclox-tulip│  embclox-hal-x86                │
│  (Tulip NIC)  │  (serial, PCI, VGA, heap)       │
└───────────────┴─────────────────────────────────┘
        ↕ MMIO (PCI BAR)      ↕ DMA (descriptor rings)
┌─────────────────────────────────────────────────┐
│  Hyper-V Gen1 VM  (legacy PC emulation)         │
└─────────────────────────────────────────────────┘
```

Compare with the QEMU e1000 path:

| Aspect         | QEMU e1000     | QEMU/Hyper-V Tulip |
|----------------|----------------|--------------------|
| Boot           | bootloader v0.11 (BIOS/UEFI) | Limine (UEFI) |
| Display        | Framebuffer via VBE/GOP | Serial + VGA text (Gen1) |
| NIC            | Intel e1000 (PCI) | DEC 21140 Tulip (PCI) |
| Serial         | COM1 stdio     | COM1 stdio / named pipe |
| DMA model      | Descriptor rings | Descriptor rings   |
| Interrupt      | IOAPIC / INTx  | IOAPIC / INTx      |
| QEMU flag      | `-net nic,model=e1000` | `-net nic,model=tulip` |

## Component 1: Bootloader — Limine

### Problem

`bootloader` v0.11 hangs on Gen1 during VBE (VESA BIOS Extensions)
mode setting. Hyper-V's Gen1 VGA does not support VBE — the BIOS call
never returns. This is documented as upstream issues #575, #222.

### Solution

Switch to [Limine](https://github.com/limine-bootloader/limine) in
**UEFI mode**. Limine supports both BIOS and UEFI; we use UEFI for a
consistent boot path across QEMU (OVMF firmware) and Hyper-V Gen1.

Advantages of Limine UEFI:
- Works identically on QEMU and Hyper-V Gen1
- Avoids VBE hang entirely (uses UEFI GOP for framebuffer if needed)
- Gen1 VMs support UEFI boot (Hyper-V Gen1 has both BIOS and UEFI)
- QEMU supports UEFI via OVMF firmware

**Rust integration**: Use the `limine` crate (crates.io) which
provides request/response bindings to the Limine boot protocol.

### Limine boot protocol

Limine uses a request/response model. The kernel declares static
request structures; the bootloader fills in response pointers:

```rust
use limine::request::{MemoryMapRequest, HhdmRequest};

#[used]
#[link_section = ".requests"]
static MEMORY_MAP: MemoryMapRequest = MemoryMapRequest::new();

#[used]
#[link_section = ".requests"]
static HHDM: HhdmRequest = HhdmRequest::new();
```

Key requests for embclox:

| Request             | Purpose                           | Maps to current |
|---------------------|-----------------------------------|------------------|
| `HhdmRequest`       | Higher-half direct map offset     | `phys_offset`    |
| `MemoryMapRequest`  | Physical memory map               | `BootInfo::memory_regions` |
| `StackSizeRequest`  | Kernel stack size                 | Boot config      |
| `EntryPointRequest` | Kernel entry point                | `entry_point!()` |

### Example project structure

```
examples-hyperv-gen1/
├── .cargo/config.toml       # target = x86_64-unknown-none
├── Cargo.toml               # deps: limine, embclox-*
├── limine.conf              # Limine bootloader config
├── linker.ld                # Linker script for Limine protocol
└── src/
    └── main.rs              # Limine entry, HAL init, Tulip + echo
```

### HAL adaptation

The current HAL (`embclox-hal-x86`) depends on `bootloader_api` types
(`BootInfo`, `BootloaderConfig`). For Gen1, we need a thin adaptation
layer:

**Option A — Adapter pattern (recommended)**: Create a
`hal_init_limine()` function in the Gen1 example that extracts
`phys_offset` and memory regions from Limine responses, then calls
into HAL internals (heap init, serial init, memory mapper) directly.

**Option B — Abstract boot info**: Define a `BootParams` struct in
the HAL that both `bootloader` and Limine callers can construct. This
is cleaner but more refactoring.

Start with Option A for speed; migrate to Option B if we add more
bootloader targets. **Note**: Option A requires duplicating some HAL
init logic (heap, serial, memory mapper calls). Document the implicit
contract: Limine's HHDM offset maps to `phys_offset`, and
`kernel_offset` must be derived by probing the heap page tables, same
as the bootloader path. If these diverge, all DMA translations are
silently wrong.

### Boot image

Limine creates bootable UEFI images. The build process:

```
1. cargo build --target x86_64-unknown-none  (kernel ELF)
2. Create FAT partition with:
   - EFI/BOOT/BOOTX64.EFI  (Limine UEFI bootloader)
   - limine.conf            (boot config)
   - kernel.elf             (our kernel)
3. Create GPT disk image with EFI System Partition
```

For QEMU, boot with OVMF:
```
qemu-system-x86_64 -bios OVMF.fd -drive file=disk.img,format=raw \
    -net nic,model=tulip -net user
```

This replaces the current `embclox-mkimage` tool for the Tulip path.
The existing QEMU e1000 path continues to use `bootloader` v0.11 +
`mkimage`.

## Component 2: Display Output

### UEFI GOP Framebuffer (primary)

Since we boot via Limine UEFI, the display is in GOP graphics mode
after `ExitBootServices()`. VGA text mode writes to `0xB8000` will
NOT produce visible output in this state — the VGA controller is in
graphics mode, not text mode.

Instead, use the **GOP framebuffer** with a pixel-font renderer.
The `examples-hyperv` project already has a `framebuffer.rs` module
with a bitmap font renderer that can be reused.

Limine provides framebuffer info via `FramebufferRequest`:

```rust
#[used]
#[link_section = ".requests"]
static FRAMEBUFFER: FramebufferRequest = FramebufferRequest::new();
```

The framebuffer gives a linear pixel buffer (typically 32bpp BGRA).
Render text by blitting bitmap glyphs into this buffer.

### VGA text mode (fallback, BIOS boot only)

If we ever add Limine BIOS boot, VGA text mode at `0xB8000` is
available (80x25, 2 bytes per cell: ASCII + attribute). This is NOT
available after UEFI boot.

### Integration with log crate

Wire the framebuffer console as a secondary log sink alongside serial:
- Serial → stdio / named pipe (machine-readable debug log)
- Framebuffer → QEMU window / VM Connect (human-readable status)

## Component 3: COM1 Serial (Named Pipe)

Already working. Gen1 emulates a real 16550 UART at 0x3F8. The
existing `embclox-hal-x86::serial` module works unchanged. Configure
the named pipe on the host:

```powershell
Set-VMComPort -VMName "embclox-gen1" -Number 1 -Path "\\.\pipe\embclox-serial"
```

Connect with PuTTY (serial, speed 115200) to the named pipe.

## Component 4: DEC 21140 Tulip NIC Driver

### Overview

The DEC 21140 "Tulip" (PCI vendor `0x1011`, device `0x0009`) is a
10/100 Mbps Ethernet controller. Hyper-V Gen1 emulates this chip for
the "Legacy Network Adapter". It is simpler than e1000.

### CSR Register Map

The Tulip uses 16 Control/Status Registers (CSRs), accessed at
8-byte intervals from the PCI BAR base address:

| Offset | CSR  | Name              | Purpose                    |
|--------|------|-------------------|----------------------------|
| 0x00   | CSR0 | Bus Mode          | Software reset, DMA config |
| 0x08   | CSR1 | TX Poll Demand    | Kick TX DMA                |
| 0x10   | CSR2 | RX Poll Demand    | Kick RX DMA                |
| 0x18   | CSR3 | RX Descriptor Base| Physical addr of RX ring   |
| 0x20   | CSR4 | TX Descriptor Base| Physical addr of TX ring   |
| 0x28   | CSR5 | Status            | Interrupt status (R/W1C)   |
| 0x30   | CSR6 | Operation Mode    | TX/RX enable, filtering    |
| 0x38   | CSR7 | Interrupt Enable  | Interrupt mask             |
| 0x40   | CSR8 | Missed Frames     | Error counter              |
| 0x48   | CSR9 | Boot ROM / MII    | Serial ROM / PHY access    |
| 0x58   | CSR11| Timer             | General purpose timer      |
| 0x60   | CSR12| SIA Status        | Media status (10base-T)    |

### Descriptor Format

TX and RX use the same 16-byte descriptor structure:

```rust
#[repr(C, align(4))]
struct TulipDescriptor {
    status: u32,    // OWN bit (31), error/status bits, frame length
    control: u32,   // Buffer sizes, chaining/ring flags
    buf1_addr: u32, // Physical address of buffer 1
    buf2_addr: u32, // Physical address of buffer 2 (or next desc)
}
```

> **32-bit DMA constraint**: Unlike e1000 (which uses 64-bit descriptor
> addresses), the DEC 21140 is a 32-bit PCI device. All DMA buffer
> physical addresses MUST be below 4 GB (`0xFFFF_FFFF`). The current
> `BootDmaAllocator` derives `paddr = heap_vaddr - kernel_offset` from
> the global heap, which on a 64-bit UEFI kernel may return addresses
> above 4 GB. The driver MUST either:
> - Use a dedicated sub-4GB DMA allocator pool, OR
> - Assert `paddr <= 0xFFFF_FFFF` on every allocation and fail
>   gracefully if violated
>
> This is a critical correctness requirement — truncation to `u32`
> causes silent memory corruption.

Key bits:
- `status[31]` (OWN): 1 = NIC owns descriptor, 0 = driver owns
- `control[10:0]` (BUF1_SIZE): Buffer 1 length
- `control[24]` (TER): TX End of Ring (last descriptor wraps)
- `control[25]` (TCH): TX Chained (buf2 → next descriptor)

### Descriptor Ring Layout

```
       TX Ring (CSR4)                    RX Ring (CSR3)
    ┌──────────────┐                 ┌──────────────┐
    │ desc[0]      │ ──→ buf[0]     │ desc[0]      │ ──→ buf[0]
    ├──────────────┤                 ├──────────────┤
    │ desc[1]      │ ──→ buf[1]     │ desc[1]      │ ──→ buf[1]
    ├──────────────┤                 ├──────────────┤
    │ ...          │                 │ ...          │
    ├──────────────┤                 ├──────────────┤
    │ desc[N-1]    │ ──→ buf[N-1]   │ desc[N-1]    │ ──→ buf[N-1]
    │ (TER=1)      │   (wraps)      │ (RER=1)      │   (wraps)
    └──────────────┘                 └──────────────┘
```

### Init Sequence

```
1. PCI: Enable bus mastering (config register 0x04 bit 2)
2. PCI: Read BAR0 → MMIO base address
3. CSR0: Write 0x01 → software reset
4. Poll CSR0 bit 0 until clear, or busy-wait with calibrated loop
   (DEC 21140 has no self-clearing reset bit — read CSR5 to verify
   reset state, or use a bounded loop with ~1000 iterations)
5. CSR0: Write DMA burst config (cache alignment, burst length)
6. Allocate RX descriptor ring + buffers (8–16 descriptors × 2048 bytes)
   Assert all physical addresses < 4 GB (see 32-bit DMA constraint)
7. Allocate TX descriptor ring + buffers (8–16 descriptors × 2048 bytes)
8. Set all RX descriptors: OWN=1, BUF1_SIZE=2048
9. CSR3: Write physical address of RX ring
10. CSR4: Write physical address of TX ring
11. CSR6: Write 0x2002 → enable RX (bit 1) + TX (bit 13)
12. CSR7: Write interrupt mask (TX complete, RX complete, errors)
13. CSR2: Write 1 → start RX polling
```

### TX Path

```
1. Find next free TX descriptor (OWN=0)
2. Copy Ethernet frame to TX buffer
3. Set descriptor: OWN=1, BUF1_SIZE=frame_len, FS=1, LS=1
4. CSR1: Write 1 → kick TX DMA
5. Poll or wait for interrupt: CSR5 TX_COMPLETE bit
```

### RX Path

```
1. Check RX descriptors for OWN=0 (NIC finished writing)
2. Read frame length from descriptor status
3. Copy frame from RX buffer
4. Reset descriptor: OWN=1 → return to NIC
5. CSR2: Write 1 → resume RX polling
```

### Comparison with e1000

| Aspect              | e1000              | DEC 21140 Tulip    |
|----------------------|--------------------|--------------------|
| Registers            | ~100+ MMIO         | 16 CSRs            |
| Descriptor size      | 16 bytes           | 16 bytes           |
| DMA model            | Ring + head/tail   | Ring + poll demand  |
| MAC address          | EEPROM via RAL/RAH | Serial ROM (CSR9)  |
| Link detection       | PHY via MDIO       | SIA (CSR12) or MII |
| Interrupt model      | ICR/IMS registers  | CSR5/CSR7           |
| Complexity           | High               | Low–Medium         |
| Estimated driver LOC | 620 (existing)     | ~800–1000 (new)    |

### MAC Address

The DEC 21140 stores the MAC address in a serial EEPROM accessed via
CSR9. The EEPROM is read 16 bits at a time using a serial protocol:

```
1. CSR9: Send EEPROM read command with address offset
2. Clock out 16-bit data via CSR9 serial interface
3. MAC is stored at EEPROM offset 20 (bytes 0-5)
```

> **Timeout requirement**: The EEPROM bit-bang protocol must have a
> bounded iteration count (e.g., max 1000 clock cycles per word).
> If the emulated EEPROM doesn't respond (possible with chip variant
> differences — see Testing Strategy), return an error rather than
> hanging the kernel at boot.

On Hyper-V, the MAC may also be available via PCI config space or
assigned by the hypervisor. If EEPROM read fails, generate a random
locally-administered MAC (`02:xx:xx:xx:xx:xx`) rather than using a
hardcoded address — hardcoded MACs cause ARP conflicts when multiple
instances run on the same network.

### Crate Structure

```
crates/embclox-tulip/
├── Cargo.toml
└── src/
    ├── lib.rs          # Public API: TulipDevice, init, send, recv
    ├── csr.rs          # CSR register definitions and access
    ├── descriptor.rs   # TX/RX descriptor types, ring management
    └── eeprom.rs       # Serial EEPROM (MAC address) access
```

### Prerequisite: Extract shared DMA traits

`DmaAllocator` trait and `DmaRegion` struct are now in a dedicated
`embclox-dma` crate (~40 LOC, no dependencies). This avoids a
circular dependency: `embclox-core` depends on `embclox-e1000` (for
E1000Device in e1000_embassy.rs), so the trait cannot live in
`embclox-core` without creating a cycle.

```
embclox-dma              ← owns DmaAllocator trait + DmaRegion (40 LOC)
  ↑       ↑       ↑
e1000   tulip    core    ← all depend on embclox-dma for DMA
                  ↑
             BootDmaAllocator impl
```

- `embclox-e1000::dma` re-exports from `embclox-dma` (backward compat)
- `embclox-hyperv` depends on `embclox-dma` directly (not e1000)
- `embclox-tulip` will depend on `embclox-dma` directly
- The 32-bit address constraint is enforced inside `embclox-tulip`
  (assert `paddr < 4GB` after calling the shared allocator), NOT in
  the trait itself — e1000 uses 64-bit descriptors and doesn't need
  the constraint

`RegisterAccess` has different semantics per driver (e1000 uses
word-index offsets, Tulip uses byte offsets at 8-byte intervals).
Each driver defines its own register access internally rather than
sharing a trait. The Tulip driver uses byte-offset CSR constants:

```rust
// In embclox-tulip — byte offsets, not shared trait
const CSR0: u32 = 0x00;
const CSR5: u32 = 0x28;
const CSR6: u32 = 0x30;

fn csr_read(base: *const u32, offset: u32) -> u32 { ... }
fn csr_write(base: *mut u32, offset: u32, value: u32) { ... }
```

### Device struct and teardown

```rust
pub struct TulipDevice<D: DmaAllocator> {
    base: *mut u32,          // MMIO base address
    tx_ring: DmaRegion,
    rx_ring: DmaRegion,
    tx_bufs: [DmaRegion; TX_RING_SIZE],
    rx_bufs: [DmaRegion; RX_RING_SIZE],
    tx_next: usize,
    rx_next: usize,
}
```

> **Drop implementation required**: `TulipDevice` must implement
> `Drop` to safely tear down DMA, following the e1000 pattern:
> 1. Disable interrupts (CSR7 = 0)
> 2. Disable TX/RX (clear CSR6 bits 1 and 13)
> 3. Software reset (CSR0 bit 0)
> 4. Wait for reset completion
> 5. DMA regions are freed when struct fields drop
>
> Without this, the NIC's DMA engine may continue writing to freed
> memory — a hardware-level use-after-free.

### Embassy Adapter

Following the `E1000Embassy` pattern in `embclox-core`. The waker
must be a `static` global (not a struct field) because ISRs can only
access statics:

```rust
// Global static waker — same pattern as e1000
static TULIP_WAKER: AtomicWaker = AtomicWaker::new();

pub struct TulipEmbassy {
    device: TulipDevice<BootDmaAllocator>,
}

impl embassy_net_driver::Driver for TulipEmbassy { ... }
```

The ISR reads CSR5 to acknowledge interrupts, then calls
`TULIP_WAKER.wake()`. The executor polls the device for
completed RX/TX descriptors.

> **CSR5 interrupt handling**: Unlike e1000's ICR (read-to-clear),
> Tulip CSR5 uses write-1-to-clear (W1C). Read CSR5, then write
> the same value back to clear handled bits. To avoid losing
> interrupts that fire between read and write-back, mask CSR7
> before reading CSR5, process, write-back, then unmask CSR7.

## Component 5: Interrupt Routing

Gen1 uses standard PC interrupt routing:

| Device   | IRQ   | IDT Vector | Notes                    |
|----------|-------|------------|--------------------------|
| Timer    | APIC  | 32         | LAPIC timer (existing)   |
| COM1     | IRQ 4 | 36         | Serial (optional)        |
| Tulip    | IRQ N | 33–35      | PCI INTx (from PCI cfg)  |

The Tulip's PCI interrupt line is read from PCI config space offset
0x3C. Route it through the IOAPIC, same as the e1000 interrupt setup
in `examples/src/main.rs`.

## Testing Strategy

All phases are developed and tested on QEMU first. QEMU emulates the
DEC 21143 Tulip (`-net nic,model=tulip`), providing a fast
edit-compile-test loop and CI integration.

> **Chip variant note**: QEMU emulates the DEC 21143 (PCI device
> `0x0019`) while Hyper-V Gen1 emulates the DEC 21140 (PCI device
> `0x0009`). These chips share the core CSR register set and
> descriptor format, but differ in MII PHY management, power
> management, serial ROM format, and some CSR6 mode bits. The
> driver should use only the common subset. Verify the QEMU source
> (`hw/net/tulip.c`) to confirm exact emulation target. Test on
> both `0x0009` and `0x0019` device IDs in the PCI scan.

```
QEMU (dev/CI):   qemu-system-x86_64 -bios OVMF.fd \
                   -net nic,model=tulip -net user \
                   -drive file=disk.img,format=raw \
                   -serial stdio

Hyper-V (later): Same disk image, Gen1 VM with Legacy Network Adapter
```

QEMU tests follow the same pattern as the existing `qemu-tests/`
framework — boot kernel, run test, check serial output, timeout.

## Implementation Plan

### Phase 0: Extract shared DMA traits ✅ DONE

1. Created `crates/embclox-dma/` with `DmaAllocator` trait + `DmaRegion`
2. `embclox-e1000::dma` re-exports from `embclox-dma` (no API break)
3. `embclox-hyperv` depends on `embclox-dma` directly (removed e1000 dep)
4. `embclox-core::dma_alloc` imports from `embclox-dma`
5. **Verified**: All QEMU tests pass, clippy clean

### Phase 1: Limine UEFI Boot on QEMU ✅ DONE

1. Created `examples-tulip/` with Limine UEFI bootloader
2. Minimal kernel: Limine entry → serial init → memory map → framebuffer
3. CMake FetchContent downloads Limine binary release, xorriso builds ISO
4. QEMU test: boots with OVMF + Tulip NIC, serial output verified
5. **Verified**: "BOOT TEST PASSED" in serial output, all 3 ctest tests pass

### Phase 2: Tulip NIC Driver ✅ DONE (~600 LOC)

1. Created `crates/embclox-tulip/` — CSR access (MMIO + I/O port), descriptors, EEPROM, device
2. `CsrAccess` enum supports both MMIO and I/O port modes (QEMU Tulip uses I/O)
3. Init with 32-bit DMA assertions, software reset, descriptor rings
4. EEPROM MAC read with fallback to random locally-administered MAC
5. Drop impl: disable interrupts → disable TX/RX → reset → free DMA
6. QEMU test: PCI scan finds DEC 21143, I/O port CSR access, init passes
7. **Verified**: "TULIP INIT PASSED" in serial, all 3 ctest tests pass

### Phase 3: Embassy Integration ✅ DONE

1. Created `tulip_embassy.rs` adapter with static `TULIP_WAKER` pattern
2. Minimal TSC-based embassy time driver (`time.rs`) with PIT calibration
3. x86 critical-section implementation (`critical_section_impl.rs`)
4. TCP echo server on port 1234 — verified via QEMU hostfwd
5. Tulip interrupt handler with CSR5 W1C mask/unmask pattern
6. HHDM-based DMA allocator for correct physical address translation
7. **Verified**: tulip-echo test passes (TCP echo via Tulip NIC on QEMU)

#### Bugs found and fixed during implementation

| Bug | Root cause | Fix |
|-----|-----------|-----|
| RX descriptor buffer size = 0 | `2048 & 0x7FF = 0` (11-bit overflow) | Use `DESC_BUF_SIZE = 2047` |
| DMA addresses not page-aligned | Bump allocator aligned within heap offset, not absolute | Fixed alignment to use absolute address |
| DMA paddr wrong (NIC reads zeros) | Kernel `.bss` physical mapping differs from `kernel_offset` | Switched to HHDM-based physical memory pool |
| TX packets never sent | `TDES1_FS`/`TDES1_LS` at bits 28/29 instead of 29/30 | Corrected to match DEC 21140 spec |
| Hyper-V serial hang | `LSR.THRE` never set on virtual UART | Bounded spin-wait with 10K iteration limit |
| Hyper-V TSC calibration panic | PIT channel 2 output bit never set | Fallback to default 1 GHz if calibration returns 0 |
| Hyper-V RX not working | Hyper-V virtual switch drops frames with EEPROM MAC ≠ VM-assigned MAC | Enable MAC address spoofing on legacy NIC |
| Hyper-V DHCP never sends | Same MAC mismatch — DHCP DISCOVER sent but response never received | MAC spoofing fix + 21140 setup frame for receive filter |

### Phase 4: Hyper-V Gen1 Deployment

**Completed.** Boot + TCP echo verified on local Hyper-V Gen1.

1. `scripts/hyperv-tulip-test.ps1`: creates Gen1 VM, reads COM1 pipe, probes TCP echo
2. `scripts/mkvhd.sh`: builds bootable VHD image (BIOS+UEFI dual boot)
3. `tests/infra/main.bicep`: Azure Gen1 VM deployment (bicep template)
4. DEC 21140 detected at I/O port `0xec00`, MAC from EEPROM
5. DHCP lease obtained, TCP echo passes on local Hyper-V Gen1
6. **Azure Gen1**: kernel boots but no Tulip NIC on PCI bus (Azure uses VMBus synthetic NIC)

### Actual LOC

| Component | LOC | Description |
|-----------|-----|-------------|
| `embclox-dma` | 40 | Shared DmaAllocator trait + DmaRegion |
| `embclox-tulip` | 509 | NIC driver (CSR, descriptors, device, EEPROM) |
| `embclox-core/tulip_embassy.rs` | 106 | Embassy Driver adapter |
| `examples-tulip` | 752 | Limine boot, PCI, DMA pool, time driver, TCP echo |
| **Total** | **~1400** | |

## Relationship to Existing Code

| Component          | Reused from QEMU e1000 | New for Tulip path  |
|--------------------|-----------------------|---------------------|
| Serial (COM1)      | ✅ `embclox-hal-x86`   | —                   |
| PCI bus scan        | ✅ `embclox-hal-x86`   | —                   |
| IOAPIC + LAPIC     | ✅ `embclox-hal-x86`   | —                   |
| Heap allocator     | ✅ `embclox-hal-x86`   | —                   |
| DMA traits         | ✅ `embclox-core` (extracted from e1000) | —  |
| Embassy executor   | ✅ `embclox-core`      | —                   |
| embassy-net        | ✅ workspace dep       | —                   |
| Framebuffer font   | ✅ `examples-hyperv/framebuffer.rs` | Reuse  |
| Bootloader         | bootloader v0.11       | Limine UEFI (new)   |
| NIC driver         | embclox-e1000          | embclox-tulip (new) |
| Display            | VBE/GOP framebuffer    | GOP framebuffer     |
| VMBus              | — (Gen2 only)          | Not used            |

The `embclox-hyperv` crate (VMBus) is NOT used for this path. It
remains available for future Gen2/Azure work.

The Tulip driver is developed and tested entirely on QEMU first.
Hyper-V Gen1 deployment uses the same kernel binary.

## Decisions

1. **QEMU-first testing**: Yes. All development and CI tests run on
   QEMU with `-net nic,model=tulip`. Hyper-V testing is Phase 4.

2. **Limine UEFI**: Use Limine in UEFI mode for both QEMU (OVMF) and
   Hyper-V Gen1. Consistent boot path, no BIOS/VBE issues.

3. **Tulip only**: Use the DEC 21140 Tulip legacy NIC. Do not detect
   or use netvsc (VMBus synthetic network). Keep the stack simple —
   one NIC driver, no VMBus dependency for Gen1.

4. **GOP framebuffer over VGA text**: UEFI boot puts the display in
   graphics mode. Use GOP framebuffer with pixel-font rendering
   instead of VGA text mode. Reuse `framebuffer.rs` from
   `examples-hyperv`.

5. **DMA traits in `embclox-dma`**: Extracted `DmaAllocator`/`DmaRegion`
   to a dedicated micro-crate (`embclox-dma`, 40 LOC). Avoids circular
   dependency between `embclox-core` ↔ `embclox-e1000`. Both drivers
   and `embclox-core` depend on `embclox-dma`. Each driver defines its
   own register access internally (different offset semantics).

6. **32-bit DMA safety**: Assert all DMA physical addresses < 4 GB.
   The DEC 21140 is a 32-bit PCI device; truncation to `u32` causes
   silent memory corruption.

7. **Static waker pattern**: Use `static TULIP_WAKER: AtomicWaker`
   (same as e1000), not a struct field. ISRs can only access statics.

## References

- [DEC 21140 Hardware Reference Manual](https://www.intel.com/Assets/PDF/datasheet/278818.pdf)
- [OSDev Wiki — Tulip](https://wiki.osdev.org/DEC_Tulip)
- [Linux kernel — drivers/net/ethernet/dec/tulip/](https://github.com/torvalds/linux/tree/master/drivers/net/ethernet/dec/tulip)
- [ReactOS — Tulip driver](https://github.com/nicabar/reactos/tree/master/drivers/network/dd/dc21x4)
- [Limine bootloader](https://github.com/limine-bootloader/limine)
- [limine crate (Rust)](https://crates.io/crates/limine)
- [Limine Rust kernel template](https://github.com/limine-bootloader/limine-kernel-template/tree/rust)
