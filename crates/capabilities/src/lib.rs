#![no_std]
#![allow(unused)]

//! capabilities -- capability objects, derivation/revocation, authority monotonicity
//!
//! VERIFIED TCB crate. See docs/nucleus-design.md and docs/verification.md.
//! TODO(M1): add pinned Verus support crates (vstd/builtin/builtin_macros) and
//! begin `verus!{ ... }` modules. Substantive lemmas start admitted (see repo-structure.md sec 5).

