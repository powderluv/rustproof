//! Sv39 paging enable + U-mode entry.
use crate::csr;
use core::arch::asm;

/// Turn on (or switch) Sv39 translation: load `satp` and flush the TLB. Execution
/// continues in the new address space, which MUST map the currently-running kernel.
pub unsafe fn enable_paging(satp: u64) {
    csr::write::<{ csr::SATP }>(satp);
    asm!("sfence.vma", options(nostack, preserves_flags));
}

/// Drop to U-mode at `entry` with stack `user_sp`, under address space `satp`.
///
/// Sets `sstatus.SUM` (so the kernel's trap handler can read/write the caller's user
/// memory), `SPP = 0` (return to U-mode), and `SPIE = 1`, then `sret`s. Never returns —
/// the process runs until it makes an `ecall` EXIT. `interrupts::init` must already have
/// parked the kernel trap stack in `sscratch`, and `satp`'s address space must share the
/// kernel mappings (so the trap vector + handler are reachable from U-mode traps).
pub unsafe fn enter_user(satp: u64, entry: u64, user_sp: u64) -> ! {
    let mut sstatus = csr::read::<{ csr::SSTATUS }>();
    sstatus &= !csr::SSTATUS_SPP; // SPP = 0 -> sret returns to U-mode
    sstatus |= csr::SSTATUS_SPIE | csr::SSTATUS_SUM;
    csr::write::<{ csr::SSTATUS }>(sstatus);
    csr::write::<{ csr::SEPC }>(entry);
    asm!(
        "csrw satp, {satp}",
        "sfence.vma",
        "mv sp, {usp}",
        "sret",
        satp = in(reg) satp,
        usp = in(reg) user_sp,
        options(noreturn),
    );
}
