//! Application-side embassy tasks for the example kernel.
//!
//! Everything in this module runs *inside* an embassy executor:
//!
//! - [`net_task`] drives the [`embassy_net`] `Runner`.
//! - [`echo_task`] is the user-visible TCP echo server on port 1234.
//! - [`ap_heartbeat_task`] is the per-AP keepalive task driven by
//!   each AP's APIC timer + per-CPU `embassy-time` alarm slot.
//!
//! The [`ap_entry`] thunk is the AP-side bridge between the HAL's
//! [`embclox_hal_x86::smp`] bring-up primitives and an embassy
//! executor running [`ap_heartbeat_task`]; it stays here because it
//! owns the [`AP_EXECUTORS`] table that the AP task pool runs in.
//!
//! `kmain` lives in [`crate`] (the binary root) and only this module's
//! public surface needs to be visible to it.

use embassy_net::Stack;
use embclox_driver::DynNic;
use embedded_io_async::Write as AsyncWrite;
use log::*;
use static_cell::StaticCell;

/// Per-AP heartbeat counter. Bumped by [`ap_heartbeat_task`] running
/// inside each AP's embassy executor. The test harness greps for
/// "AP N alive" log lines plus the final counter dump to verify each
/// AP entered Rust, brought up its own executor, and is making
/// timer-driven async progress.
pub(crate) static AP_COUNTERS: [core::sync::atomic::AtomicUsize;
    embclox_hal_x86::cpu_local::MAX_CPUS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; embclox_hal_x86::cpu_local::MAX_CPUS];

/// One embassy `Executor` per possible AP slot. Indexed by
/// `processor_id - 1` (BSP has its own `EXECUTOR` static in `kmain`).
/// Each AP's [`ap_entry`] calls `.init(...)` on its slot exactly once,
/// then hands the resulting `&'static Executor` to `run_executor`.
static AP_EXECUTORS: [StaticCell<embassy_executor::raw::Executor>;
    embclox_hal_x86::cpu_local::MAX_CPUS - 1] =
    [const { StaticCell::new() }; embclox_hal_x86::cpu_local::MAX_CPUS - 1];

/// Per-AP heartbeat task. Wakes on every `Timer::after_millis(10)`
/// expiry — i.e. driven by the AP's own APIC timer ISR + per-CPU
/// `embassy-time` alarm slot — and bumps `AP_COUNTERS[processor_id]`.
/// Over the BSP's 100 ms `SMP CHECK` window we expect ~10 ticks per
/// AP; the ctest lane only checks for `> 0`.
///
/// `pool_size = 8` must be `>= MAX_CPUS` so every AP can spawn its
/// own instance. The const assert below enforces it.
#[embassy_executor::task(pool_size = 8)]
async fn ap_heartbeat_task(processor_id: u8) {
    const _: () = assert!(8 >= embclox_hal_x86::cpu_local::MAX_CPUS);
    loop {
        AP_COUNTERS[processor_id as usize].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        embassy_time::Timer::after_millis(10).await;
    }
}

/// AP entry thunk. Limine calls this with `&Cpu` after writing
/// `cpu.goto_address`; we reconstruct `ApInit` from the per-CPU
/// `extra` field + shared params, finish per-CPU setup, then bring
/// up this AP's own embassy executor and spawn the heartbeat task.
pub(crate) unsafe extern "C" fn ap_entry(cpu: &embclox_hal_x86::limine_boot::limine::mp::Cpu) -> ! {
    let init = embclox_hal_x86::smp::ap_init_from(cpu);
    // Safety: AP context, called once per AP from Limine.
    unsafe { embclox_hal_x86::smp::ap_setup(init) };

    let processor_id = match init.cpu_id {
        embclox_hal_x86::vector_alloc::CpuId::Bsp => unreachable!("ap_entry on BSP"),
        embclox_hal_x86::vector_alloc::CpuId::Ap(n) => n,
    };
    info!(
        "AP {} alive (apic_id={}, tsc/us={})",
        processor_id, init.apic_id, init.tsc_per_us
    );

    // Bring up this AP's embassy executor and spawn the heartbeat
    // task. After this, the AP runs the canonical poll/hlt loop
    // exactly like the BSP — its APIC timer wakes it, embassy polls,
    // the heartbeat task ticks its counter, repeat.
    let executor = AP_EXECUTORS[(processor_id - 1) as usize]
        .init(embassy_executor::raw::Executor::new(core::ptr::null_mut()));
    executor
        .spawner()
        .spawn(ap_heartbeat_task(processor_id).expect("ap_heartbeat_task SpawnToken"));
    embclox_hal_x86::runtime::run_executor(executor);
}

/// Drives the [`embassy_net`] `Runner` (background `poll` /
/// link-state task that smoltcp needs to make progress).
#[embassy_executor::task]
pub(crate) async fn net_task(mut runner: embassy_net::Runner<'static, DynNic>) {
    runner.run().await
}

/// User-visible TCP echo server. Blocks until the stack has an IPv4
/// address, prints the SMP heartbeat snapshot (so the `ctest -L smp`
/// lane has something to grep for), then accepts/echoes forever on
/// port 1234.
#[embassy_executor::task]
pub(crate) async fn echo_task(stack: &'static Stack<'static>) {
    // Wait for an IPv4 address (immediate for static, ~1-3s for DHCP).
    loop {
        if let Some(cfg) = stack.config_v4() {
            // Marker for scripts/hyperv-boot-test.ps1 — string format
            // inherited from the retired examples-hyperv so the PS
            // test stayed unchanged through Phase 3c.
            info!("PHASE4B: IPv4 configured: {}", cfg.address);
            if let Some(gw) = cfg.gateway {
                info!("PHASE4B: gateway: {}", gw);
            }
            break;
        }
        embassy_time::Timer::after_millis(100).await;
    }
    // Marker for scripts/hyperv-boot-test.ps1.
    info!("PHASE4B ECHO READY: TCP port 1234");

    // If any APs were brought up, give them a beat to tick a few times
    // then dump their heartbeat counters so the SMP ctest lane can
    // assert each AP is alive.
    embassy_time::Timer::after_millis(100).await;
    let mut counts: heapless::Vec<usize, { embclox_hal_x86::cpu_local::MAX_CPUS }> =
        heapless::Vec::new();
    for counter in AP_COUNTERS.iter() {
        let _ = counts.push(counter.load(core::sync::atomic::Ordering::Relaxed));
    }
    info!("SMP CHECK: ap_counters={:?}", &counts[..]);

    let mut rx = [0u8; 1024];
    loop {
        let mut tx = [0u8; 1024];
        let mut socket = embassy_net::tcp::TcpSocket::new(*stack, &mut rx, &mut tx);
        socket.set_timeout(None);
        if socket.accept(1234).await.is_err() {
            continue;
        }
        info!("kernel-example: tcp client connected");
        loop {
            let mut data = [0u8; 256];
            match socket.read(&mut data).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if socket.write_all(&data[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        info!("kernel-example: tcp client disconnected");
    }
}
