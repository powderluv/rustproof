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
    /// Untyped physical memory that can be retyped into other objects.
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
}

/// Index of a capability slot within a capability space.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct CapId(pub usize);

// ---------------------------------------------------------------- threads / IPC

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
    /// Host contract: `a0` = Untyped capability id, `a1` = byte size, `a2` = `*mut AllocResp`.
    pub const ALLOC_VRAM: u64 = 4;
    /// Cooperatively yield the CPU to the next ready process. No args, no result.
    pub const YIELD: u64 = 5;
    /// Send one word to an endpoint (rendezvous): `a0` = an `Endpoint` capability (needs
    /// `WRITE`), `a1` = the word. Returns `syserr::OK`, or `NO_CAP` without blocking if the
    /// capability is missing/wrong-typed/lacks `WRITE`; otherwise blocks until a receiver
    /// takes the word.
    pub const SEND: u64 = 6;
    /// Receive one word from an endpoint (rendezvous): `a0` = an `Endpoint` capability
    /// (needs `READ`). Blocks until a sender delivers.
    ///
    /// Returns TWO values in separate registers: the status (`OK` / `NO_CAP`) in the usual
    /// return register, and the delivered word in the second one (x86 `rdx`, RISC-V `a1`).
    /// The split is load-bearing, not cosmetic: the word is an unrestricted `u64` chosen by
    /// the sender, so a single-register protocol would make a legitimately received word
    /// equal to a [`syserr`] sentinel indistinguishable from a real error. User stubs MUST
    /// declare the second register as an asm output.
    pub const RECV: u64 = 7;
    /// Spawn a new process running the same embedded image. Returns the new process id, or
    /// `u64::MAX` on failure (no free slot / out of memory).
    pub const SPAWN: u64 = 8;
    /// Free a VRAM frame previously returned by `ALLOC_VRAM`: `a0` = its physical address.
    /// Returns `OK`, or `FAULT` if the caller does not own that frame.
    pub const FREE_VRAM: u64 = 9;
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

/// Response to `ALLOC_VRAM` (host contract): the physical frame + size granted.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct AllocResp {
    pub phys: u64,
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
    /// Allocate one DMA-capable physical frame for `ALLOC_VRAM`, honoring the caller's
    /// per-process VRAM quota. `None` if the quota is reached or memory is exhausted.
    fn alloc_dma(&mut self) -> Option<PhysAddr>;
    /// Free a VRAM frame (at physical address `phys`) previously handed to the caller by
    /// [`alloc_dma`](Self::alloc_dma). Returns `false` if the caller does not own it (so a
    /// process can only free its own VRAM, never another's).
    fn free_dma(&mut self, phys: u64) -> bool;
    /// Copy `bytes` into the caller's memory at user virtual address `uptr`.
    /// Returns false if the pointer is not a valid, writable user address.
    fn write_user_bytes(&mut self, uptr: u64, bytes: &[u8]) -> bool;
    /// Copy from the caller's memory at `uptr` into `out`. Returns false on a bad pointer.
    fn read_user_bytes(&self, uptr: u64, out: &mut [u8]) -> bool;
}
