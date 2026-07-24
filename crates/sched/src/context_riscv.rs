//! RISC-V (rv64) machine context + cooperative switch.
use abi::VirtAddr;

/// The register state saved/restored across a cooperative [`switch`] — the RISC-V
/// callee-saved registers (`ra`, `sp`, `s0..s11`). A cooperative switch happens at a
/// call boundary, so caller-saved registers are already spilled by the compiler.
/// `#[repr(C)]` pins the field order for the fixed byte offsets in the naked `switch`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Context {
    pub ra: u64,      // 0x00 — return address; `switch` ends in `ret` (jumps to ra)
    pub sp: u64,      // 0x08
    pub s: [u64; 12], // 0x10.. — s0..s11 (x8, x9, x18..x27)
}

const _: () = {
    assert!(core::mem::offset_of!(Context, ra) == 0x00);
    assert!(core::mem::offset_of!(Context, sp) == 0x08);
    assert!(core::mem::offset_of!(Context, s) == 0x10);
};

impl Context {
    /// A zeroed context. Not runnable until `ra`/`sp` are set (e.g. via [`Context::prepare`]).
    pub const fn new() -> Self {
        Context {
            ra: 0,
            sp: 0,
            s: [0; 12],
        }
    }

    /// Prepare a new thread's context so the *first* [`switch`] into it begins executing
    /// `entry`. On RISC-V `switch` ends in `ret`, which jumps to `ra` — so we simply set
    /// `ra = entry` and `sp = stack_top` (16-aligned). No return-address is written to the
    /// stack (unlike x86, where `ret` pops one).
    ///
    /// # Safety
    /// `stack_top` must be the exclusive top of a writable, un-aliased thread-private
    /// stack. (Nothing is written here; the contract matches the x86 variant's shape.)
    ///
    /// PROOF(later): `sp` is 16-aligned and within the backing region; `ra == entry`.
    pub unsafe fn prepare(stack_top: VirtAddr, entry: extern "C" fn() -> !) -> Context {
        Context {
            ra: entry as u64,
            sp: stack_top.as_u64() & !0xF,
            s: [0; 12],
        }
    }
}

/// Cooperatively switch CPU state from `*from` to `*to` (naked; `a0`=from, `a1`=to).
///
/// # Safety
/// `from`/`to` must be valid, aligned, non-aliasing contexts; `*to` must be a real
/// suspended or freshly-`prepare`d context. Control returns only when some thread
/// switches back to `*from`.
///
/// PROOF(later): the register set saved into `*from` is exactly the set restored from
/// `*to`, so a round-trip switch is the identity on machine state.
#[unsafe(naked)]
pub unsafe extern "C" fn switch(from: *mut Context, to: *const Context) {
    core::arch::naked_asm!(
        "sd ra,  0x00(a0)",
        "sd sp,  0x08(a0)",
        "sd s0,  0x10(a0)",
        "sd s1,  0x18(a0)",
        "sd s2,  0x20(a0)",
        "sd s3,  0x28(a0)",
        "sd s4,  0x30(a0)",
        "sd s5,  0x38(a0)",
        "sd s6,  0x40(a0)",
        "sd s7,  0x48(a0)",
        "sd s8,  0x50(a0)",
        "sd s9,  0x58(a0)",
        "sd s10, 0x60(a0)",
        "sd s11, 0x68(a0)",
        "ld ra,  0x00(a1)",
        "ld sp,  0x08(a1)",
        "ld s0,  0x10(a1)",
        "ld s1,  0x18(a1)",
        "ld s2,  0x20(a1)",
        "ld s3,  0x28(a1)",
        "ld s4,  0x30(a1)",
        "ld s5,  0x38(a1)",
        "ld s6,  0x40(a1)",
        "ld s7,  0x48(a1)",
        "ld s8,  0x50(a1)",
        "ld s9,  0x58(a1)",
        "ld s10, 0x60(a1)",
        "ld s11, 0x68(a1)",
        "ret",
    );
}
