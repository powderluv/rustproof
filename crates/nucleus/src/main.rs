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

/// The `syscall` entry stub (arch-x86_64) tail-calls this symbol.
#[no_mangle]
extern "C" fn rustproof_syscall_dispatch(
    num: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
) -> u64 {
    kernel::handle_syscall::<CurrentArch>(num, a0, a1, a2, a3, a4)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    arch_x86_64::kprintln!("nucleus PANIC: {}", info);
    arch_x86_64::qemu::exit(arch_x86_64::qemu::EXIT_FAILURE);
}
