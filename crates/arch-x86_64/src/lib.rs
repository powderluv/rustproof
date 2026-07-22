#![no_std]
#![allow(unused)]

//! arch-x86_64 -- ALL raw hardware primitives (MMIO/MSR/port/asm/GDT/IDT/boot).
//!
//! TRUSTED unsafe stub: `#[verifier::external]` (Verus cannot see it); correctness is
//! hand-audited + Kani-checked (see kani/ harnesses). Keep this crate as SMALL as possible.

