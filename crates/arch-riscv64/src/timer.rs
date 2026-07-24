//! Supervisor timer (Sstc `stimecmp`) — the interrupt source that drives preemptive
//! scheduling on RISC-V, the analogue of the x86 PIT.
//!
//! The QEMU `virt` machine advertises the Sstc extension (the boot banner lists `sstc`),
//! so S-mode can schedule its own timer interrupts by writing `stimecmp` directly — no SBI
//! `set_timer` call needed. A timer interrupt is pending while `time >= stimecmp`, so
//! writing `stimecmp` forward is both the ack and the schedule of the next tick.
use crate::csr;

/// `time` ticks between interrupts. QEMU `virt` runs `time` at 10 MHz; 25_000 ≈ 400 Hz,
/// chosen so a compute process is sliced roughly per demo `tick` under emulation.
const INTERVAL: u64 = 25_000;

/// Enable the supervisor timer interrupt and arm the first tick. `sstatus.SIE` is left as
/// is (the kernel runs with it clear, non-reentrant); the interrupt still fires in U-mode,
/// where an S-mode interrupt is delivered regardless of `sstatus.SIE`.
pub unsafe fn init() {
    let sie = csr::read::<{ csr::SIE }>();
    csr::write::<{ csr::SIE }>(sie | csr::SIE_STIE);
    rearm();
}

/// Acknowledge the current tick and schedule the next: set `stimecmp = time + INTERVAL`.
/// (Writing it past `time` clears the pending interrupt.)
pub unsafe fn rearm() {
    let now = csr::read::<{ csr::TIME }>();
    csr::write::<{ csr::STIMECMP }>(now + INTERVAL);
}
