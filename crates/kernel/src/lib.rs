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
/// Frame-bitmap capacity, in `u64` words: one bit per 4 KiB frame, so 12288 words covers
/// 3 GiB of physical memory.
const BITMAP_WORDS: usize = 12288;
static mut BITMAP: [u64; BITMAP_WORDS] = [0; BITMAP_WORDS];
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
    /// Blocked in `SEND` on `ep`, holding the `word` (and `len` payload bytes parked in
    /// this process's `msg` buffer) until a receiver takes them.
    BlockedSend { ep: u64, word: u64, len: usize },
    /// Blocked in `RECV` on `ep`, having offered a `max`-byte buffer at user address `dst`.
    BlockedRecv { ep: u64, dst: u64, max: usize },
    /// Blocked in `WAIT_IRQ` for interrupts on `line`. Unlike the IPC blocks this one is
    /// answered by the hardware, so a run queue that drains with such a process waiting is
    /// idle, not deadlocked.
    BlockedIrq { line: u64 },
}

/// Capability slots per process.
const CAP_SLOTS: usize = 16;

/// How many address-space frames a process may hold (tracked for reclamation on exit): its
/// page tables + stack + ELF segments, ~25. VRAM frames are tracked + quota'd separately.
const MAX_PROC_FRAMES: usize = 64;

/// How many shareable memory regions can exist at once. Small and fixed, like every other
/// table here; exhaustion is reported, never silently absorbed.
/// Global region table size. Kept comfortably ABOVE `REGION_QUOTA * (owners in the demo)` so
/// that a process sitting at its per-owner quota never starves another process of the
/// mechanism — without that headroom the two limits are indistinguishable, and a child whose
/// `make_region` fails for want of a table slot reports it as lost authority.
const MAX_REGIONS: usize = 12;
/// How many regions one process may own at once — see [`abi::REGION_QUOTA`], which is where
/// the number lives so a caller can reason about a refusal.
///
/// This is the bound `VRAM_QUOTA_FRAMES` used to provide. Folding DMA memory into regions
/// would otherwise DELETE a limit: `make_region` had no per-owner cap, so one process could
/// take every entry in the global table and deny the mechanism to everyone else. Checked
/// BEFORE any frame is taken, the discipline the VRAM path had.
const REGION_QUOTA: usize = abi::REGION_QUOTA;
/// Largest region, in pages. Bounds both the frames one process can tie up and the size of
/// the per-process share window below.
const REGION_MAX_PAGES: u64 = 4;
/// How many regions one process may have mapped at once. Each slot is a fixed span of the
/// share window, so the kernel picks the address and the caller never supplies one.
const SHARE_SLOTS: usize = 4;

/// A shareable memory region: frames the kernel owns on behalf of a process, mappable into
/// any process holding a `Region` capability that names it.
///
/// The frames live HERE and in no `Process` list. `teardown_process` frees `frames` and
/// frame list unconditionally, with no refcount anywhere in the kernel, so a region frame
/// that also appeared in an owner's list would be freed while a borrower still had it
/// mapped — and then handed to somebody else.
#[derive(Clone, Copy)]
struct Region {
    /// False for a free table slot.
    live: bool,
    /// The region's identity, and the `object` of every capability naming it. Monotonic and
    /// NEVER reused, so a capability that outlives its region resolves to nothing rather
    /// than to whatever occupies this table slot next.
    id: u64,
    /// Identity (not slot) of the process that created it and may destroy it.
    owner_id: u64,
    frames: [abi::PhysAddr; REGION_MAX_PAGES as usize],
    npages: u64,
}

impl Region {
    const EMPTY: Region = Region {
        live: false,
        id: 0,
        owner_id: 0,
        frames: [abi::PhysAddr(0); REGION_MAX_PAGES as usize],
        npages: 0,
    };
}

static mut REGIONS: [Region; MAX_REGIONS] = [Region::EMPTY; MAX_REGIONS];
/// Next region identity. Monotonic; `0` is reserved to mean "no region", so a zeroed
/// `shares` slot is empty rather than a reference to region zero.
static mut NEXT_REGION_ID: u64 = 1;

/// Free frames at the moment userland started. Compared against the count at shutdown, and
/// a mismatch FAILS the boot.
///
/// The count was printed for a long time and checked by nothing, so a leak passed. Regions
/// are the first feature whose frames are owned by neither the allocator's process lists nor
/// a single process's lifetime, which makes leaking exactly a region's worth of memory an
/// easy mistake and an invisible one.
static mut FREE_AT_START: usize = 0;

/// One scheduled user process: its own address space (`token`), its last-saved user
/// register state (`frame`), and its own capability space — the isolation boundary.
struct Process {
    /// Paging-base token (`cr3`/`satp`) of this process's address space.
    token: u64,
    /// Saved user register state, resumed by `Arch::resume`.
    frame: UserFrame,
    /// Per-process authority: what this process may invoke via the host contract.
    caps: capabilities::CapSpace<CAP_SLOTS>,
    /// The role whose grant table `caps` was built from (recorded for the boot log and so
    /// the policy a slot is running under is inspectable rather than re-derived).
    role: Role,
    /// This process's identity — NOT its table slot, which is recycled. Kernel messages
    /// speak in identities so a slot's current and former occupants are never conflated.
    id: u64,
    /// Scheduling / IPC state.
    state: ProcState,
    /// Every address-space frame allocated for this process, freed back to the pool on exit
    /// so a spawn/exit cycle does not leak an address space. `frames[..nframes]` are live.
    frames: [abi::PhysAddr; MAX_PROC_FRAMES],
    nframes: usize,
    /// Device interrupts counted for this process, PER LINE, since it last collected them.
    /// Only a process holding an `Irq` capability for a line is ever credited for it, and a
    /// capability for one line must never read or clear another's — a driver holding two
    /// devices has to be able to tell them apart, and must not lose one by polling the other.
    irq_pending: [u64; MAX_IRQ_LINES],
    /// Kernel-side IPC payload buffer. Serves as the outbox while this process is blocked
    /// in `SEND`, and as the inbox for a payload delivered while it was blocked in `RECV`
    /// (its address space is not active at that moment, so the copy out is deferred).
    msg: [u8; abi::MAX_MSG_BYTES],
    /// Bytes parked in `msg` by a blocked sender.
    msg_len: usize,
    /// A payload waiting to be copied into this process's own address space the next time
    /// it is resumed: `pending_len` bytes of `msg` to user address `pending_dst`.
    pending_dst: u64,
    pending_len: usize,
    /// Regions this process currently has MAPPED, by region id, one per share-window slot
    /// (`0` = slot free). Slot index picks the virtual address, so the kernel chooses where
    /// a mapping lands and no user-supplied address ever reaches the mapping path.
    ///
    /// This is also the kernel's only reverse index from a region to the spaces that map
    /// it: destroying a region scans every process's `shares` to unmap it everywhere.
    shares: [u64; SHARE_SLOTS],
}

impl Process {
    const EMPTY: Process = Process {
        token: 0,
        frame: UserFrame::ZERO,
        caps: capabilities::CapSpace::new(),
        role: Role::Worker,
        id: 0,
        state: ProcState::Free,
        frames: [abi::PhysAddr(0); MAX_PROC_FRAMES],
        nframes: 0,
        shares: [0; SHARE_SLOTS],
        irq_pending: [0; MAX_IRQ_LINES],
        msg: [0; abi::MAX_MSG_BYTES],
        msg_len: 0,
        pending_dst: 0,
        pending_len: 0,
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

/// Zero a freshly allocated frame before anything can see its previous contents.
///
/// THE single place this kernel scrubs memory. Frames are recycled within their own pool: a
/// destroyed region's pages come back as another region, and a dead process's stack comes
/// back as another process's stack or page table. (They no longer cross between the two —
/// see the arena partition in `mm` — but that partition is an isolation boundary for DEVICE
/// reach, not a substitute for scrubbing, and it says nothing about reuse within a pool.)
/// Scrubbing on RELEASE would work equally well but is not enough on its own, because a
/// frame that has never been allocated still holds whatever the firmware left there.
///
/// One place, deliberately. The previous version zeroed at three sites — region creation,
/// region destruction, and stack mapping — and the result was that removing any one of them
/// was masked by the others, so no test could tell whether the kernel scrubbed at all.
/// Delete the `write_bytes` below and the demo's recycled-memory assertion fails.
///
/// # Safety
/// `frame` must be a live, identity-mapped physical frame owned by the caller.
#[inline]
unsafe fn zero_frame(frame: abi::PhysAddr) -> abi::PhysAddr {
    core::ptr::write_bytes(frame.as_u64() as *mut u8, 0, abi::PAGE_SIZE as usize);
    frame
}

impl abi::FrameAllocator for RecordingAlloc<'_> {
    fn alloc_frame(&mut self) -> Option<abi::PhysAddr> {
        // SAFETY: the allocator returns a live frame in identity-mapped physical memory.
        let p = unsafe { zero_frame(self.inner.alloc_frame()?) };
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

/// Monotonic process identity, handed to each `SPAWN`ed process. Identity is deliberately
/// NOT the table slot: slots are recycled by `EXIT`/`SPAWN`, and reusing an id would let a
/// new process be mistaken (by the kernel or by userland) for the one that freed the slot.
static mut NEXT_ID: u64 = NUM_PROCS as u64;

/// How many processes were killed by a CPU fault. Reported at the end so a run in which
/// something crashed is distinguishable from one where every process exited cleanly —
/// otherwise both print the same success line and a crash reads as a clean boot.
static mut KILLED: u64 = 0;

/// True while the kernel is parked in `Arch::idle` with no process running. The timer
/// handler needs it: an interrupt taken then interrupted the KERNEL, so there is no user
/// frame to save and `CURRENT` names nobody.
static mut IDLING: bool = false;
/// How many times the kernel parked waiting for an interrupt. Reported at the end, so
/// whether the idle path actually ran is observable rather than assumed.
static mut PARKS: u64 = 0;
/// How many times a DEVICE interrupt ended a park — woke a process from a machine that had
/// nothing to run. Counted separately from [`PARKS`] because the timer re-parks us without
/// waking anyone, so a nonzero park count says nothing about whether a device ever did the
/// waking. This is the only direct evidence for that property; without it the demo's
/// success line is also printed by a console byte that arrives while other processes are
/// still runnable, which ends no park at all.
static mut DEVICE_WAKES: u64 = 0;

/// Logical interrupt lines. These are the kernel's OWN numbering, not any controller's:
/// each arch maps its hardware source onto them (x86 IRQ0/IRQ4 through the 8259, riscv the
/// Sstc timer and PLIC source 10), so a capability names the same thing on both and the
/// role tables stay arch-neutral.
///
/// The periodic timer. It fires whether or not anything happened, so a process blocked on
/// it always wakes by itself.
const IRQ_TIMER: u64 = 0;
/// The console device (a UART receiving a byte). The QUIET line: it fires only when
/// something really happens, which is what a driver waiting on its hardware is doing, and
/// it is the only source that can end an idle park for a reason other than the clock.
const IRQ_CONSOLE: u64 = 1;

/// Interrupt lines the kernel accounts for separately. A capability naming a line at or
/// above this bound is honoured as authority but never credited, so it can never read or
/// clear another line's count.
const MAX_IRQ_LINES: usize = 8;

/// The lines this kernel actually DELIVERS, as a bitmask over `0..MAX_IRQ_LINES`. This is
/// the delivery half of interrupt authority: [`credit_irq`] is called for exactly these
/// lines and no others.
///
/// A capability is permission to receive a line; it is not a promise that anything ever
/// fires. `WAIT_IRQ` blocks, so the two halves have to agree — parking for a line nothing
/// credits is an unwakeable sleep, and because a parked process makes [`nothing_runnable`]
/// treat the machine as idle rather than deadlocked, ONE such process hangs the whole
/// kernel. That is the same failure the revoked-capability fix closed, arriving by a
/// different route, so rather than re-close it per mechanism this mask is the single place
/// the two halves are tied together: grants are validated against it at boot, `WAIT_IRQ`
/// refuses to park outside it, and the idle path refuses to wait on it.
///
/// Wiring a real device line means adding it here AND crediting it from the handler that
/// takes it. Granting a line without doing both fails the boot check loudly instead of
/// hanging the machine once someone waits on it.
const DELIVERED_IRQ_LINES: u64 = (1 << IRQ_TIMER) | (1 << IRQ_CONSOLE);

/// Does the kernel deliver `line` at all? See [`DELIVERED_IRQ_LINES`].
const fn delivers_irq(line: u64) -> bool {
    // The bound is load-bearing, not defensive: it keeps the shift in range.
    line < MAX_IRQ_LINES as u64 && DELIVERED_IRQ_LINES & (1 << line) != 0
}

/// Physical base of the stand-in "device" window every `Mmio` capability names. Reserved
/// at boot from the frame allocator and filled with a signature, so a process that maps it
/// through `MAP_BAR` can prove it really reached that physical memory — the same path a
/// driver process would use for a real BAR, without needing the hardware.
static mut DEVICE_PHYS: u64 = 0;
/// Pages of that window. One page for now: the frame allocator makes no contiguity
/// promise, and a real BAR is a contiguous physical range, so a larger stand-in would
/// have to reserve one deliberately.
const DEVICE_PAGES: u64 = 1;

/// How many live cross-space delegations the kernel tracks. A `SPAWN` that would delegate
/// with the ledger full is refused rather than performed untracked — an untracked
/// delegation is one that could never be revoked.
const MAX_DELEGATIONS: usize = 16;

/// The kernel's delegation ledger: who handed which capability to whom.
///
/// The graph logic lives in the `deleg` crate — pure index/identity bookkeeping with no
/// `unsafe`, no statics and no `Arch` — so its invariants are checked EXHAUSTIVELY on the
/// host over every forest it can be handed, instead of by whatever one scripted boot happens
/// to construct. This kernel's four confirmed revocation defects were all properties of that
/// graph, and none of them needed hardware to find.
///
/// What stays here is the part that is genuinely about this kernel: performing the effects a
/// revocation implies (stripping capabilities, tearing down the mappings and interrupt
/// credits they authorised, waking processes left blocked without authority).
static mut DELEGATIONS: deleg::Ledger<MAX_DELEGATIONS> = deleg::Ledger::new();

/// The ledger.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow.
#[allow(static_mut_refs)]
unsafe fn ledger<'a>() -> &'a mut deleg::Ledger<MAX_DELEGATIONS> {
    &mut *core::ptr::addr_of_mut!(DELEGATIONS)
}

/// This process, as a ledger endpoint naming capability slot `cap`.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn endpoint_at(proc: usize, cap: usize) -> deleg::Endpoint {
    deleg::Endpoint::new(proc, proc_at(proc).id, cap)
}

/// Destroy every capability derived from `root_cap` in process `root_proc`, transitively.
///
/// Walks the cross-space ledger to a fixpoint: any delegation whose source has been revoked
/// is itself revoked, which then makes ITS child a revoked source (grandchildren and deeper).
/// Inside each holder the capability is simply freed: capability spaces here are flat, and
/// the transitive part of revocation is the LEDGER's, not a per-space derivation tree. The root capability itself is untouched — a process revoking
/// its own grants does not disarm itself.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table or the ledger.
unsafe fn revoke_delegations<A: Arch>(root_proc: usize, root_cap: usize) {
    // WHAT the revocation reaches is the ledger's business, and is exhaustively tested there.
    // Doing it in one call rather than interleaved with the effects also means the reached set
    // is fixed before any of it is applied, so an effect can never change what is reached.
    let mut reached = [deleg::Endpoint::new(0, 0, 0); MAX_DELEGATIONS];
    let n = ledger().revoke_from(endpoint_at(root_proc, root_cap), &mut reached);

    // Everything below is the effect half: a capability going away must take the AUTHORITY it
    // conferred with it, not merely the slot.
    for k in 0..n {
        let d = reached[k];
        {
            // Only strip the capability while the slot still holds the same process.
            let p = proc_at(d.proc);
            if p.state != ProcState::Free && p.id == d.id {
                p.caps.revoke(abi::CapId(d.cap));
                // Revoking an endpoint capability must also end any rendezvous it is parked
                // in: the IPC matcher keys on the blocked state's endpoint, not on present
                // authority, so a process left blocked would go on sending or receiving
                // through an endpoint it no longer holds. Wake it with NO_CAP instead.
                // Revoking a capability must revoke the AUTHORITY it granted, not just the
                // slot: a device window stays mapped and usable after its capability is
                // gone unless the mapping is torn down too.
                if !holds_mmio(d.proc) {
                    unmap_device_window::<A>(d.proc);
                }
                // Same doctrine for a shared region: the mapping IS the authority the
                // capability granted, so a holder that has lost the capability must lose the
                // window too, or revocation would take the name and leave the access.
                for slot in 0..SHARE_SLOTS {
                    let id = proc_at(d.proc).shares[slot];
                    if id != 0 && !holds_region(d.proc, id, abi::CapRights::READ) {
                        unmap_region_from::<A>(d.proc, id);
                    }
                }
                // Same doctrine for interrupts: credits accrued under a capability are
                // authority, so they die with it rather than staying readable.
                for line in 0..MAX_IRQ_LINES {
                    if !holds_irq(d.proc, line as u64) {
                        proc_at(d.proc).irq_pending[line] = 0;
                    }
                }
                let p = proc_at(d.proc);
                let stranded = match p.state {
                    ProcState::BlockedSend { ep, .. } => {
                        !holds_endpoint(d.proc, ep, abi::CapRights::WRITE)
                    }
                    ProcState::BlockedRecv { ep, .. } => {
                        !holds_endpoint(d.proc, ep, abi::CapRights::READ)
                    }
                    // Same doctrine for an interrupt wait, and here it is load-bearing: a
                    // waiter that can no longer be credited is not merely stuck — it makes
                    // `nothing_runnable` treat the machine as idle forever, so an
                    // unprivileged process could hang the kernel and leak its slot.
                    ProcState::BlockedIrq { line } => !creditable(d.proc, line),
                    _ => false,
                };
                if stranded {
                    // Write exactly the registers the blocked call returns. RECV returns
                    // three (status, word, byte count) — leaving the third stale would hand
                    // the woken process a garbage length beside an error status. SEND
                    // returns only a status, and its stub declares the argument registers as
                    // INPUTS, so writing them would clobber values the caller believes are
                    // preserved across the trap.
                    let was_recv = matches!(p.state, ProcState::BlockedRecv { .. });
                    let p = proc_at(d.proc);
                    A::frame_set_ret(&mut p.frame, abi::syserr::NO_CAP);
                    if was_recv {
                        A::frame_set_ret2(&mut p.frame, 0);
                        A::frame_set_ret3(&mut p.frame, 0);
                    }
                    p.state = ProcState::Ready;
                    p.pending_len = 0;
                    sched().add(abi::ThreadId(d.proc));
                }
            }
        }
    }
}

/// Remove process `proc`'s device window mapping, if any, and refresh the translation if
/// that process's space is the active one.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn unmap_device_window<A: Arch>(proc: usize) {
    let token = proc_at(proc).token;
    let mut space = A::Space::from_token(token);
    for i in 0..DEVICE_PAGES {
        let _ = space.unmap_page(abi::VirtAddr(A::USER_MMIO_BASE + i * abi::PAGE_SIZE));
    }
    if proc == CURRENT {
        A::activate(token);
    }
}

/// Credit every process holding an `Irq` capability for `irq` with one interrupt. Only
/// capability holders are credited, so interrupt delivery is authority like everything
/// else: a process with no `Irq` capability cannot observe the device's interrupts at all.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn credit_irq<A: Arch>(irq: u64) {
    let line = irq as usize;
    if irq >= MAX_IRQ_LINES as u64 {
        return;
    }
    for i in 0..MAX_PROCS {
        if proc_at(i).state == ProcState::Free {
            continue;
        }
        let holds = {
            let caps = &proc_at(i).caps;
            (0..CAP_SLOTS).any(|c| {
                caps.lookup(abi::CapId(c)).is_some_and(|s| {
                    s.cap_type == abi::CapType::Irq
                        && s.object == irq
                        && s.rights.contains(abi::CapRights::READ)
                })
            })
        };
        if !holds {
            continue;
        }
        let p = proc_at(i);
        p.irq_pending[line] = p.irq_pending[line].saturating_add(1);
        // A process parked in WAIT_IRQ for this line is answered now.
        if p.state == (ProcState::BlockedIrq { line: irq }) {
            let n = p.irq_pending[line];
            p.irq_pending[line] = 0;
            A::frame_set_ret(&mut p.frame, n);
            p.state = ProcState::Ready;
            sched().add(abi::ThreadId(i));
        }
    }
}

/// Can a wait by `proc` on `line` ever be answered? It takes BOTH halves of interrupt
/// authority: present permission (`holds_irq` — a revoked capability stops delivery, since
/// [`credit_irq`] re-tests authority on every tick) and a line the kernel delivers at all
/// ([`delivers_irq`]). With either half missing the wait is unwakeable, and a process left
/// in that state parks the kernel forever, so this is the predicate every blocking and
/// idling decision about interrupts must use — never `holds_irq` alone.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn creditable(proc: usize, line: u64) -> bool {
    delivers_irq(line) && holds_irq(proc, line)
}

// ============================================================ shared memory regions

/// The table slot holding region `id`, if it still exists.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the region table.
unsafe fn region_slot(id: u64) -> Option<usize> {
    if id == 0 {
        return None;
    }
    (0..MAX_REGIONS).find(|&i| {
        let r = &*core::ptr::addr_of!(REGIONS[i]);
        r.live && r.id == id
    })
}

/// Does `proc` hold a `Region` capability naming `id` and carrying `needed`?
///
/// Object-keyed AND rights-aware, modelled on [`holds_endpoint`] rather than [`holds_mmio`].
/// `holds_mmio` asks only "any `Mmio` capability with `READ`", which is exact solely because
/// there is exactly one device window; with many regions the same shape would fail in both
/// directions — a holder of region A would appear to hold region B, and losing WRITE would
/// look like losing the region entirely.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn holds_region(proc: usize, id: u64, needed: abi::CapRights) -> bool {
    let caps = &proc_at(proc).caps;
    (0..CAP_SLOTS).any(|i| {
        caps.lookup(abi::CapId(i)).is_some_and(|slot| {
            slot.cap_type == abi::CapType::Region
                && slot.object == id
                && slot.rights.contains(needed)
        })
    })
}

/// The user address of share-window slot `slot`. Fixed per slot, so the kernel picks it.
fn share_va<A: Arch>(slot: usize) -> u64 {
    A::USER_SHARE_BASE + slot as u64 * REGION_MAX_PAGES * abi::PAGE_SIZE
}

/// Remove `proc`'s mapping of region `id`, if it has one. Returns true if something was
/// unmapped.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn unmap_region_from<A: Arch>(proc: usize, id: u64) -> bool {
    let mut hit = false;
    for slot in 0..SHARE_SLOTS {
        if proc_at(proc).shares[slot] != id {
            continue;
        }
        let token = proc_at(proc).token;
        let mut space = A::Space::from_token(token);
        let base = share_va::<A>(slot);
        for i in 0..REGION_MAX_PAGES {
            let _ = space.unmap_page(abi::VirtAddr(base + i * abi::PAGE_SIZE));
        }
        proc_at(proc).shares[slot] = 0;
        hit = true;
        // Only the ACTIVE space needs a paging-base reload; a space we are not running on is
        // covered by the reload `resume_process` does when it is next scheduled. Calling it
        // for a non-current process would switch us onto that process's tables mid-syscall.
        if proc == CURRENT {
            A::activate(token);
        }
    }
    hit
}

/// Drop every delegation edge naming capability slot `cap` of process `proc`, at BOTH ends.
///
/// Regions are the first feature that empties a live capability slot at runtime outside
/// `revoke_delegations`. An edge left naming a freed slot would later be walked as though it
/// still described a delegation, and the slot may by then hold something else entirely.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the ledger or process table.
unsafe fn forget_cap_edges(proc: usize, cap: usize) {
    ledger().forget(endpoint_at(proc, cap));
}

/// Worst-case teardown plan: every share slot unmapped, plus one cap sweep and one release
/// per region. Bounded, so the plan lives on the kernel stack.
const PLAN_STEPS: usize = MAX_PROCS * SHARE_SLOTS + 2 * MAX_REGIONS + SHARE_SLOTS;

/// Project the region table and every process's share slots onto the pure view the `regions`
/// crate plans over.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process or region tables.
unsafe fn region_view() -> (
    [regions::Region; MAX_REGIONS],
    [regions::Holder<SHARE_SLOTS>; MAX_PROCS],
) {
    let mut rs = [regions::Region::EMPTY; MAX_REGIONS];
    for (i, out) in rs.iter_mut().enumerate() {
        let r = &*core::ptr::addr_of!(REGIONS[i]);
        if r.live {
            *out = regions::Region::new(r.id, r.owner_id);
        }
    }
    let mut hs = [regions::Holder::<SHARE_SLOTS>::FREE; MAX_PROCS];
    for (i, out) in hs.iter_mut().enumerate() {
        let p = proc_at(i);
        if p.state != ProcState::Free {
            *out = regions::Holder::new(p.id, p.shares);
        }
    }
    (rs, hs)
}

/// Carry out a teardown plan, in order.
///
/// The order is the safety property, and it is the crate's to decide: every holder is
/// unmapped before the frames are released. Doing it the other way returns memory to the pool
/// while an address space still points at it, and this kernel hands recycled frames straight
/// back out as another process's stack.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process or region tables.
unsafe fn run_region_plan<A: Arch>(plan: &regions::Plan<PLAN_STEPS>) {
    // A plan that did not fit is a refusal, not something to half-execute: a partial teardown
    // is precisely how frames get released while a mapping survives.
    if plan.truncated {
        let mut con = Console::<A>::new();
        let _ = writeln!(
            con,
            "[region] teardown plan did not fit — refusing to half-run it"
        );
        A::exit(false);
    }
    for step in plan.steps() {
        match step {
            regions::Step::Unmap { proc, slot, .. } => unmap_share_slot::<A>(proc, slot),
            regions::Step::ForgetCaps { region } => {
                for p in 0..MAX_PROCS {
                    if proc_at(p).state == ProcState::Free {
                        continue;
                    }
                    for c in 0..CAP_SLOTS {
                        let names_it = proc_at(p).caps.lookup(abi::CapId(c)).is_some_and(|sl| {
                            sl.cap_type == abi::CapType::Region && sl.object == region
                        });
                        if names_it {
                            proc_at(p).caps.revoke(abi::CapId(c));
                            forget_cap_edges(p, c);
                        }
                    }
                }
            }
            regions::Step::Release { region } => {
                if let Some(i) = region_slot(region) {
                    let r = &mut *core::ptr::addr_of_mut!(REGIONS[i]);
                    if let Some(fa) = (*core::ptr::addr_of_mut!(FA)).as_mut() {
                        use abi::FrameAllocator as _;
                        for k in 0..r.npages as usize {
                            fa.free_frame(r.frames[k]);
                        }
                    }
                    *r = Region::EMPTY;
                }
            }
        }
    }
}

/// Drop one share-window slot's mapping.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn unmap_share_slot<A: Arch>(proc: usize, slot: usize) {
    if proc_at(proc).shares[slot] == 0 {
        return;
    }
    let token = proc_at(proc).token;
    let mut space = A::Space::from_token(token);
    let base = share_va::<A>(slot);
    for i in 0..REGION_MAX_PAGES {
        let _ = space.unmap_page(abi::VirtAddr(base + i * abi::PAGE_SIZE));
    }
    proc_at(proc).shares[slot] = 0;
    // Only the ACTIVE space needs a paging-base reload; another space is covered by the
    // reload `resume_process` performs when it is next scheduled.
    if proc == CURRENT {
        A::activate(token);
    }
}

/// Destroy the region in table slot `idx`: unmap it from every process that has it mapped,
/// drop every capability naming it, and return its frames to the pool.
///
/// Order is load-bearing. Unmapping happens FIRST, while every holder's page tables still
/// exist; the frames go back only once no space can reach them. The capability sweep is not
/// strictly required for safety — a stale capability names an id that no longer resolves —
/// but leaving one would let a process hold authority over memory that is gone, which is the
/// state the "revocation tears down the authority" doctrine exists to prevent.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process or region tables.
unsafe fn destroy_region<A: Arch>(idx: usize) {
    let id = (*core::ptr::addr_of!(REGIONS[idx])).id;
    let (rs, hs) = region_view();
    let plan: regions::Plan<PLAN_STEPS> = regions::destroy(&rs, &hs, id);
    run_region_plan::<A>(&plan);
}

unsafe fn destroy_regions_owned_by<A: Arch>(proc: usize, owner_id: u64) {
    let (rs, hs) = region_view();
    let plan: regions::Plan<PLAN_STEPS> = regions::teardown(&rs, &hs, proc, owner_id);
    run_region_plan::<A>(&plan);
}

/// Does process `proc` still hold an `Irq` capability carrying `READ` for `line`?
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn holds_irq(proc: usize, line: u64) -> bool {
    let caps = &proc_at(proc).caps;
    (0..CAP_SLOTS).any(|i| {
        caps.lookup(abi::CapId(i)).is_some_and(|s| {
            s.cap_type == abi::CapType::Irq
                && s.object == line
                && s.rights.contains(abi::CapRights::READ)
        })
    })
}

/// Does process `proc` still hold ANY `Mmio` capability carrying `READ` — i.e. any
/// remaining authority to have the device window mapped at all?
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn holds_mmio(proc: usize) -> bool {
    let caps = &proc_at(proc).caps;
    (0..CAP_SLOTS).any(|i| {
        caps.lookup(abi::CapId(i)).is_some_and(|s| {
            s.cap_type == abi::CapType::Mmio && s.rights.contains(abi::CapRights::READ)
        })
    })
}

/// Does process `proc` still hold ANY endpoint capability naming `ep` with `needed` rights?
/// Used after a revocation to decide whether a blocked process has been stranded — it may
/// legitimately hold a second capability to the same endpoint.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn holds_endpoint(proc: usize, ep: u64, needed: abi::CapRights) -> bool {
    let caps = &proc_at(proc).caps;
    (0..CAP_SLOTS).any(|i| {
        caps.lookup(abi::CapId(i)).is_some_and(|s| {
            s.cap_type == abi::CapType::Endpoint && s.object == ep && s.rights.contains(needed)
        })
    })
}

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

/// What a process is here to do. The role picks its capability set at load time, so the
/// grant policy is per-role rather than one uniform hand-out: a producer cannot receive, a
/// consumer cannot send, and neither holds any device or memory authority at all.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Sends on the shared endpoint. Send authority only.
    Producer,
    /// Receives on the shared endpoint. Receive authority only.
    Consumer,
    /// Does device + memory work (and may spawn). No authority on the shared endpoint.
    Worker,
    /// A `SPAWN`ed process. Its role grants NOTHING — everything it can do came from what
    /// its parent chose to delegate, so spawning cannot mint authority by itself.
    Child,
}

/// The BOOT policy: which role the `i`th initial process gets. Only sound for the initial
/// load, where the ids are fresh — a process's authority must never be re-derived from an
/// index that gets recycled (see the `SPAWN` arm, which passes its child's role explicitly).
fn boot_role(i: u64) -> Role {
    match i {
        0 => Role::Producer,
        1 => Role::Consumer,
        _ => Role::Worker,
    }
}

/// Human-readable role, for the boot log.
fn role_name(role: Role) -> &'static str {
    match role {
        Role::Producer => "producer",
        Role::Consumer => "consumer",
        Role::Worker => "worker",
        Role::Child => "child",
    }
}

/// A capability slot that exists but confers nothing — used to keep the ids of later grants
/// aligned across roles. It is a real lookup hit with `NONE` rights, so every gate refuses
/// it: possession is not authority.
const NO_AUTHORITY: (abi::CapType, abi::CapRights, u64) =
    (abi::CapType::Endpoint, abi::CapRights::NONE, 0);

/// The capability set granted to each role, positionally: entry `i` becomes `CapId(i)`.
///
/// PROOF(later): immediately after `load_process`, a process's `CapSpace` holds exactly this
/// role's table — entry `i` at `CapId(i)` — and nothing else. TWO kernel sites add to a
/// `CapSpace` afterwards:
///
/// 1. the `SPAWN` delegation `insert` in [`syscall_trap`], which appends a capability whose
///    rights are `parent_rights ∩ requested` and whose type and object are copied verbatim
///    from a capability the parent already holds; and
/// 2. the `MAKE_REGION` mint in [`make_region`], which is the only site that adds a
///    capability of a type NO role table contains. It is always [`abi::CapType::Region`],
///    with a fresh never-reused object, gated on an `Untyped` carrying `WRITE` that the
///    process already holds.
///
/// So the bound is (role table) ∪ (an attenuation of the parent's authority) ∪ (`Region`
/// capabilities minted from an `Untyped` the process holds) — and since [`Role::Child`]'s
/// table is empty, a spawned process's authority is bounded by its parent's plus whatever it
/// mints for itself. This block used to name only site 1, which the boot demo itself
/// falsifies: the worker mints a `Region`, a (type, object) pair in neither its role table
/// nor any attenuation of a parent's.
///
/// The third term is the one covered by no ledger — see the KNOWN GAP at [`make_region`].
/// Note that the structurally similar claim about `Irq` grants (in `run`) is UNAFFECTED and
/// still exact: `make_region` can only ever mint `Region`, never `Irq`.
fn grants_for(role: Role) -> &'static [(abi::CapType, abi::CapRights, u64)] {
    const MMIO_BASE: u64 = 0xE000_0000;
    match role {
        // Send-only on endpoint object 0. No Mmio, no Untyped: a producer that is
        // compromised cannot map a device, allocate memory, spawn, or even receive.
        Role::Producer => &[(abi::CapType::Endpoint, abi::CapRights::WRITE, 0)],
        // Receive-only on endpoint object 0, and nothing else.
        Role::Consumer => &[(abi::CapType::Endpoint, abi::CapRights::READ, 0)],
        Role::Worker => &[
            // CapId(0): no authority on the shared endpoint — a worker is not in the IPC
            // pair, and the placeholder keeps CapId(1..) aligned with the other roles.
            NO_AUTHORITY,
            // CapId(1)/CapId(2): the real device + memory authority.
            (abi::CapType::Mmio, abi::CapRights::ALL, MMIO_BASE),
            (abi::CapType::Untyped, abi::CapRights::ALL, 0),
            // CapId(3): endpoint object 1, RECEIVE-ONLY — holding an endpoint cap is not
            // permission to send on it, which the demo exercises.
            (abi::CapType::Endpoint, abi::CapRights::READ, 1),
            // CapId(4)/CapId(5): deliberately under-powered caps of the RIGHT type, so the
            // rights half of every gate is exercised on hardware rather than vacuously
            // true: an Untyped without WRITE cannot allocate or spawn, and an Mmio without
            // READ cannot map a BAR.
            (abi::CapType::Untyped, abi::CapRights::READ, 0),
            (abi::CapType::Mmio, abi::CapRights::WRITE, MMIO_BASE),
            // CapId(6): the timer interrupt line. A driver process would hold its device's.
            (abi::CapType::Irq, abi::CapRights::READ, IRQ_TIMER),
            // CapId(7): the CONSOLE line. Two lines, held separately, is what makes the
            // per-line claims testable rather than vacuous: with a single line, "a
            // capability for one line can never read or clear another's" is true only
            // because there is no other line.
            (abi::CapType::Irq, abi::CapRights::READ, IRQ_CONSOLE),
        ],
        // Nothing. A spawned process begins with no authority of its own; what it can do is
        // exactly what its parent delegated (never more — the rights are intersected), so
        // `SPAWN` cannot manufacture authority no matter who calls it.
        Role::Child => &[],
    }
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

/// Project the process table onto the pure state vector `runstate` reasons about.
///
/// Everything the scheduling decision depends on and nothing else, so the decision itself
/// can be checked exhaustively on the host instead of by whatever states one scripted boot
/// happens to reach. Both of this kernel's confirmed hangs were single points in this space.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn run_slots() -> [runstate::Slot; MAX_PROCS] {
    let mut out = [runstate::Slot::FREE; MAX_PROCS];
    for (i, slot) in out.iter_mut().enumerate() {
        let p = proc_at(i);
        if p.state == ProcState::Free {
            continue;
        }
        *slot = runstate::Slot::blocked(match p.state {
            ProcState::BlockedSend { ep, .. } => runstate::Blocked::Send(ep),
            ProcState::BlockedRecv { ep, .. } => runstate::Blocked::Recv(ep),
            ProcState::BlockedIrq { line } => runstate::Blocked::Irq(line),
            _ => runstate::Blocked::No,
        });
    }
    out
}

/// The first process blocked receiving on endpoint `ep`, if any.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn find_blocked_recv(ep: u64) -> Option<usize> {
    runstate::find_recv(&run_slots(), ep)
}

/// The first process blocked sending on endpoint `ep`, with its pending word, if any.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn find_blocked_send(ep: u64) -> Option<(usize, u64, usize)> {
    let i = runstate::find_send(&run_slots(), ep)?;
    match proc_at(i).state {
        ProcState::BlockedSend { word, len, .. } => Some((i, word, len)),
        _ => None,
    }
}

/// Called when the run queue is empty: either every process has exited (a clean finish —
/// `BOOT OK`) or the survivors are all blocked on IPC with no one to wake them (a
/// deadlock — a failure). Never returns.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn nothing_runnable<A: Arch>() -> ! {
    let mut con = Console::<A>::new();
    // A process waiting on an interrupt is not deadlocked — the hardware will answer it.
    // Park the CPU instead of declaring failure.
    // Wake any interrupt waiter that cannot be credited — no authority left, or a line
    // nothing delivers: parking for one would be parking for an event that can never
    // arrive, and the park is indistinguishable from a working machine.
    let cred = |i: usize, line: u64| creditable(i, line);
    let mut wake = [false; MAX_PROCS];
    for i in runstate::uncreditable(&run_slots(), &cred) {
        wake[i] = true;
    }
    for (i, w) in wake.iter().enumerate() {
        if *w {
            let p = proc_at(i);
            A::frame_set_ret(&mut p.frame, abi::syserr::NO_CAP);
            p.state = ProcState::Ready;
            sched().add(abi::ThreadId(i));
        }
    }
    if let Some(t) = sched().current() {
        CURRENT = t.0;
        resume_process::<A>(t.0);
    }
    // Decided over the state AFTER the wakes above, by logic checked over every 3-slot state
    // vector rather than over the handful a boot happens to produce.
    let verdict = runstate::classify(&run_slots(), &cred);
    if verdict == runstate::Next::Park {
        if !IDLING {
            PARKS = PARKS.wrapping_add(1);
        }
        IDLING = true;
        CURRENT = usize::MAX;
        A::idle();
    }
    if verdict == runstate::Next::Deadlock {
        let _ = writeln!(
            con,
            "\n[kernel] deadlock: no runnable process (survivors blocked on IPC)"
        );
        A::exit(false);
    }
    // Report the free-frame count: with per-process reclamation on exit it should be back
    // near the pre-userland count (proving spawn/exit does not leak an address space).
    if let Some(fa) = (*core::ptr::addr_of!(FA)).as_ref() {
        let now = fa.free_count();
        let start = *core::ptr::addr_of!(FREE_AT_START);
        let _ = writeln!(con, "[mm] {} frames free after all exits", now);
        // Checked, not just reported. Every frame handed out after this point belongs to
        // some process or region, and every one of those is destroyed by teardown, so the
        // pool must come back exactly. Anything else is a leak (or a double free) and must
        // fail the boot rather than scroll past in a log nobody greps.
        if start != 0 && now != start {
            let _ = writeln!(
                con,
                "[mm] LEAK: started userland with {} free, ended with {}",
                start, now
            );
            A::exit(false);
        }
    }
    let parks = *core::ptr::addr_of!(PARKS);
    let _ = writeln!(con, "[kernel] parked for an interrupt {} time(s)", parks);
    let woke = *core::ptr::addr_of!(DEVICE_WAKES);
    let _ = writeln!(con, "[kernel] a device ended the park {} time(s)", woke);
    let killed = *core::ptr::addr_of!(KILLED);
    if killed > 0 {
        let _ = writeln!(con, "\nrustproof: BOOT OK ({} process(es) killed)", killed);
    } else {
        let _ = writeln!(con, "\nrustproof: BOOT OK");
    }
    A::exit(true)
}

/// Load the user `elf` into process slot `slot`: a fresh address space (kernel shared in),
/// a mapped user stack, a fresh capability space holding exactly `role`'s grant table (see
/// [`grants_for`]), and an initial frame entering `_start` with `id_arg` in the
/// first-argument register. Sets the slot `Ready`; the caller adds it to the run queue.
/// Returns `false` (leaving the slot untouched enough to retry) if the address space or a
/// frame can't be allocated.
///
/// `role` is an explicit parameter, NOT derived from `slot` or `id_arg`: slots are recycled
/// by `EXIT`/`SPAWN`, so deriving authority from an index would let a process inherit the
/// authority of whoever previously occupied that slot.
///
/// Shared by boot (`run`) and the `SPAWN` syscall — the only difference is where `fa` comes
/// from (a `run` local vs the `FA` static) and how the role is chosen.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of `PROCS[slot]`. `ktoken` must be the
/// active kernel token so `share_kernel` and the page-table writes are reachable.
unsafe fn load_process<A: Arch>(
    slot: usize,
    id_arg: u64,
    role: Role,
    fa: &mut dyn abi::FrameAllocator,
    ktoken: u64,
    elf: &[u8],
) -> bool {
    use abi::FrameAllocator as _;
    // Refuse the role before allocating anything. The boot sweep validates the tables from
    // a hand-written list of roles; this validates whatever role is ACTUALLY loaded, so a
    // role added later cannot slip past by being missing from that list. It has to come
    // first: once the address space is built the frames are only tracked by `s.frames`,
    // which a later bail-out would never write — an early return further down leaks them.
    if grants_for(role)
        .iter()
        .any(|&(t, _, object)| t == abi::CapType::Irq && !delivers_irq(object))
    {
        return false;
    }
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
            // Grant this process its ROLE's capability set — least authority, not a uniform
            // hand-out. `insert` fills the first free slot of a fresh space, so entry `i` of
            // the table becomes `CapId(i)` and the ids line up across roles.
            s.caps = capabilities::CapSpace::new();
            for &(cap_type, rights, object) in grants_for(role) {
                // An `Mmio` grant names the device window the kernel reserved at boot; the
                // table cannot hold that address because it is only known at run time.
                let object = if cap_type == abi::CapType::Mmio {
                    *core::ptr::addr_of!(DEVICE_PHYS)
                } else {
                    object
                };
                let _ = s.caps.insert(cap_type, rights, object);
            }
            s.role = role;
            s.id = id_arg;
            s.msg_len = 0;
            s.irq_pending = [0; MAX_IRQ_LINES];
            // Reset here, not only in teardown: a process landing in a recycled slot must
            // never inherit the dead occupant's state. A stale `shares` entry would make
            // this process appear to hold a mapping it does not have, and the next
            // `destroy_region` would unmap a window it never mapped.
            s.shares = [0; SHARE_SLOTS];
            s.pending_len = 0;
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

    // Tie interrupt AUTHORITY to interrupt DELIVERY at the only place authority is minted.
    // The role tables are the sole origin of an `Irq` capability — `SPAWN` delegation copies
    // a parent's type and object verbatim and can only narrow rights — so validating them
    // here closes the invariant for every capability that can ever exist: no process can
    // hold authority for a line the kernel does not deliver, and therefore no process can
    // reach a `WAIT_IRQ` that never returns. Checked at boot rather than asserted in a
    // comment, because the cost of getting it wrong is a hung machine, not a wrong answer.
    for role in [Role::Producer, Role::Consumer, Role::Worker, Role::Child] {
        for &(cap_type, _, object) in grants_for(role) {
            if cap_type == abi::CapType::Irq && !delivers_irq(object) {
                let _ = writeln!(
                    con,
                    "\n[irq] the {} grant table hands out line {}, which no handler credits\
                     \n      (delivered mask {:#x}) — a process waiting on it would park the\
                     \n      kernel forever. Deliver the line or drop the grant.",
                    role_name(role),
                    object,
                    DELIVERED_IRQ_LINES
                );
                A::exit(false);
            }
        }
    }
    let _ = writeln!(
        con,
        "  irq delivery mask {:#x} — every granted line is credited",
        DELIVERED_IRQ_LINES
    );

    // The share window is the one mapping whose address the kernel picks, so its geometry is
    // checked rather than commented. Not because a collision would corrupt silently — both
    // arches' `map()` returns `AlreadyMapped` over a live leaf rather than overwriting it —
    // but because it would turn every MAP_REGION into an unexplained NO_MEM the moment the
    // window drifted onto the stack. The check names the cause at boot instead.
    // It deliberately does NOT check the loaded image: the image's extent is a property of
    // the ELF, not a constant, and the windows below are all far above where it links.
    {
        let share_end = A::USER_SHARE_BASE + SHARE_SLOTS as u64 * REGION_MAX_PAGES * abi::PAGE_SIZE;
        let stack_low = A::USER_STACK_TOP - A::USER_STACK_PAGES * abi::PAGE_SIZE;
        let mmio_end = A::USER_MMIO_BASE + DEVICE_PAGES * abi::PAGE_SIZE;
        let ok = A::USER_SHARE_BASE >= A::USER_BASE
            && share_end <= A::USER_LIMIT
            && (share_end <= stack_low || A::USER_SHARE_BASE >= A::USER_STACK_TOP)
            && (share_end <= A::USER_MMIO_BASE || A::USER_SHARE_BASE >= mmio_end);
        if !ok {
            let _ = writeln!(
                con,
                "\n[share] window {:#x}..{:#x} collides with the stack, the device window or \
                 the user bounds — a region mapping could overwrite a process's stack.",
                A::USER_SHARE_BASE,
                share_end
            );
            A::exit(false);
        }
        let _ = writeln!(
            con,
            "  share window {:#x}..{:#x} ({} slots x {} pages)",
            A::USER_SHARE_BASE,
            share_end,
            SHARE_SLOTS,
            REGION_MAX_PAGES
        );
    }

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
    // Bound BEFORE the slice is built, not inside the allocator. `words` comes from the
    // firmware memory map and is otherwise unbounded: `from_raw_parts_mut` past `BITMAP_WORDS`
    // is already out of bounds, and `BitmapAllocator::new` then writes `u64::MAX` over EVERY
    // word of whatever slice it was handed — straight through `.bss` — before its own
    // capacity clamp, which only limits the frame COUNT, can matter. Not reachable at the
    // memory sizes the runners use (riscv is at 10240/12288 with -m 512M), but raising `-m`
    // to 2G would corrupt the kernel silently, so it fails loudly instead.
    if words > BITMAP_WORDS {
        let _ = writeln!(
            con,
            "\n[mm] memory map needs {} bitmap words, capacity is {} — refusing to boot",
            words, BITMAP_WORDS
        );
        A::exit(false);
    }
    let bitmap: &'static mut [u64] = unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(BITMAP) as *mut u64, words)
    };
    let mut fa = mm::BitmapAllocator::new(regions, bitmap, A::reserve_below(), A::dma_top());
    let _ = writeln!(
        con,
        "[mm] dma arena {:#x}..{:#x}, general pool above it",
        A::reserve_below(),
        A::dma_top()
    );
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
        // Reserve the stand-in device window and sign it, so a process that maps it can
        // prove it reached this exact physical memory.
        {
            use abi::FrameAllocator as _;
            // SAFETY: the allocator just returned a live, identity-mapped frame.
            let first = unsafe { zero_frame(fa.alloc_frame().expect("device window")) };
            // Zeroed above before signing: a process maps this whole PAGE through MAP_BAR,
            // and only the first 16 bytes are ours. The rest would otherwise be whatever the
            // firmware left there — the same disclosure this kernel scrubs everywhere else,
            // reached through a capability that legitimately grants the mapping.
            let sig = b"RUSTPROOF-DEVICE";
            // SAFETY: the frames were just allocated and are identity-mapped kernel memory.
            unsafe {
                core::ptr::copy_nonoverlapping(sig.as_ptr(), first.as_u64() as *mut u8, sig.len());
                DEVICE_PHYS = first.as_u64();
            }
            let _ = writeln!(
                con,
                "[dev] device window reserved at {:#x} ({} pages)",
                first.as_u64(),
                DEVICE_PAGES
            );
        }
        // Stash the image + kernel token so the SPAWN syscall can load more processes later.
        unsafe {
            USER_ELF = user_elf;
            KTOKEN = ktoken;
        }
        // Baseline BEFORE any process exists: everything allocated from here on belongs to a
        // process or a region, and every one of those is destroyed by teardown, so the pool
        // must return to exactly this number. Sampled after the loop it would include the
        // boot processes' own frames and the check would be trivially wrong.
        unsafe { FREE_AT_START = fa.free_count() };
        for i in 0..NUM_PROCS {
            // Each process gets its own address space (kernel shared in), stack, and its
            // ROLE's capabilities, plus an initial frame entering `_start` with its id in
            // the first-arg register. Deriving the role from `i` is sound only here, at the
            // initial load, where the ids are fresh and never recycled.
            let role = boot_role(i as u64);
            let ok = unsafe { load_process::<A>(i, i as u64, role, &mut fa, ktoken, user_elf) };
            assert!(ok, "failed to load initial process");
            unsafe { sched().add(abi::ThreadId(i)) };
            let _ = writeln!(con, "  proc {} loaded ({})", i, role_name(role));
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
        // The console's line, the kernel's only quiet interrupt source. After preemption,
        // so the device stub is installed on a fully-initialised controller.
        A::start_console_irq();
        // Hand off to the scheduler: this resumes process `first` in user mode, and the
        // trap handlers (syscall + timer) drive every switch thereafter. Never returns.
        unsafe { resume_process::<A>(first) };
    }

    let _ = writeln!(con, "\nrustproof: BOOT OK (no user image)");
    A::exit(true)
}

/// Tear a process down: reclaim every frame it holds, splice it out of the delegation
/// ledger, free its slot and drop it from the run queue. Shared by `EXIT` and by the fault
/// path, so a process killed for faulting releases exactly what a clean exit would.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table or the ledger.
unsafe fn teardown_process<A: Arch>(idx: usize) {
    // FIRST, before anything below: destroy the regions this process OWNS. Order is
    // load-bearing in both directions. It must precede the frame loop, which frees the page
    // tables `destroy_region` has to walk to unmap every holder; and it must precede
    // `state = Free`, because the holder sweep skips free slots and this process is usually
    // one of the holders. Getting either wrong leaves a borrower mapped onto frames that go
    // back to the pool — a cross-process use-after-free.
    let dying_id = proc_at(idx).id;
    // One plan covers both halves — destroying the regions this process OWNS, and dropping
    // its own mappings of everyone else's — in an order the `regions` crate is responsible
    // for and checks over every configuration it can be handed.
    destroy_regions_owned_by::<A>(idx, dying_id);
    // Reclaim the process's frames (page tables + stack + ELF + any DMA frames) before
    // freeing the slot, so a spawn/exit cycle does not leak an address space.
    if let Some(fa) = (*core::ptr::addr_of_mut!(FA)).as_mut() {
        use abi::FrameAllocator as _;
        let p = proc_at(idx);
        for i in 0..p.nframes {
            fa.free_frame(p.frames[i]);
        }
        p.nframes = 0;
    }
    // Splice this process out of the delegation ledger. Its capability space is gone and
    // its slot is about to be reused, so every edge naming it must go — but an edge INTO it
    // is first re-parented onto its own source, or an ancestor's REVOKE would silently miss
    // the grandchildren this process delegated onward and report success while they kept
    // the capability.
    let gone_id = proc_at(idx).id;
    ledger().splice_out(idx, gone_id);
    proc_at(idx).state = ProcState::Free;
    sched().remove(abi::ThreadId(idx));

    // A process that dies must not strand a peer mid-rendezvous. The IPC matcher keys on
    // the blocked state's endpoint, so a peer blocked on an endpoint that no LIVE process
    // can still answer would wait forever — and the run would end reporting a deadlock
    // rather than the clean finish it actually is. Wake exactly those with NO_CAP.
    for i in 0..MAX_PROCS {
        let (ep, needed_by_peer) = match proc_at(i).state {
            // A blocked sender needs someone holding READ on that endpoint to take it.
            ProcState::BlockedSend { ep, .. } => (ep, abi::CapRights::READ),
            // A blocked receiver needs someone holding WRITE to deliver.
            ProcState::BlockedRecv { ep, .. } => (ep, abi::CapRights::WRITE),
            _ => continue,
        };
        let answerable = (0..MAX_PROCS).any(|j| {
            j != i && proc_at(j).state != ProcState::Free && holds_endpoint(j, ep, needed_by_peer)
        });
        if answerable {
            continue;
        }
        let was_recv = matches!(proc_at(i).state, ProcState::BlockedRecv { .. });
        let p = proc_at(i);
        A::frame_set_ret(&mut p.frame, abi::syserr::NO_CAP);
        if was_recv {
            A::frame_set_ret2(&mut p.frame, 0);
            A::frame_set_ret3(&mut p.frame, 0);
        }
        p.state = ProcState::Ready;
        p.pending_len = 0;
        sched().add(abi::ThreadId(i));
    }
}

/// A CPU fault taken while USER code was running: kill that process and carry on. Never
/// returns.
///
/// A fault in ring 3 / U-mode is the process's failure, not the machine's — letting it halt
/// the guest would hand every process a way to take the whole system down with one wild
/// pointer, which is the opposite of the isolation this kernel exists to provide. A fault
/// taken in the KERNEL is a different matter and stays fatal: the arch handler only routes
/// here when the saved privilege level says user.
///
/// # Safety
/// Called from an arch fault stub with interrupts masked; `CURRENT` must name the process
/// that was running when the fault was taken.
pub unsafe fn fault_trap<A: Arch>(what: &str, addr: u64) -> ! {
    let cur = CURRENT;
    let mut con = Console::<A>::new();
    let _ = writeln!(
        con,
        "[kernel] proc {} killed: {} at {:#x}",
        proc_at(cur).id,
        what,
        addr
    );
    KILLED = KILLED.wrapping_add(1);
    teardown_process::<A>(cur);
    match sched().current() {
        Some(t) => CURRENT = t.0,
        None => nothing_runnable::<A>(),
    }
    resume_process::<A>(CURRENT)
}

/// Resume process `next`, first delivering any IPC payload that arrived while it was
/// blocked. That copy has to happen HERE rather than at send time: the payload is written
/// by whoever was running then, whose address space was active instead of this one's. So
/// the sender leaves the bytes in the receiver's kernel buffer, and we switch to the
/// receiver's space and copy them out just before returning to it. Never returns.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of `PROCS[next]`.
unsafe fn resume_process<A: Arch>(next: usize) -> ! {
    let p = proc_at(next);
    let token = p.token;
    if p.pending_len > 0 {
        let n = p.pending_len;
        let dst = p.pending_dst;
        p.pending_len = 0;
        // The kernel's own mappings (including `msg`) are shared into every space, so the
        // buffer stays readable across the switch.
        A::activate(token);
        if !A::copy_to_user(dst, &p.msg[..n]) {
            // The receiver's buffer went bad between the RECV and now: report the failure
            // rather than resuming it with a success status and unwritten memory.
            A::frame_set_ret(&mut p.frame, abi::syserr::FAULT);
            A::frame_set_ret3(&mut p.frame, 0);
        }
    }
    A::resume(token, &proc_at(next).frame)
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
            let _ = writeln!(
                con,
                "[kernel] proc {} exited with code {}",
                proc_at(cur).id,
                code
            );
            teardown_process::<A>(cur);
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
            let uptr = A::frame_arg(&f, 2);
            let len = A::frame_arg(&f, 3) as usize;
            match endpoint_of(cur, A::frame_arg(&f, 0), abi::CapRights::WRITE) {
                // No such cap, wrong type, or no WRITE right: refuse without blocking.
                None => A::frame_set_ret(&mut f, abi::syserr::NO_CAP),
                // A payload the kernel buffer cannot hold is rejected outright rather than
                // silently truncated: the sender would have no way to learn it was cut.
                Some(_) if len > abi::MAX_MSG_BYTES => A::frame_set_ret(&mut f, abi::syserr::FAULT),
                Some(ep) => match find_blocked_recv(ep) {
                    Some(r) => {
                        // A receiver is waiting. Its address space is NOT active (ours is),
                        // so the payload is copied into the receiver's kernel buffer now and
                        // out into its user buffer when it is resumed. Truncate to the
                        // buffer it offered — the sender cannot know its size.
                        let (dst, max) = match proc_at(r).state {
                            ProcState::BlockedRecv { dst, max, .. } => (dst, max),
                            _ => (0, 0),
                        };
                        let n = len.min(max);
                        if n > 0 && !A::copy_from_user(uptr, &mut proc_at(r).msg[..n]) {
                            // Bad sender pointer: nothing is delivered and the receiver
                            // stays blocked, so no rendezvous is consumed.
                            A::frame_set_ret(&mut f, abi::syserr::FAULT);
                        } else {
                            let p = proc_at(r);
                            p.pending_dst = dst;
                            p.pending_len = n;
                            // The receiver gets status, word and byte count in SEPARATE
                            // registers so an arbitrary word can never be read as an error.
                            A::frame_set_ret(&mut p.frame, abi::syserr::OK);
                            A::frame_set_ret2(&mut p.frame, word);
                            A::frame_set_ret3(&mut p.frame, n as u64);
                            p.state = ProcState::Ready;
                            sched().add(abi::ThreadId(r));
                            A::frame_set_ret(&mut f, abi::syserr::OK);
                            // CURRENT unchanged: the sender resumes.
                        }
                    }
                    None => {
                        // No receiver yet: park the payload in our own kernel buffer (our
                        // address space is active now, and will not be when it is taken).
                        if len > 0 && !A::copy_from_user(uptr, &mut proc_at(cur).msg[..len]) {
                            A::frame_set_ret(&mut f, abi::syserr::FAULT);
                        } else {
                            proc_at(cur).msg_len = len;
                            proc_at(cur).state = ProcState::BlockedSend { ep, word, len };
                            sched().remove(abi::ThreadId(cur));
                            // Persist BEFORE a call that may not return here: the handler
                            // tail normally does this, but `nothing_runnable` can now PARK
                            // instead of exiting, and this process must later resume on the
                            // frame it blocked on rather than a stale one.
                            proc_at(cur).frame = f;
                            match sched().current() {
                                Some(t) => CURRENT = t.0,
                                None => nothing_runnable::<A>(),
                            }
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
            let dst = A::frame_arg(&f, 1);
            let max = (A::frame_arg(&f, 2) as usize).min(abi::MAX_MSG_BYTES);
            // Vet the buffer we are offering NOW, while our address space is active and
            // nothing has been consumed. Otherwise a bad buffer would only surface during
            // the deferred copy — after a sender had already been told OK, losing its
            // message with no way to report the loss to either side.
            if max > 0 && !A::user_write_ok(dst, max) {
                A::frame_set_ret(&mut f, abi::syserr::FAULT);
                A::frame_set_ret2(&mut f, 0);
                A::frame_set_ret3(&mut f, 0);
                proc_at(cur).frame = f;
                resume_process::<A>(cur)
            }
            match endpoint_of(cur, A::frame_arg(&f, 0), abi::CapRights::READ) {
                // No such cap, wrong type, or no READ right: refuse without blocking.
                None => {
                    A::frame_set_ret(&mut f, abi::syserr::NO_CAP);
                    A::frame_set_ret2(&mut f, 0); // no payload on the error path
                    A::frame_set_ret3(&mut f, 0);
                }
                Some(ep) => match find_blocked_send(ep) {
                    Some((s, word, len)) => {
                        // A sender waits. OUR address space is active, so its parked payload
                        // copies straight into our buffer — no deferral needed on this side.
                        let n = len.min(max);
                        if n > 0 && !A::copy_to_user(dst, &proc_at(s).msg[..n]) {
                            // Our own pointer is bad: fail us, leave the sender blocked.
                            A::frame_set_ret(&mut f, abi::syserr::FAULT);
                            A::frame_set_ret2(&mut f, 0);
                            A::frame_set_ret3(&mut f, 0);
                        } else {
                            A::frame_set_ret(&mut f, abi::syserr::OK);
                            A::frame_set_ret2(&mut f, word);
                            A::frame_set_ret3(&mut f, n as u64);
                            A::frame_set_ret(&mut proc_at(s).frame, abi::syserr::OK);
                            proc_at(s).msg_len = 0;
                            proc_at(s).state = ProcState::Ready;
                            sched().add(abi::ThreadId(s));
                            // CURRENT unchanged: the receiver resumes with the message.
                        }
                    }
                    None => {
                        // No sender yet: block, recording the buffer we offered.
                        proc_at(cur).state = ProcState::BlockedRecv { ep, dst, max };
                        sched().remove(abi::ThreadId(cur));
                        // See the SEND path: persist before a call that may not return.
                        proc_at(cur).frame = f;
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
            // capability (`a0` = cap id), like MAKE_REGION. This bounds who can spawn.
            let cap = abi::CapId(A::frame_arg(&f, 0) as usize);
            // Type AND rights, per `docs/host-contract.md`: "rights ⊇ need" on every op.
            // A spawn consumes memory out of the untyped region — a mutation — so `WRITE`.
            let authorized = proc_at(cur).caps.lookup(cap).is_some_and(|s| {
                s.cap_type == abi::CapType::Untyped && s.rights.contains(abi::CapRights::WRITE)
            });
            // Optional capability delegation: `a1` = one of the CALLER's capabilities to
            // hand to the child (or NO_DELEGATE), `a2` = the rights to hand over.
            // Authority-monotonic, exactly as `CapSpace::derive` is within one space: the
            // child receives `caller_rights ∩ requested`, so a parent can attenuate but
            // never amplify — asking for more than it holds yields only what it holds.
            // The lookup copies the slot out, so no borrow of `PROCS[cur]` stays live.
            let deleg_arg = A::frame_arg(&f, 1);
            let want_deleg = deleg_arg != abi::sysno::NO_DELEGATE;
            let requested = abi::CapRights((A::frame_arg(&f, 2) & 0b111) as u8);
            let delegated: Option<(abi::CapType, abi::CapRights, u64)> = if want_deleg {
                proc_at(cur)
                    .caps
                    .lookup(abi::CapId(deleg_arg as usize))
                    .map(|s| (s.cap_type, s.rights.intersect(requested), s.object))
            } else {
                None
            };
            // Asking to delegate a capability you do not hold refuses the whole spawn,
            // rather than silently producing a child without it. So does a full ledger:
            // an untracked delegation is one that could never be revoked.
            let deleg_ok =
                !want_deleg || (delegated.is_some() && ledger().len() < ledger().capacity());
            // Load the embedded image into a free slot, add it to the run queue, and return
            // the child's id (or u64::MAX on failure). The spawner keeps running (CURRENT
            // unchanged).
            let free = if authorized && deleg_ok {
                (0..MAX_PROCS).find(|&i| proc_at(i).state == ProcState::Free)
            } else {
                None
            };
            let elf = *core::ptr::addr_of!(USER_ELF);
            let ktoken = *core::ptr::addr_of!(KTOKEN);
            let ret = match free {
                Some(slot) => {
                    // A spawned process gets `Role::Child` — an EMPTY grant table — chosen
                    // HERE rather than derived from the slot. Slots are recycled, so
                    // `boot_role(slot)` would hand a child the exited occupant's authority
                    // (spawning into the freed producer's slot would grant WRITE on the
                    // shared endpoint). With an empty table, spawning mints no authority at
                    // all by construction: the child holds exactly what was delegated,
                    // which is an attenuation of what the caller already holds.
                    let child_id = {
                        let id = *core::ptr::addr_of!(NEXT_ID);
                        NEXT_ID = id.wrapping_add(1);
                        id
                    };
                    let loaded = match (*core::ptr::addr_of_mut!(FA)).as_mut() {
                        Some(fa) => load_process::<A>(slot, child_id, Role::Child, fa, ktoken, elf),
                        None => false,
                    };
                    if loaded {
                        // Hand over the (already attenuated) delegated capability. It lands
                        // in the first free slot of the child's space, i.e. immediately
                        // after its role's grants.
                        if let Some((cap_type, rights, object)) = delegated {
                            if let Some(child_cap) =
                                proc_at(slot).caps.insert(cap_type, rights, object)
                            {
                                // Record the cross-space edge so this can be revoked later.
                                // The child is a process created moments ago, which is what
                                // makes the ledger a forest — see `deleg::Ledger::splice_out`.
                                ledger().record(
                                    endpoint_at(cur, deleg_arg as usize),
                                    deleg::Endpoint::new(slot, child_id, child_cap.0),
                                );
                            }
                        }
                        sched().add(abi::ThreadId(slot));
                        let mut con = Console::<A>::new();
                        let _ = writeln!(
                            con,
                            "[kernel] proc {} spawned proc {} ({}, slot {}){}",
                            proc_at(cur).id,
                            child_id,
                            role_name(Role::Child),
                            slot,
                            if delegated.is_some() {
                                " + delegated cap"
                            } else {
                                ""
                            }
                        );
                        child_id
                    } else {
                        u64::MAX
                    }
                }
                None => u64::MAX,
            };
            A::frame_set_ret(&mut f, ret);
        }
        abi::sysno::WAIT_IRQ => {
            // Blocking sibling of POLL_IRQ: same capability, but park until the line has
            // fired at least once rather than returning zero.
            let cap = abi::CapId(A::frame_arg(&f, 0) as usize);
            let irq = proc_at(cur).caps.lookup(cap).and_then(|s| {
                (s.cap_type == abi::CapType::Irq && s.rights.contains(abi::CapRights::READ))
                    .then_some(s.object)
            });
            match irq {
                None => A::frame_set_ret(&mut f, abi::syserr::NO_CAP),
                Some(line) if delivers_irq(line) => {
                    let p = proc_at(cur);
                    let n = p.irq_pending[line as usize];
                    if n > 0 {
                        p.irq_pending[line as usize] = 0;
                        A::frame_set_ret(&mut f, n);
                    } else {
                        // Park. The frame we persist at the tail is what `credit_irq` will
                        // write the count into when the line fires.
                        p.state = ProcState::BlockedIrq { line };
                        sched().remove(abi::ThreadId(cur));
                        proc_at(cur).frame = f;
                        match sched().current() {
                            Some(t) => {
                                CURRENT = t.0;
                                resume_process::<A>(t.0)
                            }
                            None => nothing_runnable::<A>(),
                        }
                    }
                }
                // Authority for a line the kernel never delivers (out of range, or simply
                // not wired): waiting would never end, and a parked waiter would idle the
                // kernel forever, so report "nothing pending" rather than block. The boot
                // check makes this arm unreachable for a granted line — it is the second
                // gate, not the first.
                Some(_) => A::frame_set_ret(&mut f, 0),
            }
        }
        abi::sysno::MAKE_REGION => {
            // Creating a region is spending memory, so it takes the same authority
            // allocating VRAM does: an `Untyped` capability carrying WRITE.
            let cap = abi::CapId(A::frame_arg(&f, 0) as usize);
            let pages = A::frame_arg(&f, 1);
            let untyped = proc_at(cur).caps.lookup(cap).filter(|s| {
                s.cap_type == abi::CapType::Untyped && s.rights.contains(abi::CapRights::WRITE)
            });
            let ret = match untyped {
                None => abi::syserr::NO_CAP,
                Some(u) if pages >= 1 && pages <= REGION_MAX_PAGES => {
                    make_region::<A>(cur, pages, u.rights)
                }
                Some(_) => abi::syserr::NO_MEM,
            };
            A::frame_set_ret(&mut f, ret);
        }
        abi::sysno::MAP_REGION => {
            // The BORROWER maps, into its own space, at an address the kernel picks.
            let cap = abi::CapId(A::frame_arg(&f, 0) as usize);
            let named = proc_at(cur).caps.lookup(cap).filter(|s| {
                s.cap_type == abi::CapType::Region && s.rights.contains(abi::CapRights::READ)
            });
            let ret = match named {
                None => abi::syserr::NO_CAP,
                Some(slot) => map_region::<A>(cur, slot.object, slot.rights),
            };
            A::frame_set_ret(&mut f, ret);
        }
        abi::sysno::UNMAP_REGION => {
            let cap = abi::CapId(A::frame_arg(&f, 0) as usize);
            let named = proc_at(cur)
                .caps
                .lookup(cap)
                .filter(|s| s.cap_type == abi::CapType::Region);
            let ret = match named {
                Some(slot) if unmap_region_from::<A>(cur, slot.object) => abi::syserr::OK,
                _ => abi::syserr::NO_CAP,
            };
            A::frame_set_ret(&mut f, ret);
        }
        abi::sysno::FREE_REGION => {
            // Only the OWNER may destroy a region, and ownership is by process IDENTITY —
            // never by slot, which is recycled. A borrower must not be able to free memory
            // it was merely lent.
            let cap = abi::CapId(A::frame_arg(&f, 0) as usize);
            // Destroying a region takes WRITE, not merely possession: a READ-only loan must
            // not be able to destroy what it was lent, and ownership alone is not the whole
            // gate (docs/host-contract.md). The owner-by-IDENTITY check below still stands.
            let named = proc_at(cur).caps.lookup(cap).filter(|s| {
                s.cap_type == abi::CapType::Region && s.rights.contains(abi::CapRights::WRITE)
            });
            let me = proc_at(cur).id;
            let ret = match named.and_then(|s| region_slot(s.object)) {
                Some(idx) if (*core::ptr::addr_of!(REGIONS[idx])).owner_id == me => {
                    destroy_region::<A>(idx);
                    abi::syserr::OK
                }
                _ => abi::syserr::NO_CAP,
            };
            A::frame_set_ret(&mut f, ret);
        }
        abi::sysno::POLL_IRQ => {
            // Collecting a device's interrupts is authority: it requires an `Irq`
            // capability for that source, exactly like every other host-contract op.
            let cap = abi::CapId(A::frame_arg(&f, 0) as usize);
            let irq = proc_at(cur).caps.lookup(cap).and_then(|s| {
                (s.cap_type == abi::CapType::Irq && s.rights.contains(abi::CapRights::READ))
                    .then_some(s.object)
            });
            match irq {
                None => A::frame_set_ret(&mut f, abi::syserr::NO_CAP),
                // The capability names WHICH line to collect: a capability for one line
                // must never return or clear another's count.
                Some(line) if line < MAX_IRQ_LINES as u64 => {
                    let p = proc_at(cur);
                    let n = p.irq_pending[line as usize];
                    p.irq_pending[line as usize] = 0;
                    A::frame_set_ret(&mut f, n);
                }
                // Real authority, but a line the kernel does not account for: no interrupts
                // are ever credited to it, so it always reports none.
                Some(_) => A::frame_set_ret(&mut f, 0),
            }
        }
        abi::sysno::REVOKE => {
            // Revoke everything derived from one of OUR capabilities. Holding it is the
            // authority to revoke its derivations — no separate right, and no way to
            // revoke a capability you were never the source of.
            let cap = abi::CapId(A::frame_arg(&f, 0) as usize);
            if proc_at(cur).caps.lookup(cap).is_some() {
                revoke_delegations::<A>(cur, cap.0);
                let mut con = Console::<A>::new();
                let _ = writeln!(
                    con,
                    "[kernel] proc {} revoked delegations of cap {}",
                    proc_at(cur).id,
                    cap.0
                );
                A::frame_set_ret(&mut f, abi::syserr::OK);
            } else {
                A::frame_set_ret(&mut f, abi::syserr::NO_CAP);
            }
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
    resume_process::<A>(CURRENT)
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
    if IDLING {
        // This interrupt hit the kernel's idle park, not a process: there is no user state
        // to save and nobody to preempt. Deliver it, then run whoever it woke — or park
        // again if it woke nobody.
        A::end_of_interrupt();
        credit_irq::<A>(IRQ_TIMER);
        match sched().current() {
            Some(t) => {
                IDLING = false;
                CURRENT = t.0;
                resume_process::<A>(t.0)
            }
            None => A::idle(),
        }
    }
    let cur = CURRENT;
    // Save the preempted process's full register state (it never cooperated).
    core::ptr::copy_nonoverlapping(frame, proc_at(cur).frame.0.as_mut_ptr(), A::FRAME_WORDS);
    A::end_of_interrupt();
    // Deliver the interrupt to whoever holds a capability for it, then reschedule.
    credit_irq::<A>(IRQ_TIMER);
    // Round-robin to the next ready process (the same one if it is alone).
    CURRENT = sched().next().map(|t| t.0).unwrap_or(cur);
    resume_process::<A>(CURRENT)
}

/// Create a region of `pages` pages owned by process `owner`, and mint a capability for it
/// in the owner's space with `rights`. Returns the new capability's id, or a `syserr`.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process or region tables.
/// KNOWN GAP, recorded here because it is the natural place to look. `MAKE_REGION` mints a
/// `Region` capability out of an `Untyped` one in the SAME space, and that relationship is
/// NOT recorded in the delegation ledger. So revoking the `Untyped` does not destroy regions
/// already made from it: memory obtained through a capability outlives the revocation of
/// that capability. seL4 would call this retype-and-revoke and does destroy the children.
///
/// This is a real hole in the "revocation tears down the AUTHORITY it granted" doctrine, not
/// a stylistic one. It is left open deliberately rather than papered over: closing it means
/// recording a retype edge (`Untyped` -> `Region`, different type, different object) which
/// the ledger's forest precondition and `deleg::Endpoint` do not currently express, and that
/// is a design change rather than a patch.
unsafe fn make_region<A: Arch>(owner: usize, pages: u64, rights: abi::CapRights) -> u64 {
    use abi::FrameAllocator as _;
    // Quota first, before a frame is taken or an id burned — a process at its limit must not
    // be able to churn the monotonic counter or the pool on refused requests.
    let owner_id = proc_at(owner).id;
    let owned = (0..MAX_REGIONS)
        .filter(|&i| {
            let r = &*core::ptr::addr_of!(REGIONS[i]);
            r.live && r.owner_id == owner_id
        })
        .count();
    if owned >= REGION_QUOTA {
        return abi::syserr::NO_MEM;
    }
    let Some(idx) = (0..MAX_REGIONS).find(|&i| !(*core::ptr::addr_of!(REGIONS[i])).live) else {
        return abi::syserr::NO_MEM;
    };
    // An id that is never reused is the whole safety argument, so running out of ids is a
    // refusal, not a wrap. Wrapping would re-issue an id some stale capability still names.
    let Some(next) = NEXT_REGION_ID.checked_add(1) else {
        return abi::syserr::NO_MEM;
    };
    let id = NEXT_REGION_ID;
    let mut frames = [abi::PhysAddr(0); REGION_MAX_PAGES as usize];
    let mut got = 0usize;
    {
        let Some(fa) = (*core::ptr::addr_of_mut!(FA)).as_mut() else {
            return abi::syserr::NO_MEM;
        };
        while (got as u64) < pages {
            let Some(frame) = fa.alloc_dma_frame().map(|f| zero_frame(f)) else {
                // Give back what we took: a partial region must leave no trace.
                for i in 0..got {
                    fa.free_frame(frames[i]);
                }
                return abi::syserr::NO_MEM;
            };
            frames[got] = frame;
            got += 1;
        }
    }
    // Mint the capability BEFORE publishing the region, so a full capability space cannot
    // leave a region nobody can name (a leak of exactly `pages` frames).
    let Some(cap) = proc_at(owner).caps.insert(abi::CapType::Region, rights, id) else {
        if let Some(fa) = (*core::ptr::addr_of_mut!(FA)).as_mut() {
            for i in 0..got {
                fa.free_frame(frames[i]);
            }
        }
        return abi::syserr::NO_MEM;
    };
    NEXT_REGION_ID = next;
    *(&mut *core::ptr::addr_of_mut!(REGIONS[idx])) = Region {
        live: true,
        id,
        owner_id: proc_at(owner).id,
        frames,
        npages: pages,
    };
    cap.0 as u64
}

/// Map region `id` into process `proc`'s own address space, with permissions derived from
/// `rights`. Returns the chosen user address, or a `syserr`.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process or region tables.
unsafe fn map_region<A: Arch>(proc: usize, id: u64, rights: abi::CapRights) -> u64 {
    let Some(idx) = region_slot(id) else {
        // The capability outlived the region. This is exactly the case monotonic ids make
        // safe: there is nothing to resolve to, rather than somebody else's memory.
        return abi::syserr::NO_CAP;
    };
    // Re-mapping an already-mapped region reuses its slot, so permissions can be changed
    // and a caller cannot consume all four slots with one region.
    let slot = match (0..SHARE_SLOTS).find(|&i| proc_at(proc).shares[i] == id) {
        Some(existing) => existing,
        None => match (0..SHARE_SLOTS).find(|&i| proc_at(proc).shares[i] == 0) {
            Some(free) => free,
            None => return abi::syserr::NO_MEM,
        },
    };
    let (npages, frames) = {
        let r = &*core::ptr::addr_of!(REGIONS[idx]);
        (r.npages, r.frames)
    };
    // The mapping carries exactly the authority the capability does: attenuating the
    // capability on delegation has to attenuate the access it grants.
    let perms = if rights.contains(abi::CapRights::WRITE) {
        Perms::USER_RW
    } else {
        Perms::USER_RO
    };
    let base = share_va::<A>(slot);
    let token = proc_at(proc).token;
    {
        // Drop any previous mapping of this slot first. `Space::map_page` returns
        // `AlreadyMapped` over a live leaf rather than overwriting it, so without this a
        // second MAP_REGION on a region we already hold fails with NO_MEM — and the
        // permissions could never be changed, which is what re-mapping is for. `map_device`
        // unmaps first for exactly this reason.
        let mut space = A::Space::from_token(token);
        for i in 0..REGION_MAX_PAGES {
            let _ = space.unmap_page(abi::VirtAddr(base + i * abi::PAGE_SIZE));
        }
    }
    // The borrows of the process slot are confined to this block. `proc_at` hands out a
    // `&mut Process` derived from a static, so holding one across another is aliasing the
    // compiler cannot see — this kernel has already shipped that bug once.
    let mapped = {
        let p = proc_at(proc);
        let Some(fa) = (*core::ptr::addr_of_mut!(FA)).as_mut() else {
            return abi::syserr::NO_MEM;
        };
        let mut rec = RecordingAlloc {
            inner: fa,
            frames: &mut p.frames,
            n: &mut p.nframes,
        };
        let mut space = A::Space::from_token(token);
        let mut ok = true;
        for i in 0..npages {
            let va = abi::VirtAddr(base + i * abi::PAGE_SIZE);
            if !space.map_page(va, frames[i as usize], perms, &mut rec) {
                // Undo the pages we did install. `map_device` gets away without this only
                // because it maps exactly one page; a multi-page map that fails halfway
                // would leave a partial region mapped at an address the caller can read.
                for j in 0..i {
                    let _ = space.unmap_page(abi::VirtAddr(base + j * abi::PAGE_SIZE));
                }
                ok = false;
                break;
            }
        }
        ok
    };
    if !mapped {
        if proc == CURRENT {
            A::activate(token);
        }
        return abi::syserr::NO_MEM;
    }
    proc_at(proc).shares[slot] = id;
    if proc == CURRENT {
        A::activate(token);
    }
    base
}

/// The DEVICE-IRQ handler — the console's line. The timer's twin ([`preempt_trap`]), and
/// deliberately shaped like it, but crediting [`IRQ_CONSOLE`] instead: a process holding a
/// capability for the console is credited, and one holding only the timer's is not.
///
/// The interesting case is the first branch. This is the only interrupt that can end an
/// idle park for a real reason — the timer merely re-parks us — so a device waking a
/// machine that had nothing to run goes through here.
///
/// # Safety
/// `frame` must point at `A::FRAME_WORDS` valid `u64`s (the on-stack IRQ trap frame).
pub unsafe fn device_trap<A: Arch>(frame: *mut u64) -> ! {
    if IDLING {
        // The device woke the PARKED kernel: no user state to save, nobody to preempt.
        A::console_irq_ack();
        credit_irq::<A>(IRQ_CONSOLE);
        match sched().current() {
            Some(t) => {
                IDLING = false;
                DEVICE_WAKES = DEVICE_WAKES.wrapping_add(1);
                CURRENT = t.0;
                resume_process::<A>(t.0)
            }
            // Nobody holds the console line, or the holder is not blocked on it: park again
            // rather than falling through to the preemption path, which would save a user
            // frame that does not exist.
            None => A::idle(),
        }
    }
    let cur = CURRENT;
    core::ptr::copy_nonoverlapping(frame, proc_at(cur).frame.0.as_mut_ptr(), A::FRAME_WORDS);
    A::console_irq_ack();
    credit_irq::<A>(IRQ_CONSOLE);
    // A device interrupt is not a scheduling quantum: keep running the process it
    // interrupted unless it is no longer runnable, so console traffic cannot be used to
    // steal time from it. (`next()` is the timer's job.)
    if sched().contains(abi::ThreadId(cur)) {
        resume_process::<A>(cur)
    }
    CURRENT = sched().next().map(|t| t.0).unwrap_or(cur);
    resume_process::<A>(CURRENT)
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
    fn map_device(&mut self, phys: u64, pages: u64, writable: bool) -> Option<u64> {
        // SAFETY: single-CPU, non-reentrant; the caller's space is the active one.
        unsafe {
            // The KERNEL decides what may be mapped, not the caller and not the size the
            // contract layer asked for: the request must lie entirely inside the window
            // reserved at boot, or a mismatch between the two would map whatever ordinary
            // RAM happened to follow it into a user process.
            let window = *core::ptr::addr_of!(DEVICE_PHYS);
            if window == 0 || phys != window || pages > DEVICE_PAGES {
                return None;
            }
            let proc = proc_at(self.proc_idx);
            let token = proc.token;
            let base = A::USER_MMIO_BASE;
            // Page-table frames come from the allocator and are charged to this process, so
            // they are reclaimed on exit like the rest of its address space.
            let fa = (*core::ptr::addr_of_mut!(FA)).as_mut()?;
            let mut rec = RecordingAlloc {
                inner: fa,
                frames: &mut proc.frames,
                n: &mut proc.nframes,
            };
            let mut space = A::Space::from_token(token);
            // The mapping carries exactly the authority the capability did: a `READ`-only
            // capability must not produce a writable page, or attenuating a capability
            // (e.g. on delegation) would not attenuate the access it grants.
            let perms = if writable {
                Perms::USER_RW
            } else {
                Perms::USER_RO
            };
            for i in 0..DEVICE_PAGES {
                // Drop any previous window first, so a repeat call is idempotent rather
                // than failing "already mapped", and so re-mapping can change permissions.
                let _ = space.unmap_page(abi::VirtAddr(base + i * abi::PAGE_SIZE));
            }
            for i in 0..pages {
                let va = abi::VirtAddr(base + i * abi::PAGE_SIZE);
                let pa = abi::PhysAddr(phys + i * abi::PAGE_SIZE);
                if !space.map_page(va, pa, perms, &mut rec) {
                    return None;
                }
            }
            // The mapping went into the live tree we are running on; make sure the CPU sees
            // it rather than a stale translation for this window.
            A::activate(token);
            Some(base)
        }
    }

    fn unmap_device(&mut self) {
        // SAFETY: single-CPU, non-reentrant; the caller's space is the active one.
        unsafe {
            unmap_device_window::<A>(self.proc_idx);
        }
    }

    fn write_user_bytes(&mut self, uptr: u64, bytes: &[u8]) -> bool {
        unsafe { A::copy_to_user(uptr, bytes) }
    }
    fn read_user_bytes(&self, uptr: u64, out: &mut [u8]) -> bool {
        unsafe { A::copy_from_user(uptr, out) }
    }
}
