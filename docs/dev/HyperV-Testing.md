# Hyper-V Test Networking

This doc explains how the Hyper-V test infrastructure is configured and
why we **don't** use the Default Switch.

## TL;DR

Use the dedicated `embclox-test` Internal vSwitch. Run
`scripts/hyperv-setup-vswitch.ps1` once as Administrator. Thereafter
`scripts/hyperv-boot-test.ps1` and `scripts/hyperv-tulip-test.ps1` will
attach test VMs to it automatically.

The host-side IP on `vEthernet (embclox-test)` is `192.168.234.1/24`;
test VMs use static `192.168.234.50/24`. There is no DHCP server on
this switch — tests don't need one (see the bottom of this doc for
why DHCP testing belongs on QEMU).

## Why not the Default Switch

Hyper-V's Default Switch uses Windows **Internet Connection Sharing
(ICS)** to NAT VM traffic to the host's primary network adapter. ICS
has a documented design behavior that breaks repeated VM testing on a
long-lived dev host:

> When ICS is enabled, the Windows host pre-installs Permanent ARP
> entries for the entire NAT scope, all pointing at the host vNIC's
> MAC. ARP requests for any IP in the shared subnet are answered by
> the Windows host. (See ServerFault Q348996 and SuperUser Q511117.)

That alone is fine if the vNIC MAC never changes. **But it does
change** — Windows Updates, sleep/wake cycles, and other events cause
Windows to re-create the Default Switch vNIC with a new MAC. The new
MAC adds new Permanent entries; **the old ones are never cleaned up**.

Over time, this accumulates into thousands of stale Permanent entries.
On the dev host where this was first investigated:

```text
> Get-NetNeighbor -InterfaceAlias 'vEthernet (Default Switch)' |
    Group-Object State | Format-Table Name, Count

Name      Count
----      -----
Permanent  4114
```

Of those 4114 entries, **4092 pointed at a single MAC `00-15-5D-0A-7C-C3`**
covering the **entire 172.19.192.0/20 NAT scope** (4096 addresses).
The current vNIC MAC at the time of investigation was `00-15-5D-6A-E8-72`
— so `c3` was an older host vNIC MAC from a prior incarnation of the
Default Switch that Windows had since recreated.

### Symptoms

- Test VMs configured with **any static IP in the NAT scope** have their
  IP shadowed by a stale Permanent entry. TCP from the host times out
  because traffic is sent to a non-existent MAC.
- Gratuitous ARP from the test VM **cannot override Permanent neighbor
  entries** by design (RFC 5227 only requires unsolicited ARP to update
  Dynamic entries; Windows enforces this strictly).
- DHCP-leased IPs **may** work briefly if the lease happens to fall on
  one of the few un-poisoned IPs (verified once on this host: a NetVSC
  VM successfully leased `172.19.199.98`). But the next test cycle
  often gets a different IP, and the chance of that one being poisoned
  is ~99% on a polluted host.
- Cleaning up requires `Remove-NetNeighbor -Confirm:$false` thousands
  of times — and only an Administrator-elevated shell can do it.

### Confirmed: this is NOT caused by our code

The pollution is purely host-side ICS state. Several pieces of
evidence confirmed this:

1. The same Default Switch vNIC's stale ARP entries exist with no
   relation to current VM activity. Stopping/removing all VMs does
   not clean them up.
2. Compared with the WSL2 Internal vSwitch (`vEthernet (WSL (Hyper-V
   firewall))`) — also `/20`, also Hyper-V Internal — which has only
   7 ARP entries (5 multicast + 1 broadcast + 1 reachable). No
   pollution because it doesn't run ICS.
3. The poisoned MAC `c3` is not any current VM MAC. It belongs to the
   `00-15-5D-XX-XX-XX` Hyper-V vNIC pool that Hyper-V allocates to both
   guest NICs and host vNICs.
4. The same kernel binary booted on **Azure Gen1** completes DHCP
   normally and gets `10.0.0.4/24` from Azure's production DHCP
   server. Azure has no ICS in front of the VM. This proves the guest
   side of the DHCP exchange is correct end to end — the failure on
   local Hyper-V is purely the host's ICS DHCP/ARP layer choosing not
   to reply.

### Not the cause: an unrelated time-driver bug

While debugging this we also found an embassy-time bug in
`embclox-hal-x86::time` where two `static ApicTimeDriver` instances
existed: the one registered by `time_driver_impl!` (which embassy
wrote alarm wakers into) and a second standalone `DRIVER` (which
`set_tsc_per_us`/`on_timer_tick` mutated). Effect: any
`embassy_time::Timer::after_*().await` blocked forever because
embassy registered wakers in one alarm table and our tick handler
inspected a different one.

This was hidden on local Hyper-V because the Default Switch DHCP
issue prevented the executor from ever reaching the timer-dependent
code path. It only surfaced once Azure DHCP succeeded and the
`echo_task` sat in its `stack.config_v4()` poll loop calling
`Timer::after_millis(100).await`. Fixed by removing the duplicate
static; CI still passes.

The two issues are independent:
- Time-driver fix doesn't unblock local Hyper-V DHCP (still no
  OFFER comes back through pump_channel).
- ICS pollution doesn't affect Azure (Azure runs its own DHCP).

## Why a dedicated Internal vSwitch fixes it

`scripts/hyperv-setup-vswitch.ps1` creates `embclox-test`:

- **Type: Internal.** Allows host↔VM traffic, no external NAT.
- **No ICS attached.** Therefore no Permanent ARP pre-population.
- **Static host IP** (`192.168.234.1/24`) assigned by the setup script,
  static VM IPs in the same `/24` configured by the example kernel.
  Pure layer-2 + ARP, no DHCP.
- **Re-runnable.** Re-running the setup script resets the host's IP
  binding without affecting the switch's neighbor table (which stays
  empty because nothing pre-populates it).

Because there's no ICS NAT, the host's neighbor table for this
interface only contains entries that ARP traffic actually populates,
and those are Dynamic — they get refreshed/replaced by gratuitous ARP
from new test VMs.

## Setup

One-time:

```powershell
# In an elevated PowerShell window, from the repo root
powershell.exe -ExecutionPolicy Bypass -File scripts/hyperv-setup-vswitch.ps1
```

Verify:

```powershell
Get-VMSwitch -Name 'embclox-test'
Get-NetIPAddress -InterfaceAlias 'vEthernet (embclox-test)' -AddressFamily IPv4
```

After that, the test scripts work without elevation:

```powershell
powershell.exe -ExecutionPolicy Bypass -File scripts/hyperv-boot-test.ps1 \
  -Iso build/hyperv.iso
```

## Where DHCP testing belongs

DHCP-via-embassy/smoltcp is **already covered by the Tulip QEMU test**:

```sh
# QEMU SLIRP user network — has a built-in, well-behaved DHCP server.
ctest --test-dir build -R tulip-echo
```

QEMU SLIRP DHCP is the right place to exercise the smoltcp DHCP code
path because:

- SLIRP is a standard, predictable DHCP server (same code that Linux
  test farms use).
- It has none of the ICS pre-allocation behavior.
- The test runs on every CI build with no host-state dependency.

For Hyper-V we test the **NIC driver and embassy adapter** path with a
known-good static configuration, not the DHCP client. This is a
deliberate split:

| Test target | Exercises | Where |
|-------------|-----------|-------|
| Tulip QEMU | smoltcp + embassy + DHCP + Tulip MMIO | CI (qemu-system-x86_64) |
| Tulip Hyper-V (legacy NIC) | Tulip MMIO + Hyper-V vSwitch L2 | dev (manual) |
| NetVSC Hyper-V | VMBus + NVSP + RNDIS + embassy | dev (manual) |
| NetVSC Azure | VMBus + Azure DHCP + production stack | future (`tests/infra/main.bicep`) |

## Switching to DHCP at boot time

The hyperv example reads the Limine kernel command line at boot and
selects the network mode from it. `examples-hyperv/limine.conf` ships
two boot menu entries:

| Entry | cmdline | Network mode |
|-------|---------|--------------|
| `/embclox Hyper-V Example (static IP 192.168.234.50/24)` (default) | (empty) | static `192.168.234.50/24` gw `192.168.234.1` |
| `/embclox Hyper-V Example (DHCP)` | `net=dhcp` | embassy-net DHCPv4 |

Recognised cmdline tokens (whitespace-separated):

| Token | Meaning |
|-------|---------|
| `net=dhcp` | DHCPv4 |
| `net=static` | Static (use defaults or `ip=`/`gw=` overrides) |
| `ip=A.B.C.D/N` | Override static IPv4 + prefix |
| `gw=A.B.C.D` | Override static gateway |

To test the DHCP path on Hyper-V, either:
- Pick the second boot menu entry interactively at the Limine prompt
  (3-second timeout — press a key to interrupt the auto-boot), or
- Edit `examples-hyperv/limine.conf` to put the DHCP entry first, or
- Run on Azure where DHCP is production-grade.

Without a DHCP server on the chosen vSwitch the DHCP path will hang
in `IPv4: DOWN` forever — that's the expected behavior, not a bug.

## Future: real DHCP on Hyper-V

If we ever need to exercise DHCP-against-NetVSC specifically (e.g., to
catch a regression in our RNDIS path that only DHCP would notice), the
two practical options are:

1. **Run dnsmasq in WSL** bound to `vEthernet (embclox-test)` —
   lightweight, contained, no host state pollution.
2. **Run the test in Azure** via `tests/infra/main.bicep` — Azure has
   a production-grade DHCP server that doesn't have ICS's quirks.

Neither is set up today; static IP is sufficient for current coverage.

## References

- [ServerFault: Why does Windows ICS answer ARP requests for all addresses in its subnet?](https://serverfault.com/questions/348996)
- [SuperUser: Why does `arp -e` show ICS host MAC for all clients?](https://superuser.com/questions/511117)
- [Microsoft Docs: Internet Connection Sharing](https://learn.microsoft.com/en-us/windows/win32/ics/using-internet-connection-sharing)
- RFC 5227 §2.1.1 — gratuitous ARP cannot override Permanent neighbor entries.
