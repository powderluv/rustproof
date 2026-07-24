#![no_std]
#![no_main]

//! riscv-init -- the untrusted U-mode (ring-3 equivalent) user program the kernel
//! loads and runs on RISC-V. It is a self-contained, non-PIE ET_EXEC linked at
//! the USER virtual base 0x1_0000_0000 (4 GiB; see `link.ld`). `_start` runs a
//! small host-contract demo over the fixed `ecall` ABI (see [`abi::sysno`]) and
//! then exits.
//!
//! This mirrors the x86-64 `init` crate, but for the RISC-V `ecall` syscall
//! convention. Everything here is *user* code, not TCB: it can only touch memory
//! the kernel mapped into this process, and every kernel interaction goes through
//! the `ecall` syscall stub below.
//!
//! ## Addressing note
//! Unlike x86-64 `init` (base 512 GiB, which forces the large code model to reach
//! `.rodata` with 64-bit absolute relocations), RISC-V reaches its own image's
//! `.rodata`/`.data` symbols PC-relative through `auipc` under the default medium
//! (medany) code model. Code and data are adjacent in the image, so every
//! cross-section offset is tiny and stays in range at any link base, 4 GiB
//! included. No `movabs`/`lea`/raw-store tricks are needed: ordinary `&[u8]`
//! literals and bounds-checked indexing compile and link cleanly in every
//! profile -- the panic/precondition paths reference `.rodata` PC-relative too.
//!
//! PROOF(later): the program only touches its own mapped user memory -- every
//! load and store is to a stack local or to this image's own `static` .rodata;
//! the only pointers handed to the kernel are addresses of stack locals passed as
//! syscall out-buffers, plus `&'static` bytes passed to DEBUG_WRITE.

use abi::{syserr, sysno, AllocResp, CapId, GpuInfo, MapBarResp};

// ----------------------------------------------------------------- syscall stub
//
// The fixed user->kernel calling convention (matches the nucleus-riscv trap
// handler / `rustproof_syscall_trap`):
//   a7        = syscall number (one of `abi::sysno`)
//   a0..a4    = args
//   a0        = result (all other registers preserved, Linux-style)
//
// SAFETY (the stub): raw `ecall` traps into S-mode. We never mark `nomem`
// because several syscalls read (DEBUG_WRITE) or write (GET_INFO / MAP_BAR /
// ALLOC_VRAM) user memory through pointer args, so the compiler must treat
// memory as live across the trap. `nostack` is sound: `ecall` touches no stack.

/// Perform an `ecall` with up to five arguments. `num` goes in `a7`, `a0..a4`
/// carry the arguments, and the kernel's result comes back in `a0`.
///
/// # Safety
/// Traps into the kernel. Any pointer passed in `a0..a4` must be valid for the
/// access the named syscall performs (read for DEBUG_WRITE; write for the
/// out-buffer syscalls) for the duration of the call.
#[inline]
unsafe fn syscall(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret: u64;
    // SAFETY: see module note above; not `nomem` because pointer args alias
    // live user memory the kernel reads/writes.
    core::arch::asm!(
        "ecall",
        in("a7") num,
        inlateout("a0") a0 => ret,
        in("a1") a1,
        in("a2") a2,
        in("a3") a3,
        in("a4") a4,
        options(nostack),
    );
    ret
}

// ------------------------------------------------------------ typed host client

/// Write `bytes` to the debug console (a0 = ptr, a1 = len).
fn debug_write(bytes: &[u8]) {
    // SAFETY: `bytes` is a live borrow for the duration of the call; the kernel
    // reads exactly `len` bytes from `ptr`.
    unsafe {
        syscall(
            sysno::DEBUG_WRITE,
            bytes.as_ptr() as u64,
            bytes.len() as u64,
            0,
            0,
            0,
        );
    }
}

/// Ask the kernel for device info. Passes a stack `GpuInfo` out-pointer in a0 and
/// returns the filled-in value.
fn get_info() -> GpuInfo {
    let mut info = GpuInfo::default();
    // SAFETY: `&mut info` is a valid, writable, correctly-aligned out-buffer for
    // the syscall's duration; the compiler reloads `info` after the trap because
    // the stub is not `nomem`.
    unsafe {
        syscall(
            sysno::GET_INFO,
            &mut info as *mut GpuInfo as u64,
            0,
            0,
            0,
            0,
        );
    }
    info
}

/// Map a device BAR through an `Mmio` capability. a0 = cap id, a1 = BAR index,
/// a2 = `*mut MapBarResp`. Maps a nonzero [`syserr`] code to `Err`.
fn map_bar(cap: CapId, bar: u64) -> Result<MapBarResp, u64> {
    let mut resp = MapBarResp::default();
    // SAFETY: `&mut resp` is a valid writable out-buffer for the call's duration.
    let rc = unsafe {
        syscall(
            sysno::MAP_BAR,
            cap.0 as u64,
            bar,
            &mut resp as *mut MapBarResp as u64,
            0,
            0,
        )
    };
    if rc == syserr::OK {
        Ok(resp)
    } else {
        Err(rc)
    }
}

/// Allocate DMA-capable VRAM through an `Untyped` capability. a0 = cap id,
/// a1 = byte size, a2 = `*mut AllocResp`. Maps a nonzero [`syserr`] code to `Err`.
fn alloc_vram(cap: CapId, size: u64) -> Result<AllocResp, u64> {
    let mut resp = AllocResp::default();
    // SAFETY: `&mut resp` is a valid writable out-buffer for the call's duration.
    let rc = unsafe {
        syscall(
            sysno::ALLOC_VRAM,
            cap.0 as u64,
            size,
            &mut resp as *mut AllocResp as u64,
            0,
            0,
        )
    };
    if rc == syserr::OK {
        Ok(resp)
    } else {
        Err(rc)
    }
}

/// Terminate this process with `code`. Never returns.
fn exit(code: u64) -> ! {
    // SAFETY: EXIT does not return to user mode.
    unsafe {
        syscall(sysno::EXIT, code, 0, 0, 0, 0);
    }
    // Defensive: if the kernel ever returns from EXIT, spin instead of running
    // off the end of `_start`.
    loop {
        core::hint::spin_loop();
    }
}

/// Busy-compute for ~`iters` iterations without making any syscall, so only the supervisor
/// timer interrupt can interleave this process with its siblings. `core::hint::black_box`
/// keeps the loop from being optimized away.
fn spin(iters: u64) {
    let mut i = 0u64;
    while core::hint::black_box(i) < iters {
        i = i.wrapping_add(1);
    }
}

// --------------------------------------------------------- no_std number format
//
// Tiny fixed-buffer formatters: render a u64 into a caller-provided stack buffer,
// right-justified, and return the start index of the written text -- so the demo
// can print numbers without heap or `core::fmt`. `dbg_hex` / `dbg_dec` wrap the
// raw debug-write.
//
// Ordinary bounds-checked indexing is fine here: on RISC-V (medany) the panic
// preconditions those checks introduce reference `.rodata` PC-relative, so they
// impose no addressing constraint at the 4 GiB link base (contrast x86 `init`,
// which had to hand-roll raw stores to avoid absolute `.rodata` relocations).

/// Render `val` as `0x`-prefixed lowercase hex, right-justified into `buf`.
/// Returns the index of the first written byte. `buf` fits the worst case:
/// `"0x" + 16 hex digits` = 18 bytes.
fn fmt_hex(val: u64, buf: &mut [u8; 18]) -> usize {
    let mut i = buf.len();
    let mut v = val;
    loop {
        let nib = (v & 0xf) as u8;
        i -= 1;
        buf[i] = if nib < 10 {
            b'0' + nib
        } else {
            b'a' + (nib - 10)
        };
        v >>= 4;
        if v == 0 {
            break;
        }
    }
    i -= 1;
    buf[i] = b'x';
    i -= 1;
    buf[i] = b'0';
    i
}

/// Render `val` as base-10 ASCII, right-justified into `buf`. Returns the index
/// of the first written byte. `buf` fits the worst case: 20 digits (`u64::MAX`).
fn fmt_dec(val: u64, buf: &mut [u8; 20]) -> usize {
    let mut i = buf.len();
    let mut v = val;
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    i
}

/// Format `val` as hex into a stack buffer and write it to the debug console.
fn dbg_hex(val: u64) {
    let mut buf = [0u8; 18];
    let start = fmt_hex(val, &mut buf);
    debug_write(&buf[start..]);
}

/// Format `val` as decimal into a stack buffer and write it to the debug console.
fn dbg_dec(val: u64) {
    let mut buf = [0u8; 20];
    let start = fmt_dec(val, &mut buf);
    debug_write(&buf[start..]);
}

// ------------------------------------------------------------------- demo entry

/// Print this process's id tag (`[proc N] `) so interleaved scheduler output is legible.
fn tag(id: u64) {
    debug_write(b"[proc ");
    dbg_dec(id);
    debug_write(b"] ");
}

/// The ELF entry point. The kernel loader jumps here in U-mode with a fresh stack and this
/// process's id in the first-argument register (`a0`). It runs a compute loop with **no**
/// syscalls between prints, so the only thing that can interleave it with its siblings is
/// the supervisor timer preempting it — if the interleaved `[proc N] tick K` lines appear,
/// preemptive scheduling works on RISC-V. It then exercises the per-process host contract
/// and exits with its id. Never returns (it exits via the EXIT syscall).
///
/// Declared `extern "C"` / `#[no_mangle]` so the linker resolves `_start` (the
/// `ENTRY` of `link.ld`); the `id` parameter reads the first-arg register.
#[no_mangle]
pub extern "C" fn _start(id: u64) -> ! {
    tag(id);
    debug_write(b"start (compute loop, no yields -- preemption only)\n");

    // Pure compute: NO syscall between ticks. Without preemption, process `id` would run
    // this whole loop and exit before any sibling got the CPU; the interleaved output is
    // the timer preempting mid-`spin`. Each process's register/PC state is saved and
    // restored exactly across every preemption (that is what makes the loop resume).
    let mut tick = 0u64;
    while tick < 5 {
        tag(id);
        debug_write(b"tick ");
        dbg_dec(tick);
        debug_write(b"\n");
        spin(5_000_000);
        tick = tick.wrapping_add(1);
    }

    // Per-process capabilities still gate the host contract under preemption.
    let info = get_info();
    tag(id);
    debug_write(b"gpu gfx_version=");
    dbg_hex(info.gfx_version as u64);
    debug_write(b"\n");
    match map_bar(CapId(1), 0) {
        Ok(r) => {
            tag(id);
            debug_write(b"map_bar user_va=");
            dbg_hex(r.user_va);
            debug_write(b"\n");
        }
        Err(e) => {
            tag(id);
            debug_write(b"map_bar err=");
            dbg_hex(e);
            debug_write(b"\n");
        }
    }
    match alloc_vram(CapId(2), 4096) {
        Ok(r) => {
            tag(id);
            debug_write(b"alloc_vram phys=");
            dbg_hex(r.phys);
            debug_write(b"\n");
        }
        Err(e) => {
            tag(id);
            debug_write(b"alloc_vram err=");
            dbg_hex(e);
            debug_write(b"\n");
        }
    }

    exit(id);
}

/// Any panic in U-mode is fatal: report via the exit code and terminate.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(255);
}
