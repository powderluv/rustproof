//! Sv39 paging enable. (U-mode entry now goes through the trap-frame `interrupts::resume`
//! path so first-run and later resumes share one mechanism; see `docs/scheduling.md`.)
use crate::csr;
use core::arch::asm;

/// Invalidate every cached translation.
///
/// Needed once the kernel edits its OWN address space: every other mapping this kernel
/// installs goes into a space that is not current, so nothing needed a flush before.
///
/// # Safety
/// Always safe in itself — `sfence.vma` with no operands invalidates everything. Callers must
/// have finished the page-table writes they want visible.
pub unsafe fn sfence() {
    asm!("sfence.vma", options(nostack, preserves_flags));
}

/// Turn on (or switch) Sv39 translation: load `satp` and flush the TLB. Execution
/// continues in the new address space, which MUST map the currently-running kernel.
pub unsafe fn enable_paging(satp: u64) {
    csr::write::<{ csr::SATP }>(satp);
    asm!("sfence.vma", options(nostack, preserves_flags));
}

#[repr(C, align(16))]
struct IdleStack([u8; 8 * 1024]);
static mut IDLE_STACK: IdleStack = IdleStack([0; 8 * 1024]);

/// Park with supervisor interrupts enabled until one arrives (`wfi`). Never returns: the
/// timer handler picks what runs next.
///
/// # Safety
/// Abandons the current kernel stack (see `hal::Arch::idle`).
pub unsafe fn idle() -> ! {
    let sstatus = csr::read::<{ csr::SSTATUS }>() | csr::SSTATUS_SIE;
    csr::write::<{ csr::SSTATUS }>(sstatus);
    asm!(
        "mv sp, {stack}",
        "2:",
        "wfi",
        "j 2b",
        stack = in(reg) core::ptr::addr_of!(IDLE_STACK) as u64 + 8 * 1024,
        options(noreturn),
    );
}
