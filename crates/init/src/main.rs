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

use abi::{syserr, sysno, CapId, GpuInfo, MapBarResp, REGION_QUOTA};

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
// MAP_BAR) user memory through pointer args, so the compiler must treat
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

/// Four-argument `syscall`. Args land in `rdi, rsi, rdx, r10` per the fixed convention.
///
/// # Safety
/// Traps into the kernel; pointer args must be valid for the named syscall's access.
#[inline]
unsafe fn syscall4(num: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    // SAFETY: see module note above.
    core::arch::asm!(
        "syscall",
        in("rax") num,
        in("rdi") a0,
        in("rsi") a1,
        in("rdx") a2,
        in("r10") a3,
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

/// How many times the interrupt helper blocks before giving up and exiting. It has to
/// outlast every other process, because the kernel only parks once nothing else is
/// runnable — a helper that finishes first would prove nothing. Each iteration costs one
/// timer period, so this is a duration in ticks, not a workload -- x86 PIT runs at 100 Hz, so this is ~3 s, chosen to
/// outlast the rest of the demo with margin rather than to hit a particular count.
const IRQ_HELPER_WAITS: u64 = 300;

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

/// Send one `word` on the endpoint named by capability `cap` (needs `WRITE`; blocks until a
/// receiver takes it). Returns `syserr::OK`, or `NO_CAP` if we lack the authority.
fn send(cap: u64, word: u64) -> u64 {
    // SAFETY: a word-only SEND (len 0) passes scalars only; no user memory is read.
    unsafe { syscall4(sysno::SEND, cap, word, 0, 0) }
}

/// Send `word` plus a byte payload on `cap`. The kernel copies `bytes` out of our address
/// space before returning, so the buffer may be reused immediately.
fn send_bytes(cap: u64, word: u64, bytes: &[u8]) -> u64 {
    // SAFETY: the pointer/len describe a live borrow for the duration of the call.
    unsafe {
        syscall4(
            sysno::SEND,
            cap,
            word,
            bytes.as_ptr() as u64,
            bytes.len() as u64,
        )
    }
}

/// Receive one word on the endpoint named by capability `cap` (needs `READ`; blocks until a
/// sender delivers). Returns `(status, word)`: the status is `syserr::OK` or `NO_CAP`, and
/// the word is meaningful only when the status is `OK`.
///
/// This needs its own stub rather than [`syscall1`]: the kernel returns the payload in a
/// SECOND register (`rdx`), which the compiler must be told the `syscall` clobbers — and the
/// two-register split is what keeps a delivered word that happens to equal a [`syserr`]
/// sentinel distinguishable from a real error.
fn recv(cap: u64) -> (u64, u64) {
    let (status, word, _) = recv_bytes(cap, &mut []);
    (status, word)
}

/// Receive on `cap` into `buf`. Returns `(status, word, n)` where `n` is the number of
/// payload bytes copied — the kernel truncates to our buffer, so `n` is what we may read.
fn recv_bytes(cap: u64, buf: &mut [u8]) -> (u64, u64, usize) {
    let status: u64;
    let word: u64;
    let n: u64;
    // SAFETY: as the other stubs. `rdx` and `r10` are declared outputs because the kernel
    // returns the word and the byte count there; `buf` is a live writable borrow.
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") sysno::RECV,
            in("rdi") cap,                                  // a0 = endpoint capability
            in("rsi") buf.as_mut_ptr() as u64,              // a1 = payload buffer
            inlateout("rdx") buf.len() as u64 => word,      // a2 = capacity in, word out
            lateout("rax") status,
            lateout("r10") n,                               // a3 register = bytes copied
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    (status, word, n as usize)
}

/// Block until at least one interrupt has arrived on the line our `Irq` capability names,
/// then return the count. `syserr::NO_CAP` if we hold no such capability.
fn wait_irq(cap: u64) -> u64 {
    // SAFETY: WAIT_IRQ takes a cap id and returns a count; no user memory is touched.
    unsafe { syscall1(sysno::WAIT_IRQ, cap) }
}

/// Collect device interrupts delivered to us since the last call, via an `Irq` capability.
/// Returns the count, or `syserr::NO_CAP` if we hold no such capability.
fn poll_irq(cap: u64) -> u64 {
    // SAFETY: POLL_IRQ takes a cap id and returns a count; no user memory is touched.
    unsafe { syscall1(sysno::POLL_IRQ, cap) }
}

/// Create a shareable region of `pages` pages, paid for with `Untyped` capability `cap`.
/// Returns the new `Region` capability's id, or a `syserr`.
fn make_region(cap: u64, pages: u64) -> u64 {
    // SAFETY: MAKE_REGION takes two scalars and returns a cap id; no user memory is touched.
    unsafe { syscall2(sysno::MAKE_REGION, cap, pages) }
}

/// Map region capability `cap` into our own space. Returns the address the kernel chose.
fn map_region(cap: u64) -> u64 {
    // SAFETY: MAP_REGION takes a cap id and returns an address; no user memory is touched.
    unsafe { syscall1(sysno::MAP_REGION, cap) }
}

/// Drop our mapping of region capability `cap` (the capability survives).
fn unmap_region(cap: u64) -> u64 {
    // SAFETY: UNMAP_REGION takes a cap id and returns a status.
    unsafe { syscall1(sysno::UNMAP_REGION, cap) }
}

/// Destroy the region named by `cap`. Only its owner may do this.
fn free_region(cap: u64) -> u64 {
    // SAFETY: FREE_REGION takes a cap id and returns a status.
    unsafe { syscall1(sysno::FREE_REGION, cap) }
}

/// Probe whether `va` is still MAPPED, without risking a fault: `DEBUG_WRITE` makes the
/// kernel READ one byte from there through its permission-checked copy, so the status says
/// whether the page is still present and user-readable. (It emits that byte to the console,
/// which is why the probe is one byte of the device signature.)
fn mapped_probe(va: u64) -> u64 {
    // SAFETY: the kernel validates `va` itself; a bad address returns FAULT, not a fault.
    unsafe { syscall2(sysno::DEBUG_WRITE, va, 1) }
}

/// RECV with a RAW destination address, for the memory-safety regression tests below.
fn recv_raw(cap: u64, dst: u64, max: u64) -> u64 {
    let status: u64;
    // SAFETY: as `recv_bytes`; `dst` is deliberately arbitrary here — the point is that the
    // kernel must validate it rather than trusting us.
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") sysno::RECV,
            in("rdi") cap,
            in("rsi") dst,
            inlateout("rdx") max => _,
            lateout("rax") status,
            lateout("r10") _,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    status
}

/// Spawn a new process running this same image, presenting `cap` as spawn authority (an
/// Untyped capability carrying `WRITE`) and delegating nothing. Returns the new id, or
/// `u64::MAX` on failure.
fn spawn(cap: u64) -> u64 {
    spawn_delegating(cap, sysno::NO_DELEGATE, 0)
}

/// Spawn, additionally handing the child our capability `deleg` with at most `rights`.
/// The kernel intersects with what we actually hold, so this can attenuate but never
/// amplify. Returns the new id, or `u64::MAX` on failure.
fn spawn_delegating(cap: u64, deleg: u64, rights: u64) -> u64 {
    // SAFETY: SPAWN takes three scalars and returns a pid; no user memory is touched.
    unsafe { syscall3(sysno::SPAWN, cap, deleg, rights) }
}

/// Revoke every capability derived from our capability `cap` by delegation (transitively).
/// We keep `cap` itself. Returns `syserr::OK`, or `NO_CAP` if we do not hold it.
fn revoke(cap: u64) -> u64 {
    // SAFETY: REVOKE takes a cap id and returns a status; no user memory is touched.
    unsafe { syscall1(sysno::REVOKE, cap) }
}

/// The ELF entry point (ring 3, fresh stack, id in the first-argument register). The demo
/// is role-selected by `id`: proc 0 produces + `SEND`s five values, proc 1 `RECV`s + prints
/// them (cross-address-space IPC rendezvous), and any other proc runs a preemptible compute
/// loop + the per-process host contract. Together they show IPC blocking and preemption
/// coexisting. Never returns (each role exits via the EXIT syscall).
///
/// Our stack pages must arrive ZEROED, and this is the only thing that checks it.
///
/// The kernel scrubs a frame on its way out of the allocator, in one function with three
/// call sites. The region assertions cover one of them. THIS covers the one that matters
/// most: the frames behind every process's stack. Review demonstrated the gap by removing
/// that call site alone — spawned processes then read whole pages of an exited process's
/// stack, at ring 3, with no capability of any kind, while both arches still reported PASS.
///
/// We look BELOW our own stack pointer, at pages this program has not touched, and then
/// paint them so that whoever inherits these frames sees something attributable rather than
/// merely nonzero. Two pages of headroom are left above the scanned span for our own use.
fn check_fresh_stack(id: u64) {
    let here = 0u64;
    let sp_page = (&here as *const u64 as u64) & !(4096 - 1);
    let top = sp_page - 4096; // exclusive: leave the page we are running on, plus one
    let bottom = sp_page - 14 * 4096;
    let mut dirty = 0u64;
    let mut a = bottom;
    while a < top {
        // SAFETY: inside our own mapped stack, below the pointer we are running on.
        if unsafe { core::ptr::read_volatile(a as *const u8) } != 0 {
            dirty = dirty.wrapping_add(1);
        }
        a = a.wrapping_add(64);
    }
    tag(id);
    if dirty == 0 {
        dw!(b"stack: our stack pages arrived zeroed\n");
    } else {
        dw!(b"stack: our stack came back DIRTY -- another process's bytes (bug)\n");
    }
    // Paint, so a future leak of these frames is unmistakable rather than plausibly zero.
    let mark = 0xE0u8 | (id as u8 & 0x0f);
    let mut a = bottom;
    while a < top {
        // SAFETY: as above; this span is ours and unused.
        unsafe { core::ptr::write_volatile(a as *mut u8, mark) };
        a = a.wrapping_add(64);
    }
}

/// Declared `extern "C"` / `#[no_mangle]` so the linker resolves `_start` (the
/// `ENTRY` of `link.ld`); the `id` parameter reads the SysV first-arg register.
#[no_mangle]
pub extern "C" fn _start(id: u64) -> ! {
    // Before anything else, while the stack below us is still untouched.
    check_fresh_stack(id);
    match id {
        0 => producer(),
        1 => consumer(),
        2 => compute(id),
        // Anything beyond the boot processes was SPAWNed: it holds no authority of its own.
        _ => child(id),
    }
}

/// IPC producer: send five values on endpoint 0, blocking on each until the consumer takes
/// it. The interleaving with the consumer's `recv` lines shows the synchronous rendezvous.
fn producer() -> ! {
    tag(0);
    dw!(b"producer: sending 5 values on ep 0\n");
    // Least authority: this role holds SEND on the endpoint and nothing else. It cannot
    // receive on the very endpoint it sends to, and holds no device authority at all.
    tag(0);
    if recv(0).0 == syserr::NO_CAP {
        dw!(b"role: producer cannot RECV on its own endpoint (send-only)\n");
    } else {
        dw!(b"role: producer CAN recv (bug)\n");
    }
    tag(0);
    if map_bar(CapId(1), 0).is_err() {
        dw!(b"role: producer holds no device authority\n");
    } else {
        dw!(b"role: producer mapped a BAR (bug)\n");
    }
    let mut i = 0u64;
    while i < 4 {
        let v = 100u64.wrapping_add(i);
        send(0, v);
        tag(0);
        dw!(b"sent ");
        dbg_dec(v);
        dw!(b"\n");
        i = i.wrapping_add(1);
    }
    // Fifth message: the NO_CAP error sentinel sent as ORDINARY DATA (the word domain is
    // the whole u64, so this is a legal payload and the consumer must still see a
    // successful receive), carrying a BYTE PAYLOAD alongside it. The payload crosses
    // address spaces — the consumer has no mapping of ours — so the kernel copies it.
    send_bytes(0, syserr::NO_CAP, b"payload-across-address-spaces");
    tag(0);
    dw!(b"sent NO_CAP-valued word + byte payload\n");
    // A sixth message sent while the consumer is busy checking the fifth: we block holding
    // the payload, so the receiver takes the OTHER copy path (out of a parked sender)
    // rather than the deferred one. Both directions of the transfer get exercised.
    send_bytes(0, 0xB, b"second-payload");
    tag(0);
    dw!(b"sent second payload (sender-parked path)\n");
    exit(0);
}

/// IPC consumer: receive five values on endpoint 0, blocking until each arrives. The fifth
/// is bit-identical to the `NO_CAP` sentinel and must still report as a successful receive.
fn consumer() -> ! {
    tag(1);
    dw!(b"consumer: receiving 5 values on ep 0\n");
    // Least authority: this role holds RECEIVE and nothing else — it cannot inject a
    // message onto the endpoint it reads, nor allocate memory.
    tag(1);
    if send(0, 0xDEAD) == syserr::NO_CAP {
        dw!(b"role: consumer cannot SEND on its own endpoint (recv-only)\n");
    } else {
        dw!(b"role: consumer CAN send (bug)\n");
    }
    tag(1);
    if make_region(2, 1) == syserr::NO_CAP {
        dw!(b"role: consumer holds no memory authority\n");
    } else {
        dw!(b"role: consumer allocated VRAM (bug)\n");
    }
    let mut i = 0u64;
    while i < 4 {
        let (status, v) = recv(0);
        tag(1);
        if status == syserr::OK {
            dw!(b"recv ");
            dbg_dec(v);
            dw!(b"\n");
        } else {
            dw!(b"recv FAILED status=");
            dbg_hex(status);
            dw!(b"\n");
        }
        i = i.wrapping_add(1);
    }
    let mut buf = [0u8; 64];
    let (status, v, n) = recv_bytes(0, &mut buf);
    tag(1);
    if status == syserr::OK && v == syserr::NO_CAP {
        dw!(b"recv NO_CAP-valued word as DATA (status separate from payload)\n");
    } else {
        dw!(b"recv sentinel word MISREAD as error (bug)\n");
    }
    // The payload arrived through the kernel: we can read no memory of the sender's.
    tag(1);
    if n == 29 && &buf[..n] == b"payload-across-address-spaces" {
        dw!(b"recv byte payload intact across address spaces (");
        dbg_dec(n as u64);
        dw!(b" bytes)\n");
    } else {
        dw!(b"recv byte payload WRONG (bug)\n");
    }
    let (status2, w2, n2) = recv_bytes(0, &mut buf);
    tag(1);
    if status2 == syserr::OK && w2 == 0xB && n2 == 14 && &buf[..n2] == b"second-payload" {
        dw!(b"recv second payload intact (other copy path)\n");
    } else {
        dw!(b"recv second payload WRONG (bug)\n");
    }
    exit(1);
}

/// A preemptible compute process: a busy loop with NO syscalls between prints, so only the
/// timer can interleave it (its `tick K` lines interleaving proves preemption). Runs the
/// per-process host contract afterward, then exits. `spin` deliberately sets DF=1 (a
/// standing regression test for the kernel's interrupt-entry `cld`; see `spin`).
fn compute(id: u64) -> ! {
    // Proc 2 dynamically spawns one child process (which runs this same compute path); the
    // child's `[proc N]` ticks then appear in the schedule, proving runtime process creation.
    if id == 2 {
        // Hand our INTERRUPT line to a helper, attenuated to READ. This is the shape a
        // driver actually has -- a process whose whole job is to block on its device --
        // and it is the only thing here that makes the machine go IDLE: once everyone
        // else has exited, the helper is blocked in the kernel with nothing runnable
        // behind it, the exact state `Arch::idle` exists for and that no earlier version
        // of this demo ever reached. Spawned before the other children take the process
        // slots, and CHECKED: a refused spawn would silently cost us the only coverage
        // the idle path has, which is how it went unexercised for this long.
        let helper = spawn_delegating(2, 6, 0b001);
        tag(id);
        if helper == u64::MAX {
            dw!(b"irq: could NOT spawn the interrupt helper (bug)\n");
        } else {
            dw!(b"irq: spawned an interrupt helper -> pid ");
            dbg_dec(helper);
            dw!(b"\n");
        }

        // ---- shared memory: the owner side -------------------------------------------
        // A region is the first object here owned by a PROCESS rather than the kernel, so
        // it is the first capability that can outlive what it names.
        let rcap = make_region(2, 2);
        tag(id);
        if rcap == syserr::NO_CAP || rcap == syserr::NO_MEM {
            dw!(b"share: could not create a region (bug)\n");
        } else {
            dw!(b"share: created a 2-page region\n");
        }
        let rva = map_region(rcap);
        tag(id);
        if rva != syserr::NO_CAP && rva != syserr::NO_MEM {
            dw!(b"share: owner mapped it\n");
            // Write a signature across EVERY page, not just the first: a kernel that mapped
            // only page 0 of a multi-page region would otherwise look correct.
            let sig = b"RUSTPROOF-SHARED";
            let mut pg = 0u64;
            while pg < 2 {
                let mut i = 0usize;
                while i < 16 {
                    let at = (rva + pg * 4096) as *mut u8;
                    unsafe { core::ptr::write_volatile(at.wrapping_add(i), sig[i]) };
                    i += 1;
                }
                pg += 1;
            }
        } else {
            dw!(b"share: owner could not map its own region (bug)\n");
        }
        // A second region, lent WRITABLE, used as a mailbox. It is how the borrower reports
        // back: without it, breaking delegated mapping would silently DELETE the borrower's
        // half of this test instead of failing it, which is exactly what review found.
        let mbox = make_region(2, 1);
        let mva = map_region(mbox);
        if mva == syserr::NO_CAP || mva == syserr::NO_MEM {
            tag(id);
            dw!(b"share: could not map the mailbox (bug)\n");
        }
        let reader = spawn_delegating(2, rcap, 0b001); // READ only
        let writer = spawn_delegating(2, mbox, 0b011); // READ|WRITE
        tag(id);
        if reader == u64::MAX || writer == u64::MAX {
            dw!(b"share: could not spawn the borrowers (bug)\n");
        } else {
            dw!(b"share: lent one region READ-only and one READ-WRITE\n");
        }
        spin(60_000_000);
        // Revoke the READ-only loan. This region is deliberately NEVER freed by us -- it is
        // reclaimed by teardown at exit -- so the borrower's window can only disappear
        // because of THIS revoke. Freeing it here would let a broken revoke pass, which is
        // how the first version of this test fooled itself.
        revoke(rcap);
        spin(90_000_000);
        // The borrower wrote through its WRITABLE loan, into memory we own and can read.
        tag(id);
        let mut tok = [0u8; 12];
        let mut i = 0usize;
        while i < 12 {
            tok[i] = unsafe { core::ptr::read_volatile((mva as *const u8).wrapping_add(i)) };
            i += 1;
        }
        if &tok == b"BORROWER-RAN" {
            dw!(b"share: a WRITABLE loan let the borrower write memory we own\n");
        } else {
            dw!(b"share: the borrower never reported back (bug)\n");
        }
        // UNMAP_REGION, then map again: the window goes and comes back, contents intact.
        tag(id);
        let un = unmap_region(mbox);
        if un == syserr::OK && mapped_probe(mva) != syserr::OK {
            dw!(b"share: UNMAP_REGION dropped the window\n");
        } else {
            dw!(b"share: UNMAP_REGION left the window mapped (bug)\n");
        }
        tag(id);
        let again = map_region(mbox);
        if again == mva && unsafe { core::ptr::read_volatile(mva as *const u8) } == b'B' {
            dw!(b"share: re-mapped it, same address, contents intact\n");
        } else {
            dw!(b"share: could not re-map a region we still hold (bug)\n");
        }
        // Scrub-on-destroy, tested against RECYCLED memory. Fresh frames are zero anyway on
        // an early boot, so zeroing only a NEW region proves nothing: poison one, destroy
        // it, and require the next region to come back clean.
        let poison = make_region(2, 3); // multi-page: a per-page scrub bug must show
        let pva = map_region(poison);
        if pva != syserr::NO_CAP && pva != syserr::NO_MEM {
            let mut i = 0usize;
            while i < 3 * 4096 {
                unsafe { core::ptr::write_volatile((pva as *mut u8).wrapping_add(i), 0xAA) };
                i += 1;
            }
        }
        tag(id);
        if free_region(poison) == syserr::OK {
            dw!(b"share: owner destroyed a region\n");
        } else {
            dw!(b"share: owner could not destroy its own region (bug)\n");
        }
        // The capability went away WITH the region, so it no longer names anything. (The
        // kernel also refuses an id that no longer resolves, but that branch is unreachable
        // by construction -- destroying a region sweeps every capability naming it.)
        tag(id);
        if map_region(poison) == syserr::NO_CAP {
            dw!(b"share: destroying a region took its capability with it\n");
        } else {
            dw!(b"share: stale region capability still maps (bug)\n");
        }
        let fresh = make_region(2, 3);
        let fva = map_region(fresh);
        tag(id);
        if fva != syserr::NO_CAP && fva != syserr::NO_MEM {
            let mut dirty = false;
            let mut i = 0usize;
            while i < 3 * 4096 {
                if unsafe { core::ptr::read_volatile((fva as *const u8).wrapping_add(i)) } != 0 {
                    dirty = true;
                }
                i += 1;
            }
            if dirty {
                dw!(b"share: recycled region memory came back DIRTY (bug)\n");
            } else {
                dw!(b"share: recycled region memory comes back zeroed\n");
            }
        } else {
            dw!(b"share: could not map a fresh region (bug)\n");
        }
        // `rcap`, `mbox` and `fresh` are deliberately left alive: teardown must reclaim
        // them, and the frame-conservation check fails the boot if it does not.
    }
    // Hostile-flag regression, the sibling of the DF test in `spin`: ring 3 can set
    // RFLAGS.NT with `popfq`, and a leaked NT makes the kernel's OWN `iretq` raise #GP —
    // reported with kernel CS, so it would take the fatal branch and halt the guest. The
    // syscall entry mask must strip it. If it does not, this boot ends here.
    tag(id);
    // SAFETY: only toggles RFLAGS.NT, which is architecturally writable from ring 3 and
    // affects nothing this program does; the kernel is responsible for not leaking it in.
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {t}",
            "or {t}, 0x4000",
            "push {t}",
            "popfq",
            t = out(reg) _,
            options(nomem),
        );
    }
    let _ = get_info(); // any syscall: the return path is what NT would break
    dw!(b"flags: survived a syscall with RFLAGS.NT set\n");

    // Device interrupts are delivered to CAPABILITY HOLDERS. The worker holds CapId(6)
    // for the timer line; poll until some arrive. A driver process would do exactly this
    // for its own device's line.
    let mut ticks = 0u64;
    let mut rounds = 0u64;
    while ticks < 3 && rounds < 10 {
        // BLOCK until the line fires. This is the path a real driver takes, and it puts
        // the kernel in its idle park whenever no other process is runnable — so this also
        // exercises taking an interrupt in the KERNEL rather than in user code.
        // Never fold the error sentinel into the count: losing the capability would
        // otherwise wrap the total past the threshold and PASS this test for the wrong
        // reason. Only a real, non-error count is accumulated.
        let n = wait_irq(6);
        if n == syserr::NO_CAP || n == 0 {
            break;
        }
        ticks = ticks.wrapping_add(n);
        rounds = rounds.wrapping_add(1);
    }
    tag(id);
    if ticks >= 3 {
        dw!(b"irq: blocked and woke on real device interrupts (");
        dbg_dec(ticks);
        dw!(b")\n");
    } else {
        dw!(b"irq: no interrupts delivered (bug)\n");
    }
    // And an unheld line is invisible: CapId(9) names nothing we hold.
    tag(id);
    if poll_irq(9) == syserr::NO_CAP {
        dw!(b"irq: polling an unheld line -> NO_CAP\n");
    } else {
        dw!(b"irq: polled a line we do not hold (bug)\n");
    }

    tag(id);
    dw!(b"start (compute loop, no yields -- preemption only)\n");
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
            // The mapping is REAL: read the signature the kernel wrote into that physical
            // window, then write through it and read our write back. Neither would work if
            // MAP_BAR had only reported an address without installing page tables.
            // SAFETY: the kernel just mapped `size` bytes here user-readable/writable.
            let dev = r.user_va as *mut u8;
            let mut sig = [0u8; 16];
            let mut i = 0usize;
            while i < 16 {
                sig[i] = unsafe { core::ptr::read_volatile(dev.wrapping_add(i)) };
                i += 1;
            }
            tag(id);
            if &sig == b"RUSTPROOF-DEVICE" {
                dw!(b"mmio: read device signature through the mapping\n");
            } else {
                dw!(b"mmio: device signature WRONG (bug)\n");
            }
            unsafe { core::ptr::write_volatile(dev.wrapping_add(32), 0x5Au8) };
            tag(id);
            if unsafe { core::ptr::read_volatile(dev.wrapping_add(32)) } == 0x5A {
                dw!(b"mmio: wrote and read back through the mapping\n");
            } else {
                dw!(b"mmio: device write did not stick (bug)\n");
            }
            // Probe the page's writability WITHOUT faulting: RECV validates its buffer
            // against the page tables before checking the capability, so an unheld cap
            // yields NO_CAP when the buffer is writable and FAULT when it is not.
            tag(id);
            if recv_raw(9, r.user_va, 16) == syserr::NO_CAP {
                dw!(b"mmio: full-rights cap gives a WRITABLE window\n");
            } else {
                dw!(b"mmio: full-rights window not writable (bug)\n");
            }
        }
        Err(e) => {
            tag(id);
            dw!(b"map_bar err=");
            dbg_hex(e);
            dw!(b"\n");
        }
    }
    // IPC authority: endpoints are capabilities, not raw integers. CapId(3) is an
    // Endpoint cap with READ only, and CapId(9) is not held at all — sending on either
    // must be refused, proving the kernel gates IPC on the cap AND its rights.
    // A worker is not in the IPC pair: its CapId(0) exists but confers nothing, so even
    // the shared endpoint is closed to it. Possession is not authority.
    tag(id);
    if send(0, 0xBEEF) == syserr::NO_CAP {
        dw!(b"role: worker holds no authority on the shared endpoint\n");
    } else {
        dw!(b"role: worker CAN send on the shared endpoint (bug)\n");
    }
    tag(id);
    if send(3, 0xBEEF) == syserr::NO_CAP {
        dw!(b"ipc: send on read-only ep cap -> NO_CAP (rights enforced)\n");
    } else {
        dw!(b"ipc: send on read-only ep cap ALLOWED (bug)\n");
    }
    tag(id);
    if send(9, 0xBEEF) == syserr::NO_CAP {
        dw!(b"ipc: send on unheld cap -> NO_CAP (authority enforced)\n");
    } else {
        dw!(b"ipc: send on unheld cap ALLOWED (bug)\n");
    }
    // RECV refusal must also be unambiguous — and must not block us.
    tag(id);
    let (status, _) = recv(9);
    if status == syserr::NO_CAP {
        dw!(b"ipc: recv on unheld cap -> NO_CAP (no block)\n");
    } else {
        dw!(b"ipc: recv on unheld cap ALLOWED (bug)\n");
    }

    // Memory-safety regression tests. The kernel copies an IPC payload into a buffer WE
    // name, so it must validate that buffer against the page tables — not merely against
    // the address range. A destination that is mapped read-only (our own image text) must
    // be refused rather than written through, and one that is in range but unmapped must
    // be refused rather than faulting inside the kernel, which would kill the guest.
    tag(id);
    if recv_raw(0, 0x80_0000_0000, 16) == syserr::FAULT {
        dw!(b"memsafe: RECV into read-only mapping refused (W^X honoured)\n");
    } else {
        dw!(b"memsafe: RECV into read-only mapping ALLOWED (bug)\n");
    }
    tag(id);
    if recv_raw(0, 0x80_8000_0000, 16) == syserr::FAULT {
        dw!(b"memsafe: RECV into unmapped address refused (no kernel fault)\n");
    } else {
        dw!(b"memsafe: RECV into unmapped address ALLOWED (bug)\n");
    }

    // Rights are checked on the REST of the host contract too, not just IPC: CapId(4) is an
    // Untyped cap without WRITE and CapId(5) an Mmio cap without READ — right type, wrong
    // rights, so every one of these must be refused.
    tag(id);
    if make_region(4, 1) == syserr::NO_CAP {
        dw!(b"caps: region via WRITE-less Untyped -> NO_CAP\n");
    } else {
        dw!(b"caps: region via WRITE-less Untyped ALLOWED (bug)\n");
    }
    tag(id);
    if spawn(4) == u64::MAX {
        dw!(b"caps: spawn via WRITE-less Untyped -> refused\n");
    } else {
        dw!(b"caps: spawn via WRITE-less Untyped ALLOWED (bug)\n");
    }
    tag(id);
    if map_bar(CapId(5), 0).is_err() {
        dw!(b"caps: map_bar via READ-less Mmio -> NO_CAP\n");
    } else {
        dw!(b"caps: map_bar via READ-less Mmio ALLOWED (bug)\n");
    }

    // Per-owner REGION quota, and the gates `hostcontract` used to check on the host before
    // DMA memory became a region: a wrong-typed capability, a capability without WRITE, and
    // the quota itself. Those live in `make_region` now, which is kernel code and not
    // host-testable, so the coverage moved here rather than disappearing.
    tag(id);
    // CapId(1) is the DEVICE capability: wrong type for MAKE_REGION but carrying FULL
    // rights, so only the TYPE half of the gate can refuse it. Probing with CapId(0)
    // instead — an Endpoint with NO rights — proved nothing: both halves reject that, so
    // deleting the type check left the assertion green while a worker could have minted
    // DMA memory through its device capability.
    if make_region(1, 1) == syserr::NO_CAP {
        dw!(b"caps: region via an MMIO capability with FULL rights -> NO_CAP\n");
    } else {
        dw!(b"caps: region via an MMIO capability ALLOWED (bug)\n");
    }
    let mut n = 0u64;
    let mut last = syserr::NO_CAP;
    loop {
        let c = make_region(2, 1);
        if c == syserr::NO_CAP || c == syserr::NO_MEM {
            break;
        }
        last = c;
        n = n.wrapping_add(1);
    }
    // The count is the assertion, not decoration. `make_region` answers NO_MEM for four
    // different reasons — per-owner quota, global table full, arena empty, capability space
    // full — so a loop that merely stops proves nothing about which limit bound it. We own
    // exactly three regions here (the lent one, the mailbox, and the fresh one from the
    // scrub check), so a per-owner quota must grant exactly the remainder. Deleting the
    // quota makes this the table-full count instead, and the run fails.
    let expected = (REGION_QUOTA - 3) as u64;
    tag(id);
    if n == expected {
        dw!(b"region: the per-owner quota granted exactly ");
        dbg_dec(n);
        dw!(b" more\n");
    } else {
        dw!(b"region: quota granted ");
        dbg_dec(n);
        dw!(b" but the per-owner limit allows ");
        dbg_dec(expected);
        dw!(b" (bug)\n");
    }
    if last != syserr::NO_CAP {
        // Freeing one returns quota, so the next request succeeds.
        free_region(last);
        let c = make_region(2, 1);
        tag(id);
        if c != syserr::NO_CAP && c != syserr::NO_MEM {
            dw!(b"region: freed one, the next request is granted\n");
        } else {
            dw!(b"region: freeing one did not return quota (bug)\n");
        }
    }

    // Regression test for authority amplification via slot recycling: by now the
    // producer and consumer have exited and freed their slots, so this spawn lands in a
    // RECYCLED one. The child must STILL be a worker. If the kernel derived authority from
    // the slot index, this child would inherit the producer's send right on the shared
    // endpoint (and, running producer(), would block forever and deadlock the boot).
    if id == 2 {
        // Delegate our DEVICE capability (CapId(1), full rights) attenuated to READ only.
        // The child must get a read-only window: attenuating the capability has to
        // attenuate the access it grants, or delegation would widen authority.
        let late = spawn_delegating(2, 1, 0b001);
        tag(id);
        // CHECKED, like the helper spawn: this is the only process that ever exercises the
        // delegated-MMIO branch of `child()` -- the read-only window, the revocation
        // teardown, AND the deliberate wild write that proves a ring-3 fault kills only the
        // faulting process. Printing the u64::MAX sentinel as a pid, as this used to, meant
        // a boot that silently tested four fewer things still said BOOT OK. On riscv it did
        // exactly that.
        if late == u64::MAX {
            dw!(b"late spawn into a recycled slot REFUSED -- no free process slot (bug)\n");
        } else {
            dw!(b"late spawn into a recycled slot -> pid ");
            dbg_dec(late);
            dw!(b"\n");
        }

        // The chain starts HERE, not at the top of this function. Each child re-runs this
        // path and delegates onward, so it self-replicates until the process table is full
        // -- at any table size. Started earlier it took the recycled producer/consumer
        // slots before the spawn above could, which is what starved that spawn on riscv;
        // raising MAX_PROCS does not help, because the chain simply grows to fill it.
        // Delegate our Untyped cap (CapId(2), full rights). A child's role grants nothing,
        // so this is the ONLY authority it will have — the positive half of the test.
        let child = spawn_delegating(2, 2, 0b111);
        tag(id);
        dw!(b"spawned child pid=");
        dbg_dec(child);
        dw!(b"\n");
    }

    // Revoke everything we delegated from CapId(2). Our own capability survives; the
    // child's copy of it must vanish, which the child observes and reports.
    if id == 2 {
        tag(id);
        // Let the child we just spawned actually map its delegated window first —
        // otherwise we revoke before it ever holds the authority and the teardown is
        // untested rather than tested.
        spin(40_000_000);
        let _ = revoke(1); // also revoke what we delegated of our DEVICE capability
        if revoke(2) == syserr::OK {
            dw!(b"revoke: revoked delegations of CapId(1) and CapId(2)\n");
        } else {
            dw!(b"revoke: refused (bug)\n");
        }
        // Our own capability is untouched: revoking grants does not disarm us. NOTE the
        // error code matters here — we are at our region quota from the loop above, so a
        // refusal is expected; only NO_CAP would mean revocation destroyed our own cap.
        tag(id);
        match make_region(2, 1) {
            c if c != syserr::NO_CAP && c != syserr::NO_MEM => {
                free_region(c);
                dw!(b"revoke: our own CapId(2) still works\n");
            }
            syserr::NO_MEM => {
                dw!(b"revoke: our own CapId(2) survives (refused on quota, not authority)\n");
            }
            _ => dw!(b"revoke: revoking cost us our own cap (bug)\n"),
        }
    }

    // One last blocking wait, as our final act. By now the other processes have finished,
    // so there is nothing else to run and the kernel must PARK until the line fires rather
    // than declaring a deadlock — the idle path a driver-only system sits in most of the
    // time. If it declared deadlock instead, the boot would fail here.
    let _ = poll_irq(6); // drain first, so the wait below genuinely has to block
    tag(id);
    // `> 0` alone would pass on the error sentinel too (NO_CAP is u64::MAX - 1), printing
    // success for a wait that never happened. An oracle that can pass for the wrong reason
    // is worse than no oracle.
    let n = wait_irq(6);
    if n > 0 && n != syserr::NO_CAP {
        dw!(b"irq: blocked with no credits and hardware woke us\n");
    } else {
        dw!(b"irq: final wait returned nothing (bug)\n");
    }

    // TWO lines, held as two separate capabilities, is what makes the per-line claims
    // testable at all: with only the timer, "a capability for one line can never read or
    // clear another's" was true purely because there was no other line. The timer has been
    // firing throughout; the console has not fired at all. So CapId(7) must read ZERO while
    // CapId(6) reads a real count, and neither may drain the other.
    let ticks = poll_irq(6);
    let bytes = poll_irq(7);
    tag(id);
    if ticks > 0 && ticks != syserr::NO_CAP && bytes == 0 {
        dw!(b"irq: two lines stay separate (timer counted, console still quiet)\n");
    } else if bytes == syserr::NO_CAP {
        dw!(b"irq: no authority for the console line (bug)\n");
    } else {
        dw!(b"irq: one line's count leaked into the other (bug)\n");
    }

    // Now block on the CONSOLE line. Nothing the kernel does can end this wait: the timer
    // fires throughout and cannot credit it, so the kernel parks, wakes on each tick with
    // nobody to run, and parks again -- until a byte actually arrives from outside. Every
    // park until now was ended by the clock; this one is ended by a device, which is the
    // thing a driver waiting on its hardware is really doing.
    tag(id);
    dw!(b"irq: blocking on the console line -- only real input can wake us\n");
    let c = wait_irq(7);
    tag(id);
    if c > 0 && c != syserr::NO_CAP {
        dw!(b"irq: woke on a REAL device interrupt, not the clock\n");
    } else {
        dw!(b"irq: console wait returned nothing (bug)\n");
    }

    exit(id);
}

/// A SPAWNed process. Its role grants NOTHING, so every capability it holds was delegated
/// by its parent — which makes this the honest test of delegation: authority that works
/// here cannot have come from a role table. Proc 3 was delegated a full-rights `Untyped`
/// (so it can allocate); proc 4 was delegated a READ-only one with full rights REQUESTED
/// (so it must still be refused). Then it runs the preemptible compute loop and exits.
fn child(id: u64) -> ! {
    // No authority of its own: not on the shared endpoint, not over any device.
    tag(id);
    if send(0, 0xBEEF) == syserr::NO_CAP {
        dw!(b"child: no endpoint authority of its own\n");
    } else {
        dw!(b"child: CAN send on the shared endpoint (bug)\n");
    }
    tag(id);
    if poll_irq(6) == syserr::NO_CAP {
        dw!(b"child: no interrupt authority of its own\n");
    } else {
        dw!(b"child: could poll an interrupt line (bug)\n");
    }
    tag(id);
    if map_bar(CapId(1), 0).is_err() {
        dw!(b"child: no device authority of its own\n");
    } else {
        dw!(b"child: mapped a BAR (bug)\n");
    }

    // Were we handed a shared REGION? Then we are a borrower. Which kind we are is decided
    // by what our capability actually permits, not by anything we were told.
    let rva = map_region(0);
    if rva != syserr::NO_CAP && rva != syserr::NO_MEM {
        // Which loan is this? Decided by the region's SIZE (the mailbox is one page, the
        // lent region is two), NOT by probing what we are allowed to do with it. Choosing on
        // writability would mean a kernel that ignored our capability's rights simply ran a
        // different test and never reported the amplification -- which is precisely the
        // failure review found in the first version of this file.
        let two_pages = mapped_probe(rva + 4096) == syserr::OK;
        if !two_pages {
            // A WRITABLE loan: report back through it. The owner requires this token, so a
            // regression in delegated mapping FAILS the run instead of quietly removing it.
            let tok = b"BORROWER-RAN";
            let mut i = 0usize;
            while i < 12 {
                unsafe { core::ptr::write_volatile((rva as *mut u8).wrapping_add(i), tok[i]) };
                i += 1;
            }
            tag(id);
            if unsafe { core::ptr::read_volatile(rva as *const u8) } == b'B' {
                dw!(b"share: wrote back through a WRITABLE loan\n");
            } else {
                dw!(b"share: a WRITABLE loan would not take our write (bug)\n");
            }
            // Owner-identity, isolated. FREE_REGION now also demands WRITE, so the READ-only
            // borrower below is refused by the RIGHTS gate and proves nothing about who owns
            // the region. This borrower HAS write, so only the owner check can stop it —
            // which is the property under test.
            tag(id);
            if free_region(0) == syserr::NO_CAP {
                dw!(b"share: a WRITABLE borrower still cannot destroy what it borrowed\n");
            } else {
                dw!(b"share: a WRITABLE borrower DESTROYED a region it borrowed (bug)\n");
            }
            exit(id);
        }
        // A READ-only loan: now ASSERT the window is not writable. Probed through the
        // kernel rather than by storing, so a correct kernel does not kill us for asking.
        tag(id);
        if recv_raw(9, rva, 16) == syserr::FAULT {
            dw!(b"share: a READ-only loan gives a READ-ONLY window\n");
        } else {
            dw!(b"share: a READ-only loan gave a WRITABLE window (bug)\n");
        }
        // Read EVERY page: a kernel that mapped only the first would fail here.
        let mut good = true;
        let mut pg = 0u64;
        while pg < 2 {
            let mut i = 0usize;
            while i < 16 {
                let at = (rva + pg * 4096) as *const u8;
                if unsafe { core::ptr::read_volatile(at.wrapping_add(i)) } != b"RUSTPROOF-SHARED"[i]
                {
                    good = false;
                }
                i += 1;
            }
            pg += 1;
        }
        tag(id);
        if good {
            dw!(b"share: read every page of the owner's bytes (no copy)\n");
        } else {
            dw!(b"share: delegated region has the WRONG contents (bug)\n");
        }
        tag(id);
        if free_region(0) == syserr::NO_CAP {
            dw!(b"share: a READ-only borrower cannot destroy (refused on rights)\n");
        } else {
            dw!(b"share: a borrower DESTROYED the owner's region (bug)\n");
        }
        // The owner revokes while we run, and never frees this region, so the window can
        // only go away because the REVOKE tore it down.
        let mut gone = false;
        let mut k = 0u64;
        while k < 90 {
            if mapped_probe(rva) != syserr::OK {
                gone = true;
                break;
            }
            spin(2_000_000);
            k = k.wrapping_add(1);
        }
        tag(id);
        if gone {
            dw!(b"share: REVOKE alone tore the window down\n");
        } else {
            dw!(b"share: kept the window after revocation (bug)\n");
        }
        exit(id);
    }

    // Were we handed an INTERRUPT line? Then we are the driver's helper. No role table
    // grants a child interrupt authority, so it working at all is the delegation test;
    // and blocking on it repeatedly is what drives the kernel into its idle park.
    if poll_irq(0) != syserr::NO_CAP {
        tag(id);
        dw!(b"deleg: delegated Irq WORKS (authority no role granted us)\n");
        let mut woke = 0u64;
        let mut i = 0u64;
        while i < IRQ_HELPER_WAITS {
            let n = wait_irq(0);
            // NO_CAP means the kernel woke us because our authority went away; 0 means it
            // refused to park at all. Either ends the loop -- spinning on an error would
            // burn the CPU and, worse, report success below for a wait that never blocked.
            if n == 0 || n == syserr::NO_CAP {
                break;
            }
            woke = woke.wrapping_add(1);
            i = i.wrapping_add(1);
        }
        tag(id);
        dw!(b"irq: helper woke ");
        dbg_dec(woke);
        dw!(b" time(s) on a delegated line\n");
        exit(id);
    }

    // What did our parent delegate? A device capability takes the mapping path.
    if let Ok(r) = map_bar(CapId(0), 0) {
        let dev = r.user_va as *mut u8;
        let mut sig = [0u8; 16];
        let mut i = 0usize;
        while i < 16 {
            sig[i] = unsafe { core::ptr::read_volatile(dev.wrapping_add(i)) };
            i += 1;
        }
        tag(id);
        if &sig == b"RUSTPROOF-DEVICE" {
            dw!(b"mmio: read device signature through a delegated cap\n");
        } else {
            dw!(b"mmio: delegated device signature WRONG (bug)\n");
        }
        // Our capability was attenuated to READ, so the window must NOT be writable.
        // Probed without faulting, as in the worker.
        tag(id);
        if recv_raw(9, r.user_va, 16) == syserr::FAULT {
            dw!(b"mmio: READ-only cap gives a READ-ONLY window (no amplification)\n");
        } else {
            dw!(b"mmio: READ-only cap gave a WRITABLE window (bug)\n");
        }
        // Revoking the capability must also tear down the mapping it authorised —
        // otherwise the authority survives its capability. Watch for it to vanish.
        let mut gone = false;
        let mut k = 0u64;
        while k < 60 {
            if mapped_probe(r.user_va) != syserr::OK {
                gone = true;
                break;
            }
            spin(2_000_000);
            k = k.wrapping_add(1);
        }
        dw!(b"\n");
        tag(id);
        if gone {
            dw!(b"mmio: mapping torn down when the cap was REVOKED\n");
        } else {
            dw!(b"mmio: mapping survived revocation (bug)\n");
        }
        // Deliberately fault. A wild pointer is OUR bug, not the machine's: the kernel must
        // kill this process and keep running, and the boot must still reach BOOT OK. If a
        // user fault halted the guest, this line would end the run right here.
        tag(id);
        dw!(b"fault: dereferencing a wild pointer on purpose\n");
        unsafe { core::ptr::write_volatile(0x1u64 as *mut u8, 0) };
        tag(id);
        dw!(b"fault: SURVIVED a wild write (bug)\n");
        exit(id);
    }

    // Use the delegated capability, then pass it on once. Passing it on makes the
    // revocation test TRANSITIVE: the parent's REVOKE must reach not just us but the
    // grandchild we handed it to. Self-limiting — the spawn fails once slots run out.
    // NO_CAP means "no authority", which is what this test is about. NO_MEM means a
    // resource ran out and the test could not run at all — reporting that as lost authority
    // would print a confident, false line about delegation. Say so instead.
    let probe = make_region(0, 1);
    if probe == syserr::NO_MEM {
        tag(id);
        dw!(b"deleg: could not test delegation -- out of regions (bug)\n");
    }
    let have = {
        if probe != syserr::NO_CAP && probe != syserr::NO_MEM {
            free_region(probe);
            true
        } else {
            false
        }
    };
    tag(id);
    if have {
        dw!(b"deleg: delegated Untyped WORKS (authority no role granted it)\n");
    } else {
        dw!(b"deleg: parent asked ALL on its READ-only cap -> still refused (no amplification)\n");
    }
    if have {
        let g = spawn_delegating(0, 0, 0b111);
        tag(id);
        if g == u64::MAX {
            dw!(b"deleg: no free slot to pass it on\n");
        } else {
            dw!(b"deleg: passed our cap on to pid ");
            dbg_dec(g);
            dw!(b"\n");
        }
    }

    // Our parent revokes while we run. A capability that keeps working after revocation
    // would be the bug — and for a grandchild, so would one that is never reached.
    if have {
        let mut seen = false;
        let mut i = 0u64;
        while i < 60 {
            let c = make_region(0, 1);
            if c != syserr::NO_CAP && c != syserr::NO_MEM {
                free_region(c);
            } else {
                seen = true;
                break;
            }
            spin(2_000_000);
            i = i.wrapping_add(1);
        }
        tag(id);
        if seen {
            dw!(b"revoke: delegated cap REVOKED by parent (no longer usable)\n");
        } else {
            dw!(b"revoke: delegated cap still usable after revoke (bug)\n");
        }
    }

    // Still preemptible, with DF deliberately set inside spin() (see `spin`).
    let mut tick = 0u64;
    while tick < 3 {
        tag(id);
        dw!(b"tick ");
        dbg_dec(tick);
        dw!(b"\n");
        spin(5_000_000);
        tick = tick.wrapping_add(1);
    }
    exit(id);
}

/// Any panic in ring 3 is fatal: report via the exit code and terminate.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(255);
}
