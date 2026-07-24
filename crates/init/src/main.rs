#![no_std]
#![no_main]

//! init -- the untrusted ring-3 user program the kernel loads and runs in user
//! mode. It is a self-contained, non-PIE ET_EXEC linked at the USER virtual base
//! 0x80_0000_0000 (see `link.ld`). `_start` runs a small host-contract demo over
//! the fixed `syscall` ABI (see [`abi::sysno`]) and then exits.
//!
//! Everything here is *user* code, not TCB: it can only touch memory the kernel
//! mapped into this process, and every kernel interaction goes through the
//! syscall stubs below.
//!
//! ## Addressing note
//! The image links at 0x80_0000_0000 (512 GiB). The workspace forces
//! `relocation-model=static`, whose default (small) code model can only encode a
//! 32-bit absolute address — far short of 512 GiB, so any absolute reference to a
//! `.rodata`/`.data` symbol overflows its relocation at link time. Two source-level
//! measures keep this crate link-clean in every profile from any directory:
//!   * string constants are reached by a RIP-relative `lea` (see [`dw!`]), whose
//!     `R_X86_64_PC32` offset stays tiny (code and data are adjacent in the image);
//!   * the numeric formatters avoid every operation with a panic / debug-precondition
//!     path (bounds-checked indexing, checked arithmetic, `get_unchecked`, raw-store
//!     null checks) — each such path would reference an absolute `.rodata` message.
//! The crate also ships a `.cargo/config.toml` selecting `code-model=large` (64-bit
//! absolute relocations) for builds invoked from within the crate directory; the
//! source-level care above additionally makes root-invoked builds link, so both hold.
//!
//! PROOF(later): the program only touches its own mapped user memory — every load
//! and store is to a stack local or to this image's own `static` .rodata; the only
//! pointers handed to the kernel are addresses of stack locals passed as syscall
//! out-buffers, plus `&'static` bytes passed to DEBUG_WRITE.

use abi::{syserr, sysno, AllocResp, CapId, GpuInfo, MapBarResp};

// ----------------------------------------------------------------- syscall stubs
//
// The fixed user->kernel calling convention (matches the kernel dispatcher):
//   rax = syscall number (one of `abi::sysno`)
//   args a0..a4 in rdi, rsi, rdx, r10, r8
//   result returned in rax
//   the `syscall` instruction itself clobbers rcx (saved rip) and r11 (saved rflags)
//
// SAFETY (all stubs): raw `syscall` traps into ring 0. We never mark `nomem`
// because several syscalls read (DEBUG_WRITE) or write (GET_INFO/MAP_BAR/
// ALLOC_VRAM) user memory through pointer args, so the compiler must treat
// memory as live across the trap. `nostack` is sound: the instruction touches
// no stack and no red zone.

#[inline]
#[allow(dead_code)] // provided for completeness of the syscall0..3 family
unsafe fn syscall0(num: u64) -> u64 {
    let ret: u64;
    // SAFETY: see module note above.
    core::arch::asm!(
        "syscall",
        in("rax") num,
        lateout("rax") ret,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}

#[inline]
unsafe fn syscall1(num: u64, a0: u64) -> u64 {
    let ret: u64;
    // SAFETY: see module note above.
    core::arch::asm!(
        "syscall",
        in("rax") num,
        in("rdi") a0,
        lateout("rax") ret,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}

#[inline]
unsafe fn syscall2(num: u64, a0: u64, a1: u64) -> u64 {
    let ret: u64;
    // SAFETY: see module note above.
    core::arch::asm!(
        "syscall",
        in("rax") num,
        in("rdi") a0,
        in("rsi") a1,
        lateout("rax") ret,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}

#[inline]
unsafe fn syscall3(num: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret: u64;
    // SAFETY: see module note above.
    core::arch::asm!(
        "syscall",
        in("rax") num,
        in("rdi") a0,
        in("rsi") a1,
        in("rdx") a2,
        lateout("rax") ret,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}

// ------------------------------------------------------------ typed host client

/// Write `bytes` to the debug console (a0 = ptr, a1 = len). The typed entry point
/// of the client; the demo itself goes through [`debug_write_raw`] to stay free of
/// the slice machinery that would re-materialize a `.rodata` address absolutely.
#[allow(dead_code)]
fn debug_write(bytes: &[u8]) {
    // SAFETY: pointer/len describe a live borrow for the duration of the call.
    unsafe {
        debug_write_raw(bytes.as_ptr(), bytes.len());
    }
}

/// Raw DEBUG_WRITE over a pointer + length. Used by [`dw!`] and the number
/// formatters to avoid constructing a `&[u8]`, whose slice/precondition machinery
/// would re-materialize the address with an absolute relocation the 512 GiB base
/// cannot satisfy under the small code model.
///
/// # Safety
/// `ptr` must point to at least `len` readable bytes for the duration of the call.
unsafe fn debug_write_raw(ptr: *const u8, len: usize) {
    syscall2(sysno::DEBUG_WRITE, ptr as u64, len as u64);
}

/// Ask the kernel for device info. Passes a stack `GpuInfo` out-pointer in a0 and
/// returns the filled-in value.
fn get_info() -> GpuInfo {
    let mut info = GpuInfo::default();
    // SAFETY: `&mut info` is a valid, writable, correctly-aligned out-buffer for
    // the syscall's duration; the compiler reloads `info` after the trap because
    // the stub is not `nomem`.
    unsafe {
        syscall1(sysno::GET_INFO, &mut info as *mut GpuInfo as u64);
    }
    info
}

/// Map a device BAR through an `Mmio` capability. a0 = cap id, a1 = BAR index,
/// a2 = `*mut MapBarResp`. Maps a nonzero [`syserr`] code to `Err`.
fn map_bar(cap: CapId, bar: u64) -> Result<MapBarResp, u64> {
    let mut resp = MapBarResp::default();
    // SAFETY: `&mut resp` is a valid writable out-buffer for the call's duration.
    let rc = unsafe {
        syscall3(
            sysno::MAP_BAR,
            cap.0 as u64,
            bar,
            &mut resp as *mut MapBarResp as u64,
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
        syscall3(
            sysno::ALLOC_VRAM,
            cap.0 as u64,
            size,
            &mut resp as *mut AllocResp as u64,
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
        syscall1(sysno::EXIT, code);
    }
    // Defensive: if the kernel ever returns from EXIT, spin instead of running off
    // the end. `pause` (spin_loop) is a legal ring-3 hint.
    loop {
        core::hint::spin_loop();
    }
}

/// Busy-compute for ~`iters` iterations without making any syscall, so only the timer
/// interrupt can interleave this process with its siblings. `core::hint::black_box` keeps
/// the loop from being optimized away (and compiles to no `.rodata` reference, so the
/// 512 GiB-linked image stays link-clean).
///
/// Deliberately runs the loop with the direction flag SET (`std`), as hostile ring-3 code
/// could: `std` is unprivileged, and the timer preempts this loop while DF=1. This is a
/// regression test for the kernel — its interrupt entry MUST `cld` before any `rep movs`
/// (the trap-frame save), or entering with DF=1 corrupts kernel memory. DF is restored
/// (`cld`) before returning so the rest of this program is unaffected.
fn spin(iters: u64) {
    // SAFETY: `std`/`cld` only toggle RFLAGS.DF (legal in ring 3); the loop between them
    // performs no string/`rep` operation, so a set DF cannot affect this program.
    unsafe { core::arch::asm!("std", options(nomem, nostack)) };
    let mut i = 0u64;
    while core::hint::black_box(i) < iters {
        i = i.wrapping_add(1);
    }
    unsafe { core::arch::asm!("cld", options(nomem, nostack)) };
}

// ---------------------------------------------------------- static string output

/// Write a string literal to the debug console. The bytes live in their own
/// named `static` (in `.rodata`), whose absolute address we materialize with an
/// explicit 64-bit `movabs`. The default small code model would truncate the
/// 512 GiB address to a 32-bit relocation and overflow at link time; `movabs`
/// forces the full 64-bit immediate (an `R_X86_64_64` relocation).
macro_rules! dw {
    ($lit:expr) => {{
        const N: usize = $lit.len();
        static S: [u8; N] = *$lit;
        let ptr: *const u8;
        // SAFETY: `sym S` names a valid `'static`; the RIP-relative `lea` loads its
        // address as an `R_X86_64_PC32` (offset from here to `S`). `.rodata` sits a
        // few KB from `.text` in the same image, so the 32-bit offset always fits --
        // unlike an absolute reference, which the forced small/static code model
        // would truncate to 32 bits and overflow against the 512 GiB base. No memory
        // is accessed by the asm itself.
        unsafe {
            core::arch::asm!(
                "lea {p}, [rip + {s}]",
                p = out(reg) ptr,
                s = sym S,
                options(nomem, nostack, preserves_flags),
            );
        }
        // SAFETY: `ptr` addresses the `N`-byte `'static` `S`, valid for the whole
        // program. `N` is a compile-time constant (a plain immediate, no relocation).
        unsafe {
            debug_write_raw(ptr, N);
        }
    }};
}

// --------------------------------------------------------- no_std number format
//
// Tiny fixed-buffer formatters: render a u64 into a caller-provided stack buffer,
// right-justified, and return the start index of the written text -- so the demo
// can print numbers without heap or `core::fmt`. `dbg_hex` / `dbg_dec` wrap the
// raw debug-write.
//
// These are deliberately written without bounds-checked indexing, checked
// arithmetic, or slice helpers (`get_unchecked`, `from_raw_parts`). Every such
// operation carries a panic / debug-precondition path that references a `.rodata`
// message via a 32-bit absolute relocation — which cannot reach the 512 GiB link
// base under the forced small/static code model. Raw `wrapping_add` pointer
// stores compile to no such reference, so the demo links in every profile.

/// Store one byte through a raw pointer with a hand-written `mov`. A plain
/// `*ptr = val` compiles (in debug builds) to a null-pointer precondition check
/// that references a `.rodata` panic message with a 32-bit absolute relocation --
/// unreachable from the 512 GiB link base under the small code model. This inline
/// store carries no such check, so the formatters link in every profile.
///
/// # Safety
/// `ptr` must be valid for a 1-byte write.
#[inline]
unsafe fn store_u8(ptr: *mut u8, val: u8) {
    // SAFETY: single byte store to a caller-guaranteed-valid pointer.
    core::arch::asm!(
        "mov byte ptr [{p}], {v}",
        p = in(reg) ptr,
        v = in(reg_byte) val,
        options(nostack, preserves_flags),
    );
}

/// Render `val` as `0x`-prefixed lowercase hex, right-justified into `buf`.
/// Returns the index of the first written byte. `buf` fits the worst case:
/// `"0x" + 16 hex digits` = 18 bytes.
fn fmt_hex(val: u64, buf: &mut [u8; 18]) -> usize {
    let base = buf.as_mut_ptr();
    let mut i = buf.len();
    let mut v = val;
    loop {
        let nib = (v & 0xf) as u8;
        let d = if nib < 10 {
            b'0'.wrapping_add(nib)
        } else {
            b'a'.wrapping_add(nib.wrapping_sub(10))
        };
        i = i.wrapping_sub(1);
        // SAFETY: at most 16 iterations for a u64 leave i >= 2; base+i is inside
        // the 18-byte `buf`.
        unsafe {
            store_u8(base.wrapping_add(i), d);
        }
        v >>= 4;
        if v == 0 {
            break;
        }
    }
    i = i.wrapping_sub(1);
    // SAFETY: i >= 1 (<= 16 digits written into 18 bytes); base+i is in bounds.
    unsafe {
        store_u8(base.wrapping_add(i), b'x');
    }
    i = i.wrapping_sub(1);
    // SAFETY: i >= 0; base+i is in bounds.
    unsafe {
        store_u8(base.wrapping_add(i), b'0');
    }
    i
}

/// Render `val` as base-10 ASCII, right-justified into `buf`. Returns the index of
/// the first written byte. `buf` fits the worst case: 20 digits (`u64::MAX`).
fn fmt_dec(val: u64, buf: &mut [u8; 20]) -> usize {
    let base = buf.as_mut_ptr();
    let mut i = buf.len();
    let mut v = val;
    loop {
        let d = b'0'.wrapping_add((v % 10) as u8);
        i = i.wrapping_sub(1);
        // SAFETY: u64 has at most 20 decimal digits == buf.len(), so i stays >= 0
        // and base+i is inside `buf`.
        unsafe {
            store_u8(base.wrapping_add(i), d);
        }
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
    // SAFETY: `fmt_hex` wrote bytes `start..buf.len()`; that range is initialized
    // and lives on this stack frame for the duration of the call. `wrapping_sub`
    // is exact here because `start <= buf.len()`.
    unsafe {
        debug_write_raw(
            buf.as_ptr().wrapping_add(start),
            buf.len().wrapping_sub(start),
        );
    }
}

/// Format `val` as decimal into a stack buffer and write it to the debug console.
fn dbg_dec(val: u64) {
    let mut buf = [0u8; 20];
    let start = fmt_dec(val, &mut buf);
    // SAFETY: as in `dbg_hex`; `fmt_dec` wrote bytes `start..buf.len()`.
    unsafe {
        debug_write_raw(
            buf.as_ptr().wrapping_add(start),
            buf.len().wrapping_sub(start),
        );
    }
}

// ------------------------------------------------------------------- demo entry

/// Print this process's id tag (`[proc N] `) so the interleaved scheduler output is
/// legible. `id` is the value the kernel placed in the first-argument register at entry.
fn tag(id: u64) {
    dw!(b"[proc ");
    dbg_dec(id);
    dw!(b"] ");
}

/// The ELF entry point. The kernel loader jumps here in ring 3 with a fresh stack and this
/// process's id in the first-argument register (`rdi`). It runs a compute loop with **no**
/// syscalls between prints, so the only thing that can interleave it with its siblings is
/// the timer preempting it — if the interleaved `[proc N] tick K` lines appear, preemptive
/// scheduling works. It then exercises the per-process host contract and exits with its id.
/// Never returns (it exits via the EXIT syscall).
///
/// Declared `extern "C"` / `#[no_mangle]` so the linker resolves `_start` (the
/// `ENTRY` of `link.ld`); the `id` parameter reads the SysV first-arg register.
#[no_mangle]
pub extern "C" fn _start(id: u64) -> ! {
    tag(id);
    dw!(b"start (compute loop, no yields -- preemption only)\n");

    // Pure compute: NO syscall between ticks. Without preemption, process `id` would run
    // this whole loop and exit before any sibling got the CPU; the interleaved output is
    // the timer preempting mid-`spin`. Each process's register/RIP state is saved and
    // restored exactly across every preemption (that is what makes the loop resume).
    let mut tick = 0u64;
    while tick < 5 {
        tag(id);
        dw!(b"tick ");
        dbg_dec(tick);
        dw!(b"\n");
        spin(5_000_000);
        tick = tick.wrapping_add(1);
    }

    // Per-process capabilities still gate the host contract under preemption.
    let info = get_info();
    tag(id);
    dw!(b"gpu gfx_version=");
    dbg_hex(info.gfx_version as u64);
    dw!(b"\n");
    match map_bar(CapId(1), 0) {
        Ok(r) => {
            tag(id);
            dw!(b"map_bar user_va=");
            dbg_hex(r.user_va);
            dw!(b"\n");
        }
        Err(e) => {
            tag(id);
            dw!(b"map_bar err=");
            dbg_hex(e);
            dw!(b"\n");
        }
    }
    match alloc_vram(CapId(2), 4096) {
        Ok(r) => {
            tag(id);
            dw!(b"alloc_vram phys=");
            dbg_hex(r.phys);
            dw!(b"\n");
        }
        Err(e) => {
            tag(id);
            dw!(b"alloc_vram err=");
            dbg_hex(e);
            dw!(b"\n");
        }
    }

    exit(id);
}

/// Any panic in ring 3 is fatal: report via the exit code and terminate.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(255);
}
