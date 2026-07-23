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
