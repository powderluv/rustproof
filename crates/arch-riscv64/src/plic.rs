//! PLIC (Platform-Level Interrupt Controller) — the QEMU `virt` board's external-interrupt
//! router, and the RISC-V half of the kernel's console interrupt.
//!
//! Only what the nucleus needs: route UART0 to this hart's SUPERVISOR context, and
//! claim/complete one interrupt at a time. The timer does not come through here — it is the
//! Sstc timer CSR (see `timer.rs`) — so this exists purely to deliver the *quiet* line.
//!
//! Context numbering is the one detail worth stating: on `virt`, hart 0 gets TWO PLIC
//! contexts, 0 for M-mode and 1 for S-mode. The nucleus runs in S-mode, so every register
//! below is the context-1 one. Programming context 0 instead would look correct, enable
//! nothing we can see, and deliver no interrupt at all.

/// MMIO base of the PLIC on the `virt` board.
const PLIC: usize = 0x0C00_0000;
/// UART0's PLIC interrupt source id on `virt`.
pub const UART0_SOURCE: u32 = 10;
/// Per-source priority registers: `PLIC + 4 * source`. Priority 0 means "never deliver".
const PRIORITY: usize = PLIC;
/// Per-context enable bitmaps. Context 1 (hart 0, S-mode) starts at +0x2080.
const ENABLE_CTX1: usize = PLIC + 0x2080;
/// Per-context priority threshold. Context 1 is at +0x20_1000. Deliver anything above it.
const THRESHOLD_CTX1: usize = PLIC + 0x20_1000;
/// Per-context claim/complete register (read to claim, write back to complete).
const CLAIM_CTX1: usize = PLIC + 0x20_1004;

#[inline]
unsafe fn write(addr: usize, val: u32) {
    core::ptr::write_volatile(addr as *mut u32, val);
}

#[inline]
unsafe fn read(addr: usize) -> u32 {
    core::ptr::read_volatile(addr as *const u32)
}

/// Route `source` to this hart's S-mode context: give it a nonzero priority, enable it in
/// the context's bitmap, and drop the context threshold so it is actually delivered.
///
/// # Safety
/// Touches PLIC MMIO; call once at boot.
pub unsafe fn enable_source(source: u32) {
    write(PRIORITY + 4 * source as usize, 1);
    let word = ENABLE_CTX1 + 4 * (source as usize / 32);
    write(word, read(word) | (1 << (source % 32)));
    // Threshold is a strict floor: a source is delivered only when priority > threshold,
    // so 0 here plus priority 1 above is the minimum pair that delivers anything.
    write(THRESHOLD_CTX1, 0);
}

/// Claim the highest-priority pending interrupt for this context, if any. A claim of 0
/// means "nothing pending" — the PLIC's way of saying a spurious external interrupt.
///
/// # Safety
/// Touches PLIC MMIO; call from the external-interrupt handler.
pub unsafe fn claim() -> u32 {
    read(CLAIM_CTX1)
}

/// Tell the PLIC we are finished with `source`, re-arming it. Skipped for a spurious claim
/// (id 0), which the spec says must not be completed.
///
/// # Safety
/// Touches PLIC MMIO; `source` must be the id a matching [`claim`] returned.
pub unsafe fn complete(source: u32) {
    if source != 0 {
        write(CLAIM_CTX1, source);
    }
}
