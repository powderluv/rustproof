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

/// Send one `word` on the endpoint named by capability `cap` (needs `WRITE`; blocks until a
/// receiver takes it). Returns `syserr::OK`, or `NO_CAP` if we lack the authority.
fn send(cap: u64, word: u64) -> u64 {
    // SAFETY: SEND passes two scalars and returns a status; no user memory is touched.
    unsafe { syscall2(sysno::SEND, cap, word) }
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
    let status: u64;
    let word: u64;
    // SAFETY: as the other stubs — `syscall` traps to ring 0 and touches no user memory
    // here. `rdx` is declared as an output because the kernel writes the payload there.
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") sysno::RECV,
            in("rdi") cap,
            lateout("rax") status,
            lateout("rdx") word,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    (status, word)
}

/// Spawn a new process running this same image, presenting `cap` as spawn authority (an
/// Untyped capability carrying `WRITE`). Returns the new id, or `u64::MAX` on failure.
fn spawn(cap: u64) -> u64 {
    // SAFETY: SPAWN takes a cap id and returns a pid; no user memory is touched.
    unsafe { syscall1(sysno::SPAWN, cap) }
}

/// Allocate one VRAM frame via the Untyped cap; returns its physical address, or 0 on
/// failure (per-process quota reached or out of memory).
fn alloc_vram_phys() -> u64 {
    match alloc_vram(CapId(2), 4096) {
        Ok(r) => r.phys,
        Err(_) => 0,
    }
}

/// Free a VRAM frame (by physical address). Returns `syserr::OK` (0) or a nonzero error.
fn free_vram(phys: u64) -> u64 {
    // SAFETY: FREE_VRAM takes a phys addr and returns a status; no user memory is touched.
    unsafe { syscall1(sysno::FREE_VRAM, phys) }
}

/// The ELF entry point (ring 3, fresh stack, id in the first-argument register). The demo
/// is role-selected by `id`: proc 0 produces + `SEND`s five values, proc 1 `RECV`s + prints
/// them (cross-address-space IPC rendezvous), and any other proc runs a preemptible compute
/// loop + the per-process host contract. Together they show IPC blocking and preemption
/// coexisting. Never returns (each role exits via the EXIT syscall).
///
/// Declared `extern "C"` / `#[no_mangle]` so the linker resolves `_start` (the
/// `ENTRY` of `link.ld`); the `id` parameter reads the SysV first-arg register.
#[no_mangle]
pub extern "C" fn _start(id: u64) -> ! {
    match id {
        0 => producer(),
        1 => consumer(),
        _ => compute(id),
    }
}

/// IPC producer: send five values on endpoint 0, blocking on each until the consumer takes
/// it. The interleaving with the consumer's `recv` lines shows the synchronous rendezvous.
fn producer() -> ! {
    tag(0);
    dw!(b"producer: sending 5 values on ep 0\n");
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
    // Fifth value: the NO_CAP error sentinel sent as ORDINARY DATA. The word domain is the
    // whole u64, so this is a legal payload; the consumer must still see a successful
    // receive. (Regression test: status and payload ride in separate registers.)
    send(0, syserr::NO_CAP);
    tag(0);
    dw!(b"sent NO_CAP-valued word as data\n");
    exit(0);
}

/// IPC consumer: receive five values on endpoint 0, blocking until each arrives. The fifth
/// is bit-identical to the `NO_CAP` sentinel and must still report as a successful receive.
fn consumer() -> ! {
    tag(1);
    dw!(b"consumer: receiving 5 values on ep 0\n");
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
    let (status, v) = recv(0);
    tag(1);
    if status == syserr::OK && v == syserr::NO_CAP {
        dw!(b"recv NO_CAP-valued word as DATA (status separate from payload)\n");
    } else {
        dw!(b"recv sentinel word MISREAD as error (bug)\n");
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
        let child = spawn(2);
        tag(id);
        dw!(b"spawned child pid=");
        dbg_dec(child);
        dw!(b"\n");
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

    // Rights are checked on the REST of the host contract too, not just IPC: CapId(4) is an
    // Untyped cap without WRITE and CapId(5) an Mmio cap without READ — right type, wrong
    // rights, so every one of these must be refused.
    tag(id);
    if alloc_vram(CapId(4), 4096).is_err() {
        dw!(b"caps: alloc_vram via WRITE-less Untyped -> NO_CAP\n");
    } else {
        dw!(b"caps: alloc_vram via WRITE-less Untyped ALLOWED (bug)\n");
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

    // VRAM quota + FREE_VRAM: allocate until the per-process quota is hit (the kernel
    // refuses further allocations), then free one and re-allocate to show FREE_VRAM returns
    // quota. Remaining frames are reclaimed by EXIT.
    let mut n = 0u64;
    let mut last = 0u64;
    loop {
        let p = alloc_vram_phys();
        if p == 0 {
            break;
        }
        last = p;
        n = n.wrapping_add(1);
    }
    tag(id);
    dw!(b"vram: hit quota at ");
    dbg_dec(n);
    dw!(b" frames\n");
    if last != 0 {
        free_vram(last);
        let p = alloc_vram_phys();
        tag(id);
        if p != 0 {
            dw!(b"vram: freed 1, realloc OK\n");
        } else {
            dw!(b"vram: freed 1, realloc FAILED\n");
        }
    }

    exit(id);
}

/// Any panic in ring 3 is fatal: report via the exit code and terminate.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(255);
}
