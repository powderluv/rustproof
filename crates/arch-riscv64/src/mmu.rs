//! Sv39 paging enable. (U-mode entry now goes through the trap-frame `interrupts::resume`
//! path so first-run and later resumes share one mechanism; see `docs/scheduling.md`.)
use crate::csr;
use core::arch::asm;

/// Turn on (or switch) Sv39 translation: load `satp` and flush the TLB. Execution
/// continues in the new address space, which MUST map the currently-running kernel.
pub unsafe fn enable_paging(satp: u64) {
    csr::write::<{ csr::SATP }>(satp);
    asm!("sfence.vma", options(nostack, preserves_flags));
}
