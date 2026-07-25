//! nucleus — the bootable x86-64 Rustproof kernel image: a thin shim over the generic
//! `kernel` crate. Boot trampoline + serial init + the syscall-dispatch symbol; all the
//! kernel logic lives in `kernel::run::<CurrentArch>` behind `hal::Arch`.
#![no_std]
#![no_main]

use kernel::CurrentArch;

/// The `init` user program, staged by tools/run-qemu.sh (empty until then; build.rs
/// guarantees the file exists so include_bytes! compiles).
static USER_ELF: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/user.elf"));

// The 32->64-bit boot trampoline + PVH note. Provides `_start`; calls `kmain`.
core::arch::global_asm!(include_str!("boot.s"), options(att_syntax));

#[no_mangle]
pub extern "C" fn kmain(start_info: u64) -> ! {
    unsafe { arch_x86_64::serial::Serial::init() };
    kernel::run::<CurrentArch>(start_info, 0, USER_ELF)
}

/// The `syscall` entry stub (arch-x86_64) tail-calls this symbol with a pointer to the
/// trap frame it built on the kernel stack. The scheduler-aware handler never returns.
#[no_mangle]
extern "C" fn rustproof_syscall_trap(frame: *mut u64) -> ! {
    // SAFETY: `frame` is the on-kernel-stack trap frame the arch entry stub just built.
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

/// The timer IRQ stub (arch-x86_64, vector 0x20) tail-calls this symbol with the same
/// frame layout. The scheduler preempts the running process and never returns.
#[no_mangle]
extern "C" fn rustproof_timer_trap(frame: *mut u64) -> ! {
    // SAFETY: `frame` is the on-kernel-stack timer trap frame the arch IRQ stub built.
    unsafe { kernel::preempt_trap::<CurrentArch>(frame) }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    arch_x86_64::kprintln!("nucleus PANIC: {}", info);
    arch_x86_64::qemu::exit(arch_x86_64::qemu::EXIT_FAILURE);
}
