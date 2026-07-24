//! Supervisor CSR (control/status register) access.
//!
//! CSR numbers are encoded as a 12-bit immediate inside the `csrr`/`csrw`
//! instruction, so the register cannot be selected at runtime — it must be a
//! compile-time constant. We pass it as a `const` generic that lands in the asm
//! template as a `const` operand (an immediate), keeping these helpers branch-free.

/// `sstatus` — supervisor status (SPP bit 8 = prev priv, SUM bit 18 = S-access-to-U).
pub const SSTATUS: u32 = 0x100;
/// `stvec` — supervisor trap-vector base address + mode.
pub const STVEC: u32 = 0x105;
/// `sscratch` — supervisor scratch register (holds the kernel trap stack while in U-mode).
pub const SSCRATCH: u32 = 0x140;
/// `sepc` — supervisor exception program counter.
pub const SEPC: u32 = 0x141;
/// `scause` — supervisor trap cause.
pub const SCAUSE: u32 = 0x142;
/// `stval` — supervisor trap value (bad address / faulting instruction).
pub const STVAL: u32 = 0x143;
/// `satp` — supervisor address translation & protection (MODE | ASID | root PPN).
pub const SATP: u32 = 0x180;

/// `sstatus.SPP` (bit 8): previous privilege — 0 returns to U-mode on `sret`.
pub const SSTATUS_SPP: u64 = 1 << 8;
/// `sstatus.SPIE` (bit 5): previous interrupt-enable, restored into SIE on `sret`.
pub const SSTATUS_SPIE: u64 = 1 << 5;
/// `sstatus.SUM` (bit 18): permit S-mode loads/stores to U-mode (U=1) pages.
pub const SSTATUS_SUM: u64 = 1 << 18;

/// Read the supervisor CSR numbered `CSR`.
#[inline(always)]
pub unsafe fn read<const CSR: u32>() -> u64 {
    let value: u64;
    core::arch::asm!(
        "csrr {v}, {csr}",
        v = out(reg) value,
        csr = const CSR,
        options(nostack, preserves_flags),
    );
    value
}

/// Write `value` to the supervisor CSR numbered `CSR`.
#[inline(always)]
pub unsafe fn write<const CSR: u32>(value: u64) {
    core::arch::asm!(
        "csrw {csr}, {v}",
        csr = const CSR,
        v = in(reg) value,
        options(nostack, preserves_flags),
    );
}
