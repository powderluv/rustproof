//! Clean QEMU shutdown via the SiFive test finisher device.
//!
//! QEMU's `virt` machine maps the `sifive_test` MMIO device at 0x10_0000. A 32-bit
//! write of `0x5555` requests a successful power-off; `0x3333` requests a failed
//! exit. If the device somehow does not shut the guest down, we spin on `wfi`.

/// MMIO base of the SiFive test finisher device on the QEMU virt board.
const SIFIVE_TEST: usize = 0x10_0000;
/// Finisher command: pass / clean shutdown (QEMU process exit code 0).
const FINISHER_PASS: u32 = 0x5555;
/// Finisher command: fail / error shutdown (QEMU process exits non-zero).
const FINISHER_FAIL: u32 = 0x3333;

#[inline]
fn finish(code: u32) {
    // SAFETY: fixed MMIO register on the QEMU virt board; volatile 4-byte write.
    unsafe { core::ptr::write_volatile(SIFIVE_TEST as *mut u32, code) };
}

#[inline]
fn park() -> ! {
    loop {
        // SAFETY: `wfi` merely idles the hart until an interrupt; always valid.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}

/// Ask QEMU to power off successfully, then halt forever if it did not.
pub fn exit_success() -> ! {
    finish(FINISHER_PASS);
    park()
}

/// Ask QEMU to exit with a failure code, then halt forever if it did not.
pub fn exit_fail() -> ! {
    finish(FINISHER_FAIL);
    park()
}
