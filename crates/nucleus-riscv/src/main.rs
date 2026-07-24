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

/// The `ecall`-from-U trap path (arch-riscv64) calls this symbol.
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
    arch_riscv64::kprintln!("nucleus-riscv PANIC: {}", info);
    arch_riscv64::qemu::exit_fail();
}
