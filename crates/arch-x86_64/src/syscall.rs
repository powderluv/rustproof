//! `syscall` fast system-call support + a full trap-frame save/resume path.
//!
//! The user convention (fixed, shared via `abi`): `rax` = number, args `a0..a4` in
//! `rdi, rsi, rdx, r10, r8`, execute `syscall`; the result comes back in `rax`. The
//! instruction clobbers `rcx`/`r11` (they carry the user `rip`/`rflags`).
//!
//! Unlike a plain register-marshalling stub, the entry here saves the **entire** user
//! register state into a [`TrapFrame`] laid out so its tail is a hardware `iretq` frame,
//! then calls `rustproof_syscall_trap(frame)` — which never returns: it services the call
//! (or switches process) and re-enters user mode via [`resume`]. This is the same frame a
//! timer interrupt will build, so preemption reuses this path (see `docs/scheduling.md`).
use core::arch::{asm, naked_asm};

const IA32_EFER: u32 = 0xC000_0080;
const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;

/// User segment selectors (GDT entries with RPL 3); see [`crate::gdt`].
const USER_CS: u64 = 0x2B; // code64, RPL 3
const USER_SS: u64 = 0x23; // data,   RPL 3

/// The saved user register state on a trap. The 15 GPRs are followed by the 5-word
/// hardware `iretq` frame (`rip, cs, rflags, rsp, ss`), so [`resume`] can pop the GPRs and
/// `iretq` straight out of this struct. `#[repr(C)]` fixes the field order the entry stub's
/// push sequence and [`resume`]'s pop sequence both depend on.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    // ---- hardware iretq frame ----
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl TrapFrame {
    /// The number of `u64` words in a frame (must match `hal::Arch::FRAME_WORDS`).
    pub const WORDS: usize = 20;

    /// Build the initial frame for a fresh process: enter ring 3 at `entry` on stack `sp`,
    /// with `arg0` in `rdi` (the SysV first-argument register) and `IF` set.
    pub const fn new_user(entry: u64, sp: u64, arg0: u64) -> TrapFrame {
        TrapFrame {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rbp: 0,
            rdi: arg0,
            rsi: 0,
            rdx: 0,
            rcx: 0,
            rbx: 0,
            rax: 0,
            rip: entry,
            cs: USER_CS,
            rflags: 0x202, // IF set, reserved bit 1
            rsp: sp,
            ss: USER_SS,
        }
    }
}

#[inline]
unsafe fn wrmsr(msr: u32, val: u64) {
    let lo = val as u32;
    let hi = (val >> 32) as u32;
    asm!("wrmsr", in("ecx") msr, in("eax") lo, in("edx") hi, options(nostack, preserves_flags));
}

#[inline]
unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nostack, preserves_flags));
    ((hi as u64) << 32) | lo as u64
}

// Provided by the nucleus: the scheduler-aware trap handler. Receives the on-stack frame
// and never returns — it resumes some process via `resume`.
extern "C" {
    fn rustproof_syscall_trap(frame: *mut TrapFrame) -> !;
}

#[repr(C, align(16))]
struct KStack([u8; 16 * 1024]);

// Single-CPU kernel trap stack + a scratch slot for the user rsp while the stub builds the
// frame. A per-CPU slot would be needed for SMP.
static mut USER_RSP: u64 = 0;
static mut SYSCALL_KSTACK: KStack = KStack([0; 16 * 1024]);

/// Program the syscall MSRs. STAR selects kernel CS 0x08 for `syscall` and the 0x18 base
/// for `sysret` (-> user data 0x20, user code64 0x28, both RPL 3), matching [`crate::gdt`].
pub fn init() {
    unsafe {
        // EFER.SCE (bit 0) enables `syscall`; EFER.NXE (bit 11) makes the NX bit (63) a
        // valid PTE bit rather than a reserved bit (the loader marks data pages no-exec).
        wrmsr(IA32_EFER, rdmsr(IA32_EFER) | (1 << 0) | (1 << 11));
        wrmsr(IA32_STAR, (0x18u64 << 48) | (0x08u64 << 32));
        wrmsr(IA32_LSTAR, syscall_entry as *const () as u64);
        // Clear IF, DF, TF on kernel entry (no nested interrupts, defined direction).
        wrmsr(IA32_FMASK, 0x200 | 0x400 | 0x100);
    }
}

/// `syscall` entry: switch to the kernel trap stack, build a full [`TrapFrame`] (synthesize
/// the hardware `iretq` tail from the instruction-clobbered `rcx`/`r11` + the stashed user
/// `rsp`), then hand it to `rustproof_syscall_trap`, which never returns.
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    naked_asm!(
        "mov [{user_rsp}], rsp",           // stash user rsp (needed for the iretq frame)
        "lea rsp, [{kstack} + {ksize}]",   // switch to the kernel trap stack (16-aligned)
        // Synthesize the hardware iretq frame (pushed high addr -> low): ss, rsp, rflags, cs, rip.
        "push {user_ss}",
        "push qword ptr [{user_rsp}]",     // user rsp
        "push r11",                        // user rflags (syscall stashed it here)
        "push {user_cs}",
        "push rcx",                        // user rip (syscall stashed it here)
        // Save the GPRs so the final rsp lands on `r15` (matching TrapFrame's low->high order).
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",                        // 20 pushes total -> rsp 16-aligned for the call
        "mov rdi, rsp",                    // arg0 = &TrapFrame
        "call {trap}",
        "ud2",                             // rustproof_syscall_trap never returns
        user_rsp = sym USER_RSP,
        kstack = sym SYSCALL_KSTACK,
        ksize = const 16 * 1024,
        user_ss = const USER_SS,
        user_cs = const USER_CS,
        trap = sym rustproof_syscall_trap,
    );
}

/// Restore the user register state in `frame` and return to ring 3 via `iretq`. Never
/// returns. `cr3` must already point at the target process's address space (the caller
/// loads it); `frame` and the code here are in the kernel mappings shared into every space.
///
/// # Safety
/// `frame` must point at a coherent [`TrapFrame`] whose `rip`/`rsp`/segments are valid for
/// the currently-active address space.
pub unsafe fn resume(frame: *const TrapFrame) -> ! {
    asm!(
        "mov rsp, {f}",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",
        "iretq",
        f = in(reg) frame,
        options(noreturn),
    );
}
