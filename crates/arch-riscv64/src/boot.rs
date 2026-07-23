//! Boot entry for the RISC-V nucleus image.
//!
//! OpenSBI loads the kernel ELF (linked at 0x8020_0000) and jumps to `_start` in
//! S-mode with `a0` = hartid and `a1` = the DTB pointer. `_start`:
//!   1. sets `sp` to the top of a 16-aligned boot stack reserved in `.bss`;
//!   2. zeroes `.bss` (`[__bss_start, __bss_end)`, provided by the linker script);
//!   3. calls `kmain(a0, a1)` — supplied by the nucleus image — passing hartid + DTB
//!      through unchanged.
//! `kmain` is `-> !`; if it ever returns we park the hart on `wfi`.

/// Boot stack size (64 KiB). Lives in `.bss`; zeroed by the `_start` bss loop.
const BOOT_STACK_SIZE: usize = 64 * 1024;

/// 16-byte-aligned boot stack backing store. `sp` is set to `&BOOT_STACK + size`.
#[repr(C, align(16))]
struct BootStack([u8; BOOT_STACK_SIZE]);

// Uninitialized => placed in `.bss` (bracketed by __bss_start/__bss_end and zeroed).
static mut BOOT_STACK: BootStack = BootStack([0; BOOT_STACK_SIZE]);

core::arch::global_asm!(
    ".pushsection .text._start, \"ax\", @progbits",
    ".balign 4",
    ".global _start",
    "_start:",
    // sp = top of the boot stack (grows down). BOOT_STACK is 16-aligned; the size is a
    // multiple of 16, so the top stays 16-aligned as the SysV RV ABI requires.
    // PROOF(later): `sp` on entry to kmain is 16-byte aligned and lies within
    // [BOOT_STACK, BOOT_STACK + BOOT_STACK_SIZE], so no call spills off the boot stack.
    "la   sp, {stack}",
    "li   t0, {stack_size}",
    "add  sp, sp, t0",
    // Zero .bss word-by-word: for (t0 = __bss_start; t0 < __bss_end; t0 += 8) *t0 = 0.
    "la   t0, __bss_start",
    "la   t1, __bss_end",
    "1:",
    "bgeu t0, t1, 2f",
    "sd   zero, 0(t0)",
    "addi t0, t0, 8",
    "j    1b",
    "2:",
    // a0 = hartid, a1 = DTB — untouched above — pass straight through to kmain.
    "call kmain",
    // kmain is `-> !`; park if it ever returns.
    "3:",
    "wfi",
    "j    3b",
    ".popsection",
    stack = sym BOOT_STACK,
    stack_size = const BOOT_STACK_SIZE,
);
