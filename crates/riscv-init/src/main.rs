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

/// How many times the interrupt helper blocks before giving up and exiting. It has to
/// outlast every other process, because the kernel only parks once nothing else is
/// runnable — a helper that finishes first would prove nothing. Each iteration costs one
/// timer period, so this is a duration in ticks, not a workload -- the Sstc timer runs at ~400 Hz, so this is ~3 s, chosen to
/// outlast the rest of the demo with margin rather than to hit a particular count.
const IRQ_HELPER_WAITS: u64 = 300;

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

/// Send one `word` on the endpoint named by capability `cap` (needs `WRITE`; blocks until a
/// receiver takes it). Returns `syserr::OK`, or `NO_CAP` if we lack the authority.
fn send(cap: u64, word: u64) -> u64 {
    // SAFETY: a word-only SEND (len 0) passes scalars only; no user memory is read.
    unsafe { syscall(sysno::SEND, cap, word, 0, 0, 0) }
}

/// Send `word` plus a byte payload on `cap`. The kernel copies `bytes` out of our address
/// space before returning, so the buffer may be reused immediately.
fn send_bytes(cap: u64, word: u64, bytes: &[u8]) -> u64 {
    // SAFETY: the pointer/len describe a live borrow for the duration of the call.
    unsafe {
        syscall(
            sysno::SEND,
            cap,
            word,
            bytes.as_ptr() as u64,
            bytes.len() as u64,
            0,
        )
    }
}

/// Receive one word on the endpoint named by capability `cap` (needs `READ`; blocks until a
/// sender delivers). Returns `(status, word)`: the status is `syserr::OK` or `NO_CAP`, and
/// the word is meaningful only when the status is `OK`.
///
/// This needs its own stub rather than [`syscall`]: the kernel returns the payload in a
/// SECOND register (`a1`), which the compiler must be told the `ecall` clobbers — and the
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
    // SAFETY: `ecall` traps to S-mode. `a1`/`a3` are declared outputs because the kernel
    // returns the word and the byte count there; `buf` is a live writable borrow.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") sysno::RECV,
            inlateout("a0") cap => status,
            inlateout("a1") buf.as_mut_ptr() as u64 => word,
            in("a2") buf.len() as u64,
            lateout("a3") n,
            options(nostack),
        );
    }
    (status, word, n as usize)
}

/// Block until at least one interrupt has arrived on the line our `Irq` capability names,
/// then return the count. `syserr::NO_CAP` if we hold no such capability.
fn wait_irq(cap: u64) -> u64 {
    // SAFETY: WAIT_IRQ takes a cap id and returns a count; no user memory is touched.
    unsafe { syscall(sysno::WAIT_IRQ, cap, 0, 0, 0, 0) }
}

/// Collect device interrupts delivered to us since the last call, via an `Irq` capability.
/// Returns the count, or `syserr::NO_CAP` if we hold no such capability.
fn poll_irq(cap: u64) -> u64 {
    // SAFETY: POLL_IRQ takes a cap id and returns a count; no user memory is touched.
    unsafe { syscall(sysno::POLL_IRQ, cap, 0, 0, 0, 0) }
}

/// Probe whether `va` is still MAPPED, without risking a fault: `DEBUG_WRITE` makes the
/// kernel READ one byte from there through its permission-checked copy, so the status says
/// whether the page is still present and user-readable. (It emits that byte to the console,
/// which is why the probe is one byte of the device signature.)
fn mapped_probe(va: u64) -> u64 {
    // SAFETY: the kernel validates `va` itself; a bad address returns FAULT, not a fault.
    unsafe { syscall(sysno::DEBUG_WRITE, va, 1, 0, 0, 0) }
}

/// RECV with a RAW destination address, for the memory-safety regression tests below.
fn recv_raw(cap: u64, dst: u64, max: u64) -> u64 {
    let status: u64;
    // SAFETY: as `recv_bytes`; `dst` is deliberately arbitrary here — the point is that the
    // kernel must validate it rather than trusting us.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") sysno::RECV,
            inlateout("a0") cap => status,
            inlateout("a1") dst => _,
            in("a2") max,
            lateout("a3") _,
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
    unsafe { syscall(sysno::SPAWN, cap, deleg, rights, 0, 0) }
}

/// Revoke every capability derived from our capability `cap` by delegation (transitively).
/// We keep `cap` itself. Returns `syserr::OK`, or `NO_CAP` if we do not hold it.
fn revoke(cap: u64) -> u64 {
    // SAFETY: REVOKE takes a cap id and returns a status; no user memory is touched.
    unsafe { syscall(sysno::REVOKE, cap, 0, 0, 0, 0) }
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
    unsafe { syscall(sysno::FREE_VRAM, phys, 0, 0, 0, 0) }
}

/// The ELF entry point (U-mode, fresh stack, id in the first-argument register). The demo
/// is role-selected by `id`: proc 0 produces + `SEND`s five values, proc 1 `RECV`s + prints
/// them (cross-address-space IPC rendezvous), and any other proc runs a preemptible compute
/// loop + the per-process host contract. Together they show IPC blocking and preemption
/// coexisting. Never returns (each role exits via the EXIT syscall).
///
/// Declared `extern "C"` / `#[no_mangle]` so the linker resolves `_start` (the
/// `ENTRY` of `link.ld`); the `id` parameter reads the first-arg register.
#[no_mangle]
pub extern "C" fn _start(id: u64) -> ! {
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
    debug_write(b"producer: sending 5 values on ep 0\n");
    // Least authority: this role holds SEND on the endpoint and nothing else. It cannot
    // receive on the very endpoint it sends to, and holds no device authority at all.
    tag(0);
    if recv(0).0 == syserr::NO_CAP {
        debug_write(b"role: producer cannot RECV on its own endpoint (send-only)\n");
    } else {
        debug_write(b"role: producer CAN recv (bug)\n");
    }
    tag(0);
    if map_bar(CapId(1), 0).is_err() {
        debug_write(b"role: producer holds no device authority\n");
    } else {
        debug_write(b"role: producer mapped a BAR (bug)\n");
    }
    let mut i = 0u64;
    while i < 4 {
        let v = 100u64.wrapping_add(i);
        send(0, v);
        tag(0);
        debug_write(b"sent ");
        dbg_dec(v);
        debug_write(b"\n");
        i = i.wrapping_add(1);
    }
    // Fifth message: the NO_CAP error sentinel sent as ORDINARY DATA (the word domain is
    // the whole u64, so this is a legal payload and the consumer must still see a
    // successful receive), carrying a BYTE PAYLOAD alongside it. The payload crosses
    // address spaces — the consumer has no mapping of ours — so the kernel copies it.
    send_bytes(0, syserr::NO_CAP, b"payload-across-address-spaces");
    tag(0);
    debug_write(b"sent NO_CAP-valued word + byte payload\n");
    // A sixth message sent while the consumer is busy checking the fifth: we block holding
    // the payload, so the receiver takes the OTHER copy path (out of a parked sender)
    // rather than the deferred one. Both directions of the transfer get exercised.
    send_bytes(0, 0xB, b"second-payload");
    tag(0);
    debug_write(b"sent second payload (sender-parked path)\n");
    exit(0);
}

/// IPC consumer: receive five values on endpoint 0, blocking until each arrives. The fifth
/// is bit-identical to the `NO_CAP` sentinel and must still report as a successful receive.
fn consumer() -> ! {
    tag(1);
    debug_write(b"consumer: receiving 5 values on ep 0\n");
    // Least authority: this role holds RECEIVE and nothing else — it cannot inject a
    // message onto the endpoint it reads, nor allocate memory.
    tag(1);
    if send(0, 0xDEAD) == syserr::NO_CAP {
        debug_write(b"role: consumer cannot SEND on its own endpoint (recv-only)\n");
    } else {
        debug_write(b"role: consumer CAN send (bug)\n");
    }
    tag(1);
    if alloc_vram(CapId(2), 4096).is_err() {
        debug_write(b"role: consumer holds no memory authority\n");
    } else {
        debug_write(b"role: consumer allocated VRAM (bug)\n");
    }
    let mut i = 0u64;
    while i < 4 {
        let (status, v) = recv(0);
        tag(1);
        if status == syserr::OK {
            debug_write(b"recv ");
            dbg_dec(v);
            debug_write(b"\n");
        } else {
            debug_write(b"recv FAILED status=");
            dbg_hex(status);
            debug_write(b"\n");
        }
        i = i.wrapping_add(1);
    }
    let mut buf = [0u8; 64];
    let (status, v, n) = recv_bytes(0, &mut buf);
    tag(1);
    if status == syserr::OK && v == syserr::NO_CAP {
        debug_write(b"recv NO_CAP-valued word as DATA (status separate from payload)\n");
    } else {
        debug_write(b"recv sentinel word MISREAD as error (bug)\n");
    }
    // The payload arrived through the kernel: we can read no memory of the sender's.
    tag(1);
    if n == 29 && &buf[..n] == b"payload-across-address-spaces" {
        debug_write(b"recv byte payload intact across address spaces (");
        dbg_dec(n as u64);
        debug_write(b" bytes)\n");
    } else {
        debug_write(b"recv byte payload WRONG (bug)\n");
    }
    let (status2, w2, n2) = recv_bytes(0, &mut buf);
    tag(1);
    if status2 == syserr::OK && w2 == 0xB && n2 == 14 && &buf[..n2] == b"second-payload" {
        debug_write(b"recv second payload intact (other copy path)\n");
    } else {
        debug_write(b"recv second payload WRONG (bug)\n");
    }
    exit(1);
}

/// A preemptible compute process: a busy loop with NO syscalls between prints, so only the
/// supervisor timer can interleave it (its `tick K` lines interleaving proves preemption).
/// Runs the per-process host contract afterward, then exits.
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
            debug_write(b"irq: could NOT spawn the interrupt helper (bug)\n");
        } else {
            debug_write(b"irq: spawned an interrupt helper -> pid ");
            dbg_dec(helper);
            debug_write(b"\n");
        }
    }
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
        debug_write(b"irq: blocked and woke on real device interrupts (");
        dbg_dec(ticks);
        debug_write(b")\n");
    } else {
        debug_write(b"irq: no interrupts delivered (bug)\n");
    }
    // And an unheld line is invisible: CapId(9) names nothing we hold.
    tag(id);
    if poll_irq(9) == syserr::NO_CAP {
        debug_write(b"irq: polling an unheld line -> NO_CAP\n");
    } else {
        debug_write(b"irq: polled a line we do not hold (bug)\n");
    }

    tag(id);
    debug_write(b"start (compute loop, no yields -- preemption only)\n");
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
                debug_write(b"mmio: read device signature through the mapping\n");
            } else {
                debug_write(b"mmio: device signature WRONG (bug)\n");
            }
            unsafe { core::ptr::write_volatile(dev.wrapping_add(32), 0x5Au8) };
            tag(id);
            if unsafe { core::ptr::read_volatile(dev.wrapping_add(32)) } == 0x5A {
                debug_write(b"mmio: wrote and read back through the mapping\n");
            } else {
                debug_write(b"mmio: device write did not stick (bug)\n");
            }
            // Probe the page's writability WITHOUT faulting: RECV validates its buffer
            // against the page tables before checking the capability, so an unheld cap
            // yields NO_CAP when the buffer is writable and FAULT when it is not.
            tag(id);
            if recv_raw(9, r.user_va, 16) == syserr::NO_CAP {
                debug_write(b"mmio: full-rights cap gives a WRITABLE window\n");
            } else {
                debug_write(b"mmio: full-rights window not writable (bug)\n");
            }
        }
        Err(e) => {
            tag(id);
            debug_write(b"map_bar err=");
            dbg_hex(e);
            debug_write(b"\n");
        }
    }
    // IPC authority: endpoints are capabilities, not raw integers. CapId(3) is an
    // Endpoint cap with READ only, and CapId(9) is not held at all — sending on either
    // must be refused, proving the kernel gates IPC on the cap AND its rights.
    // A worker is not in the IPC pair: its CapId(0) exists but confers nothing, so even
    // the shared endpoint is closed to it. Possession is not authority.
    tag(id);
    if send(0, 0xBEEF) == syserr::NO_CAP {
        debug_write(b"role: worker holds no authority on the shared endpoint\n");
    } else {
        debug_write(b"role: worker CAN send on the shared endpoint (bug)\n");
    }
    tag(id);
    if send(3, 0xBEEF) == syserr::NO_CAP {
        debug_write(b"ipc: send on read-only ep cap -> NO_CAP (rights enforced)\n");
    } else {
        debug_write(b"ipc: send on read-only ep cap ALLOWED (bug)\n");
    }
    tag(id);
    if send(9, 0xBEEF) == syserr::NO_CAP {
        debug_write(b"ipc: send on unheld cap -> NO_CAP (authority enforced)\n");
    } else {
        debug_write(b"ipc: send on unheld cap ALLOWED (bug)\n");
    }
    // RECV refusal must also be unambiguous — and must not block us.
    tag(id);
    let (status, _) = recv(9);
    if status == syserr::NO_CAP {
        debug_write(b"ipc: recv on unheld cap -> NO_CAP (no block)\n");
    } else {
        debug_write(b"ipc: recv on unheld cap ALLOWED (bug)\n");
    }

    // Memory-safety regression tests. The kernel copies an IPC payload into a buffer WE
    // name, so it must validate that buffer against the page tables — not merely against
    // the address range. A destination that is mapped read-only (our own image text) must
    // be refused rather than written through, and one that is in range but unmapped must
    // be refused rather than faulting inside the kernel, which would kill the guest.
    tag(id);
    if recv_raw(0, 0x1_0000_0000, 16) == syserr::FAULT {
        debug_write(b"memsafe: RECV into read-only mapping refused (W^X honoured)\n");
    } else {
        debug_write(b"memsafe: RECV into read-only mapping ALLOWED (bug)\n");
    }
    tag(id);
    if recv_raw(0, 0x1_8000_0000, 16) == syserr::FAULT {
        debug_write(b"memsafe: RECV into unmapped address refused (no kernel fault)\n");
    } else {
        debug_write(b"memsafe: RECV into unmapped address ALLOWED (bug)\n");
    }

    // Rights are checked on the REST of the host contract too, not just IPC: CapId(4) is an
    // Untyped cap without WRITE and CapId(5) an Mmio cap without READ — right type, wrong
    // rights, so every one of these must be refused.
    tag(id);
    if alloc_vram(CapId(4), 4096).is_err() {
        debug_write(b"caps: alloc_vram via WRITE-less Untyped -> NO_CAP\n");
    } else {
        debug_write(b"caps: alloc_vram via WRITE-less Untyped ALLOWED (bug)\n");
    }
    tag(id);
    if spawn(4) == u64::MAX {
        debug_write(b"caps: spawn via WRITE-less Untyped -> refused\n");
    } else {
        debug_write(b"caps: spawn via WRITE-less Untyped ALLOWED (bug)\n");
    }
    tag(id);
    if map_bar(CapId(5), 0).is_err() {
        debug_write(b"caps: map_bar via READ-less Mmio -> NO_CAP\n");
    } else {
        debug_write(b"caps: map_bar via READ-less Mmio ALLOWED (bug)\n");
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
    debug_write(b"vram: hit quota at ");
    dbg_dec(n);
    debug_write(b" frames\n");
    if last != 0 {
        free_vram(last);
        let p = alloc_vram_phys();
        tag(id);
        if p != 0 {
            debug_write(b"vram: freed 1, realloc OK\n");
        } else {
            debug_write(b"vram: freed 1, realloc FAILED\n");
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
            debug_write(b"late spawn into a recycled slot REFUSED -- no free process slot (bug)\n");
        } else {
            debug_write(b"late spawn into a recycled slot -> pid ");
            dbg_dec(late);
            debug_write(b"\n");
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
        debug_write(b"spawned child pid=");
        dbg_dec(child);
        debug_write(b"\n");
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
            debug_write(b"revoke: revoked delegations of CapId(1) and CapId(2)\n");
        } else {
            debug_write(b"revoke: refused (bug)\n");
        }
        // Our own capability is untouched: revoking grants does not disarm us. NOTE the
        // error code matters here — we are at our VRAM quota from the loop above, so a
        // refusal is expected; only NO_CAP would mean revocation destroyed our own cap.
        tag(id);
        match alloc_vram(CapId(2), 4096) {
            Ok(r) => {
                free_vram(r.phys);
                debug_write(b"revoke: our own CapId(2) still works\n");
            }
            Err(e) if e != syserr::NO_CAP => {
                debug_write(
                    b"revoke: our own CapId(2) survives (refused on quota, not authority)\n",
                );
            }
            Err(_) => debug_write(b"revoke: revoking cost us our own cap (bug)\n"),
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
        debug_write(b"irq: blocked with no credits and hardware woke us\n");
    } else {
        debug_write(b"irq: final wait returned nothing (bug)\n");
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
        debug_write(b"irq: two lines stay separate (timer counted, console still quiet)\n");
    } else if bytes == syserr::NO_CAP {
        debug_write(b"irq: no authority for the console line (bug)\n");
    } else {
        debug_write(b"irq: one line's count leaked into the other (bug)\n");
    }

    // Now block on the CONSOLE line. Nothing the kernel does can end this wait: the timer
    // fires throughout and cannot credit it, so the kernel parks, wakes on each tick with
    // nobody to run, and parks again -- until a byte actually arrives from outside. Every
    // park until now was ended by the clock; this one is ended by a device, which is the
    // thing a driver waiting on its hardware is really doing.
    tag(id);
    debug_write(b"irq: blocking on the console line -- only real input can wake us\n");
    let c = wait_irq(7);
    tag(id);
    if c > 0 && c != syserr::NO_CAP {
        debug_write(b"irq: woke on a REAL device interrupt, not the clock\n");
    } else {
        debug_write(b"irq: console wait returned nothing (bug)\n");
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
        debug_write(b"child: no endpoint authority of its own\n");
    } else {
        debug_write(b"child: CAN send on the shared endpoint (bug)\n");
    }
    tag(id);
    if poll_irq(6) == syserr::NO_CAP {
        debug_write(b"child: no interrupt authority of its own\n");
    } else {
        debug_write(b"child: could poll an interrupt line (bug)\n");
    }
    tag(id);
    if map_bar(CapId(1), 0).is_err() {
        debug_write(b"child: no device authority of its own\n");
    } else {
        debug_write(b"child: mapped a BAR (bug)\n");
    }

    // Were we handed an INTERRUPT line? Then we are the driver's helper. No role table
    // grants a child interrupt authority, so it working at all is the delegation test;
    // and blocking on it repeatedly is what drives the kernel into its idle park.
    if poll_irq(0) != syserr::NO_CAP {
        tag(id);
        debug_write(b"deleg: delegated Irq WORKS (authority no role granted us)\n");
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
        debug_write(b"irq: helper woke ");
        dbg_dec(woke);
        debug_write(b" time(s) on a delegated line\n");
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
            debug_write(b"mmio: read device signature through a delegated cap\n");
        } else {
            debug_write(b"mmio: delegated device signature WRONG (bug)\n");
        }
        // Our capability was attenuated to READ, so the window must NOT be writable.
        // Probed without faulting, as in the worker.
        tag(id);
        if recv_raw(9, r.user_va, 16) == syserr::FAULT {
            debug_write(b"mmio: READ-only cap gives a READ-ONLY window (no amplification)\n");
        } else {
            debug_write(b"mmio: READ-only cap gave a WRITABLE window (bug)\n");
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
        debug_write(b"\n");
        tag(id);
        if gone {
            debug_write(b"mmio: mapping torn down when the cap was REVOKED\n");
        } else {
            debug_write(b"mmio: mapping survived revocation (bug)\n");
        }
        // Deliberately fault. A wild pointer is OUR bug, not the machine's: the kernel must
        // kill this process and keep running, and the boot must still reach BOOT OK. If a
        // user fault halted the guest, this line would end the run right here.
        tag(id);
        debug_write(b"fault: dereferencing a wild pointer on purpose\n");
        unsafe { core::ptr::write_volatile(0x1u64 as *mut u8, 0) };
        tag(id);
        debug_write(b"fault: SURVIVED a wild write (bug)\n");
        exit(id);
    }

    // Use the delegated capability, then pass it on once. Passing it on makes the
    // revocation test TRANSITIVE: the parent's REVOKE must reach not just us but the
    // grandchild we handed it to. Self-limiting — the spawn fails once slots run out.
    let have = match alloc_vram(CapId(0), 4096) {
        Ok(r) => {
            free_vram(r.phys);
            true
        }
        Err(_) => false,
    };
    tag(id);
    if have {
        debug_write(b"deleg: delegated Untyped WORKS (authority no role granted it)\n");
    } else {
        debug_write(
            b"deleg: parent asked ALL on its READ-only cap -> still refused (no amplification)\n",
        );
    }
    if have {
        let g = spawn_delegating(0, 0, 0b111);
        tag(id);
        if g == u64::MAX {
            debug_write(b"deleg: no free slot to pass it on\n");
        } else {
            debug_write(b"deleg: passed our cap on to pid ");
            dbg_dec(g);
            debug_write(b"\n");
        }
    }

    // Our parent revokes while we run. A capability that keeps working after revocation
    // would be the bug — and for a grandchild, so would one that is never reached.
    if have {
        let mut seen = false;
        let mut i = 0u64;
        while i < 60 {
            match alloc_vram(CapId(0), 4096) {
                Ok(r) => {
                    free_vram(r.phys);
                }
                Err(_) => {
                    seen = true;
                    break;
                }
            }
            spin(2_000_000);
            i = i.wrapping_add(1);
        }
        tag(id);
        if seen {
            debug_write(b"revoke: delegated cap REVOKED by parent (no longer usable)\n");
        } else {
            debug_write(b"revoke: delegated cap still usable after revoke (bug)\n");
        }
    }

    // Still preemptible, with DF deliberately set inside spin() (see `spin`).
    let mut tick = 0u64;
    while tick < 3 {
        tag(id);
        debug_write(b"tick ");
        dbg_dec(tick);
        debug_write(b"\n");
        spin(5_000_000);
        tick = tick.wrapping_add(1);
    }
    exit(id);
}

/// Any panic in U-mode is fatal: report via the exit code and terminate.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(255);
}
