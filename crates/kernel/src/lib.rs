//! kernel — the architecture-generic Rustproof nucleus, written once against
//! [`hal::Arch`] and instantiated per-ISA by the thin `nucleus` / `nucleus-riscv` bins.
//!
//! [`run`] is the whole kmain: console + traps, the portable core (frame allocator,
//! capabilities, IPC, a cooperative context switch), then paging, and finally several
//! isolated capability-gated user processes under a round-robin scheduler. Every entry
//! from user mode saves a [`hal::UserFrame`] and re-enters via `Arch::resume`;
//! [`syscall_trap`] services the call / `YIELD` / `EXIT` and picks the next process (see
//! `docs/scheduling.md`). The x86-64 and RISC-V specifics live entirely behind the
//! `hal::Arch` + `hal::Space` implementations in [`arch_x86`] / [`arch_riscv`].
#![no_std]

use core::fmt::Write as _;
use core::marker::PhantomData;
use hal::{Arch, Perms, Space, UserFrame};

#[cfg(target_arch = "x86_64")]
mod arch_x86;
#[cfg(target_arch = "x86_64")]
mod pvh;
#[cfg(target_arch = "x86_64")]
pub use arch_x86::X86 as CurrentArch;

#[cfg(target_arch = "riscv64")]
mod arch_riscv;
#[cfg(target_arch = "riscv64")]
pub use arch_riscv::Riscv as CurrentArch;

// ---- kernel state (single CPU: plain statics) ----
static mut BITMAP: [u64; 12288] = [0; 12288]; // one bit per 4 KiB frame
static mut FA: Option<mm::BitmapAllocator> = None;
static mut MAIN_CTX: sched::Context = sched::Context::new();
static mut B_CTX: sched::Context = sched::Context::new();
static mut B_STACK: [u8; 16 * 1024] = [0; 16 * 1024];

/// Run-queue capacity (also the process-table size): the initial processes plus headroom
/// for `SPAWN`ed ones.
const MAX_PROCS: usize = 6;
/// How many independent copies of the user image to launch at boot.
const NUM_PROCS: usize = 3;

/// Scheduling state of a process slot. `Ready` processes are exactly those in the run
/// queue (`SCHED`); a blocked process is removed from it until its IPC rendezvous completes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcState {
    /// Slot unused (available to load a process into).
    Free,
    /// Runnable — in the run queue.
    Ready,
    /// Blocked in `SEND` on `ep`, holding the `word` until a receiver takes it.
    BlockedSend { ep: u64, word: u64 },
    /// Blocked in `RECV` on `ep`, waiting for a sender.
    BlockedRecv { ep: u64 },
}

/// How many address-space frames a process may hold (tracked for reclamation on exit): its
/// page tables + stack + ELF segments, ~25. VRAM frames are tracked + quota'd separately.
const MAX_PROC_FRAMES: usize = 64;

/// Per-process VRAM quota: the most `ALLOC_VRAM` frames a process may hold at once
/// (`FREE_VRAM` returns quota). Also the capacity of the per-process VRAM tracking list.
const VRAM_QUOTA_FRAMES: usize = 8;

/// One scheduled user process: its own address space (`token`), its last-saved user
/// register state (`frame`), and its own capability space — the isolation boundary.
struct Process {
    /// Paging-base token (`cr3`/`satp`) of this process's address space.
    token: u64,
    /// Saved user register state, resumed by `Arch::resume`.
    frame: UserFrame,
    /// Per-process authority: what this process may invoke via the host contract.
    caps: capabilities::CapSpace<16>,
    /// Scheduling / IPC state.
    state: ProcState,
    /// Every address-space frame allocated for this process, freed back to the pool on exit
    /// so a spawn/exit cycle does not leak an address space. `frames[..nframes]` are live.
    frames: [abi::PhysAddr; MAX_PROC_FRAMES],
    nframes: usize,
    /// VRAM (DMA) frames the process currently holds via `ALLOC_VRAM`. `vram[..nvram]` are
    /// live; `nvram` is the process's VRAM usage (capped at the quota) and each is
    /// individually freeable via `FREE_VRAM`. Also reclaimed on exit.
    vram: [abi::PhysAddr; VRAM_QUOTA_FRAMES],
    nvram: usize,
}

impl Process {
    const EMPTY: Process = Process {
        token: 0,
        frame: UserFrame::ZERO,
        caps: capabilities::CapSpace::new(),
        state: ProcState::Free,
        frames: [abi::PhysAddr(0); MAX_PROC_FRAMES],
        nframes: 0,
        vram: [abi::PhysAddr(0); VRAM_QUOTA_FRAMES],
        nvram: 0,
    };
}

/// A [`FrameAllocator`](abi::FrameAllocator) that records every frame it hands out (into a
/// caller-provided list) while delegating to the real allocator, so a process's frames can
/// be reclaimed when it exits. Because it only records what *this* process allocates, it
/// never captures the shared kernel frames — `share_kernel` copies a pointer, it does not
/// allocate. If the list is full the allocation fails (frame returned), bounding a process
/// rather than leaking an untracked frame.
struct RecordingAlloc<'a> {
    inner: &'a mut dyn abi::FrameAllocator,
    frames: &'a mut [abi::PhysAddr; MAX_PROC_FRAMES],
    n: &'a mut usize,
}

impl abi::FrameAllocator for RecordingAlloc<'_> {
    fn alloc_frame(&mut self) -> Option<abi::PhysAddr> {
        let p = self.inner.alloc_frame()?;
        if *self.n >= MAX_PROC_FRAMES {
            self.inner.free_frame(p);
            return None;
        }
        self.frames[*self.n] = p;
        *self.n += 1;
        Some(p)
    }
    fn free_frame(&mut self, frame: abi::PhysAddr) {
        self.inner.free_frame(frame);
    }
}

/// The process table + round-robin run queue + index of the running process. Kept in
/// sync: `CURRENT == SCHED.current()` at every trap boundary.
static mut PROCS: [Process; MAX_PROCS] = [Process::EMPTY; MAX_PROCS];
static mut SCHED: sched::Scheduler<MAX_PROCS> = sched::Scheduler::new();
static mut CURRENT: usize = 0;

/// The embedded user image + kernel token, stashed at boot so the `SPAWN` syscall can load
/// a fresh process (it runs after `run` has consumed its locals).
static mut USER_ELF: &[u8] = &[];
static mut KTOKEN: u64 = 0;

/// A `&'static mut` to process slot `i`, via a raw pointer (no direct `static mut` ref).
///
/// # Safety
/// Single-CPU, non-reentrant: callers hold no other live borrow of `PROCS[i]`.
#[inline]
unsafe fn proc_at<'a>(i: usize) -> &'a mut Process {
    &mut *(core::ptr::addr_of_mut!(PROCS) as *mut Process).add(i)
}

/// A `&'static mut` to the scheduler, via a raw pointer.
///
/// # Safety
/// Single-CPU, non-reentrant: callers hold no other live borrow of `SCHED`.
#[inline]
unsafe fn sched() -> &'static mut sched::Scheduler<MAX_PROCS> {
    &mut *core::ptr::addr_of_mut!(SCHED)
}

/// Resolve an IPC capability to the endpoint it names, enforcing authority: process `proc`
/// must hold `cap` as an [`abi::CapType::Endpoint`] carrying `needed` (WRITE to send, READ
/// to receive). Returns the capability's *object* — the endpoint id — so two processes
/// rendezvous only when their caps name the same endpoint, whatever slot each holds it in.
/// `None` means no authority: the caller gets `NO_CAP` and does not block.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn endpoint_of(proc: usize, cap: u64, needed: abi::CapRights) -> Option<u64> {
    let slot = proc_at(proc).caps.lookup(abi::CapId(cap as usize))?;
    if slot.cap_type == abi::CapType::Endpoint && slot.rights.contains(needed) {
        Some(slot.object)
    } else {
        None
    }
}

/// The first process blocked receiving on endpoint `ep`, if any.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn find_blocked_recv(ep: u64) -> Option<usize> {
    (0..MAX_PROCS).find(|&i| proc_at(i).state == ProcState::BlockedRecv { ep })
}

/// The first process blocked sending on endpoint `ep`, with its pending word, if any.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn find_blocked_send(ep: u64) -> Option<(usize, u64)> {
    (0..MAX_PROCS).find_map(|i| match proc_at(i).state {
        ProcState::BlockedSend { ep: e, word } if e == ep => Some((i, word)),
        _ => None,
    })
}

/// Called when the run queue is empty: either every process has exited (a clean finish —
/// `BOOT OK`) or the survivors are all blocked on IPC with no one to wake them (a
/// deadlock — a failure). Never returns.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn nothing_runnable<A: Arch>() -> ! {
    let mut con = Console::<A>::new();
    let deadlocked = (0..MAX_PROCS).any(|i| {
        matches!(
            proc_at(i).state,
            ProcState::BlockedSend { .. } | ProcState::BlockedRecv { .. }
        )
    });
    if deadlocked {
        let _ = writeln!(
            con,
            "\n[kernel] deadlock: no runnable process (survivors blocked on IPC)"
        );
        A::exit(false);
    }
    // Report the free-frame count: with per-process reclamation on exit it should be back
    // near the pre-userland count (proving spawn/exit does not leak an address space).
    if let Some(fa) = (*core::ptr::addr_of!(FA)).as_ref() {
        let _ = writeln!(con, "[mm] {} frames free after all exits", fa.free_count());
    }
    let _ = writeln!(con, "\nrustproof: BOOT OK");
    A::exit(true)
}

/// Load the user `elf` into process slot `slot`: a fresh address space (kernel shared in),
/// a mapped user stack, a fresh capability space (Mmio@CapId(1), Untyped@CapId(2)), and an
/// initial frame entering `_start` with `id_arg` in the first-argument register. Sets the
/// slot `Ready`; the caller adds it to the run queue. Returns `false` (leaving the slot
/// untouched enough to retry) if the address space or a frame can't be allocated.
///
/// Shared by boot (`run`) and the `SPAWN` syscall — the only difference is where `fa` comes
/// from (a `run` local vs the `FA` static).
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of `PROCS[slot]`. `ktoken` must be the
/// active kernel token so `share_kernel` and the page-table writes are reachable.
unsafe fn load_process<A: Arch>(
    slot: usize,
    id_arg: u64,
    fa: &mut dyn abi::FrameAllocator,
    ktoken: u64,
    elf: &[u8],
) -> bool {
    use abi::FrameAllocator as _;
    // Build the address space through a recorder so every allocated frame is tracked for
    // reclamation on exit. Kept in a local list until the build succeeds.
    let mut frames = [abi::PhysAddr(0); MAX_PROC_FRAMES];
    let mut n = 0usize;
    let mut rec = RecordingAlloc {
        inner: fa,
        frames: &mut frames,
        n: &mut n,
    };
    let built: Option<(u64, u64)> = (|| {
        let mut space = A::Space::create(&mut rec)?;
        space.share_kernel(ktoken);
        let entry = A::load_user(elf, &mut space, &mut rec)?;
        for p in 1..=A::USER_STACK_PAGES {
            let va = abi::VirtAddr(A::USER_STACK_TOP - p * abi::PAGE_SIZE);
            let frame = rec.alloc_frame()?;
            if !space.map_page(va, frame, Perms::USER_RW, &mut rec) {
                return None;
            }
        }
        Some((space.token(), entry))
    })();
    drop(rec);

    match built {
        Some((token, entry)) => {
            let s = proc_at(slot);
            s.caps = capabilities::CapSpace::new();
            // CapId(0): the shared IPC endpoint (object 0), send + receive.
            let _ = s
                .caps
                .insert(abi::CapType::Endpoint, abi::CapRights::ALL, 0);
            let _ = s
                .caps
                .insert(abi::CapType::Mmio, abi::CapRights::ALL, 0xE000_0000);
            let _ = s.caps.insert(abi::CapType::Untyped, abi::CapRights::ALL, 0);
            // CapId(3): endpoint object 1, RECEIVE-ONLY — holding an endpoint cap is not
            // permission to send on it, which the demo exercises.
            let _ = s
                .caps
                .insert(abi::CapType::Endpoint, abi::CapRights::READ, 1);
            // CapId(4)/CapId(5): deliberately under-powered caps of the RIGHT type, so the
            // rights half of every gate is exercised on hardware rather than vacuously true:
            // an Untyped without WRITE cannot allocate or spawn, and an Mmio without READ
            // cannot map a BAR.
            let _ = s
                .caps
                .insert(abi::CapType::Untyped, abi::CapRights::READ, 0);
            let _ = s
                .caps
                .insert(abi::CapType::Mmio, abi::CapRights::WRITE, 0xE000_0000);
            s.token = token;
            s.frame = A::frame_init(entry, A::USER_STACK_TOP, id_arg);
            s.frames = frames;
            s.nframes = n;
            s.state = ProcState::Ready;
            true
        }
        None => {
            // Roll back a partial build so a failed load leaks nothing.
            for i in 0..n {
                fa.free_frame(frames[i]);
            }
            false
        }
    }
}

/// Stubbed gfx1201 identity returned by the host contract's `GET_INFO`.
const GPU_INFO: abi::GpuInfo = abi::GpuInfo {
    pci_vendor: 0x1002,
    pci_device: 0x7551,
    gfx_version: 0x1201,
    vram_bytes: 16u64 << 30,
};

/// A `core::fmt::Write` sink that routes through `A::console_write`, expanding `\n` to CRLF.
pub struct Console<A>(core::marker::PhantomData<A>);

impl<A: Arch> Console<A> {
    pub fn new() -> Self {
        Console(core::marker::PhantomData)
    }
}

impl<A: Arch> Default for Console<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A: Arch> core::fmt::Write for Console<A> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                A::console_write(b"\r");
            }
            A::console_write(&[b]);
        }
        Ok(())
    }
}

/// A second kernel thread used to exercise the cooperative context switch.
extern "C" fn thread_b<A: Arch>() -> ! {
    A::console_write(b"  [thread B] running on its own stack -- switching back to main\n");
    unsafe {
        sched::switch(
            core::ptr::addr_of_mut!(B_CTX),
            core::ptr::addr_of!(MAIN_CTX),
        )
    };
    loop {
        core::hint::spin_loop();
    }
}

/// The generic kmain. `a0`/`a1` are the arch boot args (x86 PVH `start_info`; RISC-V
/// `hartid`/`dtb`); `user_elf` is the embedded user program. Never returns.
pub fn run<A: Arch>(a0: u64, a1: u64, user_elf: &'static [u8]) -> ! {
    use abi::FrameAllocator as _;
    let mut con = Console::<A>::new();
    let _ = writeln!(con);
    let _ = writeln!(con, "Rustproof nucleus ({}) — unified kernel/hal", A::NAME);

    A::init_traps();
    let _ = writeln!(con, "  traps installed");

    #[cfg(feature = "provoke-fault")]
    {
        let _ = writeln!(
            con,
            "provoke-fault: reading unmapped 0xDEADBEEF to force a fault"
        );
        let _ = unsafe { core::ptr::read_volatile(0xDEAD_BEEF_usize as *const u32) };
    }

    // ---------------- mm: memory map + bitmap frame allocator ----------------
    let mut regions = [abi::MemoryRegion {
        start: 0,
        len: 0,
        kind: abi::MemoryKind::Reserved,
    }; 32];
    let n = A::memory_map(a0, a1, &mut regions);
    let regions = &regions[..n];
    let words = mm::BitmapAllocator::bitmap_words_needed(regions);
    let bitmap: &'static mut [u64] = unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(BITMAP) as *mut u64, words)
    };
    let mut fa = mm::BitmapAllocator::new(regions, bitmap, A::reserve_below(), A::dma_top());
    let _ = writeln!(
        con,
        "\n[mm] {} frames tracked, {} free",
        fa.total_frames(),
        fa.free_count()
    );
    let f0 = fa.alloc_frame();
    let f1 = fa.alloc_frame();
    let dma = fa.alloc_dma_frame();
    let _ = writeln!(
        con,
        "[mm] alloc -> {:#x}, {:#x}; dma -> {:#x}; free {}",
        f0.map(|p| p.as_u64()).unwrap_or(0),
        f1.map(|p| p.as_u64()).unwrap_or(0),
        dma.map(|p| p.as_u64()).unwrap_or(0),
        fa.free_count()
    );
    if let Some(p) = f0 {
        fa.free_frame(p);
    }

    // ---------------- capabilities: authority-monotonic derivation ----------------
    {
        let mut caps = capabilities::CapSpace::<64>::new();
        let root = caps
            .insert(abi::CapType::Untyped, abi::CapRights::ALL, 0xF00D)
            .expect("root cap");
        let child = caps
            .derive(root, abi::CapRights::READ)
            .expect("read-only child");
        let escalated = caps
            .derive(child, abi::CapRights::WRITE)
            .and_then(|c| caps.lookup(c))
            .map(|s| s.rights.0)
            .unwrap_or(0);
        let _ = writeln!(
            con,
            "\n[cap] READ-only child derives WRITE -> rights={:#05b} ({})",
            escalated,
            if escalated & abi::CapRights::WRITE.0 == 0 {
                "WRITE dropped — authority-monotonic"
            } else {
                "ESCALATED?! (bug)"
            }
        );
        caps.revoke_subtree(root);
    }

    // ---------------- ipc: synchronous endpoint ----------------
    {
        let mut ep = ipc::Endpoint::<8>::new();
        let waiting = ep.recv(abi::ThreadId(2));
        let _ = writeln!(con, "\n[ipc] T2 recv with no sender -> {:?}", waiting);
        let words = [0xCAFE_u64, 0xF00D_u64];
        if let ipc::IpcAction::Deliver { to, from, msg, .. } =
            ep.send(abi::ThreadId(1), abi::MessageInfo::new(0x42, 2), &words)
        {
            let _ = writeln!(
                con,
                "[ipc] T1 send -> Deliver to T{} from T{} label={:#x}",
                to.0, from.0, msg.label
            );
        }
    }

    // ---------------- sched: real cooperative context switch ----------------
    let _ = writeln!(con, "\n[sched] switching to thread B (real context switch)");
    unsafe {
        let top = abi::VirtAddr(core::ptr::addr_of!(B_STACK) as u64 + 16 * 1024);
        B_CTX = sched::Context::prepare(top, thread_b::<A>);
        sched::switch(
            core::ptr::addr_of_mut!(MAIN_CTX),
            core::ptr::addr_of!(B_CTX),
        );
    }
    let _ = writeln!(con, "  [main] resumed after thread B via context switch");

    // ---------------- paging ----------------
    let ktoken = A::setup_paging(&mut fa);
    let _ = writeln!(con, "\n[paging] enabled; kernel token = {:#018x}", ktoken);

    // ---------------- userland: N isolated capability-gated processes ----------------
    if user_elf.len() >= 64 {
        let _ = writeln!(
            con,
            "\n[proc] loading {} isolated user processes",
            NUM_PROCS
        );
        // Stash the image + kernel token so the SPAWN syscall can load more processes later.
        unsafe {
            USER_ELF = user_elf;
            KTOKEN = ktoken;
        }
        for i in 0..NUM_PROCS {
            // Each process gets its own address space (kernel shared in), stack, and caps,
            // and an initial frame entering `_start` with its id in the first-arg register.
            let ok = unsafe { load_process::<A>(i, i as u64, &mut fa, ktoken, user_elf) };
            assert!(ok, "failed to load initial process");
            unsafe { sched().add(abi::ThreadId(i)) };
            let _ = writeln!(con, "  proc {} loaded", i);
        }
        unsafe { FA = Some(fa) };
        let first = unsafe { sched().current() }.expect("a ready process").0;
        unsafe { CURRENT = first };
        let _ = writeln!(
            con,
            "[proc] starting scheduler at process {} (round-robin)\n",
            first
        );
        // Turn on preemption (a periodic timer) where the arch supports it; otherwise
        // scheduling stays cooperative (YIELD-driven). Ticks only arrive in user mode.
        A::start_preemption();
        // Hand off to the scheduler: this resumes process `first` in user mode, and the
        // trap handlers (syscall + timer) drive every switch thereafter. Never returns.
        let p = unsafe { proc_at(first) };
        unsafe { A::resume(p.token, &p.frame) };
    }

    let _ = writeln!(con, "\nrustproof: BOOT OK (no user image)");
    A::exit(true)
}

/// The scheduler-aware trap handler — the arch entry stub calls this (via the
/// `rustproof_syscall_trap` symbol the thin bin exports) with a pointer to the
/// [`UserFrame`]-shaped register save the stub just built on the kernel stack. It persists
/// the running process's state, services the syscall / `YIELD` / `EXIT`, picks the next
/// ready process, and resumes it. Never returns (it re-enters user mode via `A::resume`,
/// or halts the guest when the last process exits).
///
/// # Safety
/// `frame` must point at `A::FRAME_WORDS` valid `u64`s (the on-stack trap frame).
pub unsafe fn syscall_trap<A: Arch>(frame: *mut u64) -> ! {
    let cur = CURRENT;
    // Snapshot the running process's live user state into a local frame, so servicing the
    // syscall never holds a `&mut` to the process table across `hostcontract::dispatch`
    // (which re-borrows the same slot for the capability lookup).
    let mut f = UserFrame::ZERO;
    core::ptr::copy_nonoverlapping(frame, f.0.as_mut_ptr(), A::FRAME_WORDS);

    match A::frame_num(&f) {
        abi::sysno::YIELD => {
            // Round-robin to the next ready process (the same one if it is alone).
            CURRENT = sched().next().map(|t| t.0).unwrap_or(cur);
        }
        abi::sysno::EXIT => {
            let code = A::frame_arg(&f, 0);
            let mut con = Console::<A>::new();
            let _ = writeln!(con, "[kernel] proc {} exited with code {}", cur, code);
            // Reclaim the process's frames (page tables + stack + ELF + any DMA frames)
            // before freeing the slot, so a spawn/exit cycle does not leak an address space.
            if let Some(fa) = (*core::ptr::addr_of_mut!(FA)).as_mut() {
                use abi::FrameAllocator as _;
                let p = proc_at(cur);
                for i in 0..p.nframes {
                    fa.free_frame(p.frames[i]);
                }
                p.nframes = 0;
                for i in 0..p.nvram {
                    fa.free_frame(p.vram[i]);
                }
                p.nvram = 0;
            }
            proc_at(cur).state = ProcState::Free;
            sched().remove(abi::ThreadId(cur));
            match sched().current() {
                Some(t) => CURRENT = t.0,
                None => nothing_runnable::<A>(), // all exited (BOOT OK) or deadlocked
            }
        }
        abi::sysno::SEND => {
            // Synchronous rendezvous: `a0` = an Endpoint capability, `a1` = word. Sending
            // requires WRITE on that cap; the endpoint itself is the cap's object, so two
            // processes rendezvous only if their caps name the same endpoint.
            let word = A::frame_arg(&f, 1);
            match endpoint_of(cur, A::frame_arg(&f, 0), abi::CapRights::WRITE) {
                // No such cap, wrong type, or no WRITE right: refuse without blocking.
                None => A::frame_set_ret(&mut f, abi::syserr::NO_CAP),
                Some(ep) => match find_blocked_recv(ep) {
                    Some(r) => {
                        // A receiver is waiting: hand it the word, wake it, return OK to us.
                        // The receiver gets status + payload in SEPARATE registers (see the
                        // RECV arm) so an arbitrary word can never be read as an error.
                        A::frame_set_ret(&mut proc_at(r).frame, abi::syserr::OK);
                        A::frame_set_ret2(&mut proc_at(r).frame, word);
                        proc_at(r).state = ProcState::Ready;
                        sched().add(abi::ThreadId(r));
                        A::frame_set_ret(&mut f, abi::syserr::OK);
                        // CURRENT unchanged: the sender resumes.
                    }
                    None => {
                        // No receiver yet: block until one arrives (word rides in the state).
                        proc_at(cur).state = ProcState::BlockedSend { ep, word };
                        sched().remove(abi::ThreadId(cur));
                        match sched().current() {
                            Some(t) => CURRENT = t.0,
                            None => nothing_runnable::<A>(),
                        }
                    }
                },
            }
        }
        abi::sysno::RECV => {
            // Synchronous rendezvous: `a0` = an Endpoint capability. Receiving requires READ
            // on that cap. Status comes back in the return register and the delivered word in
            // the SECOND one: the word is an unrestricted u64 chosen by the sender, so
            // sharing one register with the `syserr` sentinels would make a legitimately
            // received `NO_CAP`-valued word indistinguishable from a refusal.
            match endpoint_of(cur, A::frame_arg(&f, 0), abi::CapRights::READ) {
                // No such cap, wrong type, or no READ right: refuse without blocking.
                None => {
                    A::frame_set_ret(&mut f, abi::syserr::NO_CAP);
                    A::frame_set_ret2(&mut f, 0); // no payload on the error path
                }
                Some(ep) => match find_blocked_send(ep) {
                    Some((s, word)) => {
                        // A sender waits: take its word, return it, wake the sender (OK).
                        A::frame_set_ret(&mut f, abi::syserr::OK);
                        A::frame_set_ret2(&mut f, word);
                        A::frame_set_ret(&mut proc_at(s).frame, abi::syserr::OK);
                        proc_at(s).state = ProcState::Ready;
                        sched().add(abi::ThreadId(s));
                        // CURRENT unchanged: the receiver resumes with the word.
                    }
                    None => {
                        // No sender yet: block until one delivers.
                        proc_at(cur).state = ProcState::BlockedRecv { ep };
                        sched().remove(abi::ThreadId(cur));
                        match sched().current() {
                            Some(t) => CURRENT = t.0,
                            None => nothing_runnable::<A>(),
                        }
                    }
                },
            }
        }
        abi::sysno::SPAWN => {
            // Creating a process is authority: require the caller to present an Untyped
            // capability (`a0` = cap id), like ALLOC_VRAM. This bounds who can spawn.
            let cap = abi::CapId(A::frame_arg(&f, 0) as usize);
            // Type AND rights, per `docs/host-contract.md`: "rights ⊇ need" on every op.
            // A spawn consumes memory out of the untyped region — a mutation — so `WRITE`.
            let authorized = proc_at(cur).caps.lookup(cap).is_some_and(|s| {
                s.cap_type == abi::CapType::Untyped && s.rights.contains(abi::CapRights::WRITE)
            });
            // Load the embedded image into a fresh process (the child's id = its slot),
            // add it to the run queue, and return its id (or u64::MAX on failure). The
            // spawner keeps running (CURRENT unchanged).
            let free = if authorized {
                (0..MAX_PROCS).find(|&i| proc_at(i).state == ProcState::Free)
            } else {
                None
            };
            let elf = *core::ptr::addr_of!(USER_ELF);
            let ktoken = *core::ptr::addr_of!(KTOKEN);
            let ret = match free {
                Some(slot) => {
                    let loaded = match (*core::ptr::addr_of_mut!(FA)).as_mut() {
                        Some(fa) => load_process::<A>(slot, slot as u64, fa, ktoken, elf),
                        None => false,
                    };
                    if loaded {
                        sched().add(abi::ThreadId(slot));
                        let mut con = Console::<A>::new();
                        let _ = writeln!(con, "[kernel] proc {} spawned proc {}", cur, slot);
                        slot as u64
                    } else {
                        u64::MAX
                    }
                }
                None => u64::MAX,
            };
            A::frame_set_ret(&mut f, ret);
        }
        num => {
            // A host-contract syscall: serviced under the current process's page tables
            // (still active — we have not switched) with its own capability space. CURRENT
            // is left unchanged, so the same process resumes with the result in `rax`/`a0`.
            let a = [
                A::frame_arg(&f, 0),
                A::frame_arg(&f, 1),
                A::frame_arg(&f, 2),
                A::frame_arg(&f, 3),
                A::frame_arg(&f, 4),
            ];
            let mut env = KEnv::<A> {
                proc_idx: cur,
                _p: PhantomData,
            };
            let ret = hostcontract::dispatch(&mut env, num, a[0], a[1], a[2], a[3], a[4]);
            A::frame_set_ret(&mut f, ret);
        }
    }

    // Persist `cur`'s (possibly result-updated) frame, then resume whoever is now current.
    proc_at(cur).frame = f;
    let next = CURRENT;
    let token = proc_at(next).token;
    A::resume(token, &proc_at(next).frame)
}

/// The timer-IRQ handler — preempts the running process and round-robins to the next ready
/// one. The arch timer stub calls this (via the `rustproof_timer_trap` symbol) with the
/// same frame layout `syscall_trap` uses, so preemption reuses the identical save/resume
/// path — the only difference is the entry point (an interrupt, not a syscall). Never
/// returns.
///
/// # Safety
/// `frame` must point at `A::FRAME_WORDS` valid `u64`s (the on-stack timer trap frame).
pub unsafe fn preempt_trap<A: Arch>(frame: *mut u64) -> ! {
    let cur = CURRENT;
    // Save the preempted process's full register state (it never cooperated).
    core::ptr::copy_nonoverlapping(frame, proc_at(cur).frame.0.as_mut_ptr(), A::FRAME_WORDS);
    A::end_of_interrupt();
    // Round-robin to the next ready process (the same one if it is alone).
    CURRENT = sched().next().map(|t| t.0).unwrap_or(cur);
    let next = CURRENT;
    let token = proc_at(next).token;
    A::resume(token, &proc_at(next).frame)
}

/// The real `HostEnv`, backed by the running process's capability space + kernel state and
/// the current `Arch`'s user-memory access.
struct KEnv<A> {
    /// Index of the process on whose behalf the syscall is serviced.
    proc_idx: usize,
    _p: PhantomData<A>,
}

impl<A: Arch> abi::HostEnv for KEnv<A> {
    fn debug_write(&mut self, bytes: &[u8]) {
        A::console_write(bytes);
    }
    fn gpu_info(&self) -> abi::GpuInfo {
        GPU_INFO
    }
    fn cap_lookup(&self, cap: abi::CapId) -> Option<(abi::CapType, abi::CapRights, u64)> {
        // SAFETY: single-CPU; no other live borrow of this process slot during dispatch.
        let caps = unsafe { &proc_at(self.proc_idx).caps };
        caps.lookup(cap).map(|s| (s.cap_type, s.rights, s.object))
    }
    fn alloc_dma(&mut self) -> Option<abi::PhysAddr> {
        // SAFETY: single-CPU, non-reentrant; FA and the process slot are disjoint statics.
        unsafe {
            // Enforce the per-process VRAM quota BEFORE allocating, so a process at quota
            // never even takes a frame from the pool.
            if proc_at(self.proc_idx).nvram >= VRAM_QUOTA_FRAMES {
                return None;
            }
            let fa = (*core::ptr::addr_of_mut!(FA)).as_mut()?;
            let p = fa.alloc_dma_frame()?;
            let proc = proc_at(self.proc_idx);
            proc.vram[proc.nvram] = p;
            proc.nvram += 1;
            Some(p)
        }
    }
    fn free_dma(&mut self, phys: u64) -> bool {
        use abi::FrameAllocator as _;
        // SAFETY: single-CPU, non-reentrant; FA and the process slot are disjoint statics.
        unsafe {
            let proc = proc_at(self.proc_idx);
            // Ownership check: only free a frame this process holds (never another's).
            let Some(i) = proc.vram[..proc.nvram]
                .iter()
                .position(|f| f.as_u64() == phys)
            else {
                return false;
            };
            let frame = proc.vram[i];
            // Swap-remove from the VRAM list (order does not matter), then return to pool.
            proc.nvram -= 1;
            proc.vram[i] = proc.vram[proc.nvram];
            if let Some(fa) = (*core::ptr::addr_of_mut!(FA)).as_mut() {
                fa.free_frame(frame);
            }
            true
        }
    }
    fn write_user_bytes(&mut self, uptr: u64, bytes: &[u8]) -> bool {
        unsafe { A::copy_to_user(uptr, bytes) }
    }
    fn read_user_bytes(&self, uptr: u64, out: &mut [u8]) -> bool {
        unsafe { A::copy_from_user(uptr, out) }
    }
}
