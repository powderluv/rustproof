//! Supervisor trap handling.
//!
//! [`init`] points `stvec` at a 4-byte-aligned trap vector in Direct mode (all traps
//! share one entry). The vector saves the 31 integer registers into a [`TrapFrame`] on
//! the stack, hands a pointer to the Rust [`trap_dispatch`], which — for the RV-M0 boot
//! spike — treats EVERY trap as FATAL: it dumps the frame plus `scause`/`sepc`/`stval`
//! to the console and exits QEMU. Returning IRQ handling (timer, etc.) comes later.
use crate::{csr, kprintln, qemu};

/// Integer register file captured on a trap. `#[repr(C)]`; `regs[i]` holds `x{i}`
/// (`regs[0]` is hard-wired-zero `x0`). The field order MUST match the store offsets
/// in the trap vector's assembly below (each `x{i}` at byte offset `i * 8`).
// PROOF(later): every `x{i}` the vector stores lands at `&frame.regs[i]` (offset i*8),
// so the dumped register values faithfully reflect the interrupted context.
#[repr(C)]
pub struct TrapFrame {
    /// `x0..=x31`; index by register number. `x2` holds the pre-trap `sp`.
    pub regs: [u64; 32],
}

core::arch::global_asm!(
    ".pushsection .text.trap, \"ax\", @progbits",
    ".balign 4", // Direct-mode stvec requires the base be 4-byte aligned.
    ".global __trap_vector",
    "__trap_vector:",
    // Carve a 256-byte TrapFrame (32 * 8) on the stack; sp now points at regs[0].
    "addi sp, sp, -256",
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
    // regs[2] = the pre-trap sp (current sp + 256). t0 (x5) already saved above.
    "addi t0, sp, 256",
    "sd t0,   2*8(sp)",
    // regs[0] = x0 (always zero) for a clean dump.
    "sd x0,   0*8(sp)",
    // a0 = &TrapFrame; dispatch never returns.
    "mv a0, sp",
    "call {dispatch}",
    // Belt-and-suspenders: dispatch is `-> !`, but park if it ever returns.
    "1: wfi",
    "j 1b",
    ".popsection",
    dispatch = sym trap_dispatch,
);

extern "C" {
    /// The assembly trap vector above (installed into `stvec`).
    fn __trap_vector();
}

/// Human-readable name for a synchronous exception cause (interrupt bit clear).
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

/// Called by `__trap_vector` with the assembled frame. Dumps state and halts the guest.
extern "C" fn trap_dispatch(frame: *const TrapFrame) -> ! {
    // SAFETY: `frame` points at the TrapFrame the vector just built on the stack.
    let f = unsafe { &*frame };
    // SAFETY: reading supervisor CSRs is always valid in S-mode.
    let scause = unsafe { csr::read::<{ csr::SCAUSE }>() };
    let sepc = unsafe { csr::read::<{ csr::SEPC }>() };
    let stval = unsafe { csr::read::<{ csr::STVAL }>() };

    let is_interrupt = scause >> 63 != 0;
    let code = scause & !(1u64 << 63);

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
        "  t0 ={:#018x} t1 ={:#018x} t2 ={:#018x} s0 ={:#018x}",
        f.regs[5],
        f.regs[6],
        f.regs[7],
        f.regs[8]
    );
    kprintln!(
        "  a0 ={:#018x} a1 ={:#018x} a2 ={:#018x} a3 ={:#018x}",
        f.regs[10],
        f.regs[11],
        f.regs[12],
        f.regs[13]
    );
    kprintln!(
        "  a4 ={:#018x} a5 ={:#018x} a6 ={:#018x} a7 ={:#018x}",
        f.regs[14],
        f.regs[15],
        f.regs[16],
        f.regs[17]
    );
    qemu::exit_fail();
}

/// Install the trap vector into `stvec` in Direct mode (all traps share one entry).
pub fn init() {
    // `__trap_vector` is `.balign 4`, so its low two bits are 0 == Direct mode.
    // PROOF(later): `base & 0b11 == 0`, so writing it to stvec selects Direct mode
    // (MODE field = 0) with the intended vector base — no accidental Vectored mode.
    let base = __trap_vector as *const () as usize as u64;
    // SAFETY: writing stvec is valid in S-mode; `base` is a 4-aligned code address.
    unsafe { csr::write::<{ csr::STVEC }>(base) };
}
