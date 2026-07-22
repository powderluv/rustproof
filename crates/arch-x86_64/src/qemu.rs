//! Clean QEMU shutdown via the `isa-debug-exit` device.
//!
//! Run QEMU with `-device isa-debug-exit,iobase=0xf4,iosize=0x04`. Writing value `v`
//! to port 0xf4 makes QEMU exit with process code `(v << 1) | 1`.
use crate::port::outl;

const ISA_DEBUG_EXIT: u16 = 0xf4;

/// Written on success -> QEMU process exit code `(0x10 << 1) | 1 = 33`.
pub const EXIT_SUCCESS: u32 = 0x10;
/// Written on failure -> QEMU process exit code `(0x11 << 1) | 1 = 35`.
pub const EXIT_FAILURE: u32 = 0x11;

/// Ask QEMU to exit with the given code, then halt forever if it did not.
pub fn exit(code: u32) -> ! {
    unsafe { outl(ISA_DEBUG_EXIT, code) };
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}
