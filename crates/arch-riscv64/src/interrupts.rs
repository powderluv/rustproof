//! Supervisor trap handling.
//!
//! [`init`] points `stvec` at a 4-byte-aligned Direct-mode vector and parks a kernel
//! trap stack in `sscratch`. On a trap the vector swaps `sp` with `sscratch` (so a U-mode
//! trap lands on the kernel stack, not the untrusted user stack), saves the integer
//! registers into a [`TrapFrame`], and calls [`trap_dispatch`]. An `ecall` from U-mode is
//! serviced as a syscall and the vector `sret`s back; every other trap is fatal (dump +
//! exit).
use crate::{csr, kprintln, qemu};

/// Integer register file captured on a trap. `#[repr(C)]`; `regs[i]` holds `x{i}`
/// (`regs[0]` is hard-wired-zero `x0`, `regs[2]` the interrupted `sp`). Field order MUST
/// match the store offsets in the trap vector below (each `x{i}` at byte offset `i * 8`).
// PROOF(later): every `x{i}` the vector stores lands at `&frame.regs[i]` (offset i*8).
#[repr(C)]
pub struct TrapFrame {
    pub regs: [u64; 32],
}

// Kernel stack for traps taken from U-mode (parked in sscratch). 16-aligned, no nesting.
#[repr(C, align(16))]
struct TrapStack([u8; 32 * 1024]);
static mut TRAP_STACK: TrapStack = TrapStack([0; 32 * 1024]);

core::arch::global_asm!(
    ".pushsection .text.trap, \"ax\", @progbits",
    ".balign 4", // Direct-mode stvec requires the base be 4-byte aligned.
    ".global __trap_vector",
    "__trap_vector:",
    // Swap sp with the kernel trap stack (sscratch). After: sp = kernel trap stack top,
    // sscratch = the interrupted sp (user or kernel).
    "csrrw sp, sscratch, sp",
    "addi sp, sp, -256", // carve a 256-byte TrapFrame; sp -> regs[0].
    "sd x1,   1*8(sp)",
    "sd x3,   3*8(sp)",
    "sd x4,   4*8(sp)",
    "sd x5,   5*8(sp)",
    "sd x6,   6*8(sp)",
    "sd x7,   7*8(sp)",
    "sd x8,   8*8(sp)",
    "sd x9,   9*8(sp)",
    "sd x10, 10*8(sp)",
    "sd x11, 11*8(sp)",
    "sd x12, 12*8(sp)",
    "sd x13, 13*8(sp)",
    "sd x14, 14*8(sp)",
    "sd x15, 15*8(sp)",
    "sd x16, 16*8(sp)",
    "sd x17, 17*8(sp)",
    "sd x18, 18*8(sp)",
    "sd x19, 19*8(sp)",
    "sd x20, 20*8(sp)",
    "sd x21, 21*8(sp)",
    "sd x22, 22*8(sp)",
    "sd x23, 23*8(sp)",
    "sd x24, 24*8(sp)",
    "sd x25, 25*8(sp)",
    "sd x26, 26*8(sp)",
    "sd x27, 27*8(sp)",
    "sd x28, 28*8(sp)",
    "sd x29, 29*8(sp)",
    "sd x30, 30*8(sp)",
    "sd x31, 31*8(sp)",
    // regs[2] = the interrupted sp (in sscratch); t0/x5 already saved, so it is free.
    "csrr t0, sscratch",
    "sd t0,   2*8(sp)",
    // Reset sscratch to the kernel trap stack top for the next trap.
    "addi t0, sp, 256",
    "csrw sscratch, t0",
    "sd x0,   0*8(sp)", // regs[0] = 0 for a clean dump.
    "mv a0, sp",        // a0 = &TrapFrame
    "call {dispatch}",  // returns for a serviced ecall; exits inside for fatal traps.
    // Restore the (possibly-updated) frame and return.
    "ld x1,   1*8(sp)",
    "ld x3,   3*8(sp)",
    "ld x4,   4*8(sp)",
    "ld x5,   5*8(sp)",
    "ld x6,   6*8(sp)",
    "ld x7,   7*8(sp)",
    "ld x8,   8*8(sp)",
    "ld x9,   9*8(sp)",
    "ld x10, 10*8(sp)",
    "ld x11, 11*8(sp)",
    "ld x12, 12*8(sp)",
    "ld x13, 13*8(sp)",
    "ld x14, 14*8(sp)",
    "ld x15, 15*8(sp)",
    "ld x16, 16*8(sp)",
    "ld x17, 17*8(sp)",
    "ld x18, 18*8(sp)",
    "ld x19, 19*8(sp)",
    "ld x20, 20*8(sp)",
    "ld x21, 21*8(sp)",
    "ld x22, 22*8(sp)",
    "ld x23, 23*8(sp)",
    "ld x24, 24*8(sp)",
    "ld x25, 25*8(sp)",
    "ld x26, 26*8(sp)",
    "ld x27, 27*8(sp)",
    "ld x28, 28*8(sp)",
    "ld x29, 29*8(sp)",
    "ld x30, 30*8(sp)",
    "ld x31, 31*8(sp)",
    "ld x2,   2*8(sp)", // LAST: sp = interrupted sp.
    "sret",
    ".popsection",
    dispatch = sym trap_dispatch,
);

extern "C" {
    fn __trap_vector();
    /// The nucleus's capability-gated syscall dispatch (`ecall` handler body).
    fn rustproof_syscall_dispatch(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64;
}

fn exception_name(code: u64) -> &'static str {
    match code {
        0 => "instruction address misaligned",
        1 => "instruction access fault",
        2 => "illegal instruction",
        3 => "breakpoint",
        4 => "load address misaligned",
        5 => "load access fault",
        6 => "store/AMO address misaligned",
        7 => "store/AMO access fault",
        8 => "ecall from U-mode",
        9 => "ecall from S-mode",
        12 => "instruction page fault",
        13 => "load page fault",
        15 => "store/AMO page fault",
        _ => "reserved/other",
    }
}

/// Called by `__trap_vector` with the assembled frame. Services an `ecall` from U-mode as
/// a syscall (and returns so the vector can `sret`); every other trap is fatal.
extern "C" fn trap_dispatch(frame: *mut TrapFrame) {
    // SAFETY: `frame` points at the TrapFrame the vector just built on the kernel stack.
    let f = unsafe { &mut *frame };
    let scause = unsafe { csr::read::<{ csr::SCAUSE }>() };
    let is_interrupt = scause >> 63 != 0;
    let code = scause & !(1u64 << 63);

    // ecall from U-mode (cause 8): a7 = number, a0..a4 = args; result -> a0; skip the ecall.
    if !is_interrupt && code == 8 {
        let num = f.regs[17]; // a7
        let ret = unsafe {
            rustproof_syscall_dispatch(
                num, f.regs[10], // a0
                f.regs[11], // a1
                f.regs[12], // a2
                f.regs[13], // a3
                f.regs[14], // a4
            )
        };
        f.regs[10] = ret; // a0 = result
                          // Advance past the 4-byte `ecall` so `sret` resumes at the following instruction.
        let sepc = unsafe { csr::read::<{ csr::SEPC }>() };
        unsafe { csr::write::<{ csr::SEPC }>(sepc + 4) };
        return;
    }

    fatal_dump(f, scause, is_interrupt, code);
}

/// Dump the trap context and shut the guest down.
fn fatal_dump(f: &TrapFrame, scause: u64, is_interrupt: bool, code: u64) -> ! {
    let sepc = unsafe { csr::read::<{ csr::SEPC }>() };
    let stval = unsafe { csr::read::<{ csr::STVAL }>() };
    kprintln!();
    kprintln!("*** RISC-V TRAP (fatal) ***");
    if is_interrupt {
        kprintln!("  scause = {:#018x}  interrupt, code {}", scause, code);
    } else {
        kprintln!(
            "  scause = {:#018x}  exception: {}",
            scause,
            exception_name(code)
        );
    }
    kprintln!("  sepc   = {:#018x}", sepc);
    kprintln!("  stval  = {:#018x}", stval);
    kprintln!(
        "  ra ={:#018x} sp ={:#018x} gp ={:#018x} tp ={:#018x}",
        f.regs[1],
        f.regs[2],
        f.regs[3],
        f.regs[4]
    );
    kprintln!(
        "  a0 ={:#018x} a1 ={:#018x} a2 ={:#018x} a7 ={:#018x}",
        f.regs[10],
        f.regs[11],
        f.regs[12],
        f.regs[17]
    );
    qemu::exit_fail();
}

/// Install the trap vector into `stvec` (Direct mode) and park the kernel trap stack in
/// `sscratch` so U-mode traps switch to a kernel stack.
pub fn init() {
    let base = __trap_vector as *const () as u64;
    // PROOF(later): `base & 0b11 == 0` (`.balign 4`), so stvec selects Direct mode.
    unsafe { csr::write::<{ csr::STVEC }>(base) };
    let top = core::ptr::addr_of!(TRAP_STACK) as u64 + core::mem::size_of::<TrapStack>() as u64;
    unsafe { csr::write::<{ csr::SSCRATCH }>(top) };
}
