//! nucleus-riscv — the bootable RISC-V Rustproof kernel image: a thin shim over the
//! generic `kernel` crate. `_start` + boot stack live in arch-riscv64; the kernel logic
//! lives in `kernel::run::<CurrentArch>` behind `hal::Arch`.
#![no_std]
#![no_main]

use kernel::CurrentArch;

/// The `riscv-init` user program, staged by tools/run-qemu-riscv.sh.
static USER_ELF: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/user.elf"));

/// S-mode entry, called by the arch-riscv64 boot trampoline with OpenSBI's a0/a1.
#[no_mangle]
pub extern "C" fn kmain(hartid: u64, dtb: u64) -> ! {
    kernel::run::<CurrentArch>(hartid, dtb, USER_ELF)
}

/// The `ecall`-from-U trap path (arch-riscv64) calls this symbol with a pointer to the
/// trap frame it assembled on the kernel stack. The scheduler-aware handler never returns.
#[no_mangle]
extern "C" fn rustproof_syscall_trap(frame: *mut u64) -> ! {
    // SAFETY: `frame` is the on-kernel-stack trap frame the arch trap vector built.
    unsafe { kernel::syscall_trap::<CurrentArch>(frame) }
}

/// The arch fault stub calls this when USER code faulted: that process is killed and
/// another resumed, so one process's wild pointer cannot take down the machine.
#[no_mangle]
extern "C" fn rustproof_fault_trap(what: *const u8, what_len: usize, addr: u64) -> ! {
    // SAFETY: the arch handler passes the parts of a `&'static str`, valid for this call.
    let what =
        unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(what, what_len)) };
    // SAFETY: called from the arch fault stub with interrupts masked.
    unsafe { kernel::fault_trap::<CurrentArch>(what, addr) }
}

/// The supervisor-timer trap path (arch-riscv64) calls this symbol with the same frame
/// layout. The scheduler preempts the running process and never returns.
#[no_mangle]
extern "C" fn rustproof_timer_trap(frame: *mut u64) -> ! {
    // SAFETY: `frame` is the on-kernel-stack timer trap frame the arch trap vector built.
    unsafe { kernel::preempt_trap::<CurrentArch>(frame) }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    arch_riscv64::kprintln!("nucleus-riscv PANIC: {}", info);
    arch_riscv64::qemu::exit_fail();
}
