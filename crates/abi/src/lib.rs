//! Shared, `no_std` vocabulary across the Rustproof nucleus crates: physical/virtual
//! addresses, page constants, the boot memory map, the frame-allocator trait, and
//! capability / IPC / syscall type tags.
//!
//! Kept deliberately minimal — richer per-subsystem types live in each crate; this is
//! only the common contract so the crates integrate. (Verus specs come later; this is
//! the executable contract, not a proof artifact.)
#![no_std]

// ---------------------------------------------------------------- pages / addresses

pub const PAGE_SIZE: u64 = 4096;
pub const PAGE_SHIFT: u64 = 12;

/// A physical address.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[repr(transparent)]
pub struct PhysAddr(pub u64);

/// A virtual address.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[repr(transparent)]
pub struct VirtAddr(pub u64);

impl PhysAddr {
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
    /// Physical frame number (address >> 12).
    #[inline]
    pub const fn frame_number(self) -> u64 {
        self.0 >> PAGE_SHIFT
    }
    /// True if 4 KiB-aligned.
    #[inline]
    pub const fn is_page_aligned(self) -> bool {
        self.0 & (PAGE_SIZE - 1) == 0
    }
}

impl VirtAddr {
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
    #[inline]
    pub const fn is_page_aligned(self) -> bool {
        self.0 & (PAGE_SIZE - 1) == 0
    }
    /// The 9-bit index into the page-table level `level` (0 = PT .. 3 = PML4).
    #[inline]
    pub const fn table_index(self, level: u32) -> usize {
        ((self.0 >> (PAGE_SHIFT + 9 * level as u64)) & 0x1ff) as usize
    }
}

// ---------------------------------------------------------------- boot memory map

/// Kind of a physical memory region, normalized from the firmware/boot map.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemoryKind {
    /// Free RAM the nucleus may allocate.
    Usable,
    /// Firmware / device / MMIO — never allocatable.
    Reserved,
    AcpiReclaimable,
    AcpiNvs,
    Unusable,
}

/// A physical memory region from the boot map.
#[derive(Clone, Copy, Debug)]
pub struct MemoryRegion {
    pub start: u64,
    pub len: u64,
    pub kind: MemoryKind,
}

impl MemoryRegion {
    #[inline]
    pub const fn end(&self) -> u64 {
        self.start + self.len
    }
}

// ---------------------------------------------------------------- frame allocator

/// Allocator of physical 4 KiB frames. Implemented by the `mm` crate; consumed by
/// `vspace` (to allocate page-table frames) and the kernel.
pub trait FrameAllocator {
    /// Allocate one physical 4 KiB frame, or `None` if out of memory.
    fn alloc_frame(&mut self) -> Option<PhysAddr>;
    /// Return a previously-allocated frame to the pool.
    fn free_frame(&mut self, frame: PhysAddr);
}

// ---------------------------------------------------------------- capabilities

/// The kind of kernel object a capability refers to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CapType {
    Null,
    /// Permission to ALLOCATE — not memory, and not an extent.
    ///
    /// Deliberately unlike seL4's untyped, whose name this borrows and whose contract it does
    /// NOT have. An `Untyped` here carries no range: its `object` is unused and always zero,
    /// it names no frames, and there is no watermark. Holding one with `WRITE` is permission
    /// to do two things — allocate a region from the shared DMA arena (`MAKE_REGION`) and
    /// create a process (`SPAWN`) — and nothing more.
    ///
    /// The distinction is load-bearing for revocation. Because an `Untyped` names nothing,
    /// there is no naming relation for `REVOKE` to tear down: revoking it removes the ability
    /// to acquire MORE, and does not reclaim what was already acquired. See [`sysno::REVOKE`].
    /// The old wording here ("physical memory that can be retyped into other objects") is what
    /// made that read as a defect rather than a design.
    Untyped,
    Frame,
    PageTable,
    Endpoint,
    Notification,
    /// Thread control block.
    Tcb,
    IommuDomain,
    /// A device MMIO window.
    Mmio,
    /// A shareable memory region. Its `object` is a monotonic REGION ID — never a physical
    /// address, never a table index, and never reused.
    ///
    /// That choice is the whole safety argument. `SPAWN` copies a capability's type and
    /// object verbatim, and nothing revalidates an object afterwards, because until now
    /// every object named something the KERNEL owns and that outlives every process (an
    /// endpoint number, the boot-reserved device window, the DMA pool, an interrupt line).
    /// A region is the first object owned by a process that can die, so a capability CAN
    /// outlive what it names. With an id that is never reused, such a capability resolves
    /// to nothing; with a physical address or a slot index it would resolve to whatever
    /// occupies that address or slot NEXT — cross-process memory disclosure.
    Region,
    /// A device interrupt line. Holding it is the authority to observe that source's
    /// interrupts; a process without it cannot see them at all.
    Irq,
}

/// Access rights carried by a capability (monotonically non-increasing on derivation).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct CapRights(pub u8);

impl CapRights {
    pub const NONE: CapRights = CapRights(0);
    pub const READ: CapRights = CapRights(1 << 0);
    pub const WRITE: CapRights = CapRights(1 << 1);
    pub const GRANT: CapRights = CapRights(1 << 2);
    pub const ALL: CapRights = CapRights(0b111);

    #[inline]
    pub const fn contains(self, other: CapRights) -> bool {
        self.0 & other.0 == other.0
    }
    /// Intersection — the only legal direction on derivation (never gain rights).
    #[inline]
    pub const fn intersect(self, other: CapRights) -> CapRights {
        CapRights(self.0 & other.0)
    }

    /// Build rights from an untrusted user word, discarding every bit that is not a defined
    /// right.
    ///
    /// This truncation is LOAD-BEARING for the exhaustive searches below, and that was not
    /// visible from where it used to live. `CapRights` is representationally 256-valued, so
    /// enumerating `0u8..8` exhausts the universe only while every construction site is
    /// either a named constant or masked — and the sole non-constant site open-coded
    /// `& 0b111` in a kernel syscall path, where NOTHING pinned it. Delete it there and the
    /// searches here silently stop covering the universe: `ALL` is no longer the top of the
    /// lattice, so `CapRights(0b1000).intersect(ALL)` is `NONE` rather than `0b1000`, and
    /// `intersect_is_idempotent_and_bounded_by_none_and_all` is asserting something false
    /// about values it never tries.
    ///
    /// Keeping the mask here means one place defines it and this crate's own tests pin it.
    #[inline]
    pub const fn from_user(word: u64) -> CapRights {
        CapRights((word as u8) & CapRights::ALL.0)
    }
}

/// Index of a capability slot within a capability space.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct CapId(pub usize);

// ---------------------------------------------------------------- threads / IPC

/// Largest byte payload a single IPC message may carry, in addition to its word. Both
/// endpoints of a rendezvous copy through a kernel buffer of this size.
pub const MAX_MSG_BYTES: usize = 128;

/// How many regions one process may own at once.
///
/// Part of the contract rather than a kernel private: a caller cannot tell a per-owner quota
/// refusal from a global-table-full refusal by the error code alone (both are `NO_MEM`), so
/// without this number no test can demonstrate that the quota exists at all. Deleting the
/// check used to leave every assertion green.
pub const REGION_QUOTA: usize = 6;

/// Identifier of a thread / scheduling context.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct ThreadId(pub usize);

/// Syscall numbers (kernel entry selectors).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum Syscall {
    Yield = 0,
    Send = 1,
    Recv = 2,
    Call = 3,
    Reply = 4,
    Notify = 5,
}

/// Header describing an IPC message: a small tag plus the number of transferred words.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct MessageInfo {
    pub label: u64,
    pub length: u16,
}

impl MessageInfo {
    #[inline]
    pub const fn new(label: u64, length: u16) -> Self {
        MessageInfo { label, length }
    }
}

// ---------------------------------------------------------------- syscall ABI

/// User→kernel calling convention for the `syscall` instruction:
/// `rax` = number (one of [`sysno`]); args `a0..a4` in `rdi, rsi, rdx, r10, r8`;
/// result returned in `rax` (`rcx` and `r11` are clobbered by `syscall`).
pub mod sysno {
    /// Write `a1` bytes from user pointer `a0` to the debug console.
    pub const DEBUG_WRITE: u64 = 0;
    /// Terminate the calling process with exit code `a0`.
    pub const EXIT: u64 = 1;
    /// Host contract: write device info to `*mut GpuInfo` at `a0`.
    pub const GET_INFO: u64 = 2;
    /// Host contract: `a0` = MMIO capability id, `a1` = BAR index, `a2` = `*mut MapBarResp`.
    pub const MAP_BAR: u64 = 3;
    /// Cooperatively yield the CPU to the next ready process. No args, no result.
    pub const YIELD: u64 = 5;
    /// Send to an endpoint (rendezvous): `a0` = an `Endpoint` capability (needs `WRITE`),
    /// `a1` = the word, `a2` = pointer to an optional byte payload, `a3` = its length
    /// (0 for a word-only message; more than [`MAX_MSG_BYTES`](crate::MAX_MSG_BYTES) is
    /// rejected with `FAULT`). The payload is copied out of the sender's address space
    /// before the call returns, so the sender's buffer may be reused immediately.
    ///
    /// Returns `syserr::OK`, or `NO_CAP` without blocking if the capability is
    /// missing/wrong-typed/lacks `WRITE`; otherwise blocks until a receiver takes it.
    pub const SEND: u64 = 6;
    /// Receive from an endpoint (rendezvous): `a0` = an `Endpoint` capability (needs
    /// `READ`), `a1` = pointer to a buffer for the byte payload, `a2` = its capacity (0 to
    /// accept the word only). Blocks until a sender delivers.
    ///
    /// Returns THREE values in separate registers: the status (`OK` / `NO_CAP`) in the
    /// usual return register, the delivered word in the second (x86 `rdx`, RISC-V `a1`),
    /// and the number of payload bytes actually copied in the third (the `a3` argument
    /// register: x86 `r10`, RISC-V `a3`). A payload larger than the receiver's capacity is
    /// truncated to it — the sender cannot know the receiver's buffer size, so the copied
    /// count is what the receiver must believe.
    /// The split is load-bearing, not cosmetic: the word is an unrestricted `u64` chosen by
    /// the sender, so a single-register protocol would make a legitimately received word
    /// equal to a [`syserr`] sentinel indistinguishable from a real error. User stubs MUST
    /// declare the second register as an asm output.
    pub const RECV: u64 = 7;
    /// Spawn a new process running the same embedded image.
    ///
    /// `a0` = an `Untyped` capability carrying `WRITE` (spawn authority). `a1` = a
    /// capability of the CALLER's to delegate to the child, or [`NO_DELEGATE`] for none;
    /// `a2` = the rights to hand over. Delegation is authority-monotonic: the child
    /// receives `caller_rights ∩ a2`, so a parent may attenuate but never amplify —
    /// requesting more than it holds yields only what it holds. A delegated capability
    /// lands in the child's space immediately after its role's grants.
    ///
    /// Returns the new process id, or `u64::MAX` on failure (no authority, no free slot,
    /// out of memory, or a request to delegate a capability the caller does not hold).
    pub const SPAWN: u64 = 8;
    /// `a1` value for [`SPAWN`] meaning "delegate nothing".
    pub const NO_DELEGATE: u64 = u64::MAX;
    /// Collect device interrupts that have arrived for the caller: `a0` = an `Irq`
    /// capability (needs `READ`). Returns the number of interrupts counted since the last
    /// call and resets that count, or `NO_CAP` without blocking if the capability is
    /// missing, wrong-typed, or lacks `READ`.
    ///
    /// Non-blocking by design for now: a blocking wait needs the kernel to idle with
    /// interrupts enabled when nothing is runnable, which is a separate mechanism.
    pub const POLL_IRQ: u64 = 11;
    /// Like [`POLL_IRQ`] but BLOCKS until at least one interrupt has arrived on the line
    /// its capability names, then returns the count. `a0` = an `Irq` capability (needs
    /// `READ`); `NO_CAP` if it is missing/wrong-typed/lacks `READ`.
    ///
    /// Returns `0` — without blocking — for a capability naming a line the kernel does not
    /// deliver. Blocking there would be an unwakeable sleep, and since the kernel treats a
    /// process waiting on an interrupt as idle rather than deadlocked, one such waiter
    /// would park the machine forever. A caller that must distinguish "none yet" from
    /// "never" should treat `0` from a *blocking* wait as "this line is not delivered".
    pub const WAIT_IRQ: u64 = 12;
    /// Create a shareable memory region: `a0` = an `Untyped` capability (needs `WRITE`),
    /// `a1` = pages (1..=`REGION_MAX_PAGES`). Returns the id of a freshly minted `Region`
    /// capability in the caller's space, or `NO_CAP` / `NO_MEM`.
    ///
    /// The new capability carries the rights the `Untyped` one did, so authority to create
    /// bounds authority over the result. The region's pages are ZEROED: they are the first
    /// recycled memory in this kernel that another process can be given to READ.
    pub const MAKE_REGION: u64 = 13;
    /// Map a region into the CALLER's address space: `a0` = a `Region` capability (needs
    /// `READ`). Returns the user address the KERNEL chose, or `NO_CAP` / `NO_MEM`.
    ///
    /// There is deliberately no address argument — the caller does not get to say where its
    /// own mappings land, so no user-supplied address reaches the mapping path and none has
    /// to be validated. Writable only if the capability carries `WRITE`: attenuating the
    /// capability attenuates the access, exactly as for `MAP_BAR`.
    pub const MAP_REGION: u64 = 14;
    /// Drop the caller's mapping of a region: `a0` = a `Region` capability. `OK`, or
    /// `NO_CAP` if the caller does not hold it. The capability survives; only the mapping
    /// goes, so the caller can map it again.
    pub const UNMAP_REGION: u64 = 15;
    /// Destroy a region and return its pages: `a0` = a `Region` capability. Only its OWNER
    /// may do this; a borrower gets `NO_CAP`. Unmaps the region from every process holding
    /// it and invalidates every capability naming it, so no mapping and no authority
    /// survives the memory.
    pub const FREE_REGION: u64 = 16;
    /// Give a DEVICE the ability to reach a region by DMA: `a0` = an `IommuDomain` capability
    /// (needs `WRITE` — handing out DMA reach is granting authority, not observing it), `a1` =
    /// a `Region` capability (needs `READ`). Returns the I/O virtual address the KERNEL chose,
    /// or `NO_CAP` / `NO_MEM`.
    ///
    /// The device may WRITE the region only if the caller's own `Region` capability carries
    /// `WRITE`, exactly as [`MAP_REGION`] decides a CPU mapping's permissions from the
    /// holder's rights rather than the request's. Two capabilities are required because two
    /// separate authorities are involved: over the memory, and over the device's DMA domain.
    ///
    /// Refused with `NO_MEM` when no IOMMU is programmed. The nucleus will not hand out DMA
    /// reach it cannot contain — on a machine with no unit, a "granted" mapping would be
    /// indistinguishable from unrestricted access to all of memory.
    ///
    /// No user-supplied address reaches the I/O page tables: the kernel picks the IOVA, the
    /// same rule [`MAP_REGION`] follows for virtual addresses.
    pub const MAP_DMA: u64 = 17;
    /// Withdraw a DMA mapping: `a0` = the `IommuDomain` capability (needs `WRITE`), `a1` = the
    /// same `Region` capability that was mapped (needs `READ`). Returns `OK`, or `NO_CAP`.
    ///
    /// The REGION is named, not the address. An IOVA travelling back in from userland would be
    /// a user-supplied address reaching the I/O page-table path, which is the thing [`MAP_DMA`]
    /// is careful to avoid on the way out.
    ///
    /// Clears the I/O page-table entries, drops the domain's grants, AND invalidates the unit's
    /// caches before returning. Clearing a table is not revocation while a translation the unit
    /// already performed is still cached — measured on the rig, where the device went on
    /// reaching a withdrawn frame until the invalidation was issued.
    pub const UNMAP_DMA: u64 = 18;
    /// Revoke every capability DELEGATED from `a0` (one of the caller's own capabilities),
    /// transitively — the children it was handed to and the grandchildren they passed it on
    /// to. Returns `OK`, or `NO_CAP` if the caller does not hold `a0`. The caller keeps its
    /// own capability; only the delegations are destroyed.
    ///
    /// With each destroyed capability goes the authority that capability NAMED: a device
    /// window is unmapped, a region mapping is torn down, interrupt credits are zeroed, a
    /// parked rendezvous is ended. Capability spaces are FLAT, so there is no per-space
    /// subtree to walk — "derived" means DELEGATED and nothing else.
    ///
    /// Revoking an [`CapType::Untyped`] additionally removes the ability to ACQUIRE MORE: the
    /// holder can no longer `MAKE_REGION` or `SPAWN`, both refused immediately by a live
    /// capability lookup. It does NOT reclaim what was already acquired. Regions already
    /// minted stay alive, owned by their minter; processes already spawned keep running. Both
    /// return only when their holder terminates.
    ///
    /// That is a decision, not an oversight, and the two halves are why: `MAKE_REGION` and
    /// `SPAWN` are gated on the SAME capability with the SAME right. Reclaiming regions on
    /// revocation while spawned processes — which retain strictly more authority, their own
    /// address spaces and capabilities included — kept running would give this kernel two
    /// different answers to one question. An `Untyped` names no extent, so there is nothing
    /// for a reclamation to be scoped BY. See docs/nucleus-design.md §1.2.
    pub const REVOKE: u64 = 10;
}

/// Syscall result codes returned in `rax`. `OK` is 0; errors are large sentinels so
/// they never collide with a valid small return value.
pub mod syserr {
    pub const OK: u64 = 0;
    pub const BAD_SYSCALL: u64 = u64::MAX;
    pub const NO_CAP: u64 = u64::MAX - 1;
    pub const NO_MEM: u64 = u64::MAX - 2;
    pub const FAULT: u64 = u64::MAX - 3;
}

/// Why a `SPAWN` was refused.
///
/// `SPAWN` returns the child's pid on success, so every failure has to be a value no pid can
/// take — hence the top of the range. One sentinel for all of them is not enough: a probe
/// asserting "a capability without WRITE cannot spawn" is satisfied just as well by an
/// exhausted process table, so the refusal is consistent with the claim rather than evidence
/// for it. Naming the reason is what makes it evidence.
///
/// `NO_CAP` deliberately keeps the historical `u64::MAX`, so a caller that tested for it was
/// already asking the authority question and stays correct. Callers that only wanted "did
/// this fail" must use [`spawnerr::failed`] — `== u64::MAX` now misses a resource refusal.
pub mod spawnerr {
    /// The capability named does not authorize spawning.
    pub const NO_CAP: u64 = u64::MAX;
    /// A capability was asked to be delegated that the caller does not hold, or the
    /// delegation ledger is full.
    pub const NO_DELEGATE: u64 = u64::MAX - 1;
    /// The process table has no free slot.
    pub const NO_SLOT: u64 = u64::MAX - 2;
    /// The image could not be loaded (out of frames).
    pub const NO_MEM: u64 = u64::MAX - 3;
    /// Lowest value that is a failure rather than a pid.
    pub const FIRST: u64 = NO_MEM;

    /// Did this `SPAWN` fail, for any reason?
    pub const fn failed(ret: u64) -> bool {
        ret >= FIRST
    }
}

/// GPU device info returned by `GET_INFO` (host contract).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct GpuInfo {
    pub pci_vendor: u16,
    pub pci_device: u16,
    pub gfx_version: u32,
    pub vram_bytes: u64,
}

/// Response to `MAP_BAR` (host contract): where a device BAR was mapped in the caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct MapBarResp {
    pub user_va: u64,
    pub size: u64,
}

/// Kernel services the host-contract dispatcher needs, supplied by the integrator so the
/// dispatch logic stays a pure, testable unit (mock in tests, real kernel state at runtime).
pub trait HostEnv {
    /// Emit debug bytes (already copied out of user memory) to the console.
    fn debug_write(&mut self, bytes: &[u8]);
    /// The device info to report for `GET_INFO`.
    fn gpu_info(&self) -> GpuInfo;
    /// Look up a capability in the calling process's space: `(type, rights, object)`.
    fn cap_lookup(&self, cap: CapId) -> Option<(CapType, CapRights, u64)>;
    /// Map `pages` 4 KiB pages of physical memory starting at `phys` into the calling
    /// process's address space, user-accessible and writable ONLY if `writable`, returning
    /// the user virtual address. The permission must come from the capability that
    /// authorised the mapping: installing a writable page for a read-only capability would
    /// hand out authority the capability does not carry. `None` if it could not be
    /// installed. Re-mapping an already-mapped window replaces it (so permissions can
    /// change and a retry is not poisoned).
    fn map_device(&mut self, phys: u64, pages: u64, writable: bool) -> Option<u64>;
    /// Remove the calling process's device mapping, if any. Used to undo a mapping whose
    /// response could not be delivered, and to tear down authority on revocation.
    fn unmap_device(&mut self);
    /// Copy `bytes` into the caller's memory at user virtual address `uptr`.
    /// Returns false if the pointer is not a valid, writable user address.
    fn write_user_bytes(&mut self, uptr: u64, bytes: &[u8]) -> bool;
    /// Copy from the caller's memory at `uptr` into `out`. Returns false on a bad pointer.
    fn read_user_bytes(&self, uptr: u64, out: &mut [u8]) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rights lattice, exhaustively.
    ///
    /// `intersect` is what makes delegation safe: a child receives
    /// `holder_rights ∩ requested`, so it can never hold a right its source lacked. Until
    /// now that property was covered only as a side effect of testing `CapSpace::derive` —
    /// a function the kernel never called on a live capability space, and which has been
    /// deleted. The property itself is load-bearing in three places that DO run (`SPAWN`
    /// delegation, `make_region`'s minted rights, and every `contains` gate), so it is
    /// asserted directly here, over every pair.
    #[test]
    fn intersect_never_grants_a_right_either_side_lacks() {
        for a in 0u8..8 {
            for b in 0u8..8 {
                let (x, y) = (CapRights(a), CapRights(b));
                let got = x.intersect(y);
                assert!(
                    x.contains(got),
                    "{a:#05b} ∩ {b:#05b} = {:#05b} exceeds the holder",
                    got.0
                );
                assert!(
                    y.contains(got),
                    "{a:#05b} ∩ {b:#05b} = {:#05b} exceeds the request",
                    got.0
                );
                assert_eq!(got, y.intersect(x), "intersect must be commutative");
                assert_eq!(got.0, a & b);
            }
        }
    }

    /// `ALL` must cover every named right. Nothing else checks this, and the exhaustive
    /// searches all assume it: they enumerate `0u8..8` because that is `0..=ALL.0`. Add a
    /// fourth right and forget to widen `ALL`, and every search below quietly stops covering
    /// the lattice while staying green.
    #[test]
    fn all_is_exactly_the_defined_rights() {
        assert_eq!(
            CapRights::ALL.0,
            CapRights::READ.0 | CapRights::WRITE.0 | CapRights::GRANT.0,
            "ALL must be the union of the named rights"
        );
        for r in [CapRights::READ, CapRights::WRITE, CapRights::GRANT] {
            assert!(CapRights::ALL.contains(r));
        }
    }

    /// The universe-narrowing mask, pinned where it is defined.
    ///
    /// Over EVERY u8 and a spread of wider words: what comes back is always inside the
    /// lattice the searches enumerate, and bits below the mask survive untouched.
    #[test]
    fn from_user_truncates_to_the_defined_lattice() {
        for w in 0u64..256 {
            let got = CapRights::from_user(w);
            assert!(
                CapRights::ALL.contains(got),
                "from_user({w:#010b}) = {:#010b} escaped the lattice",
                got.0
            );
            assert_eq!(got.0, (w as u8) & CapRights::ALL.0);
        }
        // Wide words must not smuggle bits in through the cast either.
        for w in [0x100u64, 0xFF00, 0xDEAD_BEEF, u64::MAX] {
            assert!(CapRights::ALL.contains(CapRights::from_user(w)));
        }
        // And the mask is not a no-op: an undefined bit is actually dropped.
        assert_eq!(CapRights::from_user(0b1000), CapRights::NONE);
        assert_eq!(CapRights::from_user(0b1011), CapRights(0b011));
    }

    #[test]
    fn intersect_is_idempotent_and_bounded_by_none_and_all() {
        for a in 0u8..8 {
            let x = CapRights(a);
            assert_eq!(x.intersect(x), x);
            assert_eq!(x.intersect(CapRights::ALL), x);
            assert_eq!(x.intersect(CapRights::NONE), CapRights::NONE);
            assert!(CapRights::ALL.contains(x));
            assert!(x.contains(CapRights::NONE));
        }
    }

    #[test]
    fn contains_is_subset_not_overlap() {
        // The gate is "holds every requested right", not "holds any of them" — a capability
        // with READ alone must not pass a check for READ|WRITE.
        assert!(!CapRights::READ.contains(CapRights(CapRights::READ.0 | CapRights::WRITE.0)));
        assert!(CapRights::ALL.contains(CapRights::WRITE));
        assert!(!CapRights::WRITE.contains(CapRights::READ));
    }

    /// `failed` is a BOUND, and a bound is a claim about ONE value. Testing it from far
    /// outside — pid 3 is not a failure, u64::MAX is — never asks the question. The values
    /// that matter are the two either side of the edge.
    #[test]
    fn spawnerr_failed_is_exact_at_its_boundary() {
        assert!(spawnerr::failed(spawnerr::FIRST));
        assert!(
            !spawnerr::failed(spawnerr::FIRST - 1),
            "the value just below the failure range is a valid pid"
        );
        assert!(spawnerr::failed(u64::MAX));
        assert!(!spawnerr::failed(0), "pid 0 is a pid");
    }

    /// Four distinct reasons, or the attribution they exist for is worthless: the whole point
    /// is that "refused for lack of authority" and "the process table is full" stop reading
    /// the same. Every one must also be a failure.
    #[test]
    fn spawnerr_reasons_are_distinct_and_all_are_failures() {
        let all = [
            spawnerr::NO_CAP,
            spawnerr::NO_DELEGATE,
            spawnerr::NO_SLOT,
            spawnerr::NO_MEM,
        ];
        for (i, a) in all.iter().enumerate() {
            assert!(spawnerr::failed(*a), "reason {i} must count as a failure");
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "reasons {i} and {j} collide");
                }
            }
        }
        assert_eq!(
            spawnerr::NO_CAP,
            u64::MAX,
            "NO_CAP keeps the historical sentinel so an old authority test stays correct"
        );
    }
}
