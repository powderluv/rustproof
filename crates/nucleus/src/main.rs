//! nucleus — the bootable Rustproof guest kernel image.
//!
//! M0 boot spike (T0.1): PVH entry -> long-mode trampoline (src/boot.s) -> `kmain`,
//! which brings up COM1 serial, prints a banner, and exits QEMU cleanly. Everything
//! above this (address spaces, capabilities, IPC, scheduler) is later M0/M1 work; see
//! docs/milestone-M0.md.
#![no_std]
#![no_main]

use arch_x86_64::{kprintln, qemu};

// The 32->64-bit boot trampoline + PVH note. Provides `_start`; calls `kmain`.
core::arch::global_asm!(include_str!("boot.s"), options(att_syntax));

/// 64-bit Rust entry, called by the trampoline with the PVH `start_info` pointer.
#[no_mangle]
pub extern "C" fn kmain(start_info: u64) -> ! {
    unsafe { arch_x86_64::serial::Serial::init() };
    kprintln!();
    kprintln!("Rustproof nucleus — M0 boot spike");
    kprintln!("  long mode reached; COM1 serial up");
    kprintln!("  identity map: low 1 GiB, 2 MiB pages");
    kprintln!("  PVH start_info @ {:#018x}", start_info);
    kprintln!("rustproof: BOOT OK");
    qemu::exit(qemu::EXIT_SUCCESS);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    kprintln!("nucleus PANIC: {}", info);
    qemu::exit(qemu::EXIT_FAILURE);
}
