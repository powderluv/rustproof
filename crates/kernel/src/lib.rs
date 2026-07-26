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
    /// Blocked in `SEND` on `ep`, holding the `word` (and `len` payload bytes parked in
    /// this process's `msg` buffer) until a receiver takes them.
    BlockedSend { ep: u64, word: u64, len: usize },
    /// Blocked in `RECV` on `ep`, having offered a `max`-byte buffer at user address `dst`.
    BlockedRecv { ep: u64, dst: u64, max: usize },
}

/// Capability slots per process.
const CAP_SLOTS: usize = 16;

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
        role: Role::Worker,
        id: 0,
        state: ProcState::Free,
        frames: [abi::PhysAddr(0); MAX_PROC_FRAMES],
        nframes: 0,
        vram: [abi::PhysAddr(0); VRAM_QUOTA_FRAMES],
        nvram: 0,
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

/// Monotonic process identity, handed to each `SPAWN`ed process. Identity is deliberately
/// NOT the table slot: slots are recycled by `EXIT`/`SPAWN`, and reusing an id would let a
/// new process be mistaken (by the kernel or by userland) for the one that freed the slot.
static mut NEXT_ID: u64 = NUM_PROCS as u64;

/// How many processes were killed by a CPU fault. Reported at the end so a run in which
/// something crashed is distinguishable from one where every process exited cleanly —
/// otherwise both print the same success line and a crash reads as a clean boot.
static mut KILLED: u64 = 0;

/// The interrupt source the kernel currently delivers: the periodic timer. A real driver
/// would hold an `Irq` capability for its device's line instead.
const IRQ_TIMER: u64 = 0;

/// Interrupt lines the kernel accounts for separately. A capability naming a line at or
/// above this bound is honoured as authority but never credited, so it can never read or
/// clear another line's count.
const MAX_IRQ_LINES: usize = 8;

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

/// One cross-space delegation edge: the capability at `child_cap` in process `child` was
/// derived from `parent_cap` in process `parent`.
///
/// This lives in the kernel rather than in `CapSpace` on purpose: `CapSlot.parent` is a slot
/// index interpreted *within one space*, so it cannot express a cross-space edge — writing a
/// parent's index into the child's space would name an unrelated slot of the child's own
/// table and corrupt `revoke_subtree`. The two mechanisms compose: this ledger walks edges
/// between spaces, and `revoke_subtree` finishes the job inside each one.
#[derive(Clone, Copy)]
struct Delegation {
    parent: usize,
    parent_cap: usize,
    child: usize,
    child_cap: usize,
    /// Identity of the child at delegation time. Slots are recycled, so a record is only
    /// acted on while the slot still holds *that* process — otherwise a stale edge could
    /// strip a capability from an unrelated later occupant.
    child_id: u64,
    /// Identity of the parent, for exactly the same reason: BOTH endpoints of an edge are
    /// slot indices, so matching a revocation source by slot alone would let whoever later
    /// occupies that slot revoke through an edge it has no relation to.
    parent_id: u64,
    live: bool,
}

impl Delegation {
    const EMPTY: Delegation = Delegation {
        parent: 0,
        parent_cap: 0,
        child: 0,
        child_cap: 0,
        child_id: 0,
        parent_id: 0,
        live: false,
    };
}

static mut DELEGATIONS: [Delegation; MAX_DELEGATIONS] = [Delegation::EMPTY; MAX_DELEGATIONS];

/// A `&'static mut` to delegation record `i`.
///
/// # Safety
/// Single-CPU, non-reentrant: callers hold no other live borrow of `DELEGATIONS[i]`.
#[inline]
unsafe fn deleg_at<'a>(i: usize) -> &'a mut Delegation {
    &mut *(core::ptr::addr_of_mut!(DELEGATIONS) as *mut Delegation).add(i)
}

/// Index of a free delegation record, if any.
///
/// # Safety
/// Single-CPU, non-reentrant.
unsafe fn free_delegation() -> Option<usize> {
    (0..MAX_DELEGATIONS).find(|&i| !deleg_at(i).live)
}

/// Destroy every capability derived from `root_cap` in process `root_proc`, transitively.
///
/// Walks the cross-space ledger to a fixpoint: any delegation whose source has been revoked
/// is itself revoked, which then makes ITS child a revoked source (grandchildren and deeper).
/// Inside each holder, `revoke_subtree` removes the delegated capability along with anything
/// that holder derived from it. The root capability itself is untouched — a process revoking
/// its own grants does not disarm itself.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table or the ledger.
unsafe fn revoke_delegations<A: Arch>(root_proc: usize, root_cap: usize) {
    // Sources whose derivations must die, as (slot, identity, cap slot). Identity is part
    // of the key because slots are recycled: matching on the slot pair alone would let an
    // unrelated later occupant of a slot inherit revocation authority over someone else's
    // children. Bounded: at most one entry per ledger record, plus the root.
    let mut revoked = [(usize::MAX, u64::MAX, usize::MAX); MAX_DELEGATIONS + 1];
    revoked[0] = (root_proc, proc_at(root_proc).id, root_cap);
    let mut nrev = 1;

    loop {
        let mut progress = false;
        for i in 0..MAX_DELEGATIONS {
            let d = *deleg_at(i);
            if !d.live {
                continue;
            }
            if !revoked[..nrev]
                .iter()
                .any(|&(p, pid, c)| p == d.parent && pid == d.parent_id && c == d.parent_cap)
            {
                continue;
            }
            deleg_at(i).live = false;
            progress = true;
            // Only strip the capability while the slot still holds the same process.
            let p = proc_at(d.child);
            if p.state != ProcState::Free && p.id == d.child_id {
                p.caps.revoke_subtree(abi::CapId(d.child_cap));
                // Revoking an endpoint capability must also end any rendezvous it is parked
                // in: the IPC matcher keys on the blocked state's endpoint, not on present
                // authority, so a process left blocked would go on sending or receiving
                // through an endpoint it no longer holds. Wake it with NO_CAP instead.
                // Revoking a capability must revoke the AUTHORITY it granted, not just the
                // slot: a device window stays mapped and usable after its capability is
                // gone unless the mapping is torn down too.
                if !holds_mmio(d.child) {
                    unmap_device_window::<A>(d.child);
                }
                // Same doctrine for interrupts: credits accrued under a capability are
                // authority, so they die with it rather than staying readable.
                for line in 0..MAX_IRQ_LINES {
                    if !holds_irq(d.child, line as u64) {
                        proc_at(d.child).irq_pending[line] = 0;
                    }
                }
                let p = proc_at(d.child);
                let stranded = match p.state {
                    ProcState::BlockedSend { ep, .. } => {
                        !holds_endpoint(d.child, ep, abi::CapRights::WRITE)
                    }
                    ProcState::BlockedRecv { ep, .. } => {
                        !holds_endpoint(d.child, ep, abi::CapRights::READ)
                    }
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
                    let p = proc_at(d.child);
                    A::frame_set_ret(&mut p.frame, abi::syserr::NO_CAP);
                    if was_recv {
                        A::frame_set_ret2(&mut p.frame, 0);
                        A::frame_set_ret3(&mut p.frame, 0);
                    }
                    p.state = ProcState::Ready;
                    p.pending_len = 0;
                    sched().add(abi::ThreadId(d.child));
                }
            }
            if nrev < revoked.len() {
                revoked[nrev] = (d.child, d.child_id, d.child_cap);
                nrev += 1;
            }
        }
        if !progress {
            break;
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
unsafe fn credit_irq(irq: u64) {
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
        if holds {
            let p = proc_at(i);
            p.irq_pending[line] = p.irq_pending[line].saturating_add(1);
        }
    }
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
/// PROOF(later): immediately after `load_process`, a process's `CapSpace` holds exactly
/// this role's table — entry `i` at `CapId(i)` — and nothing else. Exactly ONE kernel site
/// adds to a `CapSpace` afterwards: the `SPAWN` delegation `insert` in [`syscall_trap`],
/// which appends a capability whose rights are `parent_rights ∩ requested` and whose type
/// and object are copied verbatim from a capability the parent already holds. So a
/// process's authority is bounded by (its role's table) ∪ (an attenuation of its parent's
/// authority) — and since [`Role::Child`]'s table is empty, a spawned process's authority
/// is bounded by its parent's.
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

/// The first process blocked receiving on endpoint `ep`, if any.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn find_blocked_recv(ep: u64) -> Option<usize> {
    (0..MAX_PROCS)
        .find(|&i| matches!(proc_at(i).state, ProcState::BlockedRecv { ep: e, .. } if e == ep))
}

/// The first process blocked sending on endpoint `ep`, with its pending word, if any.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn find_blocked_send(ep: u64) -> Option<(usize, u64, usize)> {
    (0..MAX_PROCS).find_map(|i| match proc_at(i).state {
        ProcState::BlockedSend { ep: e, word, len } if e == ep => Some((i, word, len)),
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
        // Reserve the stand-in device window and sign it, so a process that maps it can
        // prove it reached this exact physical memory.
        {
            use abi::FrameAllocator as _;
            let first = fa.alloc_frame().expect("device window");
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
    // Reclaim the process's frames (page tables + stack + ELF + any DMA frames) before
    // freeing the slot, so a spawn/exit cycle does not leak an address space.
    if let Some(fa) = (*core::ptr::addr_of_mut!(FA)).as_mut() {
        use abi::FrameAllocator as _;
        let p = proc_at(idx);
        for i in 0..p.nframes {
            fa.free_frame(p.frames[i]);
        }
        p.nframes = 0;
        for i in 0..p.nvram {
            fa.free_frame(p.vram[i]);
        }
        p.nvram = 0;
    }
    // Splice this process out of the delegation ledger. Its capability space is gone and
    // its slot is about to be reused, so every edge naming it must go — but an edge INTO it
    // is first re-parented onto its own source, or an ancestor's REVOKE would silently miss
    // the grandchildren this process delegated onward and report success while they kept
    // the capability.
    let gone_id = proc_at(idx).id;
    for i in 0..MAX_DELEGATIONS {
        let inc = *deleg_at(i);
        if !inc.live || inc.child != idx || inc.child_id != gone_id {
            continue;
        }
        for j in 0..MAX_DELEGATIONS {
            let out = deleg_at(j);
            if out.live
                && out.parent == idx
                && out.parent_id == gone_id
                && out.parent_cap == inc.child_cap
            {
                out.parent = inc.parent;
                out.parent_id = inc.parent_id;
                out.parent_cap = inc.parent_cap;
            }
        }
        deleg_at(i).live = false;
    }
    // Anything still rooted here derives from a capability this process held in its own
    // right, so no surviving process can revoke through it: drop those edges rather than
    // leave them naming a slot someone else is about to occupy.
    for i in 0..MAX_DELEGATIONS {
        let d = deleg_at(i);
        if d.live && d.parent == idx && d.parent_id == gone_id {
            d.live = false;
        }
    }
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
            let deleg_ok = !want_deleg || (delegated.is_some() && free_delegation().is_some());
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
                                if let Some(rec) = free_delegation() {
                                    *deleg_at(rec) = Delegation {
                                        parent: cur,
                                        parent_cap: deleg_arg as usize,
                                        child: slot,
                                        child_cap: child_cap.0,
                                        child_id,
                                        parent_id: proc_at(cur).id,
                                        live: true,
                                    };
                                }
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
    let cur = CURRENT;
    // Save the preempted process's full register state (it never cooperated).
    core::ptr::copy_nonoverlapping(frame, proc_at(cur).frame.0.as_mut_ptr(), A::FRAME_WORDS);
    A::end_of_interrupt();
    // Deliver the interrupt to whoever holds a capability for it, then reschedule.
    credit_irq(IRQ_TIMER);
    // Round-robin to the next ready process (the same one if it is alone).
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
