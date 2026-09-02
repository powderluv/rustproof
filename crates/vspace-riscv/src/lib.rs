#![cfg_attr(not(test), no_std)]

//! vspace-riscv — RISC-V Sv39 (3-level, 4 KiB page, 39-bit VA) page tables + a
//! small address-space model.
//!
//! VERIFIED TCB crate. This is the executable Rust; Verus specs come later.
//! See docs/nucleus-design.md and docs/verification.md.
//!
//! This mirrors the x86-64 [`vspace`] crate's shape for a RISC-V Sv39 MMU. The
//! nucleus identity-maps low RAM, so physical page-table frames are reached
//! through a caller-supplied physical→virtual offset (`phys_offset`). With an
//! identity map the offset is `0` (physical address == virtual address).
//!
//! Only 4 KiB leaf pages are produced by [`AddressSpace::map`]; superpages
//! (2 MiB / 1 GiB megapages/gigapages) that appear on a walk (created by some
//! other agent) are recognized but not created.
//!
//! Sv39 page-table entry (64 bits):
//! ```text
//!  63        54 53                         10 9 8 7 6 5 4 3 2 1 0
//! [ reserved  ][           PPN[43:0]         ][RSW][D A G U X W R V]
//! ```
//! A **leaf** PTE has `(R|W|X) != 0`; a **pointer** to the next-level table has
//! `R=W=X=0` and `V=1`. The physical address of the frame/table named by a PTE
//! is `PPN << 12`, i.e. `PPN = phys_addr >> 12`.

use core::ops::{BitAnd, BitOr, Not};

use abi::{FrameAllocator, PhysAddr, VirtAddr, PAGE_SIZE};

// -------------------------------------------------------------------- bit masks

/// A physical page number in Sv39 is 44 bits (`PPN[43:0]`), giving a 56-bit
/// physical address space (`PPN << 12`).
const PPN_BITS: u64 = (1 << 44) - 1;

/// The PPN occupies bits 10..=53 of a page-table entry.
const PPN_SHIFT: u64 = 10;

/// Mask selecting the PPN field (bits 10..=53) inside a raw entry.
const PPN_MASK: u64 = PPN_BITS << PPN_SHIFT;

/// Everything that is not the PPN field is a flag / control / available bit
/// (bits 0..=9 = the architectural flags + RSW, bits 54..=63 reserved).
const FLAG_MASK: u64 = !PPN_MASK;

// -------------------------------------------------------------------- PageFlags

/// Architectural Sv39 page-table entry flags (the subset the nucleus uses).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct PageFlags(u64);

impl PageFlags {
    /// Valid — the entry participates in a walk (leaf *or* pointer).
    pub const V: PageFlags = PageFlags(1 << 0);
    /// Readable (leaf permission).
    pub const R: PageFlags = PageFlags(1 << 1);
    /// Writable (leaf permission).
    pub const W: PageFlags = PageFlags(1 << 2);
    /// Executable (leaf permission).
    pub const X: PageFlags = PageFlags(1 << 3);
    /// User — accessible from U-mode (leaf).
    pub const U: PageFlags = PageFlags(1 << 4);
    /// Global — mapping present in every address space.
    pub const G: PageFlags = PageFlags(1 << 5);
    /// Accessed — set by hardware (or eagerly) when the page is used.
    pub const A: PageFlags = PageFlags(1 << 6);
    /// Dirty — set by hardware (or eagerly) when the page is written.
    pub const D: PageFlags = PageFlags(1 << 7);

    /// The three leaf-permission bits `R | W | X` — a PTE is a leaf iff any is set.
    pub const RWX: PageFlags = PageFlags((1 << 1) | (1 << 2) | (1 << 3));

    /// No flags set.
    #[inline]
    pub const fn empty() -> PageFlags {
        PageFlags(0)
    }

    /// Raw flag bits.
    #[inline]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Build from raw bits, keeping only flag (non-PPN) bits.
    #[inline]
    pub const fn from_bits_truncate(bits: u64) -> PageFlags {
        PageFlags(bits & FLAG_MASK)
    }

    /// True if every bit in `other` is also set in `self`.
    #[inline]
    pub const fn contains(self, other: PageFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// True if *any* bit in `other` is set in `self`.
    #[inline]
    pub const fn intersects(self, other: PageFlags) -> bool {
        self.0 & other.0 != 0
    }

    /// Set-union of two flag sets.
    #[inline]
    pub const fn union(self, other: PageFlags) -> PageFlags {
        PageFlags(self.0 | other.0)
    }
}

impl BitOr for PageFlags {
    type Output = PageFlags;
    #[inline]
    fn bitor(self, rhs: PageFlags) -> PageFlags {
        PageFlags(self.0 | rhs.0)
    }
}

impl BitAnd for PageFlags {
    type Output = PageFlags;
    #[inline]
    fn bitand(self, rhs: PageFlags) -> PageFlags {
        PageFlags(self.0 & rhs.0)
    }
}

impl Not for PageFlags {
    type Output = PageFlags;
    #[inline]
    fn not(self) -> PageFlags {
        PageFlags(!self.0 & FLAG_MASK)
    }
}

// --------------------------------------------------------------------------- Pte

/// A single 64-bit Sv39 page-table entry: physical page number ⊕ flags.
///
/// Unlike x86-64 (whose address field sits in place at bits 12..=51), Sv39
/// stores the frame as a *shifted* PPN at bits 10..=53, so encode/decode go
/// through `>> 12` / `<< 12`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct Pte(pub u64);

impl Pte {
    /// An empty (invalid) entry.
    pub const EMPTY: Pte = Pte(0);

    /// Encode a frame physical address + flags into an entry. The address is
    /// converted to its 44-bit PPN (masked) and the flags to their bit field,
    /// so a malformed input can never bleed from one region into the other.
    ///
    // PROOF(later): decode∘encode is identity — for a page-aligned `pa` whose
    // frame number fits in 44 bits and any `flags`, `Pte::new(pa, flags).addr()
    // == pa` and `.flags() == PageFlags::from_bits_truncate(flags.bits())`.
    #[inline]
    pub const fn new(pa: PhysAddr, flags: PageFlags) -> Pte {
        let ppn = (pa.as_u64() >> abi::PAGE_SHIFT) & PPN_BITS;
        Pte((ppn << PPN_SHIFT) | (flags.bits() & FLAG_MASK))
    }

    /// The physical page number (frame number) this entry names.
    #[inline]
    pub const fn ppn(self) -> u64 {
        (self.0 >> PPN_SHIFT) & PPN_BITS
    }

    /// The physical address of the frame / next-level table (`PPN << 12`).
    #[inline]
    pub const fn addr(self) -> PhysAddr {
        PhysAddr(self.ppn() << abi::PAGE_SHIFT)
    }

    /// Decode the flags.
    #[inline]
    pub const fn flags(self) -> PageFlags {
        PageFlags(self.0 & FLAG_MASK)
    }

    /// True if the V (valid) bit is set — the entry participates in a walk.
    #[inline]
    pub const fn is_valid(self) -> bool {
        self.0 & PageFlags::V.0 != 0
    }

    /// True if this is a leaf entry, i.e. any of `R | W | X` is set. (A valid
    /// entry with `R=W=X=0` is instead a *pointer* to the next-level table.)
    #[inline]
    pub const fn is_leaf(self) -> bool {
        self.0 & PageFlags::RWX.0 != 0
    }
}

// -------------------------------------------------------------------- PageTable

/// One level of the radix tree: 512 entries, exactly one 4 KiB frame.
#[derive(Clone, Copy)]
#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [Pte; 512],
}

impl PageTable {
    /// A fully-zeroed (all invalid) table.
    #[inline]
    pub const fn new() -> PageTable {
        PageTable {
            entries: [Pte::EMPTY; 512],
        }
    }
}

impl Default for PageTable {
    #[inline]
    fn default() -> PageTable {
        PageTable::new()
    }
}

// A page table must be exactly one frame so it can be handed out by the frame
// allocator and reached at a page boundary through `phys_offset`.
const _: () = assert!(core::mem::size_of::<PageTable>() as u64 == PAGE_SIZE);

// -------------------------------------------------------------------- MapError

/// Why an [`AddressSpace::map`] failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapError {
    /// The virtual address is not 4 KiB aligned.
    UnalignedVirt,
    /// The physical address is not 4 KiB aligned.
    UnalignedPhys,
    /// The frame allocator ran out of frames for an intermediate table.
    OutOfFrames,
    /// A 4 KiB mapping already exists at this virtual address.
    AlreadyMapped,
    /// A superpage (2 MiB / 1 GiB) occupies the path; can't insert a 4 KiB leaf.
    SuperpagePresent,
    /// The requested leaf flags name no permission — a leaf must set at least
    /// one of `R | W | X` (otherwise it would encode a pointer, not a page).
    InvalidLeafFlags,
}

// ----------------------------------------------------------------- AddressSpace

/// A RISC-V Sv39 virtual address space: the root (level-2) page-table frame plus
/// the offset used to reach physical page-table frames from kernel virtual
/// addresses.
///
/// The [`AddressSpace`] never writes `satp` (that needs an arch dependency); use
/// [`AddressSpace::satp`] to obtain the value the integrator loads.
pub struct AddressSpace {
    /// Physical address of the root (level-2) page-table frame.
    root: PhysAddr,
    /// physical→virtual offset: `virt = phys + phys_offset` for identity-mapped
    /// low RAM. Default `0` == identity map.
    phys_offset: u64,
}

impl AddressSpace {
    /// Wrap an existing, already-zeroed root frame.
    ///
    /// The caller must guarantee `root_phys` names a live 4 KiB frame reachable
    /// at `root_phys + phys_offset` and that it is zeroed (all entries invalid).
    /// Frame allocators in this project hand out zeroed frames.
    #[inline]
    pub const fn new(root_phys: PhysAddr, phys_offset: u64) -> AddressSpace {
        AddressSpace {
            root: root_phys,
            phys_offset,
        }
    }

    /// Allocate and zero a fresh root table from `fa`, returning the new space.
    pub fn create(phys_offset: u64, fa: &mut dyn FrameAllocator) -> Option<AddressSpace> {
        let root = fa.alloc_frame()?;
        let space = AddressSpace::new(root, phys_offset);
        // SAFETY: `root` is a live frame just handed out by `fa`, reachable at
        // `root + phys_offset`. We zero it in case the allocator does not
        // guarantee zeroed frames; nothing else aliases it yet.
        unsafe {
            *space.table_ptr(root) = PageTable::new();
        }
        Some(space)
    }

    /// Physical address of the root (level-2) table.
    #[inline]
    pub const fn root_phys(&self) -> PhysAddr {
        self.root
    }

    /// The root table's physical page number — the `PPN` field of `satp`.
    #[inline]
    pub const fn root_ppn(&self) -> u64 {
        self.root.as_u64() >> abi::PAGE_SHIFT
    }

    /// The `satp` value the integrator loads: `MODE=Sv39 (8)`, `ASID=0`, and the
    /// root table's PPN.
    #[inline]
    pub const fn satp(&self) -> u64 {
        (8u64 << 60) | self.root_ppn()
    }

    /// The physical→virtual offset in effect for this space.
    #[inline]
    pub const fn phys_offset(&self) -> u64 {
        self.phys_offset
    }

    /// Raw pointer to the page-table frame at physical address `pa`.
    ///
    /// The returned pointer is only valid while `pa` is a live page-table frame
    /// mapped at `pa + phys_offset`; all dereferences are in `unsafe` blocks.
    #[inline]
    fn table_ptr(&self, pa: PhysAddr) -> *mut PageTable {
        (self.phys_offset.wrapping_add(pa.as_u64())) as *mut PageTable
    }

    /// Read entry `idx` from the table at `table_pa`.
    ///
    /// SAFETY: `table_pa` must be a live page-table frame reachable at the
    /// offset; `idx < 512`.
    #[inline]
    unsafe fn read_entry(&self, table_pa: PhysAddr, idx: usize) -> Pte {
        (*self.table_ptr(table_pa)).entries[idx]
    }

    /// Write entry `idx` of the table at `table_pa`.
    ///
    /// SAFETY: `table_pa` must be a live page-table frame reachable at the
    /// offset; `idx < 512`. `&mut self` serializes writes into the tree.
    #[inline]
    unsafe fn write_entry(&mut self, table_pa: PhysAddr, idx: usize, e: Pte) {
        (*self.table_ptr(table_pa)).entries[idx] = e;
    }

    /// Map `va` → `pa` (4 KiB) with `flags`, allocating intermediate tables from
    /// `fa` as needed. V is forced on the leaf; `flags` must carry at least one
    /// of `R | W | X`. Intermediate pointer entries are written V-only
    /// (`R=W=X=0`), as Sv39 requires — permissions are checked only at the leaf.
    ///
    // PROOF(later): after `map(va, pa, ..)` returns `Ok`, `translate(va)` yields
    // `Some(pa)` (the concrete walk refines the abstract va→pa map).
    pub fn map(
        &mut self,
        va: VirtAddr,
        pa: PhysAddr,
        flags: PageFlags,
        fa: &mut dyn FrameAllocator,
    ) -> Result<(), MapError> {
        if !va.is_page_aligned() {
            return Err(MapError::UnalignedVirt);
        }
        if !pa.is_page_aligned() {
            return Err(MapError::UnalignedPhys);
        }
        // A leaf with no R/W/X would decode as a pointer and corrupt the walk.
        if !flags.intersects(PageFlags::RWX) {
            return Err(MapError::InvalidLeafFlags);
        }

        // Descend the two upper levels (VPN2 = level 2, VPN1 = level 1),
        // creating pointer entries to fresh tables where absent.
        let mut table_pa = self.root;
        for level in [2u32, 1] {
            let idx = va.table_index(level);
            // SAFETY: `table_pa` is a live table frame (root, or a pointer we
            // just followed / created); `idx` is a 9-bit table index (< 512).
            let entry = unsafe { self.read_entry(table_pa, idx) };
            if entry.is_valid() {
                if entry.is_leaf() {
                    return Err(MapError::SuperpagePresent);
                }
                table_pa = entry.addr();
            } else {
                let frame = fa.alloc_frame().ok_or(MapError::OutOfFrames)?;
                // SAFETY: fresh live frame from `fa`, reachable at the offset.
                unsafe {
                    *self.table_ptr(frame) = PageTable::new();
                }
                // A pointer PTE is V-only (R=W=X=0); anything else would make it
                // a leaf. Sv39 checks leaf permissions only, so this is correct.
                let link = Pte::new(frame, PageFlags::V);
                // SAFETY: as above; `idx` < 512.
                unsafe { self.write_entry(table_pa, idx, link) };
                table_pa = frame;
            }
        }

        // Leaf at VPN0 (level 0).
        let idx = va.table_index(0);
        // SAFETY: `table_pa` is the live level-0 table frame; `idx` < 512.
        let leaf = unsafe { self.read_entry(table_pa, idx) };
        if leaf.is_valid() {
            return Err(MapError::AlreadyMapped);
        }
        let entry = Pte::new(pa, flags | PageFlags::V);
        // SAFETY: as above.
        unsafe { self.write_entry(table_pa, idx, entry) };
        Ok(())
    }

    /// Remove the 4 KiB mapping for `va`, returning the physical frame it named.
    ///
    /// Intermediate tables are left in place (not reclaimed — that needs the
    /// frame allocator and reference counts). Superpages are not unmapped.
    ///
    // PROOF(later): after `unmap(va)` returns, `translate(va) == None`.
    pub fn unmap(&mut self, va: VirtAddr) -> Option<PhysAddr> {
        let mut table_pa = self.root;
        for level in [2u32, 1] {
            let idx = va.table_index(level);
            // SAFETY: live table frame; `idx` < 512.
            let entry = unsafe { self.read_entry(table_pa, idx) };
            if !entry.is_valid() || entry.is_leaf() {
                return None;
            }
            table_pa = entry.addr();
        }
        let idx = va.table_index(0);
        // SAFETY: live level-0 table frame; `idx` < 512.
        let leaf = unsafe { self.read_entry(table_pa, idx) };
        if !leaf.is_valid() {
            return None;
        }
        let pa = leaf.addr();
        // SAFETY: as above; clearing the leaf drops the mapping.
        unsafe { self.write_entry(table_pa, idx, Pte::EMPTY) };
        Some(pa)
    }

    /// Translate `va` to a physical address by walking the tables. Handles 4 KiB
    /// leaves and superpage (2 MiB / 1 GiB) leaves; returns `None` if unmapped.
    ///
    // PROOF(later): `translate` reflects the last `map`/`unmap` on `va` — i.e.
    // the concrete page-walk is a refinement of the abstract va→pa mapping.
    /// Flags of the leaf PTE mapping `va`, or `None` if `va` is unmapped.
    ///
    /// The kernel uses this to check that a user pointer is not merely inside the user
    /// window but actually MAPPED with the rights an access needs — a range check alone
    /// lets a supervisor copy fault on an unmapped page or write through a read-only one.
    pub fn leaf_flags(&self, va: VirtAddr) -> Option<PageFlags> {
        let mut table_pa = self.root;
        for level in [2u32, 1, 0] {
            let idx = va.table_index(level);
            // SAFETY: `table_pa` is a live table frame reached by the walk; `idx` < 512.
            let entry = unsafe { self.read_entry(table_pa, idx) };
            if !entry.is_valid() {
                return None;
            }
            if entry.is_leaf() {
                return Some(entry.flags());
            }
            table_pa = entry.addr();
        }
        None
    }

    pub fn translate(&self, va: VirtAddr) -> Option<PhysAddr> {
        let mut table_pa = self.root;
        // Levels VPN2 = 2, VPN1 = 1, VPN0 = 0.
        for level in [2u32, 1, 0] {
            let idx = va.table_index(level);
            // SAFETY: `table_pa` is a live table frame reached by the walk;
            // `idx` < 512.
            let entry = unsafe { self.read_entry(table_pa, idx) };
            if !entry.is_valid() {
                return None;
            }
            if entry.is_leaf() {
                // Leaf at this level: the page spans bits [12 + 9*level ..].
                let page_shift = 12 + 9 * level as u64;
                let page_mask = (1u64 << page_shift) - 1;
                let base = entry.addr().as_u64() & !page_mask;
                let offset = va.as_u64() & page_mask;
                return Some(PhysAddr(base + offset));
            }
            table_pa = entry.addr();
        }
        None
    }
}

// ===================================================================== tests ===

#[cfg(test)]
mod tests {
    use super::*;
    use abi::{FrameAllocator, PhysAddr, VirtAddr, PAGE_SIZE};

    /// A page-aligned 4 KiB backing frame for the mock allocator.
    #[derive(Clone, Copy)]
    #[repr(C, align(4096))]
    struct Frame([u8; 4096]);

    /// A bump frame allocator backed by a `Vec` of page-aligned frames.
    ///
    /// Frame `i` is handed out with physical address `i * PAGE_SIZE`, and
    /// `phys_offset` is the base virtual address of the `Vec`'s storage, so that
    /// `phys + phys_offset == &frames[i]` — a faithful identity-style offset map.
    struct MockAlloc {
        frames: Vec<Frame>,
        next: usize,
        base: u64,
    }

    impl MockAlloc {
        fn new(capacity: usize) -> MockAlloc {
            let mut frames = Vec::with_capacity(capacity);
            for _ in 0..capacity {
                frames.push(Frame([0u8; 4096]));
            }
            // `with_capacity` + exactly `capacity` pushes ⇒ no realloc, so the
            // base pointer stays valid for the allocator's lifetime.
            let base = frames.as_mut_ptr() as u64;
            MockAlloc {
                frames,
                next: 0,
                base,
            }
        }

        /// The physical→virtual offset to give `AddressSpace::new`.
        fn phys_offset(&self) -> u64 {
            self.base
        }
    }

    impl FrameAllocator for MockAlloc {
        fn alloc_frame(&mut self) -> Option<PhysAddr> {
            if self.next >= self.frames.len() {
                return None;
            }
            let i = self.next;
            self.next += 1;
            // Frames are pre-zeroed at construction and never reused.
            Some(PhysAddr((i as u64) * PAGE_SIZE))
        }

        fn free_frame(&mut self, _frame: PhysAddr) {
            // Bump allocator: frames are not reclaimed in the test harness.
        }
    }

    #[test]
    fn flags_bits_are_distinct_architectural_positions() {
        assert_eq!(PageFlags::V.bits(), 1 << 0);
        assert_eq!(PageFlags::R.bits(), 1 << 1);
        assert_eq!(PageFlags::W.bits(), 1 << 2);
        assert_eq!(PageFlags::X.bits(), 1 << 3);
        assert_eq!(PageFlags::U.bits(), 1 << 4);
        assert_eq!(PageFlags::G.bits(), 1 << 5);
        assert_eq!(PageFlags::A.bits(), 1 << 6);
        assert_eq!(PageFlags::D.bits(), 1 << 7);
        assert_eq!(
            PageFlags::RWX,
            PageFlags::R | PageFlags::W | PageFlags::X,
            "RWX is exactly the three permission bits"
        );
    }

    #[test]
    fn flags_union_and_contains() {
        let f = PageFlags::V | PageFlags::R | PageFlags::W;
        assert!(f.contains(PageFlags::V));
        assert!(f.contains(PageFlags::R));
        assert!(f.contains(PageFlags::W));
        assert!(!f.contains(PageFlags::X));
        assert!(f.contains(PageFlags::R | PageFlags::W));
        assert!(f.intersects(PageFlags::RWX));
        assert!(!PageFlags::V.intersects(PageFlags::RWX));
    }

    #[test]
    fn pte_encode_decode_round_trip() {
        // A page-aligned physical address whose frame number exercises high
        // PPN bits.
        let pa = PhysAddr(0x0000_00AB_CDEF_1000);
        let flags = PageFlags::V | PageFlags::R | PageFlags::W | PageFlags::D;
        let e = Pte::new(pa, flags);

        assert_eq!(
            e.ppn(),
            pa.as_u64() >> 12,
            "PPN is the shifted frame number"
        );
        assert_eq!(e.addr(), pa, "address field must round-trip");
        assert_eq!(e.flags(), flags, "flag field must round-trip");
        assert!(e.is_valid());
        assert!(e.is_leaf());
    }

    #[test]
    fn pte_masks_isolate_ppn_and_flags() {
        // A physical address whose low bits are dirty must be truncated to the
        // 4 KiB frame, and flags must never leak into the PPN field.
        let dirty_pa = PhysAddr(0x1234_5FFF);
        let e = Pte::new(dirty_pa, PageFlags::V | PageFlags::X);
        assert_eq!(e.addr(), PhysAddr(0x1234_5000));
        assert!(e.is_leaf());
        assert!(e.flags().contains(PageFlags::V | PageFlags::X));
        // The PPN field and the flag field do not overlap.
        assert_eq!(PPN_MASK & FLAG_MASK, 0);
    }

    #[test]
    fn leaf_vs_pointer_classification() {
        let pa = PhysAddr(0x0002_2000);

        // Pointer: V set, R=W=X=0.
        let pointer = Pte::new(pa, PageFlags::V);
        assert!(pointer.is_valid());
        assert!(!pointer.is_leaf(), "V-only entry is a pointer, not a leaf");

        // Leaf: any of R/W/X set.
        for perm in [PageFlags::R, PageFlags::W, PageFlags::X] {
            let leaf = Pte::new(pa, PageFlags::V | perm);
            assert!(leaf.is_valid());
            assert!(leaf.is_leaf(), "R/W/X set ⇒ leaf");
        }

        // Empty: neither valid nor a leaf.
        assert!(!Pte::EMPTY.is_valid());
        assert!(!Pte::EMPTY.is_leaf());
        assert_eq!(Pte::EMPTY.flags(), PageFlags::empty());
    }

    #[test]
    fn table_index_math() {
        // Sv39 VA: VPN2=1, VPN1=2, VPN0=3, page offset 0x123.
        let va = VirtAddr((1 << 30) | (2 << 21) | (3 << 12) | 0x123);
        assert_eq!(va.table_index(2), 1); // VPN2
        assert_eq!(va.table_index(1), 2); // VPN1
        assert_eq!(va.table_index(0), 3); // VPN0
    }

    #[test]
    fn page_table_is_one_frame() {
        assert_eq!(core::mem::size_of::<PageTable>() as u64, PAGE_SIZE);
        assert_eq!(core::mem::align_of::<PageTable>() as u64, PAGE_SIZE);
    }

    #[test]
    fn map_translate_unmap_round_trip() {
        let mut fa = MockAlloc::new(16);
        let phys_offset = fa.phys_offset();

        // Root frame from the same allocator/offset window.
        let root = fa.alloc_frame().unwrap();
        let mut space = AddressSpace::new(root, phys_offset);
        assert_eq!(space.root_phys(), root);

        let va = VirtAddr(0x0000_0000_4020_1000); // Sv39-range, 4 KiB aligned
        let pa = fa.alloc_frame().unwrap(); // the frame we will map
        let flags = PageFlags::R | PageFlags::W | PageFlags::U;

        // Before mapping: unmapped.
        assert_eq!(space.translate(va), None);

        // Map, then translate reflects it — including the in-page offset.
        space.map(va, pa, flags, &mut fa).unwrap();
        assert_eq!(space.translate(va), Some(pa));
        let va_mid = VirtAddr(va.as_u64() + 0x321);
        assert_eq!(space.translate(va_mid), Some(PhysAddr(pa.as_u64() + 0x321)));

        // Double-map is rejected.
        assert_eq!(
            space.map(va, pa, flags, &mut fa),
            Err(MapError::AlreadyMapped)
        );

        // Unmap returns the frame and clears the translation.
        assert_eq!(space.unmap(va), Some(pa));
        assert_eq!(space.translate(va), None);

        // Unmapping again is a no-op.
        assert_eq!(space.unmap(va), None);
    }

    #[test]
    fn map_rejects_unaligned() {
        let mut fa = MockAlloc::new(8);
        let phys_offset = fa.phys_offset();
        let root = fa.alloc_frame().unwrap();
        let mut space = AddressSpace::new(root, phys_offset);

        let bad_va = VirtAddr(0x1001);
        let pa = PhysAddr(0x2000);
        assert_eq!(
            space.map(bad_va, pa, PageFlags::R, &mut fa),
            Err(MapError::UnalignedVirt)
        );

        let va = VirtAddr(0x1000);
        let bad_pa = PhysAddr(0x2001);
        assert_eq!(
            space.map(va, bad_pa, PageFlags::R, &mut fa),
            Err(MapError::UnalignedPhys)
        );
    }

    #[test]
    fn map_rejects_leaf_without_permissions() {
        let mut fa = MockAlloc::new(8);
        let phys_offset = fa.phys_offset();
        let root = fa.alloc_frame().unwrap();
        let mut space = AddressSpace::new(root, phys_offset);

        let va = VirtAddr(0x0000_0000_0040_0000);
        let pa = fa.alloc_frame().unwrap();
        // V without any of R/W/X would encode a pointer, not a page.
        assert_eq!(
            space.map(va, pa, PageFlags::V, &mut fa),
            Err(MapError::InvalidLeafFlags)
        );
        assert_eq!(
            space.map(va, pa, PageFlags::empty(), &mut fa),
            Err(MapError::InvalidLeafFlags)
        );
    }

    #[test]
    fn map_reports_out_of_frames() {
        // Capacity 2: one frame becomes the root, one is the target frame — none
        // left for the two intermediate tables the first map needs.
        let mut fa = MockAlloc::new(2);
        let phys_offset = fa.phys_offset();
        let root = fa.alloc_frame().unwrap();
        let mut space = AddressSpace::new(root, phys_offset);

        let va = VirtAddr(0x0000_0000_0040_0000);
        let pa = fa.alloc_frame().unwrap();
        assert_eq!(
            space.map(va, pa, PageFlags::R | PageFlags::W, &mut fa),
            Err(MapError::OutOfFrames)
        );
    }

    #[test]
    fn two_mappings_share_intermediate_tables() {
        let mut fa = MockAlloc::new(16);
        let phys_offset = fa.phys_offset();
        let root = fa.alloc_frame().unwrap();
        let mut space = AddressSpace::new(root, phys_offset);
        let flags = PageFlags::R | PageFlags::W;

        // Two VAs in the same 2 MiB region ⇒ share VPN2/VPN1, differ in VPN0.
        let va0 = VirtAddr(0x0000_0000_0020_0000);
        let va1 = VirtAddr(0x0000_0000_0020_1000);
        let pa0 = fa.alloc_frame().unwrap();
        let pa1 = fa.alloc_frame().unwrap();

        space.map(va0, pa0, flags, &mut fa).unwrap();
        // Second map into the same 2 MiB window must not need more than the one
        // extra target frame — the VPN2/VPN1 tables are reused.
        let used_before = fa.next;
        space.map(va1, pa1, flags, &mut fa).unwrap();
        assert_eq!(
            fa.next, used_before,
            "sibling map reuses the shared intermediate tables"
        );

        assert_eq!(space.translate(va0), Some(pa0));
        assert_eq!(space.translate(va1), Some(pa1));

        // Unmapping one leaves the other intact.
        assert_eq!(space.unmap(va0), Some(pa0));
        assert_eq!(space.translate(va0), None);
        assert_eq!(space.translate(va1), Some(pa1));
    }

    #[test]
    fn create_allocates_zeroed_root() {
        let mut fa = MockAlloc::new(16);
        let phys_offset = fa.phys_offset();
        let mut space = AddressSpace::create(phys_offset, &mut fa).unwrap();

        let va = VirtAddr(0x0000_0000_1000_0000);
        let pa = fa.alloc_frame().unwrap();
        assert_eq!(space.translate(va), None);
        space
            .map(va, pa, PageFlags::R | PageFlags::W, &mut fa)
            .unwrap();
        assert_eq!(space.translate(va), Some(pa));
    }

    #[test]
    fn satp_encodes_sv39_mode_and_root_ppn() {
        let mut fa = MockAlloc::new(8);
        let phys_offset = fa.phys_offset();
        // Force a non-zero root frame so the PPN field is exercised.
        let _throwaway = fa.alloc_frame().unwrap();
        let root = fa.alloc_frame().unwrap();
        let space = AddressSpace::new(root, phys_offset);

        assert_eq!(space.root_ppn(), root.as_u64() >> 12);
        let satp = space.satp();
        assert_eq!(satp >> 60, 8, "MODE field == Sv39 (8)");
        assert_eq!(
            satp & PPN_BITS,
            root.as_u64() >> 12,
            "PPN field == root PPN"
        );
        assert_eq!((satp >> 44) & 0xffff, 0, "ASID field == 0");
    }

    /// A superpage in the walk must be REFUSED, not walked through.
    ///
    /// Nothing in this tree ever creates one -- the kernel maps 4 KiB pages and builds its
    /// tables from scratch -- so this guard is unreachable through the public API and was
    /// covered by nothing. That does not make it dead code: it is the defence for the day a
    /// superpage arrives or a foreign table is walked, and a defence nothing exercises is
    /// indistinguishable from one that is absent. So construct the state the API cannot
    /// produce. (On riscv a superpage is simply a leaf -- any of R/W/X -- at level 2 or 1.)
    #[test]
    fn a_superpage_in_the_walk_is_refused_rather_than_walked_through() {
        let mut fa = MockAlloc::new(16);
        let phys_offset = fa.phys_offset();
        let root = fa.alloc_frame().unwrap();
        let mut space = AddressSpace::new(root, phys_offset);

        let va = VirtAddr(0x0000_003F_1234_5000);
        let pa = fa.alloc_frame().unwrap();
        let flags = PageFlags::V | PageFlags::R | PageFlags::W | PageFlags::U;

        // Plant a level-2 leaf on this address's path: V + R/W/X makes it a superpage.
        let idx = va.table_index(2);
        let superpage = Pte::new(pa, PageFlags::V | PageFlags::R | PageFlags::W);
        // SAFETY: the mock allocator's frames live for the whole test and are reachable at
        // `phys + phys_offset` — exactly how `AddressSpace` itself reaches them.
        unsafe {
            let table = (root.as_u64() + phys_offset) as *mut Pte;
            core::ptr::write(table.add(idx), superpage);
        }

        assert_eq!(
            space.map(va, pa, flags, &mut fa),
            Err(MapError::SuperpagePresent),
            "map walked through a superpage instead of refusing it"
        );
    }
}
