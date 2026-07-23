//! Control-register access.
use core::arch::asm;

/// Read CR3 (the physical address of the active PML4, plus flags in the low bits).
#[inline]
pub unsafe fn read_cr3() -> u64 {
    let v: u64;
    asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags));
    v
}

/// Load CR3 — switch the active address space. Flushes the non-global TLB.
#[inline]
pub unsafe fn write_cr3(v: u64) {
    asm!("mov cr3, {}", in(reg) v, options(nostack, preserves_flags));
}
