//! arch-riscv64 — raw hardware primitives (NS16550A serial, supervisor traps, CSR
//! access, QEMU/SiFive-test exit) for the Rustproof nucleus on RISC-V (rv64gc).
//!
//! TRUSTED unsafe stub: this crate is (eventually) `#[verifier::external]` — Verus
//! cannot see it, so correctness is hand-audited. Keep it as SMALL as possible; every
//! function here is in the TCB. Mirrors `arch-x86_64`. See docs/nucleus-design.md §6.
#![no_std]
#![allow(clippy::missing_safety_doc)]

pub mod boot;
pub mod csr;
pub mod interrupts;
pub mod mmu;
pub mod qemu;
pub mod serial;
pub mod timer;

/// Write formatted text to the NS16550A UART. The console is a fixed, stateless sink (ZST).
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::serial::Uart, $($arg)*);
    }};
}

/// `kprint!` followed by a newline (the UART driver expands `\n` to CRLF).
#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {{ $crate::kprint!($($arg)*); $crate::kprint!("\n"); }};
}
