//! Interrupt Descriptor Table + CPU exception handling.
//!
//! On stable Rust there is no `x86-interrupt` ABI, so each of the 32 CPU exception
//! vectors gets a small naked-function stub that normalizes the stack (pushes a dummy
//! error code where the CPU does not), records the vector number, saves all GP
//! registers, and jumps to a common stub that calls the Rust dispatcher with a
//! pointer to the assembled [`ExceptionFrame`].
//!
//! For the M0 boot spike an exception is fatal: the dispatcher dumps the frame and
//! exits QEMU. Returning IRQ handlers (timer, etc.) come in a later slice.
use crate::{kprintln, qemu};

/// The register + trap state assembled on the stack by the ISR stubs. `#[repr(C)]`
/// field order MUST match the push order in [`isr_common`] and the per-vector stubs.
#[repr(C)]
pub struct ExceptionFrame {
    // pushed by isr_common (rax first .. r15 last, so r15 is at offset 0)
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rax: u64,
    // pushed by the per-vector stub
    pub vector: u64,
    pub error_code: u64,
    // pushed by the CPU
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// Common tail: save GP registers, align the stack, call the dispatcher (never returns).
#[unsafe(naked)]
extern "C" fn isr_common() {
    core::arch::naked_asm!(
        // Clear DF before the Rust dispatcher (see `timer_isr`): an exception from ring 3
        // may carry DF=1, which would corrupt the `rep movs` in the dump's formatting.
        "cld",
        "push rax",
        "push rcx",
        "push rdx",
        "push rbx",
        "push rbp",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov rdi, rsp", // arg0 = &ExceptionFrame
        "and rsp, -16", // re-align for the SysV ABI before the call
        "call {dispatch}",
        "ud2",
        dispatch = sym exception_dispatch,
    );
}

/// Per-vector stub without a CPU-pushed error code: push a dummy 0 to keep the frame
/// layout uniform, then the vector number.
macro_rules! isr_noerr {
    ($name:ident, $vec:expr) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            core::arch::naked_asm!(
                "push 0",
                "push {v}",
                "jmp {c}",
                v = const $vec,
                c = sym isr_common,
            );
        }
    };
}

/// Per-vector stub for vectors where the CPU already pushed an error code.
macro_rules! isr_err {
    ($name:ident, $vec:expr) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            core::arch::naked_asm!(
                "push {v}",
                "jmp {c}",
                v = const $vec,
                c = sym isr_common,
            );
        }
    };
}

isr_noerr!(isr0, 0);
isr_noerr!(isr1, 1);
isr_noerr!(isr2, 2);
isr_noerr!(isr3, 3);
isr_noerr!(isr4, 4);
isr_noerr!(isr5, 5);
isr_noerr!(isr6, 6);
isr_noerr!(isr7, 7);
isr_err!(isr8, 8);
isr_noerr!(isr9, 9);
isr_err!(isr10, 10);
isr_err!(isr11, 11);
isr_err!(isr12, 12);
isr_err!(isr13, 13);
isr_err!(isr14, 14);
isr_noerr!(isr15, 15);
isr_noerr!(isr16, 16);
isr_err!(isr17, 17);
isr_noerr!(isr18, 18);
isr_noerr!(isr19, 19);
isr_noerr!(isr20, 20);
isr_err!(isr21, 21);
isr_noerr!(isr22, 22);
isr_noerr!(isr23, 23);
isr_noerr!(isr24, 24);
isr_noerr!(isr25, 25);
isr_noerr!(isr26, 26);
isr_noerr!(isr27, 27);
isr_noerr!(isr28, 28);
isr_err!(isr29, 29);
isr_err!(isr30, 30);
isr_noerr!(isr31, 31);

const STUBS: [extern "C" fn(); 32] = [
    isr0, isr1, isr2, isr3, isr4, isr5, isr6, isr7, isr8, isr9, isr10, isr11, isr12, isr13, isr14,
    isr15, isr16, isr17, isr18, isr19, isr20, isr21, isr22, isr23, isr24, isr25, isr26, isr27,
    isr28, isr29, isr30, isr31,
];

fn vector_name(v: u64) -> &'static str {
    match v {
        0 => "divide error",
        1 => "debug",
        2 => "NMI",
        3 => "breakpoint",
        4 => "overflow",
        5 => "bound range exceeded",
        6 => "invalid opcode",
        7 => "device not available",
        8 => "double fault",
        10 => "invalid TSS",
        11 => "segment not present",
        12 => "stack-segment fault",
        13 => "general protection fault",
        14 => "page fault",
        16 => "x87 floating point",
        17 => "alignment check",
        18 => "machine check",
        19 => "SIMD floating point",
        20 => "virtualization",
        21 => "control protection",
        _ => "reserved/other",
    }
}

#[inline]
unsafe fn read_cr2() -> u64 {
    let v: u64;
    core::arch::asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags));
    v
}

/// Called by `isr_common` with the assembled frame. Dumps state and halts the guest.
extern "C" fn exception_dispatch(frame: *const ExceptionFrame) -> ! {
    let f = unsafe { &*frame };
    kprintln!();
    kprintln!(
        "*** CPU EXCEPTION {} ({}) ***",
        f.vector,
        vector_name(f.vector)
    );
    kprintln!("  error_code = {:#x}", f.error_code);
    if f.vector == 14 {
        kprintln!("  CR2 (faulting addr) = {:#018x}", unsafe { read_cr2() });
    }
    kprintln!(
        "  RIP={:#018x} CS={:#06x} RFLAGS={:#018x}",
        f.rip,
        f.cs,
        f.rflags
    );
    kprintln!("  RSP={:#018x} SS={:#06x}", f.rsp, f.ss);
    kprintln!(
        "  RAX={:#018x} RBX={:#018x} RCX={:#018x} RDX={:#018x}",
        f.rax,
        f.rbx,
        f.rcx,
        f.rdx
    );
    kprintln!(
        "  RSI={:#018x} RDI={:#018x} RBP={:#018x}",
        f.rsi,
        f.rdi,
        f.rbp
    );
    kprintln!(
        "  R8 ={:#018x} R9 ={:#018x} R10={:#018x} R11={:#018x}",
        f.r8,
        f.r9,
        f.r10,
        f.r11
    );
    kprintln!(
        "  R12={:#018x} R13={:#018x} R14={:#018x} R15={:#018x}",
        f.r12,
        f.r13,
        f.r14,
        f.r15
    );
    qemu::exit(qemu::EXIT_FAILURE);
}

// ---- timer IRQ (preemption) ----

extern "C" {
    /// The scheduler-aware timer handler (provided by the nucleus). Receives the on-stack
    /// frame and never returns — it resumes some process via `syscall::resume`.
    fn rustproof_timer_trap(frame: *mut u64) -> !;
}

/// Timer IRQ entry (vector [`pic::TIMER_VECTOR`]). On an interrupt from ring 3 the CPU has
/// already pushed `rip/cs/rflags/rsp/ss`; we push the 15 GPRs in the SAME order the syscall
/// stub uses, so the result is exactly a [`syscall::TrapFrame`]. It is handed to the
/// scheduler, which never returns (it resumes a process via `syscall::resume`). Because the
/// timer only fires in ring 3 (the kernel runs with interrupts masked), the CPU always
/// switches to the TSS.rsp0 stack and pushes the full 5-word frame, so the 16-byte
/// alignment for the call is exact without a manual fixup.
#[unsafe(naked)]
extern "C" fn timer_isr() {
    core::arch::naked_asm!(
        // Clear DF: an interrupt gate clears IF/TF but NOT DF, and a ring-3 process may
        // have set it (`std` is unprivileged). The Rust handler's `rep movs`-lowered
        // copies assume the SysV DF=0 invariant, so entering with DF=1 would copy backward
        // and corrupt kernel memory. (The syscall path gets this from FMASK bit 0x400.)
        "cld",
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
        "push r15", // 15 GPRs over the CPU frame -> a syscall::TrapFrame
        "mov rdi, rsp",
        "call {trap}",
        "ud2",
        trap = sym rustproof_timer_trap,
    );
}

/// Install a handler at `vector` (used for the timer IRQ after the base exception vectors
/// are set by [`init`]). Uses the same 64-bit interrupt gate (IF auto-clears on entry).
pub fn set_gate(vector: usize, handler: u64) {
    unsafe {
        let idt = core::ptr::addr_of_mut!(IDT);
        (*idt)[vector].set_handler(handler);
    }
}

/// Address of the timer IRQ stub, for [`set_gate`].
pub fn timer_handler_addr() -> u64 {
    timer_isr as *const () as u64
}

// ---- IDT ----

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const fn missing() -> Self {
        IdtEntry {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    fn set_handler(&mut self, handler: u64) {
        self.offset_low = handler as u16;
        self.offset_mid = (handler >> 16) as u16;
        self.offset_high = (handler >> 32) as u32;
        self.selector = 0x08; // kernel code segment from the boot GDT
        self.ist = 0;
        self.type_attr = 0x8E; // present, DPL0, 64-bit interrupt gate
        self.reserved = 0;
    }
}

#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

static mut IDT: [IdtEntry; 256] = [IdtEntry::missing(); 256];

/// Install handlers for vectors 0..32 and load the IDT.
pub fn init() {
    unsafe {
        let idt = core::ptr::addr_of_mut!(IDT);
        for (i, stub) in STUBS.iter().enumerate() {
            (*idt)[i].set_handler(*stub as usize as u64);
        }
        let idtr = Idtr {
            limit: (core::mem::size_of::<[IdtEntry; 256]>() - 1) as u16,
            base: idt as u64,
        };
        core::arch::asm!("lidt [{}]", in(reg) &idtr, options(readonly, nostack, preserves_flags));
    }
}
