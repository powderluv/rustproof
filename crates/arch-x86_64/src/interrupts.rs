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

/// Called by `isr_common` with the assembled frame. A fault taken in RING 3 kills just that
/// process and the nucleus keeps running; a fault taken in the kernel dumps and halts,
/// because that means the kernel itself is broken.
extern "C" fn exception_dispatch(frame: *const ExceptionFrame) -> ! {
    let f = unsafe { &*frame };
    // The CPU pushes the interrupted CS; its low two bits are the privilege level that was
    // running, so RPL 3 means user code faulted — its failure, not the machine's. But only
    // for a SYNCHRONOUS exception: NMI (2) and #MC (18) are machine events that merely
    // happened to arrive while a process was running, and #DF (8) means the CPU could not
    // deliver an earlier exception at all. Killing the current process for those would
    // blame it for something it did not do, and would paper over a broken kernel.
    let attributable = !matches!(f.vector, 2 | 8 | 18);
    if attributable && f.cs & 3 == 3 {
        let name = vector_name(f.vector);
        let addr = if f.vector == 14 {
            // SAFETY: reading CR2 is always valid; it holds the faulting address for #PF.
            unsafe { read_cr2() }
        } else {
            f.rip
        };
        // SAFETY: on the kernel fault stack with interrupts masked; the nucleus's CURRENT
        // is the process that was running in ring 3.
        unsafe { rustproof_fault_trap(name.as_ptr(), name.len(), addr) }
    }
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
    /// The nucleus's user-fault handler: kills the faulting process and resumes another.
    /// The reason crosses as a (ptr, len) pair so no `str` crosses the FFI boundary.
    fn rustproof_fault_trap(what: *const u8, what_len: usize, addr: u64) -> !;
    /// The nucleus's DEVICE interrupt handler (the console's IRQ4). Same frame layout and
    /// same never-returns contract as the timer; it credits a different logical line.
    fn rustproof_device_trap(frame: *mut u64) -> !;
}

/// Timer IRQ entry (vector [`pic::TIMER_VECTOR`]). On an interrupt from ring 3 the CPU has
/// already pushed `rip/cs/rflags/rsp/ss`; we push the 15 GPRs in the SAME order the syscall
/// stub uses, so the result is exactly a [`syscall::TrapFrame`]. It is handed to the
/// scheduler, which never returns (it resumes a process via `syscall::resume`).
///
/// This used to say the timer only ever fires in ring 3, since the kernel runs with
/// interrupts masked. That stopped being true when the idle park arrived: `Arch::idle`
/// enables interrupts, so a tick now lands in RING 0 on the idle stack hundreds of times a
/// boot, and the nucleus tells the two apart with its `IDLING` flag. The frame layout is
/// identical either way, but for a stronger reason than the old one: in long mode the CPU
/// pushes `rip/cs/rflags/rsp/ss` for EVERY interrupt — unlike 32-bit, `ss:rsp` is pushed
/// even without a privilege change — and 16-byte-aligns `rsp` as part of delivery. So the
/// 5-word frame and the alignment this stub's `call` needs hold whether we came from ring 3
/// via `TSS.rsp0` or from the parked kernel on its own stack.
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

/// Console IRQ entry (vector [`pic::CONSOLE_VECTOR`], COM1 receive). Byte-for-byte the
/// timer stub's twin — same `cld`, same 15 GPRs over the CPU frame, same never-returns
/// contract — differing only in which nucleus entry point it calls, and therefore which
/// logical interrupt line gets credited. Kept as a separate stub rather than one
/// parameterised handler because a naked function cannot take arguments, and passing the
/// vector on the stack would change the frame layout the scheduler depends on.
///
/// Like the timer, this can fire while the kernel is PARKED in `Arch::idle` — for this one
/// that is the entire point, a device waking an idle machine — so it lands in ring 0 on the
/// idle stack rather than in ring 3. See `timer_isr` for why the frame layout and stack
/// alignment are the same in both cases; the nucleus tells them apart via `IDLING`.
#[unsafe(naked)]
extern "C" fn console_isr() {
    core::arch::naked_asm!(
        "cld", // see `timer_isr`: an interrupt gate does not clear DF, ring 3 can set it
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
        "push r15",
        "mov rdi, rsp",
        "call {trap}",
        "ud2",
        trap = sym rustproof_device_trap,
    );
}

/// Address of the console IRQ entry, for [`set_gate`].
pub fn console_handler_addr() -> u64 {
    console_isr as *const () as u64
}

/// Spurious-interrupt entry (vector [`pic::SPURIOUS_VECTOR`], IRQ7 on the master 8259).
///
/// The 8259 delivers IRQ7 when a line deasserts between the CPU's two interrupt-acknowledge
/// cycles — it has no real requester to report, so it reports the lowest-priority one. We
/// never unmask IRQ7, so any delivery here is spurious by construction: return immediately
/// and, deliberately, do NOT send an end-of-interrupt. A spurious interrupt sets no
/// in-service bit, so EOI-ing it would clear some *other* interrupt's, silently losing it.
///
/// Reachable only since a second line was unmasked. Before that, vector 0x27 had no gate at
/// all (`STUBS` covers the 32 exception vectors), so its present bit was clear and a
/// spurious IRQ would have raised #GP and taken the fatal path — a hard guest death from a
/// condition the hardware considers routine.
#[unsafe(naked)]
extern "C" fn spurious_isr() {
    core::arch::naked_asm!("iretq");
}

/// Address of the spurious-interrupt entry, for [`set_gate`].
pub fn spurious_handler_addr() -> u64 {
    spurious_isr as *const () as u64
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
