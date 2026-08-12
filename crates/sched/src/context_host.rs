//! A machine context for hosts that are neither x86-64 nor riscv64.
//!
//! This exists for exactly one reason: so `crates/kernel` COMPILES for `cargo test` on any
//! development host, and can therefore be host-tested at all.
//!
//! `kernel` holds a `static mut MAIN_CTX: sched::Context`, so without some `Context` in
//! scope the whole crate fails to build off-target — and a crate that cannot build cannot
//! have tests, which is how the largest crate in the tree came to be the only one exempt
//! from `tools/host-tests.sh`'s coverage guard. The guard fires on the PRESENCE of `#[test]`,
//! so a crate with none never trips it; "cannot build for the host" and "has nothing worth
//! testing" had been silently conflated.
//!
//! Nothing here ever runs on a real target: both real arches select their own module, and
//! this one is chosen only when neither applies. It is deliberately inert rather than
//! plausible — [`switch`] panics instead of pretending, because a context switch that
//! silently did nothing would let a host test appear to schedule and quietly prove nothing.
//! Host tests exercise the kernel's DECISION logic; anything that actually switches stacks
//! belongs under QEMU.

use abi::VirtAddr;

/// Placeholder register state. The field exists so the type has the same shape of use as the
/// real ones (constructed, copied, stored in a static); its value is never meaningful.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Context {
    pub sp: u64,
}

impl Context {
    /// A zeroed context. Never runnable — see the module note.
    pub const fn new() -> Self {
        Context { sp: 0 }
    }

    /// Present only to match the real arches' surface. Writes nothing and prepares nothing.
    ///
    /// # Safety
    /// Trivially safe: it touches no memory. The `unsafe` mirrors the real signature so
    /// callers compile unchanged.
    pub unsafe fn prepare(_stack_top: VirtAddr, _entry: extern "C" fn() -> !) -> Context {
        Context::new()
    }
}

/// Refuses to run.
///
/// # Safety
/// Never safe to call, and never called: it panics unconditionally.
pub unsafe fn switch(_from: *mut Context, _to: *const Context) {
    panic!(
        "sched::switch is not implemented for this host architecture — it exists so the \
         kernel can be host-TESTED, not host-RUN. A test that reaches a context switch is \
         testing the wrong thing; put it under QEMU."
    );
}
