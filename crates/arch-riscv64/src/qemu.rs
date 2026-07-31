//! Clean QEMU shutdown via the SiFive test finisher device.
//!
//! QEMU's `virt` machine maps the `sifive_test` MMIO device at 0x10_0000. A 32-bit
//! write of `0x5555` requests a successful power-off; `0x3333` requests a failed
//! exit. If the device somehow does not shut the guest down, we spin on `wfi`.

/// MMIO base of the SiFive test finisher device on the QEMU virt board.
const SIFIVE_TEST: usize = 0x10_0000;
/// Finisher command: pass / clean shutdown (QEMU process exit code 0).
const FINISHER_PASS: u32 = 0x5555;
/// Finisher command: fail / error shutdown -> QEMU process exit code 1.
///
/// The exit code is the HIGH half: `sifive_test` computes `(value >> 16) & 0xffff` and exits
/// with it, so a bare `0x3333` asks QEMU to fail and then exits **zero** — indistinguishable
/// from the clean shutdown `FINISHER_PASS` produces, i.e. a kernel that gave up looking
/// successful to anything reading `$?`. Measured, not assumed: `0x3333` -> 0, `0x1_3333` -> 1.
///
/// x86 differs and cannot be made to match: its `isa-debug-exit` device computes
/// `(code << 1) | 1`, so EVERY exit is odd — 33 on success, 35 on failure. There "0 means
/// success" never holds. Hence the runner scripts decide PASS/FAIL from console output on
/// both arches; this constant only stops the riscv status from actively lying.
const FINISHER_FAIL: u32 = (1 << 16) | 0x3333;

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
