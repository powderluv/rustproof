//! `syscall`/`sysret` fast system-call support + ring-3 entry.
//!
//! The convention (fixed, shared with userland via `abi`): user sets `rax` = number,
//! args `a0..a4` in `rdi, rsi, rdx, r10, r8`, executes `syscall`; the result comes back
//! in `rax`. `rcx`/`r11` are clobbered by the instruction (they carry the user `rip`/
//! `rflags`). The entry stub marshals those registers into the C-ABI call
//! `rustproof_syscall_dispatch(num, a0, a1, a2, a3, a4)`, which the nucleus provides.
use core::arch::{asm, naked_asm};

const IA32_EFER: u32 = 0xC000_0080;
const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;

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

// Provided by the nucleus: the actual capability-gated dispatch (uses kernel state).
extern "C" {
    fn rustproof_syscall_dispatch(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64;
}

#[repr(C, align(16))]
struct KStack([u8; 16 * 1024]);

// Where the entry stub parks the user rsp while it runs on the kernel stack. Single-CPU
// only (a per-CPU slot would be needed for SMP).
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

/// `syscall` entry: switch to the kernel stack, marshal the user register args into the
/// C-ABI dispatch call, then `sysretq` back to ring 3 with the result in `rax`.
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    // The syscall must be register-transparent (Linux convention): everything except the
    // return value (rax) and the instruction-clobbered rcx/r11 is preserved for the user.
    // Callee-saved regs (rbx/rbp/r12-r15) are preserved by the dispatch fn per the C ABI;
    // we save the caller-saved regs the user relies on across the call.
    naked_asm!(
        "mov [{user_rsp}], rsp",           // stash user rsp
        "lea rsp, [{kstack} + {ksize}]",   // switch to the kernel syscall stack (16-aligned)
        "push rcx",                        // user rip  (syscall put it in rcx)
        "push r11",                        // user rflags
        "push rdi",                        // save caller-saved user regs
        "push rsi",
        "push rdx",
        "push r10",
        "push r8",
        "push r9",                         // 8 pushes -> rsp stays 16-aligned for the call
        // user (rax=num, rdi,rsi,rdx,r10,r8) -> dispatch(rdi=num, rsi,rdx,rcx,r8,r9)
        "mov r9, r8",
        "mov r8, r10",
        "mov rcx, rdx",
        "mov rdx, rsi",
        "mov rsi, rdi",
        "mov rdi, rax",
        "call {dispatch}",                 // rax = result (preserved through the pops below)
        "pop r9",
        "pop r8",
        "pop r10",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop r11",                         // user rflags
        "pop rcx",                         // user rip
        "mov rsp, [{user_rsp}]",           // restore user rsp
        "sysretq",
        user_rsp = sym USER_RSP,
        kstack = sym SYSCALL_KSTACK,
        ksize = const 16 * 1024,
        dispatch = sym rustproof_syscall_dispatch,
    );
}

/// Drop to ring 3 at `entry` with stack `user_stack_top`, via `iretq`. Never returns
/// (the process runs until it makes an `EXIT` syscall). CR3 must already point at the
/// user address space (with the kernel shared in), and both `entry` and the stack must be
/// mapped USER-accessible.
pub unsafe fn enter_user(entry: u64, user_stack_top: u64) -> ! {
    asm!(
        "push {ss}",       // user SS  (0x20 | RPL3)
        "push {rsp}",      // user RSP
        "push {rflags}",   // RFLAGS with IF set
        "push {cs}",       // user CS  (0x28 | RPL3)
        "push {rip}",      // user RIP
        "iretq",
        ss = in(reg) 0x23u64,
        rsp = in(reg) user_stack_top,
        rflags = in(reg) 0x202u64,
        cs = in(reg) 0x2Bu64,
        rip = in(reg) entry,
        options(noreturn),
    );
}
