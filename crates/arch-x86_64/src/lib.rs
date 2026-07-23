//! arch-x86_64 — raw hardware primitives (port I/O, 16550 serial, QEMU exit) for
//! the Rustproof nucleus.
//!
//! TRUSTED unsafe stub: this crate is (eventually) `#[verifier::external]` — Verus
//! cannot see it, so correctness is hand-audited + Kani-checked. Keep it as SMALL as
//! possible; every function here is in the TCB. See docs/nucleus-design.md §6.
#![no_std]
#![allow(clippy::missing_safety_doc)]

pub mod interrupts;
pub mod port;
pub mod qemu;
pub mod serial;

/// Write formatted text to COM1. The serial port is a fixed, stateless sink (ZST).
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::serial::Serial, $($arg)*);
    }};
}

/// `kprint!` followed by CRLF.
#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => {{ $crate::kprint!($($arg)*); $crate::kprint!("\n"); }};
}
