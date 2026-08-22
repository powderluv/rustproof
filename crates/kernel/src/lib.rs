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
#![cfg_attr(not(test), no_std)]

use core::fmt::Write as _;
use core::marker::PhantomData;
use hal::{Arch, Perms, Space, UserFrame};

#[cfg(target_arch = "x86_64")]
mod arch_x86;
// Compiled on EVERY host, not just x86, so the bound it puts on the hypervisor-supplied
// memory map can be host-tested anywhere — same reasoning as crates/sched's off-target
// `Context`. Only x86 ever calls it.
// Compiled on every host, like `pvh`, so its pure decisions (the register-base composition)
// are host-testable anywhere. Only x86 ever calls the config cycles.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
mod pci;
// Same reasoning as `pci`: compiled everywhere so its pure validation rules are host-tested.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
mod acpi;
// The firmware (multiboot) boot path's info parsing. Compiled everywhere so its pure decisions
// are host-tested; only x86 ever boots this way.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
mod multiboot;

#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
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
///
/// Sized with HEADROOM above what any role is granted, deliberately. `MAKE_REGION` mints into a
/// free slot, so a capability space that is nearly full makes CAPABILITY SPACE the limit that
/// bounds the region-quota demo instead of the per-owner quota it is testing — and that demo
/// exists precisely to distinguish which limit bound it. Adding one capability to the worker
/// role at 16 slots was enough to flip it, and the assertion caught it.
const CAP_SLOTS: usize = 20;

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

/// Every DMA frame the nucleus has handed out, as the device domain would see it.
///
/// This is the BOOKKEEPING half of an `abi::CapType::IommuDomain`. It writes no Device Table
/// Entry and pokes no register — there is no IOMMU in this tree and no device behind it, so
/// nothing is mapped here and `contained()` is trivially satisfied on the mapping side. What
/// it does buy TODAY, and what the boot asserts, is that the grant set tracks the live DMA
/// regions exactly: a `Release` that frees frames without withdrawing their authorization
/// would make the two diverge, and that divergence is precisely the stale-authorization bug
/// the hardware half would turn into a device still writing reclaimed memory.
///
/// How many device domains the nucleus keeps. Two, because the rig has two DMA-capable
/// functions and the property worth having — a capability for one device's domain cannot grant
/// reach into another's — has no second side with only one domain.
const MAX_DOMAINS: usize = 2;

/// One domain: its identity, the device it is bound to, and its OWN I/O page table.
///
/// Separate tables are what makes the isolation real rather than bookkeeping. Two devices
/// sharing a table would have identical reach no matter what the models said.
#[derive(Clone, Copy)]
struct DomainSlot {
    /// 1..N, or 0 for a slot no device claimed.
    id: u64,
    /// The BDF whose device-table entry points at this domain's table. Reported at shutdown,
    /// so which device a domain bounds is inspectable rather than inferred from the order the
    /// bus scan happened to return.
    bdf: u16,
    /// Level-1 table for IOVA 0..2 MiB, or 0 if none was built.
    l1: u64,
}

impl DomainSlot {
    const EMPTY: DomainSlot = DomainSlot {
        id: 0,
        bdf: 0,
        l1: 0,
    };
}

static mut DOMAIN_SLOTS: [DomainSlot; MAX_DOMAINS] = [DomainSlot::EMPTY; MAX_DOMAINS];

/// Sized for the worst case the kernel can reach: every region at its page limit.
static mut DEVICE_DOMAINS: [iommu::Domain<{ MAX_REGIONS * REGION_MAX_PAGES as usize }, 8>;
    MAX_DOMAINS] = [iommu::Domain::new(), iommu::Domain::new()];

/// Which slot a capability's domain object names, if any.
///
/// Pure, so the naming rule is checkable without a machine — including the case that matters
/// most: `0` is what an unclaimed slot carries AND what a zeroed capability names, and that
/// coincidence must never become authority.
fn domain_lookup(ids: &[u64; MAX_DOMAINS], named: u64) -> Option<usize> {
    ids.iter().position(|&id| id != 0 && id == named)
}

/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the domain table.
unsafe fn domain_ids() -> [u64; MAX_DOMAINS] {
    core::array::from_fn(|i| (*core::ptr::addr_of!(DOMAIN_SLOTS[i])).id)
}

/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the domains.
unsafe fn domain_at(
    i: usize,
) -> &'static mut iommu::Domain<{ MAX_REGIONS * REGION_MAX_PAGES as usize }, 8> {
    &mut (*core::ptr::addr_of_mut!(DEVICE_DOMAINS))[i]
}

/// The I/O page table of the domain a capability names.
///
/// # Safety
/// Single-CPU, non-reentrant.
unsafe fn domain_l1(named: u64) -> Option<u64> {
    let i = domain_lookup(&domain_ids(), named)?;
    match (*core::ptr::addr_of!(DOMAIN_SLOTS[i])).l1 {
        0 => None,
        l1 => Some(l1),
    }
}

/// Domain 1 — the one bound to the device this nucleus can actually drive, and therefore the
/// one every hardware oracle in the boot demo is written against.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the domain.
unsafe fn device_domain(
) -> &'static mut iommu::Domain<{ MAX_REGIONS * REGION_MAX_PAGES as usize }, 8> {
    domain_at(0)
}
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
    /// DMA mappings this process ASKED FOR, as `(region id, domain id)`; `(0, _)` is free.
    ///
    /// Mappings were anonymous: `MAP_DMA` recorded nothing about who requested one, so nothing
    /// could withdraw them per process even in principle. Teardown appeared to handle it only
    /// because a process happens to OWN the regions it maps in this demo — destroying those
    /// regions clears their entries as a side effect. Map a BORROWED region and the reach
    /// outlives the process. Attribution is what makes the teardown true by construction
    /// rather than by luck, and it is what lets `REVOKE` withdraw DMA the way it already
    /// withdraws a device window, a share window and interrupt credits.
    dma: [(u64, u64); SHARE_SLOTS],
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
        dma: [(0, 0); SHARE_SLOTS],
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

/// Physical base of the BOUNDED device's real register aperture, or 0 if none was found.
///
/// Handing a bus-mastering BAR to an untrusted process is only defensible once that device's
/// DMA is bounded, which is the precondition docs/host-contract.md §5 states and which per-device
/// domains now satisfy: the nucleus owns the I/O page tables, the process owns nothing but the
/// registers, and what the device can reach is exactly what some capability of that process
/// asked `MAP_DMA` for. Until then this was deliberately a RAM stand-in with no bus master
/// behind it.
#[cfg(target_arch = "x86_64")]
static mut DEVICE_BAR: u64 = 0;

/// DMA mappings withdrawn by process teardown, rather than by an explicit `UNMAP_DMA` or by the
/// destruction of a region. Reported at shutdown so the attribution path is shown to RUN.
#[cfg(target_arch = "x86_64")]
static mut DMA_WITHDRAWN_AT_EXIT: u64 = 0;

/// Grant-table selector meaning "the bounded device's real BAR" (see [`DEVICE_BAR`]). Any other
/// `Mmio` object selects the RAM stand-in.
const MMIO_DEVICE_BAR: u64 = 1;

/// The AMD-Vi register base IVRS named, or 0 if this machine has no IOMMU (or no ACPI).
///
/// Discovery happens before paging is enabled, and the aperture sits above the identity map,
/// so the address has to be carried from one boot phase to the other.
#[cfg(target_arch = "x86_64")]
static mut AMDVI_BASE: u64 = 0;

/// BDF of the device whose DMA will be bounded, with bit 16 as a "found" flag so that
/// 0000:00.0 is distinguishable from "no device".
#[cfg(target_arch = "x86_64")]
static mut TARGET_BDF: u32 = 0;

/// Physical base of the AMD-Vi event log, or 0 if none is armed. The log is how a REFUSED DMA
/// reports itself; with none, a blocked transfer is silent.
#[cfg(target_arch = "x86_64")]
static mut EVENT_LOG: u64 = 0;

/// Command buffer base, and our own copy of the tail offset. The unit owns the head; the tail
/// is ours to advance, and nothing else may write it.
static mut CMD_BUF: u64 = 0;
static mut CMD_TAIL: u64 = 0;

/// Where COMPLETION_WAIT deposits its word. Its own frame: the ring's 256 entries fill a page
/// exactly, so there is no spare corner of it to borrow.
static mut CMD_STORE: u64 = 0;

/// Address of the target device's device-table entry, kept so the boot can perturb it for the
/// event log's positive control. Zero until [`program_dte`] has written one.
static mut DEVICE_TABLE_ENTRY: u64 = 0;

/// Physical base of the device domain's I/O page-table root, or 0 if none is programmed.
#[cfg(target_arch = "x86_64")]
static mut IOMMU_PT_ROOT: u64 = 0;

/// The last-level I/O page table of the proof's mapping, so its leaves can be cleared when the
/// grants are withdrawn.
#[cfg(target_arch = "x86_64")]
static mut IOMMU_L1: Option<u64> = None;
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
                // Same doctrine for DMA, which was the one authority this function did not
                // cover. A mapping IS the reach the capability granted: a holder that has lost
                // the `Region` capability, or the `IommuDomain` capability it mapped through,
                // must lose the device's reach as well — otherwise revocation takes the name
                // and leaves the access, in the one place where the access is a bus master
                // writing memory directly.
                #[cfg(target_arch = "x86_64")]
                {
                    let mut any = false;
                    for slot in 0..SHARE_SLOTS {
                        let (region, domain) = proc_at(d.proc).dma[slot];
                        if region == 0 {
                            continue;
                        }
                        let keeps = holds_region(d.proc, region, abi::CapRights::READ)
                            && holds_domain(d.proc, domain, abi::CapRights::WRITE);
                        if !keeps {
                            any |= withdraw_dma_slot(d.proc, slot);
                        }
                    }
                    if any {
                        let base = *core::ptr::addr_of!(AMDVI_BASE);
                        if base != 0 {
                            invalidate_all_domains(base);
                        }
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
        // ONE copy of the Irq authority predicate. This site used to open-code a second.
        let holds = caps_hold_irq(&proc_at(i).caps, irq);
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
                    // Withdraw the device's authorization BEFORE the frames return to the
                    // pool. The other order is the stale-authorization bug: a frame reissued
                    // to someone else while the domain still names it.
                    //
                    // Both halves, hardware FIRST. Revoking only the model left a PRESENT leaf
                    // in the I/O page table pointing at a frame on its way back to the
                    // allocator — measured on the rig at IOVA 0x100000 — and every check in
                    // the tree missed it, because the model agreed with itself perfectly.
                    // Nothing in the ABI obliges a caller to UNMAP_DMA first, and a killed
                    // process cannot be relied on to have done anything, so it closes here.
                    let mut touched = false;
                    for k in 0..r.npages as usize {
                        let pfn = r.frames[k].as_u64() >> abi::PAGE_SHIFT;
                        #[cfg(target_arch = "x86_64")]
                        {
                            touched |= clear_io_mappings_of(pfn);
                        }
                        for di in 0..MAX_DOMAINS {
                            domain_at(di).revoke(pfn);
                        }
                    }
                    #[cfg(target_arch = "x86_64")]
                    if touched {
                        let base = *core::ptr::addr_of!(AMDVI_BASE);
                        if base != 0 {
                            invalidate_all_domains(base);
                        }
                    }
                    let _ = touched;
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

// ---------------------------------------------------------------- authority predicates
//
// The questions "does this hold authority for X" separated from "where the process table
// lives". Both halves were previously fused into `unsafe fn`s reading `PROCS`, which made
// them untestable off-hardware — and one of them was silently duplicated: the WAIT_IRQ
// credit path open-coded a second copy of the Irq check rather than calling it, so the two
// could drift and only one would ever be fixed.
//
// Measured, and the reason these are here: deleting the `READ` check from `holds_mmio`
// leaves the QEMU boot completely GREEN. The grant tables give the worker an `Mmio` WITHOUT
// READ precisely so that the rights half of this gate is exercised, but the revocation
// teardown the demo checks runs in the CHILD, which holds no second `Mmio` at all -- so the
// discriminating case exists in the tables and is never reached. The same is true of the
// `Region` READ gate. A comment in `grants_for` claimed both were exercised on hardware;
// they were not.

/// Does this capability space hold any `Irq` capability for `line`, carrying READ?
fn caps_hold_irq(caps: &capabilities::CapSpace<CAP_SLOTS>, line: u64) -> bool {
    (0..CAP_SLOTS).any(|i| {
        caps.lookup(abi::CapId(i)).is_some_and(|s| {
            s.cap_type == abi::CapType::Irq
                && s.object == line
                && s.rights.contains(abi::CapRights::READ)
        })
    })
}

/// Resolve one capability id to the interrupt LINE it names, enforcing type AND rights.
///
/// `WAIT_IRQ` and `POLL_IRQ` each open-coded this, which made three copies of the Irq
/// authority predicate in the crate (the delivery-credit path had the third). Collapsed to
/// one, because an authority check that exists in triplicate is two that can be fixed while
/// the other is not.
///
/// Measured: deleting the READ requirement from either caller leaves the x86 boot at
/// `RESULT: PASS`. The reason differs from the `Mmio` case and is worth distinguishing —
/// there, the discriminating capability EXISTS in the grant tables and the scenario never
/// reaches it. Here no `Irq` capability without READ is granted to anyone, so the case does
/// not exist at all. A gate can be vacuous because the fixture cannot reach the case, or
/// because the case was never built; only the first looks like a testing problem.
fn caps_irq_line(caps: &capabilities::CapSpace<CAP_SLOTS>, cap: abi::CapId) -> Option<u64> {
    let slot = caps.lookup(cap)?;
    (slot.cap_type == abi::CapType::Irq && slot.rights.contains(abi::CapRights::READ))
        .then_some(slot.object)
}

/// Does this capability space hold ANY `Mmio` capability carrying READ — i.e. any remaining
/// authority to have the device window mapped at all?
///
/// The READ requirement is the whole content: holding an `Mmio` without it is possession
/// without authority, and a mapping must not survive on the strength of one.
fn caps_hold_mmio(caps: &capabilities::CapSpace<CAP_SLOTS>) -> bool {
    (0..CAP_SLOTS).any(|i| {
        caps.lookup(abi::CapId(i)).is_some_and(|s| {
            s.cap_type == abi::CapType::Mmio && s.rights.contains(abi::CapRights::READ)
        })
    })
}

/// Does this capability space hold any endpoint capability naming `ep` with `needed`?
fn caps_hold_endpoint(
    caps: &capabilities::CapSpace<CAP_SLOTS>,
    ep: u64,
    needed: abi::CapRights,
) -> bool {
    (0..CAP_SLOTS).any(|i| {
        caps.lookup(abi::CapId(i)).is_some_and(|s| {
            s.cap_type == abi::CapType::Endpoint && s.object == ep && s.rights.contains(needed)
        })
    })
}

/// Is this capability type a MINT SOURCE — something a process can create new authority FROM?
///
/// docs/nucleus-design.md §1.2's revocation argument (revoking a mint source does NOT reclaim
/// what was already minted from it) holds only while every mint source names NO EXTENT. An
/// extent-owning mint source can be revoked while its derivations still hold that extent, and
/// the argument stops applying.
///
/// The guard used to be keyed on the TYPE `Untyped` rather than on the PROPERTY, which made it
/// silently narrow: it would still pass the day a second mint source appeared. The two mint
/// gates are `SPAWN` and `MAKE_REGION`, both `Untyped` + WRITE, and both are written in terms
/// of this predicate so that adding a third has to come here.
///
/// The known future case is `Cap<Device>` (docs/host-contract.md): extent-owning AND a mint
/// source for `Mmio`, `DmaMem` and `Irq` at once. It re-opens §1.2 by construction, which is
/// why this is a property and not a list.
const fn is_mint_source(t: abi::CapType) -> bool {
    matches!(t, abi::CapType::Untyped)
}

/// Resolve one capability id to the REGION it names, enforcing type AND rights.
///
/// Returns the region id AND the rights the holder actually carries, because the caller maps
/// with those rights: a READ-only loan that came back as writable would be amplification at
/// the point of use rather than at the point of grant.
///
/// Measured: deleting the rights check from EITHER caller — `MAP_REGION`'s READ or
/// `FREE_REGION`'s WRITE — leaves the x86 boot at `RESULT: PASS`. The second is the sharper
/// one: without it a process holding a READ-only loan can destroy memory it was merely lent,
/// and nothing on hardware notices.
fn caps_region(
    caps: &capabilities::CapSpace<CAP_SLOTS>,
    cap: abi::CapId,
    needed: abi::CapRights,
) -> Option<(u64, abi::CapRights)> {
    let slot = caps.lookup(cap)?;
    if slot.cap_type == abi::CapType::Region && slot.rights.contains(needed) {
        Some((slot.object, slot.rights))
    } else {
        None
    }
}

/// Resolve one capability id to the IOMMU DOMAIN it names, enforcing type AND rights.
///
/// `IommuDomain` was an `abi::CapType` variant with a kernel-side referent and no way to reach
/// it: the nucleus programmed the I/O page tables from its own boot path, so "the driver is an
/// untrusted process that reaches hardware only through capabilities" had no ABI behind it for
/// the one thing a driver fundamentally needs. This is the gate for that.
///
/// WRITE, not READ. Handing a device the ability to reach memory is granting authority, and a
/// capability that only permits looking at a domain must not permit extending it.
fn caps_iommu_domain(
    caps: &capabilities::CapSpace<CAP_SLOTS>,
    cap: abi::CapId,
    needed: abi::CapRights,
) -> Option<u64> {
    let slot = caps.lookup(cap)?;
    if slot.cap_type == abi::CapType::IommuDomain && slot.rights.contains(needed) {
        Some(slot.object)
    } else {
        None
    }
}

/// Resolve one capability id to the endpoint it names, enforcing type AND rights.
fn caps_endpoint_object(
    caps: &capabilities::CapSpace<CAP_SLOTS>,
    cap: u64,
    needed: abi::CapRights,
) -> Option<u64> {
    let slot = caps.lookup(abi::CapId(cap as usize))?;
    if slot.cap_type == abi::CapType::Endpoint && slot.rights.contains(needed) {
        Some(slot.object)
    } else {
        None
    }
}
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn holds_irq(proc: usize, line: u64) -> bool {
    caps_hold_irq(&proc_at(proc).caps, line)
}

/// Does process `proc` still hold an `IommuDomain` capability for `domain` with `needed`?
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn holds_domain(proc: usize, domain: u64, needed: abi::CapRights) -> bool {
    (0..CAP_SLOTS).any(|i| {
        proc_at(proc).caps.lookup(abi::CapId(i)).is_some_and(|sl| {
            sl.cap_type == abi::CapType::IommuDomain
                && sl.object == domain
                && sl.rights.contains(needed)
        })
    })
}

/// Does process `proc` still hold ANY `Mmio` capability carrying `READ` — i.e. any
/// remaining authority to have the device window mapped at all?
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn holds_mmio(proc: usize) -> bool {
    caps_hold_mmio(&proc_at(proc).caps)
}

/// Does process `proc` still hold ANY endpoint capability naming `ep` with `needed` rights?
/// Used after a revocation to decide whether a blocked process has been stranded — it may
/// legitimately hold a second capability to the same endpoint.
///
/// # Safety
/// Single-CPU, non-reentrant: no other live borrow of the process table.
unsafe fn holds_endpoint(proc: usize, ep: u64, needed: abi::CapRights) -> bool {
    caps_hold_endpoint(&proc_at(proc).caps, ep, needed)
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
/// The third term NEEDS no ledger: minting creates no cross-space authority, and a Region
/// capability IS ledgered the moment it is delegated. See [`make_region`].
/// Note that the structurally similar claim about `Irq` grants (in `run`) is UNAFFECTED and
/// still exact: `make_region` can only ever mint `Region`, never `Irq`.
fn grants_for(role: Role) -> &'static [(abi::CapType, abi::CapRights, u64)] {
    // Logical selectors for `Mmio` grants, resolved to real physical bases at mint time.
    // The stand-in is a kernel RAM frame; the other is the bounded device's real registers.
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
            // CapId(4)/CapId(5): deliberately under-powered caps of the RIGHT type — an
            // Untyped without WRITE cannot allocate or spawn, and an Mmio without READ
            // cannot map a BAR.
            //
            // This block used to claim these make "the rights half of EVERY gate" non-vacuous
            // on hardware. Measured, that was false for three of them. Deleting the rights
            // check from `holds_mmio`, `MAP_REGION`, `FREE_REGION`, `WAIT_IRQ` or `POLL_IRQ`
            // each leaves the boot at RESULT: PASS. Two distinct reasons, worth keeping apart:
            // the `Mmio` case IS represented here and the demo never reaches it from a process
            // holding both caps, whereas no under-powered `Irq` or `Region` capability is
            // granted to anyone at all, so those cases do not exist to be reached. The gates
            // are covered by host properties in this crate's `tests` module instead; what
            // these two entries actually buy is the SPAWN and MAP_BAR refusals, which the
            // demo does exercise.
            (abi::CapType::Untyped, abi::CapRights::READ, 0),
            (abi::CapType::Mmio, abi::CapRights::WRITE, MMIO_BASE),
            // CapId(6): the timer interrupt line. A driver process would hold its device's.
            (abi::CapType::Irq, abi::CapRights::READ, IRQ_TIMER),
            // CapId(7): the CONSOLE line. Two lines, held separately, is what makes the
            // per-line claims testable rather than vacuous: with a single line, "a
            // capability for one line can never read or clear another's" is true only
            // because there is no other line.
            (abi::CapType::Irq, abi::CapRights::READ, IRQ_CONSOLE),
            // CapId(8)/CapId(9): the device's DMA domain, and the same domain WITHOUT WRITE.
            // The under-powered one is here rather than merely described, because the note
            // above records what happens otherwise: three rights checks turned out to be
            // vacuous on hardware precisely because no under-powered capability of the right
            // type was granted to anyone, so the refusing branch could not be reached. Both
            // are exercised by the demo — the first maps, the second is refused.
            (abi::CapType::IommuDomain, abi::CapRights::ALL, 1),
            (abi::CapType::IommuDomain, abi::CapRights::READ, 1),
            // CapId(10): fully powered, naming a domain that does not exist. Granted rather
            // than merely described, so the object check has a reachable refusing branch —
            // without one it was decorative, and measured as such: a capability naming domain
            // 999 mapped into the real domain and the boot passed. It named 2 until a second
            // device got a domain, at which point the assertion correctly began to fail.
            (abi::CapType::IommuDomain, abi::CapRights::ALL, 3),
            // CapId(11): the OTHER device's domain, fully powered. Holding it is real
            // authority — over a different device.
            (abi::CapType::IommuDomain, abi::CapRights::ALL, 2),
            // CapId(12): the BOUNDED DEVICE'S REAL REGISTERS. Not a stand-in — mapping this
            // hands an untrusted process a live bus-mastering device. That is defensible only
            // because the same device's DMA is bounded by a domain the nucleus owns and the
            // process cannot reach: it can command transfers, and they land only where some
            // capability of its own asked `MAP_DMA` to put them. On a machine with no such
            // device the slot resolves to no authority rather than to physical zero.
            (abi::CapType::Mmio, abi::CapRights::ALL, MMIO_DEVICE_BAR),
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
    caps_endpoint_object(&proc_at(proc).caps, cap, needed)
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
    // ---- the HARDWARE table must agree with the model ----
    //
    // Every check above this one is the model talking to itself: `contained()` compares the
    // domain's mappings with its own grants, so a `Release` that withdraws both while leaving
    // a PRESENT leaf in the I/O page table looks perfectly clean — and the device goes on
    // reaching a frame that has since been reissued to someone else. That is the stale-mapping
    // hazard `crates/iommu` exists to prevent, arriving in the half the model cannot see.
    //
    // So walk the real table. Any present leaf must be covered by a live grant for the same
    // frame; anything else is the model and the hardware having diverged.
    #[cfg(target_arch = "x86_64")]
    for di in 0..MAX_DOMAINS {
        let l1 = match (*core::ptr::addr_of!(DOMAIN_SLOTS[di])).l1 {
            0 => continue,
            l1 => l1,
        };
        let dom = domain_at(di);
        let mut stale = 0u64;
        let mut first = 0u64;
        for slot in 0..512u64 {
            let pte = core::ptr::read_volatile((l1 + slot * 8) as *const u64);
            if pte & iopte::PR == 0 {
                continue;
            }
            let frame = (pte & iopte::ADDR) >> abi::PAGE_SHIFT;
            // Checked against THIS domain's grants. A leaf covered by some other domain's
            // grant is exactly the failure worth catching: reach installed in the wrong table.
            if dom.granted(frame).is_none() {
                if stale == 0 {
                    first = slot * abi::PAGE_SIZE;
                }
                stale += 1;
            }
        }
        if stale > 0 {
            let _ = writeln!(
                con,
                "\n[iommu] (bug) STALE HARDWARE MAPPING: {stale} present I/O page-table \
                 entr(ies) name a frame no grant covers, first at IOVA {first:#x} — the device \
                 can still reach memory the nucleus has reclaimed"
            );
            A::exit(false);
        }
    }

    // No grant may outlive the memory it names.
    //
    // This used to assert `grants == the page count of every live region`, which held only
    // because `MAKE_REGION` granted every page it allocated. That made the count a restatement
    // of the region table: it could not distinguish a domain holding the right NUMBER of
    // grants from one holding the right ONES, and it said nothing about whether anybody had
    // asked for them. Grants are now issued by `MAP_DMA`, so the number is a fact about what
    // was requested and no longer predictable from the region table at all.
    //
    // What replaces it is the property that actually matters: every frame the domain still
    // authorizes belongs to a region that still exists. A `Release` that returns frames to the
    // pool while the domain names them is a device authorized to write memory the nucleus has
    // reissued — and that is checkable frame by frame rather than by counting.
    for di in 0..MAX_DOMAINS {
        let slot = *core::ptr::addr_of!(DOMAIN_SLOTS[di]);
        if slot.id == 0 {
            continue;
        }
        let dom = domain_at(di);
        let live_frame = |pfn: u64| -> bool {
            (0..MAX_REGIONS).any(|i| {
                let r = &*core::ptr::addr_of!(REGIONS[i]);
                r.live
                    && (0..r.npages as usize)
                        .any(|k| r.frames[k].as_u64() >> abi::PAGE_SHIFT == pfn)
            })
        };
        let orphan = dom.grants().find(|(pfn, _)| !live_frame(*pfn));
        let _ = writeln!(
            con,
            "[iommu] domain {} ({:#06x}) holds {} grant(s) and {} mapping(s), all within live \
             regions: {}",
            slot.id,
            slot.bdf,
            dom.grant_count(),
            dom.reachable().count(),
            orphan.is_none() && dom.contained()
        );
        if let Some((pfn, _)) = orphan {
            let _ = writeln!(
                con,
                "\n[iommu] the device domain still authorizes frame {pfn:#x}, which belongs to \
                 no live region — a region was released without withdrawing its authorization, \
                 which is the stale-authorization bug."
            );
            A::exit(false);
        }
        if !dom.contained() {
            let _ = writeln!(
                con,
                "\n[iommu] the device can reach an address no grant covers (contained=false)"
            );
            A::exit(false);
        }
        // And no grant without a mapping under it. Grants are issued by `MAP_DMA` and
        // withdrawn by `UNMAP_DMA`/`FREE_REGION`, so at a quiescent point every one of them is
        // backing something. Without this the decision that allocating memory is NOT authority
        // for a device to reach it is a sentence in a comment: putting the grant back into
        // `MAKE_REGION` produces no orphan and breaks no containment, and the boot passes.
        // Measured — that mutant survived every other check here.
        if let Some((pfn, _)) = dom
            .grants()
            .find(|(pfn, _)| !dom.reachable().any(|(_, f, _)| f == *pfn))
        {
            let _ = writeln!(
                con,
                "\n[iommu] frame {pfn:#x} is authorized for DMA with nothing mapped to it — an \
                 authority nobody asked for, which is what granting at allocation looked like"
            );
            A::exit(false);
        }
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
    #[cfg(target_arch = "x86_64")]
    {
        let n = *core::ptr::addr_of!(DMA_WITHDRAWN_AT_EXIT);
        let _ = writeln!(
            con,
            "[iommu] teardown withdrew {n} DMA mapping(s) by attribution"
        );
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
                // An `Mmio` grant names a window whose address is only known at run time, so
                // the table carries a LOGICAL selector and it is resolved here — at mint time,
                // so the capability the process holds names the real physical base and every
                // path downstream (delegation, attenuation, `map_device`) is unchanged.
                //
                // Every `Mmio` grant used to resolve to the stand-in window regardless of what
                // the table said, which made the object in the table decorative.
                let mut skip = false;
                let object = if cap_type == abi::CapType::Mmio {
                    let resolved = match object {
                        // ONLY IF ITS DMA IS BOUNDED. This is docs/host-contract.md §5's
                        // precondition, enforced rather than stated: a bus-mastering BAR in an
                        // untrusted process is a DMA engine that can write the process table,
                        // the capability spaces and the ledger, at which point every other gate
                        // is advisory. A device with no domain therefore yields NO capability.
                        //
                        // Not hypothetical: QEMU's default machine carries an e1000, so the
                        // scan finds a real bus master on a boot with no IOMMU at all, and the
                        // first version of this handed it over.
                        #[cfg(target_arch = "x86_64")]
                        MMIO_DEVICE_BAR => {
                            let bounded = (*core::ptr::addr_of!(DOMAIN_SLOTS[0])).id != 0;
                            if bounded {
                                *core::ptr::addr_of!(DEVICE_BAR)
                            } else {
                                0
                            }
                        }
                        _ => *core::ptr::addr_of!(DEVICE_PHYS),
                    };
                    // A window this machine does not have is NOT authority over physical zero.
                    // The slot is still consumed so capability ids stay aligned across roles.
                    if resolved == 0 {
                        skip = true;
                    }
                    resolved
                } else {
                    object
                };
                // TRIPWIRE. Everything in docs/nucleus-design.md §1.2 — why revoking an
                // `Untyped` does not reclaim regions or processes already created from it —
                // rests on an `Untyped` naming NO extent. The day one names a range (a
                // contiguity constraint, an IOMMU window, a below-4G limit), that argument
                // stops holding and the decision must be revisited rather than inherited.
                // This is where you find out.
                //
                // This was a `debug_assert!` until 2026-08-14, which means it was compiled out
                // of every build anyone runs: both runners default to `--release`
                // (tools/run-qemu.sh:15) and `[profile.release]` sets no `debug-assertions`.
                // A tripwire that cannot trip is the thing this project calls theatre, and
                // this one guards a load-bearing design argument. It is a real assertion now,
                // and the static half of the same property is a host test
                // (`no_role_grants_an_untyped_that_names_an_extent`) so a bad TABLE fails the
                // suite without needing a boot at all.
                assert!(
                    !is_mint_source(cap_type) || object == 0,
                    "a mint source acquired an extent; revisit the revocation decision"
                );
                let (cap_type, rights, object) = if skip {
                    NO_AUTHORITY
                } else {
                    (cap_type, rights, object)
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
/// Follow an RSDT to the IVRS table and report the AMD-Vi register base it names.
///
/// Every pointer here comes from a structure the hypervisor placed, so each is bounded to the
/// identity-mapped window before it is followed: reading outside it is not merely a fault, low
/// physical memory holds device MMIO and a read there can have side effects. Tables are also
/// re-read at their DECLARED length before their checksum is believed, because a 36-byte header
/// read cannot validate a whole-table checksum and the body is the half that gets parsed.
#[cfg(target_arch = "x86_64")]
fn report_ivrs<A: Arch>(con: &mut Console<A>, rsdt_addr: u64) {
    /// # Safety
    /// Caller has bounded `[at, at+len)` to the identity-mapped window.
    unsafe fn table_at<'a>(at: u64, len: usize) -> &'a [u8] {
        core::slice::from_raw_parts(at as *const u8, len)
    }

    let reachable = |at: u64, len: u64| at != 0 && len >= 36 && at + len < pvh::IDENTITY_LIMIT;

    if !reachable(rsdt_addr, 36) {
        let _ = writeln!(
            con,
            "[acpi] RSDT at {rsdt_addr:#x} is outside the mapped window"
        );
        return;
    }
    // SAFETY: bounded above; single-CPU boot path.
    let head = unsafe { table_at(rsdt_addr, 36) };
    let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as u64;
    if !reachable(rsdt_addr, len) {
        let _ = writeln!(
            con,
            "[acpi] RSDT claims {len} bytes, which is not reachable"
        );
        return;
    }
    // SAFETY: as above, at the declared length.
    let rsdt = unsafe { table_at(rsdt_addr, len as usize) };
    if acpi::parse_table(rsdt).is_err() {
        let _ = writeln!(con, "[acpi] RSDT failed its checksum — not walking it");
        return;
    }

    let mut tables = 0usize;
    for ptr in acpi::rsdt_entries(rsdt) {
        tables += 1;
        let p = ptr as u64;
        if !reachable(p, 36) {
            continue;
        }
        // SAFETY: bounded above.
        let h = unsafe { table_at(p, 36) };
        if &h[0..4] != b"IVRS" {
            continue;
        }
        let l = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as u64;
        if !reachable(p, l) {
            let _ = writeln!(con, "[acpi] IVRS at {p:#x} claims {l} bytes, not reachable");
            return;
        }
        // SAFETY: as above, at the declared length.
        let ivrs = unsafe { table_at(p, l as usize) };
        if acpi::parse_table(ivrs).is_err() {
            let _ = writeln!(
                con,
                "[acpi] IVRS at {p:#x} failed its checksum — refusing it"
            );
            return;
        }
        match acpi::ivrs_first_base(ivrs) {
            Some((base, n)) => {
                let _ = writeln!(
                    con,
                    "[iommu] IVRS names {n} IOMMU(s); AMD-Vi register base {base:#x}"
                );
                // SAFETY: single-CPU boot path, before any process exists.
                unsafe { AMDVI_BASE = base };
                if n > 1 {
                    let _ = writeln!(
                        con,
                        "        (only the first is used; no story here for more than one)"
                    );
                }
            }
            None => {
                let _ = writeln!(con, "[acpi] IVRS at {p:#x} carries no IVHD block");
            }
        }
        return;
    }
    let _ = writeln!(
        con,
        "[acpi] {tables} table(s), no IVRS — no AMD-Vi on this machine"
    );
}

/// Program a Device Table Entry for the target device, then enable translation.
///
/// The I/O page-table root is allocated EMPTY on purpose. An empty root means no IOVA
/// translates, so every DMA the device attempts is refused — which is the strictest possible
/// containment and the only configuration whose correctness needs no map/unmap logic to be
/// right first. Loosening it is the next step, not this one.
///
/// # Safety
/// Single-CPU boot path; `base` is the mapped AMD-Vi aperture and `dt` the device table.
#[cfg(target_arch = "x86_64")]
unsafe fn program_dte<A: Arch>(
    con: &mut Console<A>,
    fa: &mut mm::BitmapAllocator,
    base: u64,
    dt: u64,
) {
    if *core::ptr::addr_of!(TARGET_BDF) & 0x1_0000 == 0 {
        let _ = writeln!(con, "[iommu] no target device — leaving the unit disabled");
        return;
    }

    use abi::FrameAllocator as _;

    // DTE word 0: V | TV | Mode (bits 11:9) | root (bits 51:12) | IR | IW.
    // Mode 3 = a three-level table. Mode 0 would mean "translation disabled", i.e. the device
    // DMAs untranslated — the opposite of the intent, and a plausible typo, so it is named.
    const MODE_3_LEVEL: u64 = 3 << 9;
    const V: u64 = 1 << 0;
    const TV: u64 = 1 << 1;
    const IR: u64 = 1 << 61;
    const IW: u64 = 1 << 62;
    const PR: u64 = 1 << 0;
    const NEXT_LEVEL_2: u64 = 2 << 9;
    const NEXT_LEVEL_1: u64 = 1 << 9;

    // ONE TABLE PER DEVICE. Two devices sharing a table would have identical reach whatever
    // their models said, so separate tables are what makes per-device containment a fact about
    // the machine rather than bookkeeping. Domain 1 is `edu` — the device this nucleus can
    // drive — so every hardware oracle in the demo is written against a domain that can answer.
    // ONE enumeration, following bridges, used for everything below: which devices to bind,
    // and which to deny. Three separate scans of bus 0 answered three questions about a flat
    // topology — and a function behind a bridge was in none of the answers, so it got no
    // device-table entry at all, which this unit reads as passthrough.
    let mut all = [pci::Function::EMPTY; 32];
    let (present, truncated) = pci::enumerate(&mut all);
    if truncated {
        let _ = writeln!(
            con,
            "\n[iommu] (bug) more PCI functions than this scan can hold — the ones it never \
             reached would be passed through, so no domain is published"
        );
        return;
    }
    let total = all[..present]
        .iter()
        .filter(|f| pci::is_dma_capable(f))
        .count();
    // How many BUSES the scan reached, not just how many functions. The read-back below can
    // only check the set it wrote, so it is blind to whatever the scan never reached — and
    // "every function is bounded" is exactly the claim a truncated scan makes vacuously true.
    // The bus count is the one number that moves when the walk stops early.
    let mut buses_seen = 0usize;
    for i in 0..present {
        if !all[..i].iter().any(|g| g.bus == all[i].bus) {
            buses_seen += 1;
        }
    }
    let _ = writeln!(
        con,
        "[iommu] {present} PCI function(s) across {buses_seen} bus(es), {total} DMA-capable; \
         room to bound {}",
        MAX_DOMAINS
    );
    // `edu` first, so the device every hardware oracle is written against is domain 1 whatever
    // order the bus scan returns.
    let mut devs = [pci::Function::EMPTY; MAX_DOMAINS];
    let mut found = 0usize;
    for f in all[..present].iter().filter(|f| pci::is_edu(f)) {
        if found < devs.len() {
            devs[found] = *f;
            found += 1;
        }
    }
    for f in all[..present]
        .iter()
        .filter(|f| pci::is_dma_capable(f) && !pci::is_edu(f))
    {
        if found < devs.len() {
            devs[found] = *f;
            found += 1;
        }
    }
    let mut live = 0usize;
    let mut dev0_w0 = 0u64;
    // STAGED, not published. A domain slot is what `MAP_DMA` and the real-BAR capability both
    // consult to decide whether a device's DMA is bounded — but until the unit is ENABLED
    // nothing translates, and the enable can fail: its branch prints a line and the boot
    // carries on. Publishing here handed ring 3 a live bus master and IOVAs from a unit that
    // was passing everything through. A proxy for a property, published before the property
    // held; the two are committed together below.
    let mut staged = [DomainSlot::EMPTY; MAX_DOMAINS];
    for (i, f) in devs[..found].iter().enumerate() {
        let bdf = f.bdf() as u64;
        let Some(root) = fa.alloc_frame().map(|f| zero_frame(f)) else {
            let _ = writeln!(con, "[iommu] no frame for an I/O page-table root");
            break;
        };
        let w0 = V | TV | MODE_3_LEVEL | (root.as_u64() & 0x000F_FFFF_FFFF_F000) | IR | IW;

        // Both lower levels are built here, EMPTY: an entry per level covering IOVA 0..2 MiB
        // with no leaf under it, so what can be reached is unchanged — every address still
        // resolves to a not-present leaf until something writes one. They exist up front
        // because a syscall that maps DMA must write into a table that is already there.
        let (Some(l2), Some(l1)) = (
            fa.alloc_frame().map(|f| zero_frame(f)),
            fa.alloc_frame().map(|f| zero_frame(f)),
        ) else {
            let _ = writeln!(con, "[iommu] no frames for an I/O page table");
            break;
        };
        core::ptr::write_volatile(
            root.as_u64() as *mut u64,
            (l2.as_u64() & 0x000F_FFFF_FFFF_F000) | PR | IR | IW | NEXT_LEVEL_2,
        );
        core::ptr::write_volatile(
            l2.as_u64() as *mut u64,
            (l1.as_u64() & 0x000F_FFFF_FFFF_F000) | PR | IR | IW | NEXT_LEVEL_1,
        );

        let dte = (dt + bdf * 32) as *mut u64;
        core::ptr::write_volatile(dte, w0);
        // Word 1 carries the DomainID. Distinct per device, so the unit's own invalidation and
        // caching treat them as separate domains rather than as one.
        core::ptr::write_volatile(dte.add(1), i as u64);
        core::ptr::write_volatile(dte.add(2), 0);
        core::ptr::write_volatile(dte.add(3), 0);
        let back = core::ptr::read_volatile(dte);
        if back != w0 {
            let _ = writeln!(
                con,
                "[iommu] DTE write did NOT take for {bdf:#06x}: {back:#018x} != {w0:#018x}"
            );
            continue;
        }

        // The domain exists only now that there is a device table entry AND a table under it.
        // Claiming it earlier would let `MAP_DMA` accept a capability for a domain that could
        // not yet hold a mapping.
        staged[i] = DomainSlot {
            id: (i + 1) as u64,
            bdf: f.bdf(),
            l1: l1.as_u64(),
        };
        live += 1;
        if i == 0 {
            IOMMU_PT_ROOT = root.as_u64();
            IOMMU_L1 = Some(l1.as_u64());
            DEVICE_TABLE_ENTRY = dte as u64;
            dev0_w0 = w0;
        }
    }
    if live == 0 {
        let _ = writeln!(
            con,
            "[iommu] no domain could be built — leaving the unit disabled"
        );
        return;
    }

    // ---- DEFAULT DENY for everything else on the bus ----
    //
    // An entry with V = 0 is PASSTHROUGH, not deny. So enabling the unit while programming
    // entries only for the devices we have domains for leaves every other function with
    // UNRESTRICTED access to memory — and the machine really does have more than we can bound:
    // the rig reports three DMA-capable functions and there is room for two. The nucleus was
    // claiming to bound DMA while a bus master sat outside the claim.
    //
    // Every remaining function therefore gets a VALID entry pointing at an EMPTY table: the
    // walk reaches a not-present level and the transfer is refused. Reaching nothing is the
    // right default for a device nobody has asked to use.
    if let Some(deny) = fa.alloc_frame().map(|f| zero_frame(f)) {
        let deny_w0 = V | TV | MODE_3_LEVEL | (deny.as_u64() & 0x000F_FFFF_FFFF_F000) | IR | IW;
        let mut denied = 0usize;
        for f in all[..present].iter() {
            let bdf = f.bdf();
            if staged.iter().any(|sl| sl.id != 0 && sl.bdf == bdf) {
                continue;
            }
            let dte = (dt + bdf as u64 * 32) as *mut u64;
            core::ptr::write_volatile(dte, deny_w0);
            // A DomainID of its own, so a flush for a bounded device never speaks for it.
            core::ptr::write_volatile(dte.add(1), MAX_DOMAINS as u64);
            core::ptr::write_volatile(dte.add(2), 0);
            core::ptr::write_volatile(dte.add(3), 0);
            denied += 1;
        }
        // Read every present function's entry back. A store that did not take leaves V = 0,
        // which is the passthrough this whole block exists to remove — and a count of writes
        // says nothing about whether any of them landed.
        let mut passthrough = 0usize;
        for f in all[..present].iter() {
            let w0 = core::ptr::read_volatile((dt + f.bdf() as u64 * 32) as *const u64);
            if w0 & (V | TV) != V | TV {
                passthrough += 1;
            }
        }
        let _ = writeln!(
            con,
            "[iommu] {denied} other function(s) given an EMPTY table; {present} present, \
             {passthrough} still passed through"
        );
        if passthrough > 0 {
            let _ = writeln!(
                con,
                "\n[iommu] (bug) {passthrough} PCI function(s) have no valid device-table entry \
                 — with the unit enabled that is unrestricted DMA, not containment"
            );
            A::exit(false);
        }
    } else {
        let _ = writeln!(
            con,
            "\n[iommu] (bug) no frame for the deny table — every unbound function would be \
             passed through"
        );
        return;
    }
    let bdf = (*core::ptr::addr_of!(TARGET_BDF) & 0xFFFF) as u64;
    let w0 = dev0_w0;
    let root = abi::PhysAddr(*core::ptr::addr_of!(IOMMU_PT_ROOT));

    // ---- event log: without it, a refused DMA is SILENT ----
    //
    // The unit reports IO_PAGE_FAULT (and friends) by appending 16-byte entries to a ring in
    // memory. With no log armed, a blocked transfer produces no record anywhere — the device
    // simply does not get its data, which is indistinguishable from a device that was never
    // asked. Arming it is what makes "refused" observable rather than assumed.
    //
    // Armed BEFORE the unit is enabled. It used to be armed after, which the AMD-Vi spec
    // does not permit — the base registers are to be programmed while the unit is off — and
    // which left a window where translation was live with no log behind it.
    //
    // 4 KiB = 256 entries; the EventLen field encodes 256 as 8.
    const EVENT_ENTRIES_LOG2: u64 = 8;
    const EVENT_LOG_EN: u64 = 1 << 2;
    if let Some(log) = fa.alloc_frame().map(|f| zero_frame(f)) {
        let val = (log.as_u64() & 0x000F_FFFF_FFFF_F000) | (EVENT_ENTRIES_LOG2 << 56);
        core::ptr::write_volatile((base + 0x10) as *mut u64, val);
        let back = core::ptr::read_volatile((base + 0x10) as *const u64);
        if back == val {
            let _ = writeln!(
                con,
                "[iommu] event log {:#x} (256 entries) armed; ELBR reads back {back:#018x}",
                log.as_u64()
            );
            // SAFETY: single-CPU boot path.
            EVENT_LOG = log.as_u64();
        } else {
            let _ = writeln!(con, "[iommu] event log base did not take: {back:#018x}");
        }
    }

    // ---- command buffer: the only way to tell the unit that a table CHANGED ----
    //
    // Adding a mapping needs no announcement — the unit had nothing cached for an address it
    // had never translated. REMOVING one does, and that difference was assumed rather than
    // measured: withdrawing a mapping cleared the page-table entry and the model's record and
    // stopped there, and the device went on reaching the frame from a cached translation.
    // Clearing a table is not revocation until the unit has been told.
    const CMD_ENTRIES_LOG2: u64 = 8;
    const CMD_BUF_EN: u64 = 1 << 12;
    if let Some(cmd) = fa.alloc_frame().map(|f| zero_frame(f)) {
        let val = (cmd.as_u64() & 0x000F_FFFF_FFFF_F000) | (CMD_ENTRIES_LOG2 << 56);
        core::ptr::write_volatile((base + 0x08) as *mut u64, val);
        let back = core::ptr::read_volatile((base + 0x08) as *const u64);
        if back == val {
            // SAFETY: single-CPU boot path.
            CMD_BUF = cmd.as_u64();
            CMD_TAIL = 0;
            if let Some(store) = fa.alloc_frame().map(|f| zero_frame(f)) {
                CMD_STORE = store.as_u64();
            }
            let _ = writeln!(
                con,
                "[iommu] command buffer {:#x} (256 entries) armed; CBBR reads back {back:#018x}",
                cmd.as_u64()
            );
        } else {
            let _ = writeln!(
                con,
                "[iommu] command buffer base did not take: {back:#018x}"
            );
        }
    }

    // Enable translation. Safe here for a specific reason rather than by luck: nothing in this
    // guest does DMA — the demo touches the serial port and debug-exit only — so a unit that
    // refuses every transfer cannot break the boot. On a machine that WAS doing DMA this line
    // would need the command buffer and an invalidation first.
    const IOMMU_EN: u64 = 1 << 0;
    let ctrl = base + 0x18;
    let before = core::ptr::read_volatile(ctrl as *const u64);
    core::ptr::write_volatile(
        ctrl as *mut u64,
        before | IOMMU_EN | EVENT_LOG_EN | CMD_BUF_EN,
    );
    let after = core::ptr::read_volatile(ctrl as *const u64);

    if after & IOMMU_EN != 0 {
        // The unit translates, so the domains become real NOW. Everything that treats a domain
        // as evidence that a device's DMA is bounded — `MAP_DMA`, and the capability naming the
        // real BAR — is gated on this store, not on the earlier table writes.
        for i in 0..MAX_DOMAINS {
            DOMAIN_SLOTS[i] = staged[i];
            if staged[i].id != 0 {
                // Reported HERE, not where the tables were written: "bound" is a statement
                // about a unit that translates, and until this store none of them did.
                let _ = writeln!(
                    con,
                    "[iommu] domain {} bound to {:#06x} with its own page table {:#x}",
                    staged[i].id, staged[i].bdf, staged[i].l1
                );
            }
        }
        let _ = writeln!(
            con,
            "[iommu] DTE[{bdf:#06x}] = {w0:#018x} (V TV mode=3 root={:#x}); unit ENABLED, \
             CTRL={after:#018x}",
            root.as_u64()
        );
        let _ = writeln!(
            con,
            "        the I/O page table is EMPTY, so every DMA from that device is refused"
        );
        prove_containment::<A>(con, fa, base);
    } else {
        let _ = writeln!(
            con,
            "[iommu] CTRL write did not take: {after:#018x} — no domain is published, so no \
             process can be handed a bus master or a DMA mapping"
        );
    }
}

/// The first of `frame`'s 64 sentinel bytes that is NOT the sentinel, or the sentinel itself if
/// all eight words are intact.
///
/// Each transfer moves 64 bytes and every check used to read only the first eight, so a partial
/// or offset write — bytes 8..64 landing while 0..8 did not — read as untouched. Returning the
/// offending word rather than a bool keeps the existing `== SENTINEL` comparisons and puts the
/// value that broke it into the message.
#[cfg(target_arch = "x86_64")]
unsafe fn first_disturbed(frame: u64, sentinel: u64) -> u64 {
    for i in 0..8u64 {
        let v = core::ptr::read_volatile((frame + i * 8) as *const u64);
        if v != sentinel {
            return v;
        }
    }
    sentinel
}

/// Append one 128-bit command and ring the tail. False if no buffer is armed.
///
/// The tail is ours to advance and the head is the unit's. 256 entries with a handful of
/// commands per boot never wraps, and the assert states that rather than a comment claiming it.
#[cfg(target_arch = "x86_64")]
unsafe fn iommu_cmd(base: u64, cmd: [u32; 4]) -> bool {
    let buf = *core::ptr::addr_of!(CMD_BUF);
    if buf == 0 {
        return false;
    }
    let tail = *core::ptr::addr_of!(CMD_TAIL);
    assert!(
        tail + 16 <= 4096,
        "IOMMU command ring wrapped and wrap handling is not implemented"
    );
    let slot = (buf + tail) as *mut u32;
    for (i, w) in cmd.iter().enumerate() {
        core::ptr::write_volatile(slot.add(i), *w);
    }
    // SAFETY: single-CPU boot path.
    CMD_TAIL = tail + 16;
    core::ptr::write_volatile((base + 0x2008) as *mut u64, tail + 16);
    true
}

/// Drop everything the unit may have cached for the target device, and WAIT for it to say so.
///
/// Completion is observed, not assumed: these commands are asynchronous, so a reachability
/// check run before the invalidation landed would report withdrawn or stale purely by timing.
#[cfg(target_arch = "x86_64")]
unsafe fn iommu_invalidate(base: u64, di: usize) -> bool {
    let store = *core::ptr::addr_of!(CMD_STORE);
    if store == 0 {
        return false;
    }
    const DONE: u64 = 0x5A5A_5A5A_A5A5_A5A5;
    core::ptr::write_volatile(store as *mut u64, 0);

    // INVALIDATE_IOMMU_PAGES for THIS domain, S=1 with an all-ones address meaning all of it.
    // The DomainID is load-bearing and was hardcoded to 0: each device-table entry carries its
    // own (`i`), so every invalidation named the FIRST device's domain while the second's leaves
    // were the ones being cleared. Measured in the emulator's own trace — seven page
    // invalidations, all "domain 0x0", and seven device-table invalidations, all 00:05.0 —
    // while UNMAP_DMA and FREE_REGION were clearing entries in the other domain.
    let dom_id = di as u32 & 0xFFFF;
    if !iommu_cmd(
        base,
        [0, (0x3 << 28) | dom_id, 0xFFFF_F000 | 1, 0x7FFF_FFFF],
    ) {
        return false;
    }
    // The device-table entry is cacheable in its own right, so it is invalidated too — for the
    // device this domain is bound to, not for whichever one the scan happened to find first.
    let bdf = (*core::ptr::addr_of!(DOMAIN_SLOTS[di])).bdf as u32;
    if !iommu_cmd(base, [bdf, 0x2 << 28, 0, 0]) {
        return false;
    }
    // COMPLETION_WAIT, with a store so there is something to actually observe.
    let cw = [
        (store as u32 & 0xFFFF_FFF8) | 1,
        (((store >> 32) as u32) & 0x000F_FFFF) | (0x1 << 28),
        DONE as u32,
        (DONE >> 32) as u32,
    ];
    if !iommu_cmd(base, cw) {
        return false;
    }
    for _ in 0..10_000_000u64 {
        if core::ptr::read_volatile(store as *const u64) == DONE {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// I/O page-table entry bits, shared by the boot demo and the `MAP_DMA` path so the two cannot
/// drift into writing different-looking entries for the same intent.
#[cfg(target_arch = "x86_64")]
mod iopte {
    pub const PR: u64 = 1 << 0;
    pub const IR: u64 = 1 << 61;
    pub const IW: u64 = 1 << 62;
    pub const LEAF: u64 = 0 << 9;
    pub const ADDR: u64 = 0x000F_FFFF_FFFF_F000;

    /// The entry for `frame` with `rights`, permission bits DERIVED rather than constant.
    pub fn leaf(frame: u64, rights: abi::CapRights) -> u64 {
        let mut pte = (frame & ADDR) | PR | LEAF;
        if rights.contains(abi::CapRights::READ) {
            pte |= IR;
        }
        if rights.contains(abi::CapRights::WRITE) {
            pte |= IW;
        }
        pte
    }
}

/// The IOVA window `MAP_DMA` allocates from: L1 slots 256..512, i.e. IOVA 0x10_0000..0x20_0000.
/// Inside the single L1 table (which covers 0..2 MiB) and above every IOVA the boot demo uses,
/// so the demo and userland cannot collide.
#[cfg(target_arch = "x86_64")]
const DMA_IOVA_FIRST_SLOT: u64 = 256;
#[cfg(target_arch = "x86_64")]
const DMA_IOVA_SLOTS: u64 = 256;

/// Index of the live region with identity `id`.
#[cfg(target_arch = "x86_64")]
unsafe fn region_index(id: u64) -> Option<usize> {
    (0..MAX_REGIONS).find(|&i| {
        let r = &*core::ptr::addr_of!(REGIONS[i]);
        r.live && r.id == id
    })
}

/// Give the device DMA reach to every page of a region, at an IOVA the KERNEL picks.
///
/// The grant already exists — `make_region` grants each page into the device domain as it
/// allocates it — so this adds the MAPPING half, which nothing could previously ask for. The
/// domain still decides: `Domain::map` refuses rights wider than the grant, and a refusal
/// writes no page-table entry, so a borrower holding a READ-only capability gets a read-only
/// I/O mapping rather than the owner's.
///
/// # Safety
/// Single-CPU, non-reentrant.
#[cfg(target_arch = "x86_64")]
unsafe fn map_dma(who: usize, domain: u64, region_id: u64, rights: abi::CapRights) -> u64 {
    // The capability's object has to name a domain that EXISTS. Without this the object is
    // decorative: measured, a capability naming domain 999 mapped into the real one.
    // Two different refusals, kept apart because they mean different things to the caller.
    // No domain at all is NO_MEM: there is nothing here that could bound DMA, and a "granted"
    // mapping would be indistinguishable from access to all of memory. A domain that exists
    // but is not the one this capability names is NO_CAP: an authority question.
    let ids = domain_ids();
    if ids.iter().all(|&id| id == 0) {
        return abi::syserr::NO_MEM;
    }
    let Some(di) = domain_lookup(&ids, domain) else {
        return abi::syserr::NO_CAP;
    };
    let Some(l1) = domain_l1(domain) else {
        return abi::syserr::NO_MEM;
    };
    let Some(ri) = region_index(region_id) else {
        return abi::syserr::NO_CAP;
    };
    let npages = (*core::ptr::addr_of!(REGIONS[ri])).npages;

    // A free run of L1 slots, read from the table itself rather than from a side index that
    // could disagree with it.
    let mut first = None;
    let mut run = 0u64;
    for slot in DMA_IOVA_FIRST_SLOT..DMA_IOVA_FIRST_SLOT + DMA_IOVA_SLOTS {
        if core::ptr::read_volatile((l1 + slot * 8) as *const u64) & iopte::PR == 0 {
            run += 1;
            if run == npages {
                first = Some(slot + 1 - npages);
                break;
            }
        } else {
            run = 0;
        }
    }
    let Some(first) = first else {
        return abi::syserr::NO_MEM;
    };
    // A slot to record it in, BEFORE installing anything. An untracked mapping is one nothing
    // can withdraw when this process dies or loses the capability, which is precisely the hole
    // attribution exists to close — so refusing is the only honest answer when the table is full.
    let Some(rec) = (0..SHARE_SLOTS).find(|&i| proc_at(who).dma[i].0 == 0) else {
        return abi::syserr::NO_MEM;
    };

    for k in 0..npages {
        let frame = (*core::ptr::addr_of!(REGIONS[ri])).frames[k as usize].as_u64();
        let iova = (first + k) * abi::PAGE_SIZE;
        // The grant is issued HERE, with the rights of the capability that asked, INTO THE
        // DOMAIN THAT CAPABILITY NAMES. `map` still decides — it refuses rights wider than the
        // grant — but the grant now records an actual request rather than the fact that memory
        // was once allocated.
        domain_at(di).grant(frame >> abi::PAGE_SHIFT, rights);
        if domain_at(di)
            .map(iova, frame >> abi::PAGE_SHIFT, rights)
            .is_err()
        {
            // All or nothing. A half-mapped region would leave the device reaching part of
            // something the caller was told it could not reach at all.
            // Undo exactly what THIS call installed, and nothing else. `Domain::revoke` is far
            // too broad here: it withdraws EVERY model mapping of a frame — including ones
            // earlier calls made at other IOVAs — while this loop can only zero the leaves it
            // wrote itself. That combination STRANDED present hardware entries the model had
            // just forgotten about, so no later `UNMAP_DMA`, `FREE_REGION` or teardown could
            // find them, and the frames went back to the allocator still reachable by the
            // device. `contained()` stayed true throughout: the model was self-consistent and
            // only the hardware disagreed, which is the one shape a model-only check misses.
            for done in 0..=k {
                domain_at(di).unmap((first + done) * abi::PAGE_SIZE);
                core::ptr::write_volatile((l1 + (first + done) * 8) as *mut u64, 0);
            }
            // A grant goes only where nothing still rests on it. (A grant this call WIDENED for
            // a frame an earlier mapping still holds keeps the wider rights — a model-level
            // residue with no hardware consequence, since reach is decided by the page-table
            // entries, and re-granting needs the caller's own capability rights anyway.)
            for done in 0..=k {
                let f = (*core::ptr::addr_of!(REGIONS[ri])).frames[done as usize].as_u64()
                    >> abi::PAGE_SHIFT;
                let still = domain_at(di).reachable().any(|(_, frame, _)| frame == f);
                if !still {
                    domain_at(di).revoke(f);
                }
            }
            return abi::syserr::NO_CAP;
        }
        core::ptr::write_volatile(
            (l1 + (first + k) * 8) as *mut u64,
            iopte::leaf(frame, rights),
        );
    }
    proc_at(who).dma[rec] = (region_id, domain);
    first * abi::PAGE_SIZE
}

/// Clear every I/O page-table entry that points at `pfn`, leaving the model untouched.
///
/// Returns whether anything was cleared, so the caller can skip an invalidation with nothing
/// to invalidate. The domain is the index — it knows which IOVAs it handed out, so no caller
/// ever names one — which is why this must run BEFORE the model is revoked. A revoked domain
/// has forgotten where the mappings were, and the hardware entries would be left behind with
/// nothing remaining that knows to look for them.
///
/// # Safety
/// Single-CPU, non-reentrant.
#[cfg(target_arch = "x86_64")]
unsafe fn clear_io_mappings_in(di: usize, pfn: u64) -> bool {
    let l1 = match (*core::ptr::addr_of!(DOMAIN_SLOTS[di])).l1 {
        0 => return false,
        l1 => l1,
    };
    // Driven by the TABLE, not by the model's index of it. Asking the model where a frame is
    // mapped works only while the two agree, and the one time they did not — a rollback that
    // revoked a grant and dropped mappings it had not cleared — the entries became invisible to
    // every withdrawal path in the tree. Scanning the level the device actually walks cannot be
    // fooled that way: whatever is present here is what the device can reach.
    let mut any = false;
    for slot in 0..512u64 {
        let at = l1 + slot * 8;
        let pte = core::ptr::read_volatile(at as *const u64);
        if pte & iopte::PR != 0 && (pte & iopte::ADDR) >> abi::PAGE_SHIFT == pfn {
            core::ptr::write_volatile(at as *mut u64, 0);
            any = true;
        }
    }
    any
}

/// The same, across EVERY domain.
///
/// Freeing memory has to reach all of them. A frame may be mapped by more than one device, and
/// the region being destroyed knows nothing about which domains took it — so a sweep that
/// stopped at the first would return a frame to the allocator while another device still
/// reached it, which is the bug this whole path exists to prevent, one domain over.
///
/// # Safety
/// Single-CPU, non-reentrant.
#[cfg(target_arch = "x86_64")]
unsafe fn clear_io_mappings_of(pfn: u64) -> bool {
    let mut any = false;
    for di in 0..MAX_DOMAINS {
        any |= clear_io_mappings_in(di, pfn);
    }
    any
}

/// Withdraw a region's mappings from ONE domain: hardware first, then the model, then the grant.
///
/// Returns whether anything was cleared, so a caller withdrawing several can invalidate once.
///
/// # Safety
/// Single-CPU, non-reentrant.
#[cfg(target_arch = "x86_64")]
unsafe fn withdraw_region_from_domain(di: usize, ri: usize) -> bool {
    let npages = (*core::ptr::addr_of!(REGIONS[ri])).npages;
    let mut any = false;
    for k in 0..npages {
        let pfn =
            (*core::ptr::addr_of!(REGIONS[ri])).frames[k as usize].as_u64() >> abi::PAGE_SHIFT;
        // Hardware first — the model is the index that finds the hardware.
        any |= clear_io_mappings_in(di, pfn);
        let mut iovas = [0u64; REGION_MAX_PAGES as usize * 2];
        let mut n = 0;
        for (iova, frame, _) in domain_at(di).reachable() {
            if frame == pfn && n < iovas.len() {
                iovas[n] = iova;
                n += 1;
            }
        }
        for &iova in iovas.iter().take(n) {
            domain_at(di).unmap(iova);
        }
        domain_at(di).revoke(pfn);
    }
    any
}

/// Withdraw one DMA mapping this process asked for, by its slot in `Process::dma`.
///
/// # Safety
/// Single-CPU, non-reentrant.
#[cfg(target_arch = "x86_64")]
unsafe fn withdraw_dma_slot(who: usize, slot: usize) -> bool {
    let (region_id, domain) = proc_at(who).dma[slot];
    if region_id == 0 {
        return false;
    }
    proc_at(who).dma[slot] = (0, 0);
    let Some(di) = domain_lookup(&domain_ids(), domain) else {
        return false;
    };
    let Some(ri) = region_index(region_id) else {
        // The region is already gone, which means `Release` cleared its entries on the way out.
        return false;
    };
    withdraw_region_from_domain(di, ri)
}

/// Flush every live domain. Used where a sweep may have touched more than one.
///
/// # Safety
/// Single-CPU, non-reentrant.
#[cfg(target_arch = "x86_64")]
unsafe fn invalidate_all_domains(base: u64) {
    for di in 0..MAX_DOMAINS {
        if (*core::ptr::addr_of!(DOMAIN_SLOTS[di])).id != 0 {
            iommu_invalidate(base, di);
        }
    }
}

/// Withdraw every DMA mapping `who` asked for, and tell the unit once.
///
/// # Safety
/// Single-CPU, non-reentrant.
#[cfg(target_arch = "x86_64")]
unsafe fn withdraw_all_dma_of(who: usize) {
    let mut any = false;
    for slot in 0..SHARE_SLOTS {
        if withdraw_dma_slot(who, slot) {
            any = true;
            DMA_WITHDRAWN_AT_EXIT += 1;
        }
    }
    if any {
        let base = *core::ptr::addr_of!(AMDVI_BASE);
        if base != 0 {
            invalidate_all_domains(base);
        }
    }
}

/// Withdraw every DMA mapping of a region, and TELL THE UNIT.
///
/// # Safety
/// Single-CPU, non-reentrant.
#[cfg(target_arch = "x86_64")]
unsafe fn unmap_dma(who: usize, domain: u64, region_id: u64) -> u64 {
    let ids = domain_ids();
    if ids.iter().all(|&id| id == 0) {
        return abi::syserr::NO_MEM;
    }
    let Some(di) = domain_lookup(&ids, domain) else {
        return abi::syserr::NO_CAP;
    };
    let Some(ri) = region_index(region_id) else {
        return abi::syserr::NO_CAP;
    };
    withdraw_region_from_domain(di, ri);
    // Drop the record too: the mapping is gone, so nothing should later try to withdraw it
    // again — and the slot has to come back or a process could exhaust its own table.
    for i in 0..SHARE_SLOTS {
        if proc_at(who).dma[i] == (region_id, domain) {
            proc_at(who).dma[i] = (0, 0);
        }
    }
    // Clearing the table is not revocation while the unit still holds a translation it has
    // already performed. Measured on the rig: without this the device kept reaching the frame.
    let base = *core::ptr::addr_of!(AMDVI_BASE);
    if base != 0 {
        iommu_invalidate(base, di);
    }
    abi::syserr::OK
}

/// What the read-only probe observed: whether the narrowed mapping was written, whether the
/// model refused wider rights, and what the frame and the event log looked like afterwards.
#[cfg(target_arch = "x86_64")]
struct RoProbe {
    /// What the ungranted frame read after the device was aimed at its IOVA. The refusal used
    /// to be reported straight from the model — `!dom.map(..).is_ok()` printed as the hardware
    /// fact "no PTE written" — so a defect that wrote the entry anyway would still have said
    /// "refused". This is the device's answer instead of the model's.
    ungranted_seen: u64,
    frame: u64,
    mapped: bool,
    wider_refused: bool,
    done: bool,
    seen: u64,
    tail_before: u64,
    tail_after: u64,
}

/// Make the bounded device attempt a DMA, and report what the IOMMU did about it.
///
/// This is the step that turns "every DMA is refused" from a property of the CONFIGURATION
/// into an OBSERVATION. Everything before it programmed the unit; nothing had asked a device to
/// transfer, so nothing had actually been refused.
///
/// The device is QEMU's `edu`: a register-driven DMA engine whose entire interface is a
/// source, a destination, a count and a command. Driving a real NIC would mean writing a NIC
/// driver, and the thing under test is the IOMMU.
///
/// # Safety
/// Single-CPU boot path; `base` is the mapped AMD-Vi aperture.
#[cfg(target_arch = "x86_64")]
unsafe fn prove_containment<A: Arch>(
    con: &mut Console<A>,
    fa: &mut mm::BitmapAllocator,
    base: u64,
) {
    use abi::FrameAllocator as _;
    let Some(dev) = pci::find_dma_device() else {
        return;
    };
    if !pci::is_edu(&dev) {
        let _ = writeln!(
            con,
            "[iommu] the bounded device is not one this nucleus can drive; no DMA attempted"
        );
        return;
    }
    let Some(bar) = pci::bar0(&dev) else {
        let _ = writeln!(
            con,
            "[iommu] the target device has no assigned BAR — nothing can be told to DMA"
        );
        return;
    };

    let ktoken = *core::ptr::addr_of!(KTOKEN);
    let mut space = A::Space::from_token(ktoken);
    if !space.map_page(
        abi::VirtAddr(bar),
        abi::PhysAddr(bar),
        Perms::KERNEL_DEVICE,
        fa,
    ) {
        let _ = writeln!(con, "[iommu] could not map the target BAR at {bar:#x}");
        return;
    }
    A::flush_tlb();

    // Oracle FIRST: `edu` reports 0x010000ed in its identification register. A BAR that mapped
    // nothing reads all-ones or zeros, and this is what tells those apart — without it a silent
    // DMA failure could equally mean "the device was never reachable".
    let ident = core::ptr::read_volatile(bar as *const u32);
    if ident != 0x0100_00ed {
        let _ = writeln!(
            con,
            "[iommu] target BAR {bar:#x} does not look like edu: ident={ident:#010x}"
        );
        return;
    }
    let _ = writeln!(
        con,
        "[iommu] target BAR {bar:#x} mapped; edu ident={ident:#010x}"
    );

    let cmd = pci::enable_bus_master(&dev);
    let _ = writeln!(con, "        bus mastering enabled (command={cmd:#06x})");

    // A frame the kernel owns, so a SUCCESSFUL transfer would be detectable too — this has to
    // be able to show translation working later, not only failing.
    let Some(target) = fa.alloc_frame().map(|f| zero_frame(f)) else {
        return;
    };
    // Pre-fill with a SENTINEL rather than leaving it zeroed. "Contained" has to mean nothing
    // was written, and a zeroed target cannot show that: the inbound RAM->device load is
    // refused too, so edu's buffer is empty, and a transfer the IOMMU ALLOWED would deposit
    // zeros — byte-identical to one it blocked. Checking `!= PATTERN` therefore only
    // established "the pattern did not arrive", while the line claimed "nothing reached
    // memory". With a sentinel, ANY write is visible, including a write of zeros.
    const SENTINEL: u64 = 0x5E17_11E1_5E17_11E1;
    for i in 0..8u64 {
        core::ptr::write_volatile((target.as_u64() + i * 8) as *mut u64, SENTINEL);
    }

    let tail_before = core::ptr::read_volatile((base + 0x2018) as *const u64);

    // TWO transfers, because one cannot be told apart from a refusal. `edu`'s internal buffer
    // starts ZEROED, so a device->RAM transfer of it writes zeros — exactly what a blocked
    // transfer leaves behind. The positive control caught this: with translation OFF the DMA
    // "completed" and the target still read zero. So push a known pattern IN first, then read
    // it back OUT, and let the pattern be the oracle.
    const PATTERN: u64 = 0xD1CE_D1CE_D1CE_D1CE;
    let Some(src) = fa.alloc_frame().map(|f| zero_frame(f)) else {
        return;
    };
    for i in 0..8u64 {
        core::ptr::write_volatile((src.as_u64() + i * 8) as *mut u64, PATTERN);
    }

    // edu: 0x80 source, 0x88 destination, 0x90 count, 0x98 command.
    // cmd bit0 = start; bit1 = direction (0 = RAM->device, 1 = device->RAM). 0x40000 is the
    // device's own buffer, and the direction decides which side must lie inside it.
    let run = |src_addr: u64, dst_addr: u64, dir_to_ram: bool| {
        core::ptr::write_volatile((bar + 0x80) as *mut u64, src_addr);
        core::ptr::write_volatile((bar + 0x88) as *mut u64, dst_addr);
        core::ptr::write_volatile((bar + 0x90) as *mut u64, 64);
        core::ptr::write_volatile((bar + 0x98) as *mut u64, if dir_to_ram { 0x3 } else { 0x1 });
        let mut n = 0u64;
        while n < 200_000_000 {
            if core::ptr::read_volatile((bar + 0x98) as *const u64) & 1 == 0 {
                return true;
            }
            core::hint::spin_loop();
            n += 1;
        }
        false
    };

    // RAM -> device, then device -> RAM. Under translation BOTH are refused; with translation
    // off both land and `target` ends up holding PATTERN.
    let in_ok = run(src.as_u64(), 0x4_0000, false);
    let out_ok = run(0x4_0000, target.as_u64(), true);
    let _ = writeln!(
        con,
        "        transfers: RAM->dev {} dev->RAM {}",
        if in_ok { "done" } else { "STUCK" },
        if out_ok { "done" } else { "STUCK" }
    );

    // POLL the command register rather than sleeping a guessed interval. QEMU's `edu` defers
    // the transfer on a 100 ms timer and clears the RUN bit when it finishes, so a fixed spin
    // is a race: the first version waited a few milliseconds, saw nothing, and reported "no
    // event logged" — which read exactly like a refusal and was in fact impatience. The
    // positive control (translation off, transfer should land) is what exposed it.
    let finished = in_ok && out_ok;

    let tail_after = core::ptr::read_volatile((base + 0x2018) as *const u64);
    let wrote = first_disturbed(target.as_u64(), SENTINEL);
    let _ = writeln!(
        con,
        "[iommu] DMA {}: event tail {tail_before:#x} -> {tail_after:#x}, target reads \
         {wrote:#018x} (untouched = {SENTINEL:#018x})",
        if finished {
            "completed"
        } else {
            "DID NOT FINISH (RUN still set)"
        }
    );

    // ---- now MAP an IOVA and show the same transfer succeed through translation ----
    //
    // Blocking everything is the easy half: an empty table refuses by having nothing in it, and
    // a unit that was simply broken would look identical. Translating something is what
    // distinguishes "the IOMMU is enforcing a policy" from "the IOMMU is enforcing a wall".
    //
    // AMD-Vi's I/O page tables are a radix tree like the CPU's, with one difference that
    // matters: each entry carries a NEXT LEVEL field in bits [11:9], and a LEAF is next-level
    // 0. Writing the level number of the table you are pointing AT (rather than of the table
    // you are in) is the obvious mistake, so the levels are named.
    const PR: u64 = 1 << 0; // present
    const IR: u64 = 1 << 61; // device may read
    const IW: u64 = 1 << 62; // device may write
    const LEAF: u64 = 0 << 9;
    const IOVA: u64 = 0x1000;
    const IOVA_SRC: u64 = 0x2000;
    const IOVA_UNGRANTED: u64 = 0x3000;
    const IOVA_RO: u64 = 0x5000;
    /// A FRESH IOVA for the wider-rights attempt. It used to reuse `IOVA_RO`, where the mapping
    /// already installed made `IovaInUse` refuse the request whatever its rights were — so the
    /// refusal was over-determined and the rights check untested. Measured: deleting
    /// `Domain::map`'s rights check left all five rig modes PASS with byte-identical output,
    /// "wider rights refused" and "RIGHTS ENFORCED" included.
    const IOVA_WIDER: u64 = 0x6000;

    // BOTH directions need a mapping. The first attempt mapped only the destination and got
    // "NOT TRANSLATED" — because the inbound RAM->device transfer that loads the pattern was
    // itself refused, so the device faithfully delivered an EMPTY buffer. Same oracle trap as
    // the zeroed buffer earlier, one level up: the payload has to be able to reach the device
    // before its arrival back in memory can mean anything.
    let translated = (|| {
        // The table skeleton is built with the DTE now; the demo only writes leaves into it.
        let l1 = abi::PhysAddr((*core::ptr::addr_of!(IOMMU_L1))?);
        let dst = fa.alloc_frame().map(|f| zero_frame(f))?;
        // ---- the DOMAIN decides; the page table only records what it allowed ----
        //
        // `crates/iommu` has been exhaustively host-tested since it was written, and until now
        // it ran BESIDE the hardware: the model said what was authorized while these stores
        // said what the device could reach, and nothing tied the two together. A model with no
        // authority over the machine it models is documentation.
        //
        // So every leaf below goes through `Domain::map`, which refuses a frame no capability
        // granted and refuses rights wider than the grant. A refusal writes NO page-table
        // entry, which is what makes the tie observable: an ungranted frame stays unreachable
        // by the device, not merely unrecorded in a table.
        // Allocated and granted here, above the closure below, because that closure takes
        // `dom` mutably for its whole life and nothing else may touch the domain while it is
        // alive. Used further down, in the rights phase.
        let ro = fa.alloc_frame().map(|f| zero_frame(f))?;

        let dom = device_domain();
        let rw = abi::CapRights(0b011);
        let r_only = abi::CapRights::READ;
        dom.grant(dst.as_u64() >> abi::PAGE_SHIFT, rw);
        dom.grant(src.as_u64() >> abi::PAGE_SHIFT, rw);
        dom.grant(ro.as_u64() >> abi::PAGE_SHIFT, r_only);

        // The PTE's permission bits are derived FROM the granted rights. They used to be a
        // constant `IR | IW`: the domain would refuse rights wider than the grant and then
        // write a read-WRITE entry regardless, so a READ-only grant produced a WRITABLE
        // mapping. The model's authority reached which FRAME the device could touch but not
        // what it could DO to it — and nothing caught it, because every grant here happened
        // to be RW, which made the constant accidentally correct in every case exercised.
        let mut leaf = |iova: u64, frame: u64, rights: abi::CapRights| -> bool {
            if dom.map(iova, frame >> abi::PAGE_SHIFT, rights).is_err() {
                return false;
            }
            let mut pte = (frame & 0x000F_FFFF_FFFF_F000) | PR | LEAF;
            if rights.contains(abi::CapRights::READ) {
                pte |= IR;
            }
            if rights.contains(abi::CapRights::WRITE) {
                pte |= IW;
            }
            let idx = ((iova >> 12) & 0x1FF) as u64;
            core::ptr::write_volatile((l1.as_u64() + idx * 8) as *mut u64, pte);
            true
        };
        let dst_ok = leaf(IOVA, dst.as_u64(), rw);
        let src_ok = leaf(IOVA_SRC, src.as_u64(), rw);

        // The NEGATIVE case, in the same boot: a frame nobody granted. The domain must refuse
        // it, and because a refusal writes no entry, the device is left unable to reach it —
        // the model's decision and the hardware's behaviour are the same fact.
        let ungranted = fa.alloc_frame().map(|f| zero_frame(f))?;
        for i in 0..8u64 {
            core::ptr::write_volatile((ungranted.as_u64() + i * 8) as *mut u64, SENTINEL);
        }
        let refused = !leaf(IOVA_UNGRANTED, ungranted.as_u64(), rw);
        let _ = writeln!(
            con,
            "[iommu] domain: dst {} src {} ungranted-frame {}",
            if dst_ok { "mapped" } else { "REFUSED" },
            if src_ok { "mapped" } else { "REFUSED" },
            if refused {
                "refused (no PTE written)"
            } else {
                "MAPPED — the domain let an ungranted frame through"
            }
        );
        if !refused || !dst_ok || !src_ok {
            return None;
        }
        // Adding a mapping needs no invalidation: the unit populates its cache from a
        // SUCCESSFUL walk, never from a fault, so it cannot hold a stale translation for an
        // address every previous attempt was refused at. That is true here and stays true.
        // Withdrawal is the other case — which the older wording of this note called correctly
        // ("the moment a mapping is CHANGED rather than added, this needs the command buffer")
        // and which shipped without one anyway. It is handled where the withdrawal happens.
        // Load the pattern through the SOURCE mapping, then read it back through the
        // destination one. Both legs now go through translation, which is the point.
        let in_ok = run(IOVA_SRC, 0x4_0000, false);
        let ok = in_ok && run(0x4_0000, IOVA, true);
        let landed = core::ptr::read_volatile(dst.as_u64() as *const u64);

        // ---- rights, not merely reachability ----
        //
        // A frame granted READ and not WRITE. The domain narrows the mapping, the leaf above
        // therefore writes IR without IW, and the device is then told to WRITE there. The
        // refusal has to come from the unit's PERMISSION check rather than from an absent
        // entry: this page is present and is readable, which separates "the device cannot see
        // it" from "the device may not write it". Every refusal demonstrated until now was of
        // the first kind.
        //
        // It also settles the event log, which has recorded nothing across a refused transfer
        // and left "refusal not reported" indistinguishable from "our log setup is broken".
        // QEMU's walker returns SILENTLY when a next-level entry reads as zero — precisely the
        // empty-table refusal above, blocked correctly and never logged. A PRESENT entry with
        // insufficient permissions takes the other path, the one that writes an event. So if
        // the tail moves here and not there, the log works and the earlier zero was the
        // emulator's shape; if it moves in neither, the fault is ours.
        for i in 0..8u64 {
            core::ptr::write_volatile((ro.as_u64() + i * 8) as *mut u64, SENTINEL);
        }
        let ro_mapped = leaf(IOVA_RO, ro.as_u64(), r_only);
        // And the model must refuse WIDER rights than the grant. This goes through `leaf`
        // rather than calling the model directly, so a wrongly-allowed map would write a real
        // WRITABLE entry to the same IOVA — meaning the device write below would land and
        // `ro_seen` would catch it. The check and the consequence are the same code path.
        let wider_refused = !leaf(IOVA_WIDER, ro.as_u64(), rw);
        let tail_ro_before = core::ptr::read_volatile((base + 0x2018) as *const u64);
        let ro_done = run(0x4_0000, IOVA_RO, true);
        // And through the wider mapping, which must not exist. With the rights check gone the
        // leaf above IS written, this write lands, and `ro_seen` catches it — which is what
        // makes the check load-bearing instead of merely present.
        let _ = run(0x4_0000, IOVA_WIDER, true);
        let ro_seen = first_disturbed(ro.as_u64(), SENTINEL);
        // And aim one at the ungranted IOVA, so "refused" is something the DEVICE demonstrates.
        let _ = run(0x4_0000, IOVA_UNGRANTED, true);
        let ungranted_seen = first_disturbed(ungranted.as_u64(), SENTINEL);
        let tail_ro_after = core::ptr::read_volatile((base + 0x2018) as *const u64);

        Some((
            ok,
            landed,
            dst.as_u64(),
            RoProbe {
                ungranted_seen,
                frame: ro.as_u64(),
                mapped: ro_mapped,
                wider_refused,
                done: ro_done,
                seen: ro_seen,
                tail_before: tail_ro_before,
                tail_after: tail_ro_after,
            },
        ))
    })();

    if let Some((ok, landed, dst, _)) = translated {
        let _ = writeln!(
            con,
            "[iommu] mapped IOVA {IOVA:#x} -> {dst:#x}; transfer {} and that frame reads \
             {landed:#018x}",
            if ok { "completed" } else { "STUCK" }
        );
        if landed == PATTERN {
            let _ = writeln!(
                con,
                "[iommu] TRANSLATED: the same device reached exactly the frame it was granted"
            );
        } else {
            let _ = writeln!(
                con,
                "[iommu] NOT TRANSLATED: a granted IOVA did not reach its frame"
            );
        }
    }

    // ---- withdraw: the model AND the hardware, in that order ----
    //
    // The proof's grants are not a standing authority — the device has no business reaching
    // those frames afterwards, and the boot's own consistency check (grants must equal live
    // DMA pages) would otherwise fail, correctly, because two grants would be outstanding with
    // no region behind them. That check catching this is the check working.
    //
    // Mappings go before grants, and the PTE is cleared as well as the model entry. Clearing
    // only the model would leave the device still able to reach the frame while nothing said
    // it could — the stale-mapping hazard `crates/iommu`'s exhaustive search exists to
    // prevent, which would be a poor thing to demonstrate and then commit here.
    if let Some((_, _, dst_phys, ro)) = translated {
        let _ = writeln!(
            con,
            "[iommu] read-only grant: mapped {} | wider rights {} | device write {} | frame \
             reads {:#018x} | event tail {:#x} -> {:#x}",
            if ro.mapped { "yes (IR, no IW)" } else { "NO" },
            if ro.wider_refused {
                "refused"
            } else {
                "ALLOWED"
            },
            if ro.done { "completed" } else { "stuck" },
            ro.seen,
            ro.tail_before,
            ro.tail_after
        );
        if ro.seen != SENTINEL {
            let _ = writeln!(
                con,
                "[iommu] (bug) WRITE-THROUGH: a READ-only mapping accepted a device write"
            );
        } else {
            let _ = writeln!(
                con,
                "[iommu] RIGHTS ENFORCED: the device could reach that page but not write it"
            );
        }
        if ro.ungranted_seen == SENTINEL {
            let _ = writeln!(
                con,
                "[iommu] UNREACHABLE: the ungranted IOVA was refused by the DEVICE, not just \
                 by the model (frame still {:#018x})",
                ro.ungranted_seen
            );
        } else {
            let _ = writeln!(
                con,
                "[iommu] (bug) an ungranted IOVA reached its frame ({:#018x})",
                ro.ungranted_seen
            );
        }
        if ro.tail_after != ro.tail_before {
            let _ = writeln!(
                con,
                "[iommu] EVENT LOGGED: the unit RECORDED the refusal it performed"
            );
        } else {
            let _ = writeln!(
                con,
                "[iommu] event log silent on a permission fault (see the controls below for \
                 what that does and does not show)"
            );
        }

        let dom = device_domain();
        dom.revoke(ro.frame >> abi::PAGE_SHIFT);
        {
            if let Some(l1) = *core::ptr::addr_of!(IOMMU_L1) {
                for iova in [IOVA_RO, IOVA_WIDER] {
                    let idx = ((iova >> 12) & 0x1FF) as u64;
                    core::ptr::write_volatile((l1 + idx * 8) as *mut u64, 0);
                }
            }
        }
        for (iova, frame) in [(IOVA, dst_phys), (IOVA_SRC, src.as_u64())] {
            dom.revoke(frame >> abi::PAGE_SHIFT);
            let idx = ((iova >> 12) & 0x1FF) as u64;
            if let Some(l1) = *core::ptr::addr_of!(IOMMU_L1) {
                core::ptr::write_volatile((l1 + idx * 8) as *mut u64, 0);
            }
        }
        let _ = writeln!(
            con,
            "[iommu] withdrew both mappings and grants; domain holds {} grant(s)",
            dom.grant_count()
        );

        // ---- does WITHDRAWAL take effect in the HARDWARE? ----
        //
        // Clearing the PTE and the model entry was assumed to settle this. It does not. The
        // unit may CACHE a translation it has already performed, and this exact IOVA was
        // translated successfully moments ago — so the standing note that "nothing was ever
        // cached" stopped being true the instant the first transfer succeeded. Clearing a
        // table without invalidating leaves any cached entry live, and the device keeps
        // reaching a frame nothing grants it: the stale-mapping hazard `crates/iommu` refuses
        // in the model, arriving by way of the hardware instead.
        //
        // Re-fill with the sentinel and tell the device to write there again. If the pattern
        // comes back, a withdrawn mapping is still being honoured.
        let flushed = iommu_invalidate(base, 0);
        let _ = writeln!(
            con,
            "[iommu] invalidation {}",
            if flushed {
                "issued and COMPLETED (the unit acknowledged)"
            } else {
                "NOT completed — no command buffer, or the unit never acknowledged"
            }
        );

        for i in 0..8u64 {
            core::ptr::write_volatile((dst_phys + i * 8) as *mut u64, SENTINEL);
        }
        let _ = run(0x4_0000, IOVA, true);
        let after_withdraw = first_disturbed(dst_phys, SENTINEL);
        if after_withdraw == SENTINEL {
            let _ = writeln!(
                con,
                "[iommu] REVOKED: after invalidation the device can no longer reach the frame \
                 it lost (still {after_withdraw:#018x})"
            );
        } else {
            let _ = writeln!(
                con,
                "[iommu] (bug) STALE MAPPING: a withdrawn IOVA still reached its frame \
                 ({after_withdraw:#018x}) — the table was cleared but the unit was not told"
            );
        }
    }

    // ---- PER-DEVICE CONTAINMENT: one device's domain is not another's ----
    //
    // Domain 2 belongs to the other DMA-capable function and has its own page table. A frame
    // mapped there must not become reachable by `edu`, whose domain is 1. Both halves are the
    // same transfer by the same device with exactly one thing changed — WHICH TABLE the leaf
    // was written into — so "refused" and "reached" are one measurement rather than two
    // stories. Without the second half, a wall would look like containment.
    if let Some(other) = fa.alloc_frame().map(|f| zero_frame(f)) {
        let l1_two = (*core::ptr::addr_of!(DOMAIN_SLOTS[1])).l1;
        let l1_one = (*core::ptr::addr_of!(DOMAIN_SLOTS[0])).l1;
        if l1_two != 0 && l1_one != 0 {
            const IOVA_OTHER: u64 = 0x8000;
            let slot = (IOVA_OTHER >> abi::PAGE_SHIFT) & 0x1FF;
            let rw = abi::CapRights(0b011);
            let pfn = other.as_u64() >> abi::PAGE_SHIFT;
            for i in 0..8u64 {
                core::ptr::write_volatile((other.as_u64() + i * 8) as *mut u64, SENTINEL);
            }

            // Granted and mapped in domain 2 ONLY.
            domain_at(1).grant(pfn, rw);
            let mapped_two = domain_at(1).map(IOVA_OTHER, pfn, rw).is_ok();
            core::ptr::write_volatile(
                (l1_two + slot * 8) as *mut u64,
                iopte::leaf(other.as_u64(), rw),
            );
            let _ = run(0x4_0000, IOVA_OTHER, true);
            let across = first_disturbed(other.as_u64(), SENTINEL);

            // Now the SAME frame at the SAME IOVA in edu's own domain: the control that says
            // the refusal above was the table and not the transfer.
            domain_at(0).grant(pfn, rw);
            let mapped_one = domain_at(0).map(IOVA_OTHER, pfn, rw).is_ok();
            core::ptr::write_volatile(
                (l1_one + slot * 8) as *mut u64,
                iopte::leaf(other.as_u64(), rw),
            );
            let _ = run(0x4_0000, IOVA_OTHER, true);
            let within = first_disturbed(other.as_u64(), SENTINEL);

            let _ = writeln!(
                con,
                "[iommu] cross-domain: mapped in 2 {} / in 1 {} | through 2 only the frame \
                 reads {across:#018x}, once 1 maps it {within:#018x}",
                if mapped_two { "yes" } else { "NO" },
                if mapped_one { "yes" } else { "NO" }
            );
            if across == SENTINEL && within != SENTINEL {
                let _ = writeln!(
                    con,
                    "[iommu] PER-DEVICE: a frame mapped in another device's domain stayed \
                     UNREACHABLE, and became reachable only when this device's own domain \
                     mapped it"
                );
            } else if across != SENTINEL {
                let _ = writeln!(
                    con,
                    "[iommu] (bug) CROSS-DOMAIN REACH: the device read through a mapping that \
                     belongs to another device's domain"
                );
            } else {
                let _ = writeln!(
                    con,
                    "[iommu] (bug) the device could not reach its OWN domain's mapping — the \
                     refusal above shows nothing"
                );
            }

            // Withdraw both, hardware first, and tell the unit.
            for (di, l1) in [(0usize, l1_one), (1usize, l1_two)] {
                core::ptr::write_volatile((l1 + slot * 8) as *mut u64, 0);
                domain_at(di).unmap(IOVA_OTHER);
                domain_at(di).revoke(pfn);
            }
            invalidate_all_domains(base);
        }
    }

    // ---- POSITIVE CONTROL for the event log itself ----
    //
    // Reported silence means nothing until the log is shown capable of speaking. Bit 2 of DTE
    // word0 is RESERVED, so a unit that validates the entry must reject it and record an
    // illegal-device-table-entry event — the one case that cannot be blamed on how a walk is
    // shaped. The log buffer is read directly rather than the tail register, because a tail
    // is only evidence if the unit reflects it back through MMIO, and that is the thing in
    // question.
    //
    // It does not move, and neither does the illegal-command control below. The attribution
    // is UNRESOLVED and deliberately not asserted either way — an earlier version of this
    // comment claimed the fault was ours, on no more evidence than the silence itself.
    //
    // What the instrumentation has RULED OUT, all measured on this rig:
    //   - not "logging disabled": the unit reports EventLogRun=1 in its own STATUS register,
    //     which QEMU sets exactly when it considers event logging enabled;
    //   - not overflow: EventOverflow=0, and the overflow path would set it;
    //   - not a failed log write: the `amdvi_evntlog_fail` trace point never fires;
    //   - not "we read the wrong entry": the whole 4 KiB ring is scanned, not word 0;
    //   - not unmapped registers: four aperture pages are mapped and STATUS reads sensibly;
    //   - not "the unit cannot write our memory": COMPLETION_WAIT's store lands every boot.
    // And the unit DOES detect these errors — `amdvi_invalid_dte` fires sixteen times and
    // `amdvi_unhandled_command` once (QEMU_TRACE='amdvi_*'), while upstream 8.2.2 has every
    // one of those paths call `amdvi_log_event`. Observation and source disagree, so the next
    // step is the distro build's actual sources, not another guess.
    //
    // The refusals themselves are real and separately demonstrated by the payload. What is
    // missing is the unit REPORTING them, which a driver diagnosing a bad device will need.
    {
        let dte = *core::ptr::addr_of!(DEVICE_TABLE_ENTRY);
        let log = *core::ptr::addr_of!(EVENT_LOG);
        let before = core::ptr::read_volatile((base + 0x2018) as *const u64);
        let buf_before = core::ptr::read_volatile(log as *const u64);
        let w0 = core::ptr::read_volatile(dte as *const u64);
        core::ptr::write_volatile(dte as *mut u64, w0 | 0b100);
        // A FRESH IOVA. Reusing one that already translated successfully would be answered
        // from the unit's cache without the device table being consulted, so the deliberately
        // invalid entry below would never be looked at and the control would prove nothing.
        let _ = run(0x4_0000, 0x9000, true);
        let after = core::ptr::read_volatile((base + 0x2018) as *const u64);
        let buf_after = core::ptr::read_volatile(log as *const u64);
        let buf_next = core::ptr::read_volatile((log + 8) as *const u64);
        core::ptr::write_volatile(dte as *mut u64, w0);
        let _ = writeln!(
            con,
            "[iommu] CONTROL invalid-DTE: tail {before:#x} -> {after:#x}, log word0 \
             {buf_before:#018x} -> {buf_after:#018x}, word1 {buf_next:#018x}"
        );
    }

    // ---- POSITIVE CONTROL, take two: an ILLEGAL COMMAND ----
    //
    // The DTE probe above cannot answer the question. QEMU's trace shows `amdvi_invalid_dte`
    // firing for it — the entry IS rejected — and still no event is written, so this emulator
    // does not report that class either, and a second silence adds nothing.
    //
    // An unknown command opcode is a different path, and the one whose logging is actually
    // evidenced: the command buffer works end to end here (COMPLETION_WAIT's store lands), so
    // if an event never appears for a command the unit itself calls illegal, the log is ours.
    // Done last, because an illegal command may stop the command processor.
    {
        let log = *core::ptr::addr_of!(EVENT_LOG);
        let before = core::ptr::read_volatile(log as *const u64);
        let _ = iommu_cmd(base, [0, 0xF << 28, 0, 0]);
        // The unit consumes the ring asynchronously; give it a bounded chance to be seen.
        for _ in 0..1_000_000u64 {
            if core::ptr::read_volatile(log as *const u64) != before {
                break;
            }
            core::hint::spin_loop();
        }
        let after = core::ptr::read_volatile(log as *const u64);
        // Scan the WHOLE ring, not word 0. If the unit wrote at a nonzero tail offset — a
        // stale internal tail, a different base than the one we programmed — checking only the
        // first entry would report silence for a log that is in fact being written.
        let mut nonzero_at = -1i64;
        for i in 0..512u64 {
            if core::ptr::read_volatile((log + i * 8) as *const u64) != 0 {
                nonzero_at = (i * 8) as i64;
                break;
            }
        }
        let head = core::ptr::read_volatile((base + 0x2010) as *const u64);
        let tail = core::ptr::read_volatile((base + 0x2018) as *const u64);
        // The unit's own STATUS register, which is what settles WHY nothing is logged. The
        // emulator's log path has exactly three early-outs — logging disabled, the overflow
        // bit already set, or a zero-length ring — and two of them are visible right here:
        // EventLogRun (bit 3) is set iff it considers logging enabled, and EventOverflow
        // (bit 0) is the second. Reading it beats inferring from silence.
        let status = core::ptr::read_volatile((base + 0x2020) as *const u64);
        let _ = writeln!(
            con,
            "[iommu] STATUS {status:#018x} (EventOverflow={}, EventLogRun={}, CmdBufRun={})",
            status & 1,
            (status >> 3) & 1,
            (status >> 4) & 1
        );
        if after != before {
            let _ = writeln!(
                con,
                "[iommu] EVENT LOG WORKS: an illegal command was RECORDED — log word0 \
                 {after:#018x}, head {head:#x} tail {tail:#x}"
            );
        } else {
            let _ = writeln!(
                con,
                "[iommu] event log wrote NOTHING even for an illegal command (head {head:#x} \
                 tail {tail:#x}, first nonzero byte in the 4 KiB ring: {nonzero_at})"
            );
        }
    }

    // The PAYLOAD is the oracle, not the event log. Measured both ways with the same code and
    // the same device, differing only in whether the unit was translating:
    //   translation OFF -> target reads 0xd1ce...   (the transfer lands)
    //   translation ON  -> target still reads the SENTINEL it was filled with (refused)
    // That difference is the containment result. The event log is a separate question and is
    // reported separately, because a refusal that is not RECORDED is still a refusal.
    if wrote != SENTINEL {
        let _ = writeln!(
            con,
            "[iommu] NOT CONTAINED: the target frame changed ({wrote:#018x}) — the device wrote \
             to memory it was not granted"
        );
    } else {
        let _ = writeln!(
            con,
            "[iommu] CONTAINED: the transfer completed at the device and the target frame is \
             UNTOUCHED (still {wrote:#018x})"
        );
    }

    if tail_after != tail_before {
        let log = *core::ptr::addr_of!(EVENT_LOG);
        let e0 = core::ptr::read_volatile(log as *const u64);
        let e1 = core::ptr::read_volatile((log + 8) as *const u64);
        let kind = (e1 >> 60) & 0xF;
        let _ = writeln!(
            con,
            "[iommu] REFUSED AND RECORDED: {e0:#018x} {e1:#018x} type={kind:#x}{}",
            if kind == 0x2 { " (IO_PAGE_FAULT)" } else { "" }
        );
    } else {
        let _ = writeln!(
            con,
            "        (no event logged — the refusal is real but the unit is not RECORDING it; \
             event-log setup is unfinished)"
        );
    }
}

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

    // Locate the IOMMU. The nucleus must do this itself and no one else may: the whole
    // untrusted-driver story turns on the driver never holding an `Mmio` capability for this
    // aperture (docs/nucleus-design.md). Reported either way — the runner requires the line
    // that matches the rig it launched, so a scan that silently found nothing fails the run
    // rather than reading as "this machine has no IOMMU".
    #[cfg(target_arch = "x86_64")]
    {
        // Under FIRMWARE (multiboot) there is no `rsdp_paddr` field to read: the pointer was
        // never handed to us, because firmware placed the tables itself. Scan the BIOS window
        // for it instead, validating rather than signature-matching.
        #[cfg(feature = "firmware-boot")]
        let found = {
            let n = (acpi::BIOS_SCAN_END - acpi::BIOS_SCAN_START) as usize;
            // SAFETY: the window is below 1 MiB and inside the identity map.
            let win = unsafe { core::slice::from_raw_parts(acpi::BIOS_SCAN_START as *const u8, n) };
            acpi::scan_for_rsdp(win).map(|off| acpi::BIOS_SCAN_START + off as u64)
        };
        // SAFETY: dereferences the boot-info pointer, same as the memory map does.
        #[cfg(not(feature = "firmware-boot"))]
        let found = unsafe { pvh::rsdp(a0) };
        match found {
            Some(p) => {
                // SAFETY: `pvh::rsdp` bounded it to the identity-mapped window.
                let buf = unsafe { core::slice::from_raw_parts(p as *const u8, 36) };
                match acpi::parse_rsdp(buf) {
                    Ok(r) => {
                        let _ = writeln!(
                            con,
                            "[acpi] RSDP at {p:#x} rev {} xsdt={:#x} rsdt={:#x}",
                            r.revision,
                            r.xsdt.unwrap_or(0),
                            r.rsdt
                        );
                        report_ivrs::<A>(&mut con, r.rsdt as u64);
                    }
                    Err(e) => {
                        let _ = writeln!(
                            con,
                            "[acpi] RSDP at {p:#x} REFUSED: {e:?} — not trusting a structure \
                             that does not check out"
                        );
                    }
                }
            }
            None => {
                let raw = unsafe { pvh::rsdp_raw(a0) };
                let _ = writeln!(
                    con,
                    "[acpi] no RSDP (rsdp_paddr={raw:#x}) — no ACPI on THIS host's QEMU build"
                );
                let _ = writeln!(
                    con,
                    "       QEMU builds the tables but delivers them over fw_cfg for FIRMWARE \
                     to fetch and place;"
                );
                let _ = writeln!(
                    con,
                    "       a -kernel/PVH boot runs none. QEMU 8.2.2 on shark-a DOES supply \
                     one, so this is a"
                );
                let _ = writeln!(
                    con,
                    "       property of the QEMU build, not of PVH. See docs/verification.md."
                );
            }
        }
        // Finding the device to bound is a PCI scan and does not depend on ACPI, so it is
        // reported on every machine — including the one with no ACPI, which is the only place
        // a scan bug would otherwise go unseen.
        // SAFETY: single-CPU boot path; nothing else performs a config cycle.
        match unsafe { pci::find_dma_device() } {
            Some(d) => {
                let _ = writeln!(
                    con,
                    "[iommu] target {:02x}:{:02x}.{} vendor={:04x} bdf={:#06x} (DMA-capable)",
                    d.bus,
                    d.dev,
                    d.func,
                    d.vendor,
                    d.bdf()
                );
                // SAFETY: as above.
                unsafe { TARGET_BDF = d.bdf() as u32 | 0x1_0000 };
                // SAFETY: as above. Zero if the firmware assigned no BAR, which the mint-time
                // resolution below treats as "no such capability" rather than as address 0.
                if let Some(bar) = unsafe { pci::bar0(&d) } {
                    unsafe { DEVICE_BAR = bar };
                }
            }
            None => {
                let _ = writeln!(con, "[iommu] no DMA-capable device to bound");
            }
        }
        // SAFETY: single-CPU boot path; nothing else performs a config cycle.
        match unsafe { pci::find_iommu() } {
            Some(f) => {
                let _ = writeln!(
                    con,
                    "[iommu] AMD-Vi at {:02x}:{:02x}.{} vendor={:04x} device={:04x} bdf={:#06x}",
                    f.bus,
                    f.dev,
                    f.func,
                    f.vendor,
                    f.device,
                    f.bdf()
                );
                // SAFETY: as above — single-CPU boot path, no concurrent config cycle.
                match unsafe { pci::amd_vi_cap(&f) } {
                    Some(c) => match c.base {
                        Some(base) => {
                            let _ = writeln!(
                                con,
                                "        registers at {base:#x} (aperture enabled), hdr={:#010x}",
                                c.header
                            );
                        }
                        None => {
                            let _ = writeln!(
                                con,
                                "        capability found (hdr={:#010x}) but its base register \
                                 is UNPROGRAMMED",
                                c.header
                            );
                            let _ = writeln!(
                                con,
                                "        firmware normally assigns it; this boot is -kernel/PVH \
                                 with none, so the base must come from ACPI IVRS"
                            );
                        }
                    },
                    None => {
                        let _ = writeln!(
                            con,
                            "        no AMD-Vi capability block — cannot reach its registers"
                        );
                    }
                }
                let _ = writeln!(
                    con,
                    "        (located only; NO Device Table, NO I/O page tables, NOT programmed)"
                );
            }
            None => {
                let _ = writeln!(con, "[iommu] no IOMMU on this machine");
            }
        }
    }

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
            // The aperture is above the identity map, so it is unreachable until now, and it
            // must be UNCACHED — the first mapping in this tree that needs `Perms::device`.
            #[cfg(target_arch = "x86_64")]
            {
                let base = *core::ptr::addr_of!(AMDVI_BASE);
                if base != 0 {
                    let mut space = A::Space::from_token(ktoken);
                    // FOUR pages, not one. The control and capability registers live in the
                    // first page, but the event-log HEAD and TAIL are at 0x2010/0x2018 — the
                    // third page. Mapping one page reads those as unmapped memory, which
                    // faulted the boot as soon as anything asked whether an event had been
                    // logged. The aperture is 512 KiB; four pages is what this code touches.
                    const APERTURE_PAGES: u64 = 4;
                    let mut mapped = true;
                    for i in 0..APERTURE_PAGES {
                        let off = i * abi::PAGE_SIZE;
                        mapped &= space.map_page(
                            abi::VirtAddr(base + off),
                            abi::PhysAddr(base + off),
                            Perms::KERNEL_DEVICE,
                            &mut fa,
                        );
                    }
                    if mapped {
                        // Editing the ACTIVE space: without this the mapping silently does
                        // not take effect and the reads below return whatever was cached.
                        A::flush_tlb();
                        let efr = core::ptr::read_volatile((base + 0x30) as *const u64);
                        let ctrl = core::ptr::read_volatile((base + 0x18) as *const u64);
                        let _ = writeln!(
                            con,
                            "[iommu] aperture mapped uncached at {base:#x}: EFR={efr:#018x} \
                             CTRL={ctrl:#018x}"
                        );
                        // ---- first WRITE to the IOMMU: install a Device Table ----
                        //
                        // AMD-Vi indexes this table by BDF, and its base register names ONE
                        // physical address plus a size, so the memory must be contiguous —
                        // which is why `mm::alloc_contiguous` had to exist first. 2 MiB covers
                        // the full 64K-BDF space at 32 bytes per entry.
                        //
                        // Allocated HERE, before `FREE_AT_START` is captured, because it is
                        // never freed: taking it afterwards would read as a 512-frame leak to
                        // the conservation check at exit.
                        const DT_PAGES: usize = 512;
                        match fa.alloc_contiguous(DT_PAGES) {
                            Some(dt) => {
                                // Every entry zero = V=0 = no device is described yet. The
                                // table must be quiescent before its base is published,
                                // because the unit may fetch from it the moment it is enabled.
                                for i in 0..DT_PAGES {
                                    let _ = zero_frame(abi::PhysAddr(
                                        dt.as_u64() + (i as u64) * abi::PAGE_SIZE,
                                    ));
                                }
                                // Size field is (entries*32/4096)-1; 2 MiB -> 511.
                                let val =
                                    (dt.as_u64() & 0x000F_FFFF_FFFF_F000) | (DT_PAGES as u64 - 1);
                                core::ptr::write_volatile(base as *mut u64, val);
                                // READ BACK. A blind write has no oracle — this is the only
                                // thing that distinguishes "the unit accepted it" from "the
                                // store went into a hole and nothing happened".
                                let got = core::ptr::read_volatile(base as *const u64);
                                if got == val {
                                    let _ = writeln!(
                                        con,
                                        "[iommu] device table {:#x} (2 MiB, 64K BDFs) \
                                         installed; DTBR reads back {got:#018x}",
                                        dt.as_u64()
                                    );
                                    program_dte::<A>(&mut con, &mut fa, base, dt.as_u64());
                                } else {
                                    let _ = writeln!(
                                        con,
                                        "[iommu] DTBR write did NOT take: wrote {val:#018x}, \
                                         read {got:#018x}"
                                    );
                                }
                            }
                            None => {
                                let _ = writeln!(
                                    con,
                                    "[iommu] no 2 MiB contiguous run for a device table"
                                );
                            }
                        }
                        let _ = writeln!(
                            con,
                            "        (table is EMPTY and the unit is NOT enabled — no \
                             translation is in effect)"
                        );
                    } else {
                        let _ = writeln!(
                            con,
                            "[iommu] could not map the AMD-Vi aperture at {base:#x}"
                        );
                    }
                }
            }
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
    // Withdraw the DMA this process asked for FIRST, and do it by attribution rather than by
    // ownership. Destroying the regions it OWNS clears their entries as a side effect, which is
    // why this looked handled — but a mapping of a BORROWED region belongs to no region this
    // process owns, and outlived it. Correct by construction now instead of by luck.
    #[cfg(target_arch = "x86_64")]
    withdraw_all_dma_of(idx);
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
            // `WRITE` because spawning CONSUMES: it allocates an address space, a stack and an
            // image. Not "out of the untyped region" — there is no such region; an `Untyped`
            // names no extent. And the frames come from the GENERAL pool via `alloc_frame`,
            // not the DMA arena `make_region` draws from, which `mm` keeps strictly disjoint.
            let authorized = proc_at(cur).caps.lookup(cap).is_some_and(|s| {
                is_mint_source(s.cap_type) && s.rights.contains(abi::CapRights::WRITE)
            });
            // Optional capability delegation: `a1` = one of the CALLER's capabilities to
            // hand to the child (or NO_DELEGATE), `a2` = the rights to hand over.
            // Authority-monotonic, exactly as `CapSpace::derive` is within one space: the
            // child receives `caller_rights ∩ requested`, so a parent can attenuate but
            // never amplify — asking for more than it holds yields only what it holds.
            // The lookup copies the slot out, so no borrow of `PROCS[cur]` stays live.
            let deleg_arg = A::frame_arg(&f, 1);
            let want_deleg = deleg_arg != abi::sysno::NO_DELEGATE;
            let requested = abi::CapRights::from_user(A::frame_arg(&f, 2));
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
            let irq = caps_irq_line(&proc_at(cur).caps, cap);
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
            let untyped = proc_at(cur)
                .caps
                .lookup(cap)
                .filter(|s| is_mint_source(s.cap_type) && s.rights.contains(abi::CapRights::WRITE));
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
            let named = caps_region(&proc_at(cur).caps, cap, abi::CapRights::READ);
            let ret = match named {
                None => abi::syserr::NO_CAP,
                // The holder's OWN rights decide the mapping's permissions, never the
                // request's — see `caps_region`.
                Some((object, rights)) => map_region::<A>(cur, object, rights),
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
        abi::sysno::MAP_DMA => {
            // TWO capabilities, because two separate authorities are involved: over the
            // device's DMA domain, and over the memory. Holding one without the other is not
            // enough — a process that may drive a device must not thereby be able to point it
            // at memory it was never lent, and one holding memory must not be able to hand it
            // to a device it has no authority over.
            let dom_cap = abi::CapId(A::frame_arg(&f, 0) as usize);
            let reg_cap = abi::CapId(A::frame_arg(&f, 1) as usize);
            let caps = &proc_at(cur).caps;
            let ret = match (
                caps_iommu_domain(caps, dom_cap, abi::CapRights::WRITE),
                caps_region(caps, reg_cap, abi::CapRights::READ),
            ) {
                // The HOLDER's rights decide what the device may do, never the request's.
                (Some(domain), Some((region, rights))) => {
                    #[cfg(target_arch = "x86_64")]
                    {
                        map_dma(cur, domain, region, rights)
                    }
                    #[cfg(not(target_arch = "x86_64"))]
                    {
                        let _ = (domain, region, rights);
                        abi::syserr::NO_MEM
                    }
                }
                _ => abi::syserr::NO_CAP,
            };
            A::frame_set_ret(&mut f, ret);
        }
        abi::sysno::UNMAP_DMA => {
            let dom_cap = abi::CapId(A::frame_arg(&f, 0) as usize);
            let reg_cap = abi::CapId(A::frame_arg(&f, 1) as usize);
            let caps = &proc_at(cur).caps;
            let ret = match (
                caps_iommu_domain(caps, dom_cap, abi::CapRights::WRITE),
                caps_region(caps, reg_cap, abi::CapRights::READ),
            ) {
                (Some(domain), Some((region, _))) => {
                    #[cfg(target_arch = "x86_64")]
                    {
                        unmap_dma(cur, domain, region)
                    }
                    #[cfg(not(target_arch = "x86_64"))]
                    {
                        let _ = (domain, region);
                        abi::syserr::NO_MEM
                    }
                }
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
            let named = caps_region(&proc_at(cur).caps, cap, abi::CapRights::WRITE);
            let me = proc_at(cur).id;
            let ret = match named.and_then(|(object, _)| region_slot(object)) {
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
            let irq = caps_irq_line(&proc_at(cur).caps, cap);
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
/// `MAKE_REGION` mints a `Region` out of an `Untyped` in the SAME capability space, and that
/// relation is recorded nowhere — deliberately. An `Untyped` here names no extent (its object
/// is always zero), so there is no naming relation for revocation to tear down: revoking it
/// removes the ability to acquire more, not what was already acquired.
///
/// This block used to call that a KNOWN GAP in the "revocation tears down the authority it
/// granted" doctrine. It is not. `SPAWN` is gated on the same capability with the same right
/// and retains strictly more authority than a region — a whole process with its own address
/// space, capabilities and regions — so reclaiming regions here while spawned processes kept
/// running would give the kernel two answers to one question. See docs/nucleus-design.md §1.2
/// for the decision and its reversal condition.
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
    // No grant here. Creating a region is allocating MEMORY; it is not a decision that a
    // device may reach it. Granting at allocation made every region DMA-authorized for the
    // device whether or not anyone had asked, which is authority nobody requested and nobody
    // could decline — and it made `grant_count` a restatement of the region table rather than
    // a record of what had been handed out. The grant is issued by `MAP_DMA`, from the
    // capability of whoever asks, and withdrawn again by `UNMAP_DMA` and `FREE_REGION`.
    let _ = rights;
    // TRIPWIRE, at the site, because nothing downstream can see this. Restoring the old grant
    // loop here passes every other check in the tree: the grants are not orphans (their regions
    // are live), containment holds, and by the time the shutdown checks run every region has
    // been freed and its grants revoked with it. Measured — that mutant survived both of them.
    // An `assert!` and not `debug_assert!`: the release build is the one that boots.
    for k in 0..pages as usize {
        for di in 0..MAX_DOMAINS {
            assert!(
                domain_at(di)
                    .granted(frames[k].as_u64() >> abi::PAGE_SHIFT)
                    .is_none(),
                "MAKE_REGION handed a device DMA authority for a frame nobody asked to map"
            );
        }
    }
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
            #[cfg(target_arch = "x86_64")]
            let bar = *core::ptr::addr_of!(DEVICE_BAR);
            #[cfg(not(target_arch = "x86_64"))]
            let bar = 0u64;
            // A real register aperture must be mapped UNCACHED, or the stores a device is meant
            // to see in order are reordered and coalesced — invisible under QEMU TCG and wrong
            // on silicon. The stand-in is ordinary RAM and stays cacheable.
            let is_device = bar != 0 && phys == bar;
            let ok = (window != 0 && phys == window) || is_device;
            if !ok || pages > DEVICE_PAGES {
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
            let mut perms = if writable {
                Perms::USER_RW
            } else {
                Perms::USER_RO
            };
            perms.device = is_device;
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

// ============================================================ host tests
//
// The kernel had NO host tests at all — 2360 lines, the largest crate in the tree, checked
// only by one scripted QEMU boot. That was not a considered exemption: `tools/host-tests.sh`
// guards against a tested crate going unlisted, but it fires on the PRESENCE of `#[test]`,
// so a crate with none can never trip it. "Cannot build for the host" and "has nothing worth
// testing" had been conflated, and the first was not even true — only a missing off-target
// `sched::Context` stood in the way (see crates/sched/src/context_host.rs).
//
// What belongs here is the kernel's DECISION logic, not its effects: anything that maps a
// page, switches a stack, or touches a register belongs under QEMU and stays there. The
// boot grant tables are the clearest such case — they are pure data, they are the ROOT of
// every capability that can ever exist, and until now nothing but the demo's own success
// depended on them being right.
#[cfg(test)]
mod tests {
    use super::*;

    const ROLES: [Role; 4] = [Role::Producer, Role::Consumer, Role::Worker, Role::Child];

    /// Rights a role holds on a given endpoint object, unioned across its whole table.
    fn endpoint_rights(role: Role, object: u64) -> abi::CapRights {
        let mut bits = 0u8;
        for &(t, r, o) in grants_for(role) {
            if t == abi::CapType::Endpoint && o == object {
                bits |= r.0;
            }
        }
        abi::CapRights(bits)
    }

    fn holds_type(role: Role, want: abi::CapType) -> bool {
        grants_for(role).iter().any(|&(t, _, _)| t == want)
    }

    /// THE separation property. A producer that is compromised must not be able to map a
    /// device, allocate memory, spawn, or wait on an interrupt — and the only thing that
    /// makes that true is the absence of those capability TYPES from its table.
    ///
    /// Stated as "which roles may hold a privileged type" rather than as the table's
    /// contents, so it fails on an authority change and not on a cosmetic one.
    #[test]
    fn only_the_worker_holds_device_memory_or_interrupt_authority() {
        for role in ROLES {
            for privileged in [abi::CapType::Mmio, abi::CapType::Untyped, abi::CapType::Irq] {
                let held = holds_type(role, privileged);
                let allowed = matches!(role, Role::Worker);
                assert!(
                    !held || allowed,
                    "the {} role holds a {:?} capability; only the worker may",
                    role_name(role),
                    privileged
                );
            }
        }
    }

    /// No role may hold both halves of one endpoint. A role carrying READ and WRITE on the
    /// same object could rendezvous with ITSELF, which makes the producer/consumer split
    /// decorative: the demo's "a producer cannot receive" is then a fact about the demo
    /// rather than about authority.
    #[test]
    fn no_role_holds_both_ends_of_the_same_endpoint() {
        for role in ROLES {
            for object in 0..4u64 {
                let r = endpoint_rights(role, object);
                assert!(
                    !(r.contains(abi::CapRights::READ) && r.contains(abi::CapRights::WRITE)),
                    "the {} role holds both READ and WRITE on endpoint {}, so it can \
                     rendezvous with itself",
                    role_name(role),
                    object
                );
            }
        }
    }

    /// `SPAWN` must not be able to mint authority. A child's table is empty, so everything a
    /// spawned process can do came from an explicit, rights-intersected delegation.
    #[test]
    fn a_spawned_child_starts_with_no_authority_of_its_own() {
        assert!(
            grants_for(Role::Child).is_empty(),
            "Role::Child's grant table is not empty: spawning now mints authority by itself"
        );
    }

    /// The claim `NO_AUTHORITY` exists to make: the placeholder is a real lookup HIT that
    /// nonetheless passes no gate. Possession is not authority.
    #[test]
    fn the_placeholder_capability_confers_nothing() {
        let (t, rights, _) = NO_AUTHORITY;
        assert_eq!(
            t,
            abi::CapType::Endpoint,
            "placeholder must still be a real slot"
        );
        for needed in [
            abi::CapRights::READ,
            abi::CapRights::WRITE,
            abi::CapRights::GRANT,
        ] {
            assert!(
                !rights.contains(needed),
                "NO_AUTHORITY passes a gate for {:?}",
                needed
            );
        }
    }

    /// Why the placeholder is there at all: entry `i` becomes `CapId(i)`, so the shared
    /// endpoint must sit at `CapId(0)` in every role that has any table. Drop the
    /// placeholder from the worker and its device authority silently shifts down a slot.
    #[test]
    fn the_shared_endpoint_is_capid_zero_in_every_non_empty_role() {
        for role in ROLES {
            let table = grants_for(role);
            if table.is_empty() {
                continue;
            }
            let (t, _, object) = table[0];
            assert_eq!(
                (t, object),
                (abi::CapType::Endpoint, 0),
                "the {} role's CapId(0) is not the shared endpoint, so CapId(1..) no longer \
                 line up across roles",
                role_name(role)
            );
        }
    }

    /// Protects a TESTING property rather than a safety one, which is why it is easy to
    /// delete by accident. The worker deliberately holds under-powered caps of the right
    /// TYPE — an `Untyped` without WRITE, an `Mmio` without READ — so that the rights half
    /// of every gate is exercised on hardware instead of being vacuously true. Tidy those
    /// away and the demo still passes while checking strictly less.
    #[test]
    fn the_worker_holds_an_under_powered_cap_of_each_privileged_type() {
        for privileged in [abi::CapType::Mmio, abi::CapType::Untyped] {
            let entries: Vec<abi::CapRights> = grants_for(Role::Worker)
                .iter()
                .filter(|&&(t, _, _)| t == privileged)
                .map(|&(_, r, _)| r)
                .collect();
            assert!(
                entries.iter().any(|r| *r == abi::CapRights::ALL),
                "the worker holds no fully-powered {:?}",
                privileged
            );
            assert!(
                entries.iter().any(|r| *r != abi::CapRights::ALL),
                "every {:?} the worker holds carries ALL rights, so the rights half of that \
                 gate is never exercised — the on-hardware refusal becomes vacuous",
                privileged
            );
        }
    }

    /// The boot policy must put the two halves of the IPC pair in DIFFERENT processes.
    #[test]
    fn boot_role_splits_the_ipc_pair_across_processes() {
        let p = boot_role(0);
        let c = boot_role(1);
        assert!(
            endpoint_rights(p, 0).contains(abi::CapRights::WRITE),
            "boot process 0 cannot send on the shared endpoint"
        );
        assert!(
            endpoint_rights(c, 0).contains(abi::CapRights::READ),
            "boot process 1 cannot receive on the shared endpoint"
        );
        // And every later process is a worker, which holds neither half.
        for i in 2..6u64 {
            let r = endpoint_rights(boot_role(i), 0);
            assert!(
                !r.contains(abi::CapRights::READ) && !r.contains(abi::CapRights::WRITE),
                "boot process {} has authority on the shared endpoint",
                i
            );
        }
    }

    // ------------------------------------------------------- authority predicates
    //
    // These cover gates the QEMU boot demonstrably CANNOT see. Measured before writing them:
    // deleting the READ requirement from `holds_mmio`, and separately from the MAP_REGION
    // Region gate, each leaves the x86 boot at `RESULT: PASS`. The grant tables contain the
    // discriminating capabilities — the worker holds an `Mmio` without READ — but the demo
    // reaches them from the wrong process, so the rights half of the gate never decides
    // anything on hardware.

    fn space(entries: &[(abi::CapType, abi::CapRights, u64)]) -> capabilities::CapSpace<CAP_SLOTS> {
        let mut cs = capabilities::CapSpace::new();
        for &(t, r, o) in entries {
            cs.insert(t, r, o).expect("test space overflow");
        }
        cs
    }

    /// Possession is not authority. An `Mmio` capability without READ must not keep a device
    /// window alive — this is the exact case the boot cannot see.
    #[test]
    fn an_mmio_capability_without_read_is_not_authority_to_stay_mapped() {
        let full = space(&[(abi::CapType::Mmio, abi::CapRights::ALL, 0xE000_0000)]);
        assert!(caps_hold_mmio(&full), "a full Mmio cap is authority");

        let write_only = space(&[(abi::CapType::Mmio, abi::CapRights::WRITE, 0xE000_0000)]);
        assert!(
            !caps_hold_mmio(&write_only),
            "an Mmio without READ counted as authority to stay mapped"
        );

        // The worker's actual shape: a full cap AND an under-powered one. Revoking the full
        // one must drop the authority, rather than the leftover propping the mapping up.
        let mut both = space(&[
            (abi::CapType::Mmio, abi::CapRights::ALL, 0xE000_0000),
            (abi::CapType::Mmio, abi::CapRights::WRITE, 0xE000_0000),
        ]);
        assert!(caps_hold_mmio(&both));
        both.revoke(abi::CapId(0));
        assert!(
            !caps_hold_mmio(&both),
            "revoking the only READ-bearing Mmio left the process still 'holding' one"
        );
    }

    /// A capability of the wrong TYPE is not authority either, whatever its rights.
    #[test]
    fn only_an_mmio_capability_counts_as_device_authority() {
        for other in [
            abi::CapType::Untyped,
            abi::CapType::Endpoint,
            abi::CapType::Region,
            abi::CapType::Irq,
        ] {
            let cs = space(&[(other, abi::CapRights::ALL, 0xE000_0000)]);
            assert!(
                !caps_hold_mmio(&cs),
                "a {:?} with ALL rights counted as device authority",
                other
            );
        }
    }

    /// Interrupt authority is PER LINE. Holding line 0 must confer nothing on line 1 — with a
    /// single line in the system this property is vacuously true, which is why the grant
    /// tables deliberately carry two.
    #[test]
    fn interrupt_authority_does_not_cross_lines_or_survive_without_read() {
        let timer = space(&[(abi::CapType::Irq, abi::CapRights::READ, IRQ_TIMER)]);
        assert!(caps_hold_irq(&timer, IRQ_TIMER));
        assert!(
            !caps_hold_irq(&timer, IRQ_CONSOLE),
            "a capability for one line granted authority over another"
        );

        let no_read = space(&[(abi::CapType::Irq, abi::CapRights::WRITE, IRQ_TIMER)]);
        assert!(
            !caps_hold_irq(&no_read, IRQ_TIMER),
            "an Irq capability without READ credited its line"
        );

        // A capability of the WRONG TYPE whose object collides with a line. This is not a
        // contrived shape: endpoint objects are 0 and 1 and the interrupt lines are 0 (timer)
        // and 1 (console), so they collide EXACTLY — the worker holds an Endpoint on object 1
        // and an Irq on line 1 at the same time. Without the type check, its endpoint
        // capability would confer console-interrupt authority. Deleting `s.cap_type ==
        // CapType::Irq` left the whole kernel suite green until this case existed.
        for wrong in [
            abi::CapType::Endpoint,
            abi::CapType::Region,
            abi::CapType::Mmio,
            abi::CapType::Untyped,
        ] {
            let decoy = space(&[(wrong, abi::CapRights::ALL, IRQ_CONSOLE)]);
            assert!(
                !caps_hold_irq(&decoy, IRQ_CONSOLE),
                "a {:?} capability on object {} counted as interrupt authority",
                wrong,
                IRQ_CONSOLE
            );
        }
    }

    /// Endpoint resolution must enforce type and rights, and must return the capability's
    /// OBJECT rather than its slot id — two processes rendezvous only when their caps name
    /// the same endpoint, whatever slot each holds it in.
    #[test]
    fn endpoint_resolution_enforces_type_and_rights_and_returns_the_object() {
        // Slot 0 is a decoy of the wrong type; the endpoint lives at slot 1 naming object 7.
        let cs = space(&[
            (abi::CapType::Mmio, abi::CapRights::ALL, 0),
            (abi::CapType::Endpoint, abi::CapRights::WRITE, 7),
        ]);
        assert_eq!(
            caps_endpoint_object(&cs, 1, abi::CapRights::WRITE),
            Some(7),
            "must return the endpoint OBJECT, not the slot id"
        );
        assert_eq!(
            caps_endpoint_object(&cs, 1, abi::CapRights::READ),
            None,
            "a send-only capability resolved for a receive"
        );
        assert_eq!(
            caps_endpoint_object(&cs, 0, abi::CapRights::WRITE),
            None,
            "a non-Endpoint capability resolved as an endpoint"
        );
        assert_eq!(
            caps_endpoint_object(&cs, 9, abi::CapRights::WRITE),
            None,
            "an empty slot resolved"
        );
    }

    /// The stranding check after a revocation: a second capability to the SAME endpoint keeps
    /// a blocked process legitimately waiting, but one naming a different endpoint does not.
    #[test]
    fn holding_an_endpoint_is_specific_to_its_object() {
        let cs = space(&[(abi::CapType::Endpoint, abi::CapRights::READ, 3)]);
        assert!(caps_hold_endpoint(&cs, 3, abi::CapRights::READ));
        assert!(
            !caps_hold_endpoint(&cs, 4, abi::CapRights::READ),
            "a capability for endpoint 3 answered for endpoint 4"
        );
        assert!(
            !caps_hold_endpoint(&cs, 3, abi::CapRights::WRITE),
            "a receive-only capability answered a send query"
        );

        // Wrong TYPE, right object and rights — the same collision as above, from the other
        // side: an `Irq` capability for line 3 must not answer "holds endpoint 3". Deleting
        // the type check here also left the suite green.
        for wrong in [
            abi::CapType::Irq,
            abi::CapType::Region,
            abi::CapType::Mmio,
            abi::CapType::Untyped,
        ] {
            let decoy = space(&[(wrong, abi::CapRights::ALL, 3)]);
            assert!(
                !caps_hold_endpoint(&decoy, 3, abi::CapRights::READ),
                "a {:?} capability on object 3 answered an endpoint query",
                wrong
            );
        }
    }

    /// The DMA gate: WRITE on the domain, and the right TYPE.
    ///
    /// `MAP_DMA` hands a device the ability to reach memory, which is granting authority. A
    /// capability that merely names a domain must not extend it, and no other capability type
    /// may stand in for one — an `Mmio` cap for the same device is authority over its
    /// REGISTERS, which is a different thing from authority over what it may reach by DMA.
    #[test]
    fn granting_dma_reach_needs_a_domain_cap_carrying_write() {
        // Decoy at slot 0 so a slot/object confusion cannot pass by coincidence.
        let cs = space(&[
            (abi::CapType::Mmio, abi::CapRights::ALL, 0),
            (abi::CapType::IommuDomain, abi::CapRights::READ, 7),
            (abi::CapType::IommuDomain, abi::CapRights::ALL, 7),
        ]);
        assert_eq!(
            caps_iommu_domain(&cs, abi::CapId(1), abi::CapRights::WRITE),
            None,
            "a domain capability without WRITE granted DMA reach"
        );
        assert_eq!(
            caps_iommu_domain(&cs, abi::CapId(2), abi::CapRights::WRITE),
            Some(7),
            "a fully-powered domain capability must resolve, to its OBJECT not its slot"
        );
        assert_eq!(
            caps_iommu_domain(&cs, abi::CapId(0), abi::CapRights::WRITE),
            None,
            "an Mmio capability resolved as an IOMMU domain"
        );
        assert_eq!(
            caps_iommu_domain(&cs, abi::CapId(9), abi::CapRights::WRITE),
            None,
            "an empty slot resolved"
        );
    }

    /// A capability's domain OBJECT must name a domain that EXISTS, and resolve to that one.
    ///
    /// This shipped decorative once: `map_dma` took no domain at all, so a capability naming
    /// domain 999 granted DMA reach into the real one and the boot passed.
    #[test]
    fn a_capability_resolves_to_the_domain_it_names_and_no_other() {
        let ids = [1u64, 2];
        assert_eq!(domain_lookup(&ids, 1), Some(0));
        assert_eq!(
            domain_lookup(&ids, 2),
            Some(1),
            "the second domain is reachable"
        );
        assert_eq!(domain_lookup(&ids, 3), None, "an unknown object resolved");
        assert_eq!(domain_lookup(&ids, 999), None);

        // Unclaimed slots carry 0, and so does a zeroed capability. That coincidence must
        // never become authority — over an empty table or a partly filled one.
        assert_eq!(
            domain_lookup(&[0, 0], 0),
            None,
            "domain 0 resolved on an empty table"
        );
        assert_eq!(
            domain_lookup(&[1, 0], 0),
            None,
            "domain 0 resolved to an unclaimed slot"
        );
        for named in [1u64, 2, 999] {
            assert_eq!(
                domain_lookup(&[0, 0], named),
                None,
                "domain {named} was usable before any domain existed"
            );
        }
        // A slot that exists must not answer for its neighbour.
        assert_eq!(domain_lookup(&[1, 0], 2), None);
        assert_eq!(domain_lookup(&[0, 2], 2), Some(1));
    }

    /// The two Region gates, both of which the boot cannot see (measured: deleting either
    /// leaves `RESULT: PASS`).
    ///
    /// `FREE_REGION` is the one that matters most. Requiring WRITE is what stops a process
    /// holding a READ-only LOAN from destroying memory it was merely lent — ownership is
    /// checked separately, by identity, but the rights half is the part that was decorative.
    #[test]
    fn a_read_only_region_loan_can_be_mapped_but_not_destroyed() {
        let loan = space(&[(abi::CapType::Region, abi::CapRights::READ, 42)]);
        assert_eq!(
            caps_region(&loan, abi::CapId(0), abi::CapRights::READ),
            Some((42, abi::CapRights::READ)),
            "a READ loan must still be mappable"
        );
        assert_eq!(
            caps_region(&loan, abi::CapId(0), abi::CapRights::WRITE),
            None,
            "a READ-only loan resolved for FREE_REGION — it can destroy what it was lent"
        );

        let owned = space(&[(abi::CapType::Region, abi::CapRights::ALL, 42)]);
        assert!(
            caps_region(&owned, abi::CapId(0), abi::CapRights::WRITE).is_some(),
            "a fully-powered region cap must be destroyable by its owner"
        );
    }

    /// A region capability must resolve to its region ID, not its slot, and must hand back the
    /// holder's OWN rights — the mapping's permissions are derived from them, so returning
    /// anything wider is amplification at the point of use.
    #[test]
    fn region_resolution_returns_the_region_id_and_the_holders_own_rights() {
        // Decoy at slot 0 so a slot/object confusion cannot pass by coincidence.
        let cs = space(&[
            (abi::CapType::Endpoint, abi::CapRights::ALL, 0),
            (abi::CapType::Region, abi::CapRights::READ, 9),
        ]);
        assert_eq!(
            caps_region(&cs, abi::CapId(1), abi::CapRights::READ),
            Some((9, abi::CapRights::READ)),
            "must return (region id, holder's rights)"
        );
        assert_eq!(
            caps_region(&cs, abi::CapId(0), abi::CapRights::READ),
            None,
            "a non-Region capability resolved as a region"
        );
        assert_eq!(
            caps_region(&cs, abi::CapId(7), abi::CapRights::READ),
            None,
            "an empty slot resolved"
        );
        // Rights come back verbatim: never widened to what was asked for.
        for held in [
            abi::CapRights::READ,
            abi::CapRights::ALL,
            abi::CapRights(0b011),
        ] {
            let cs = space(&[(abi::CapType::Region, held, 5)]);
            let (_, got) = caps_region(&cs, abi::CapId(0), abi::CapRights::READ).unwrap();
            assert_eq!(got, held, "resolution altered the holder's rights");
        }
    }

    /// Resolving an interrupt capability: type, rights, and WHICH line.
    ///
    /// The boot cannot see any of this — deleting the READ requirement from `WAIT_IRQ` or
    /// `POLL_IRQ` leaves `RESULT: PASS`, because no `Irq` capability without READ is granted
    /// to anyone, so the case the gate exists for is never constructed on hardware.
    #[test]
    fn interrupt_resolution_enforces_type_rights_and_names_one_line() {
        // NOTE the padding, which is load-bearing. `IRQ_TIMER` is 0 and `IRQ_CONSOLE` is 1,
        // so an Irq capability placed at CapId(0)/CapId(1) has a slot index EQUAL to the line
        // it names — and a resolver returning the SLOT ID instead of the line then passes
        // every such case. The first version of this test did exactly that, and a mutation
        // (`.then_some(cap.0 as u64)`) survived it. Every Irq capability below therefore sits
        // at an index that differs from its line.
        let cs = space(&[
            (abi::CapType::Endpoint, abi::CapRights::ALL, 0),
            (abi::CapType::Endpoint, abi::CapRights::ALL, 0),
            (abi::CapType::Irq, abi::CapRights::READ, IRQ_CONSOLE), // CapId(2), line 1
        ]);
        assert_eq!(
            caps_irq_line(&cs, abi::CapId(2)),
            Some(IRQ_CONSOLE),
            "must return the LINE the capability names, not the slot it sits in"
        );
        assert_eq!(
            caps_irq_line(&cs, abi::CapId(0)),
            None,
            "a non-Irq capability resolved as an interrupt source"
        );
        assert_eq!(
            caps_irq_line(&cs, abi::CapId(5)),
            None,
            "an empty slot resolved"
        );

        // The case that does not exist in any grant table, and so was never exercised.
        for r in [
            abi::CapRights::NONE,
            abi::CapRights::WRITE,
            abi::CapRights::GRANT,
        ] {
            let no_read = space(&[(abi::CapType::Irq, r, IRQ_TIMER)]);
            assert_eq!(
                caps_irq_line(&no_read, abi::CapId(0)),
                None,
                "an Irq capability with rights {:#05b} and no READ resolved a line",
                r.0
            );
        }

        // Two capabilities, two lines: each resolves to its own. With one line in the system
        // this is vacuously true, which is why the grant tables carry two.
        // Padded again, and deliberately in the REVERSE order relative to the line numbers, so
        // neither index equals its line and a position-keyed resolver cannot pass.
        let both = space(&[
            (abi::CapType::Endpoint, abi::CapRights::ALL, 0),
            (abi::CapType::Endpoint, abi::CapRights::ALL, 0),
            (abi::CapType::Irq, abi::CapRights::READ, IRQ_CONSOLE), // CapId(2), line 1
            (abi::CapType::Irq, abi::CapRights::READ, IRQ_TIMER),   // CapId(3), line 0
        ]);
        assert_eq!(caps_irq_line(&both, abi::CapId(2)), Some(IRQ_CONSOLE));
        assert_eq!(caps_irq_line(&both, abi::CapId(3)), Some(IRQ_TIMER));
        assert_ne!(IRQ_TIMER, IRQ_CONSOLE, "the two lines must be distinct");
    }

    // ------------------------------------------------------- untrusted firmware map
    //
    // `hvm_start_info` comes from the hypervisor, i.e. from outside the TCB, and the PVH walk
    // dereferences one struct per declared entry. Both bounds below were absent.

    #[test]
    fn a_declared_entry_count_cannot_walk_off_the_table() {
        // The hypervisor may declare anything up to u32::MAX. The walk must not follow it.
        let n = pvh::walkable_entries(0x9000, u32::MAX);
        assert!(
            n <= pvh::MAX_MAP_ENTRIES,
            "walked {n} entries on a declared count of u32::MAX"
        );
        // A sane count passes through untouched.
        assert_eq!(pvh::walkable_entries(0x9000, 8), 8);
        assert_eq!(pvh::walkable_entries(0x9000, 0), 0);
    }

    #[test]
    fn the_map_table_must_lie_inside_the_identity_mapped_window() {
        // Low physical memory holds device MMIO; a read outside the identity window is not
        // merely a fault, it can have side effects.
        assert_eq!(
            pvh::walkable_entries(pvh::IDENTITY_LIMIT, 4),
            0,
            "a table at the identity limit was walked"
        );
        assert_eq!(
            pvh::walkable_entries(pvh::IDENTITY_LIMIT + 0x1000, 4),
            0,
            "a table beyond the identity limit was walked"
        );
        assert_eq!(pvh::walkable_entries(0, 4), 0, "a null table was walked");

        // A table that STARTS inside the window but whose declared entries would run past the
        // end is clipped to what actually fits — not accepted whole, and not refused whole.
        let near = pvh::IDENTITY_LIMIT - 100; // room for 4 x 24-byte entries
        let n = pvh::walkable_entries(near, 64);
        assert_eq!(n, 4, "expected the count clipped to what fits, got {n}");
        assert!(
            near + (n as u64) * 24 <= pvh::IDENTITY_LIMIT,
            "the walk would still run past the identity limit"
        );

        // Exactly one entry of room.
        assert_eq!(pvh::walkable_entries(pvh::IDENTITY_LIMIT - 24, 9), 1);
        assert_eq!(pvh::walkable_entries(pvh::IDENTITY_LIMIT - 23, 9), 0);
    }

    /// The TRIPWIRE, checked statically as well as at load time.
    ///
    /// docs/nucleus-design.md §1.2 — why revoking an `Untyped` does NOT reclaim regions or
    /// processes already created from it — rests entirely on an `Untyped` naming no extent.
    /// The load-time guard was a `debug_assert!`, i.e. absent from every release build, which
    /// is every build the runners and CI produce. This half cannot be compiled out.
    ///
    /// If this fails, do not "fix" it by zeroing the object: the design decision it protects
    /// has to be revisited, because an `Untyped` that names a range can be revoked while its
    /// derivations still hold that range.
    #[test]
    fn no_role_grants_a_mint_source_that_names_an_extent() {
        for role in ROLES {
            for &(cap_type, _, object) in grants_for(role) {
                assert!(
                    !is_mint_source(cap_type) || object == 0,
                    "the {} role grants a MINT SOURCE ({:?}) naming {:#x}; see \
                     docs/nucleus-design.md §1.2 — the revocation argument does not survive a \
                     mint source with an extent",
                    role_name(role),
                    cap_type,
                    object
                );
            }
        }
    }
}
