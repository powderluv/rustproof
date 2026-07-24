//! x86-64 machine context + cooperative switch.
use abi::VirtAddr;

/// The register state saved/restored across a cooperative [`switch`] — the x86-64
/// callee-saved registers plus the stack pointer. `#[repr(C)]` pins the field order so
/// the naked `switch` can address fields by fixed byte offsets (checked below).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Context {
    pub rbx: u64, // 0x00
    pub rbp: u64, // 0x08
    pub r12: u64, // 0x10
    pub r13: u64, // 0x18
    pub r14: u64, // 0x20
    pub r15: u64, // 0x28
    pub rsp: u64, // 0x30
}

const _: () = {
    assert!(core::mem::offset_of!(Context, rbx) == 0x00);
    assert!(core::mem::offset_of!(Context, rbp) == 0x08);
    assert!(core::mem::offset_of!(Context, r12) == 0x10);
    assert!(core::mem::offset_of!(Context, r13) == 0x18);
    assert!(core::mem::offset_of!(Context, r14) == 0x20);
    assert!(core::mem::offset_of!(Context, r15) == 0x28);
    assert!(core::mem::offset_of!(Context, rsp) == 0x30);
};

impl Context {
    /// A zeroed context. Not runnable until an `rsp` is set (e.g. via [`Context::prepare`]).
    pub const fn new() -> Self {
        Context {
            rbx: 0,
            rbp: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rsp: 0,
        }
    }

    /// Lay out a new thread's initial stack so the *first* [`switch`] into the returned
    /// context begins executing `entry`. `switch` ends in `ret`, which pops a return
    /// address off the incoming `rsp`; so we write `entry` at the top of `stack_top` and
    /// point `rsp` at it. The slot is 16-byte aligned so `entry` sees `rsp % 16 == 8`.
    ///
    /// # Safety
    /// Writes one `u64` just below `stack_top`, which must be the exclusive, un-aliased
    /// top of a writable thread-private stack with >= 16 bytes of headroom.
    ///
    /// PROOF(later): the written byte lies within `[stack_base, stack_top)` and `rsp` is
    /// 16-aligned and in range.
    pub unsafe fn prepare(stack_top: VirtAddr, entry: extern "C" fn() -> !) -> Context {
        let slot = (stack_top.as_u64() - 8) & !0xF;
        core::ptr::write(slot as *mut u64, entry as u64);
        Context {
            rsp: slot,
            ..Context::new()
        }
    }
}

/// Cooperatively switch CPU state from `*from` to `*to` (naked; `rdi`=from, `rsi`=to).
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
        "mov [rdi + 0x00], rbx",
        "mov [rdi + 0x08], rbp",
        "mov [rdi + 0x10], r12",
        "mov [rdi + 0x18], r13",
        "mov [rdi + 0x20], r14",
        "mov [rdi + 0x28], r15",
        "mov [rdi + 0x30], rsp",
        "mov rbx, [rsi + 0x00]",
        "mov rbp, [rsi + 0x08]",
        "mov r12, [rsi + 0x10]",
        "mov r13, [rsi + 0x18]",
        "mov r14, [rsi + 0x20]",
        "mov r15, [rsi + 0x28]",
        "mov rsp, [rsi + 0x30]",
        "ret",
    );
}
