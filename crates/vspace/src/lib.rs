#![cfg_attr(not(test), no_std)]

//! vspace — x86-64 4-level page tables + a small address-space model.
//!
//! VERIFIED TCB crate. This is the executable Rust; Verus specs come later.
//! See docs/nucleus-design.md and docs/verification.md.
//!
//! The nucleus identity-maps low RAM, so physical page-table frames are reached
//! through a caller-supplied physical→virtual offset (`phys_offset`). With an
//! identity map the offset is `0` (physical address == virtual address).
//!
//! Only 4 KiB leaf pages are produced by [`AddressSpace::map`]; huge pages that
//! appear on a walk (created by some other agent) are recognized but not created.

use core::ops::{BitAnd, BitOr, Not};

use abi::{FrameAllocator, PhysAddr, VirtAddr, PAGE_SIZE};

// -------------------------------------------------------------------- bit masks

/// Bits 12..=51 of a page-table entry hold the physical frame address (4 KiB
/// aligned, up to a 52-bit physical address space).
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Everything that is not the address field is a flag / control / available bit
/// (bits 0..=11, 52..=62 available, and 63 = NX).
const FLAG_MASK: u64 = !ADDR_MASK;

// -------------------------------------------------------------------- PageFlags

/// Architectural page-table entry flags (the subset the nucleus uses).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct PageFlags(u64);

impl PageFlags {
    /// Entry is valid / the mapping is live.
    pub const PRESENT: PageFlags = PageFlags(1 << 0);
    /// Writes are permitted.
    pub const WRITABLE: PageFlags = PageFlags(1 << 1);
    /// User (ring 3) access is permitted.
    pub const USER: PageFlags = PageFlags(1 << 2);
    /// Page-Size bit: at PDPT/PD this entry is a 1 GiB / 2 MiB leaf, not a link.
    pub const HUGE: PageFlags = PageFlags(1 << 7);
    /// Execution is forbidden (requires EFER.NXE).
    pub const NO_EXEC: PageFlags = PageFlags(1 << 63);
    /// Uncached: PWT (bit 3) + PCD (bit 4) together.
    ///
    /// Both, not just PCD: with the default PAT, PCD alone selects UC- which a later PAT
    /// change can weaken, while PWT+PCD selects strong UC. For a register aperture the
    /// difference is between "the device sees the stores it was sent, in order" and a fault
    /// that reproduces on silicon and never under QEMU TCG.
    pub const NO_CACHE: PageFlags = PageFlags((1 << 3) | (1 << 4));

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

    /// Build from raw bits, keeping only flag (non-address) bits.
    #[inline]
    pub const fn from_bits_truncate(bits: u64) -> PageFlags {
        PageFlags(bits & FLAG_MASK)
    }

    /// True if every bit in `other` is also set in `self`.
    #[inline]
    pub const fn contains(self, other: PageFlags) -> bool {
        self.0 & other.0 == other.0
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

// --------------------------------------------------------------- PageTableEntry

/// A single 64-bit page-table entry: physical frame address ⊕ flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct PageTableEntry(pub u64);

impl PageTableEntry {
    /// An empty (not-present) entry.
    pub const EMPTY: PageTableEntry = PageTableEntry(0);

    /// Encode a frame physical address + flags into an entry. The address is
    /// masked to its 4 KiB-aligned field and the flags to their bit field, so a
    /// malformed input can never bleed into the other region.
    ///
    // PROOF(later): decode∘encode is identity — for a page-aligned `pa` and any
    // `flags`, `PageTableEntry::new(pa, flags).addr() == pa` and
    // `.flags() == PageFlags::from_bits_truncate(flags.bits())`.
    #[inline]
    pub const fn new(pa: PhysAddr, flags: PageFlags) -> PageTableEntry {
        PageTableEntry((pa.as_u64() & ADDR_MASK) | (flags.bits() & FLAG_MASK))
    }

    /// Decode the physical frame address (4 KiB aligned).
    #[inline]
    pub const fn addr(self) -> PhysAddr {
        PhysAddr(self.0 & ADDR_MASK)
    }

    /// Decode the flags.
    #[inline]
    pub const fn flags(self) -> PageFlags {
        PageFlags(self.0 & FLAG_MASK)
    }

    /// True if the PRESENT bit is set.
    #[inline]
    pub const fn is_present(self) -> bool {
        self.0 & PageFlags::PRESENT.0 != 0
    }

    /// True if the HUGE (page-size) bit is set.
    #[inline]
    pub const fn is_huge(self) -> bool {
        self.0 & PageFlags::HUGE.0 != 0
    }
}

// -------------------------------------------------------------------- PageTable

/// One level of the radix tree: 512 entries, exactly one 4 KiB frame.
#[derive(Clone, Copy)]
#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [PageTableEntry; 512],
}

impl PageTable {
    /// A fully-zeroed (all not-present) table.
    #[inline]
    pub const fn new() -> PageTable {
        PageTable {
            entries: [PageTableEntry::EMPTY; 512],
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

/// Why a [`AddressSpace::map`] failed.
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
    /// A huge (2 MiB / 1 GiB) page occupies the path; can't insert a 4 KiB leaf.
    HugePagePresent,
}

// ----------------------------------------------------------------- AddressSpace

/// An x86-64 virtual address space: the PML4 root frame plus the offset used to
/// reach physical page-table frames from kernel virtual addresses.
///
/// The [`AddressSpace`] never writes CR3 (that needs an arch dependency); use
/// [`AddressSpace::pml4_phys`] to obtain the value the integrator loads.
pub struct AddressSpace {
    /// Physical address of the PML4 (level-3) frame.
    pml4: PhysAddr,
    /// physical→virtual offset: `virt = phys + phys_offset` for identity-mapped
    /// low RAM. Default `0` == identity map.
    phys_offset: u64,
}

impl AddressSpace {
    /// Wrap an existing, already-zeroed PML4 frame.
    ///
    /// The caller must guarantee `pml4_phys` names a live 4 KiB frame reachable
    /// at `pml4_phys + phys_offset` and that it is zeroed (all entries
    /// not-present). Frame allocators in this project hand out zeroed frames.
    #[inline]
    pub const fn new(pml4_phys: PhysAddr, phys_offset: u64) -> AddressSpace {
        AddressSpace {
            pml4: pml4_phys,
            phys_offset,
        }
    }

    /// Allocate and zero a fresh PML4 from `fa`, returning the new space.
    pub fn create(phys_offset: u64, fa: &mut dyn FrameAllocator) -> Option<AddressSpace> {
        let pml4 = fa.alloc_frame()?;
        let space = AddressSpace::new(pml4, phys_offset);
        // SAFETY: `pml4` is a live frame just handed out by `fa`, reachable at
        // `pml4 + phys_offset`. We zero it in case the allocator does not
        // guarantee zeroed frames; nothing else aliases it yet.
        unsafe {
            *space.table_ptr(pml4) = PageTable::new();
        }
        Some(space)
    }

    /// The PML4 physical address — the value the integrator loads into CR3.
    #[inline]
    pub const fn pml4_phys(&self) -> PhysAddr {
        self.pml4
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
    unsafe fn read_entry(&self, table_pa: PhysAddr, idx: usize) -> PageTableEntry {
        (*self.table_ptr(table_pa)).entries[idx]
    }

    /// Write entry `idx` of the table at `table_pa`.
    ///
    /// SAFETY: `table_pa` must be a live page-table frame reachable at the
    /// offset; `idx < 512`. `&mut self` serializes writes into the tree.
    #[inline]
    unsafe fn write_entry(&mut self, table_pa: PhysAddr, idx: usize, e: PageTableEntry) {
        (*self.table_ptr(table_pa)).entries[idx] = e;
    }

    /// Map `va` → `pa` (4 KiB) with `flags`, allocating intermediate tables from
    /// `fa` as needed. PRESENT is forced on the leaf.
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

        // Descend the three upper levels (PML4=3, PDPT=2, PD=1), creating links.
        let mut table_pa = self.pml4;
        for level in [3u32, 2, 1] {
            let idx = va.table_index(level);
            // SAFETY: `table_pa` is a live table frame (PML4 root, or a link we
            // just followed / created); `idx` is a 9-bit table index (< 512).
            let entry = unsafe { self.read_entry(table_pa, idx) };
            if entry.is_present() {
                if entry.is_huge() {
                    return Err(MapError::HugePagePresent);
                }
                table_pa = entry.addr();
            } else {
                let frame = fa.alloc_frame().ok_or(MapError::OutOfFrames)?;
                // SAFETY: fresh live frame from `fa`, reachable at the offset.
                unsafe {
                    *self.table_ptr(frame) = PageTable::new();
                }
                // Intermediate links are permissive (P|W|U); the leaf's flags are
                // what actually gate access, since a hardware walk ANDs every level.
                let link = PageTableEntry::new(
                    frame,
                    PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER,
                );
                // SAFETY: as above; `idx` < 512.
                unsafe { self.write_entry(table_pa, idx, link) };
                table_pa = frame;
            }
        }

        // Leaf (PT = level 0).
        let idx = va.table_index(0);
        // SAFETY: `table_pa` is the live PT frame; `idx` < 512.
        let leaf = unsafe { self.read_entry(table_pa, idx) };
        if leaf.is_present() {
            return Err(MapError::AlreadyMapped);
        }
        let entry = PageTableEntry::new(pa, flags | PageFlags::PRESENT);
        // SAFETY: as above.
        unsafe { self.write_entry(table_pa, idx, entry) };
        Ok(())
    }

    /// Remove the 4 KiB mapping for `va`, returning the physical frame it named.
    ///
    /// Intermediate tables are left in place (not reclaimed — that needs the
    /// frame allocator and reference counts). Huge pages are not unmapped.
    ///
    // PROOF(later): after `unmap(va)` returns, `translate(va) == None`.
    pub fn unmap(&mut self, va: VirtAddr) -> Option<PhysAddr> {
        let mut table_pa = self.pml4;
        for level in [3u32, 2, 1] {
            let idx = va.table_index(level);
            // SAFETY: live table frame; `idx` < 512.
            let entry = unsafe { self.read_entry(table_pa, idx) };
            if !entry.is_present() || entry.is_huge() {
                return None;
            }
            table_pa = entry.addr();
        }
        let idx = va.table_index(0);
        // SAFETY: live PT frame; `idx` < 512.
        let leaf = unsafe { self.read_entry(table_pa, idx) };
        if !leaf.is_present() {
            return None;
        }
        let pa = leaf.addr();
        // SAFETY: as above; clearing the leaf drops the mapping.
        unsafe { self.write_entry(table_pa, idx, PageTableEntry::EMPTY) };
        Some(pa)
    }

    /// Translate `va` to a physical address by walking the tables. Handles 4 KiB
    /// leaves and huge (2 MiB / 1 GiB) leaves; returns `None` if unmapped.
    ///
    // PROOF(later): `translate` reflects the last `map`/`unmap` on `va` — i.e.
    // the concrete page-walk is a refinement of the abstract va→pa mapping.
    /// Flags of the leaf entry mapping `va`, or `None` if `va` is unmapped.
    ///
    /// The kernel uses this to check that a user pointer is not merely inside the user
    /// window but actually MAPPED with the rights an access needs — a range check alone
    /// lets a ring-0 copy fault on an unmapped page or write through a read-only one.
    /// Intermediate tables here are built permissive (`USER | WRITABLE`), so the leaf's
    /// flags govern the effective permission.
    pub fn leaf_flags(&self, va: VirtAddr) -> Option<PageFlags> {
        let mut table_pa = self.pml4;
        for level in [3u32, 2, 1, 0] {
            let idx = va.table_index(level);
            // SAFETY: `table_pa` is a live table frame reached by the walk; `idx` < 512.
            let entry = unsafe { self.read_entry(table_pa, idx) };
            if !entry.is_present() {
                return None;
            }
            if level == 0 || entry.is_huge() {
                return Some(entry.flags());
            }
            table_pa = entry.addr();
        }
        None
    }

    pub fn translate(&self, va: VirtAddr) -> Option<PhysAddr> {
        let mut table_pa = self.pml4;
        // Levels PML4=3, PDPT=2, PD=1, PT=0.
        for level in [3u32, 2, 1, 0] {
            let idx = va.table_index(level);
            // SAFETY: `table_pa` is a live table frame reached by the walk;
            // `idx` < 512.
            let entry = unsafe { self.read_entry(table_pa, idx) };
            if !entry.is_present() {
                return None;
            }
            if level == 0 {
                let offset = va.as_u64() & (PAGE_SIZE - 1);
                return Some(PhysAddr(entry.addr().as_u64() + offset));
            }
            if entry.is_huge() {
                // Leaf at this level: page spans bits [12 + 9*level ..].
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
        assert_eq!(PageFlags::PRESENT.bits(), 1 << 0);
        assert_eq!(PageFlags::WRITABLE.bits(), 1 << 1);
        assert_eq!(PageFlags::USER.bits(), 1 << 2);
        assert_eq!(PageFlags::HUGE.bits(), 1 << 7);
        assert_eq!(PageFlags::NO_EXEC.bits(), 1 << 63);
        // NO_CACHE was added without being added HERE, and nothing else checks it: zeroing it
        // left all 219 tests green, the nucleus building, and the boot still printing
        // "aperture mapped uncached" — because that line is printed on `map_page` returning
        // true, not on the bits. QEMU TCG ignores PAT/PCD, so the rig cannot see it either.
        // PWT|PCD, both: with the default PAT, PCD alone selects UC-, which a later PAT change
        // can weaken, while PWT+PCD selects strong UC.
        assert_eq!(PageFlags::NO_CACHE.bits(), (1 << 3) | (1 << 4));
        assert!(PageFlags::NO_CACHE.contains(PageFlags(1 << 3)));
        assert!(PageFlags::NO_CACHE.contains(PageFlags(1 << 4)));
    }

    #[test]
    fn flags_union_and_contains() {
        let f = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::NO_EXEC;
        assert!(f.contains(PageFlags::PRESENT));
        assert!(f.contains(PageFlags::WRITABLE));
        assert!(f.contains(PageFlags::NO_EXEC));
        assert!(!f.contains(PageFlags::USER));
        assert!(f.contains(PageFlags::PRESENT | PageFlags::WRITABLE));
    }

    #[test]
    fn entry_encode_decode_round_trip() {
        let pa = PhysAddr(0x0000_00AB_CDEF_1000); // 4 KiB aligned
        let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::NO_EXEC;
        let e = PageTableEntry::new(pa, flags);

        assert_eq!(e.addr(), pa, "address field must round-trip");
        assert_eq!(e.flags(), flags, "flag field must round-trip");
        assert!(e.is_present());
        assert!(!e.is_huge());
    }

    #[test]
    fn entry_masks_isolate_address_and_flags() {
        // A physical address whose low bits are dirty must be truncated to the
        // 4 KiB frame, and flags must never leak into the address field.
        let dirty_pa = PhysAddr(0x1234_5FFF);
        let e = PageTableEntry::new(dirty_pa, PageFlags::PRESENT | PageFlags::HUGE);
        assert_eq!(e.addr(), PhysAddr(0x1234_5000));
        assert!(e.is_huge());
        assert!(e.flags().contains(PageFlags::PRESENT | PageFlags::HUGE));
    }

    #[test]
    fn empty_entry_is_absent() {
        assert_eq!(PageTableEntry::EMPTY, PageTableEntry(0));
        assert!(!PageTableEntry::EMPTY.is_present());
        assert_eq!(PageTableEntry::EMPTY.flags(), PageFlags::empty());
    }

    #[test]
    fn table_index_math() {
        // va with a distinct 9-bit index at every level:
        // PML4=1, PDPT=2, PD=3, PT=4, page offset 0x123.
        let va = VirtAddr((1 << 39) | (2 << 30) | (3 << 21) | (4 << 12) | 0x123);
        assert_eq!(va.table_index(3), 1); // PML4
        assert_eq!(va.table_index(2), 2); // PDPT
        assert_eq!(va.table_index(1), 3); // PD
        assert_eq!(va.table_index(0), 4); // PT
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

        // PML4 frame from the same allocator/offset window.
        let pml4 = fa.alloc_frame().unwrap();
        let mut space = AddressSpace::new(pml4, phys_offset);
        assert_eq!(space.pml4_phys(), pml4);

        let va = VirtAddr(0x0000_7F00_1234_5000); // canonical, 4 KiB aligned
        let pa = fa.alloc_frame().unwrap(); // the frame we will map
        let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;

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

    /// A HUGE entry anywhere in the walk must be REFUSED, not walked through.
    ///
    /// Nothing in this tree ever SETS the huge bit — the kernel creates 4 KiB mappings only and
    /// builds its tables from scratch — so both guards are unreachable through the public API
    /// and were covered by nothing: `tools/mutate.py` deleted each in turn and the suite stayed
    /// green. They are defensive, for the day a huge page arrives or a table this kernel did not
    /// build is walked, and a defence nothing exercises is indistinguishable from one that is
    /// absent. Constructing the state directly is the only way to test a check written for a
    /// state the API cannot produce — the same reason `Domain::force_mapping` exists.
    #[test]
    fn a_huge_entry_in_the_walk_is_refused_rather_than_walked_through() {
        let mut fa = MockAlloc::new(16);
        let phys_offset = fa.phys_offset();
        let pml4 = fa.alloc_frame().unwrap();
        let mut space = AddressSpace::new(pml4, phys_offset);

        let va = VirtAddr(0x0000_7F00_1234_5000);
        let pa = fa.alloc_frame().unwrap();
        let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;

        // Plant it at the top level of this address's walk.
        let idx = ((va.as_u64() >> 39) & 0x1FF) as usize;
        let huge = PageTableEntry::new(pa, PageFlags::PRESENT | PageFlags::HUGE);
        // SAFETY: the mock allocator's frames live for the whole test and are reachable at
        // `phys + phys_offset` — exactly how `AddressSpace` itself reaches them.
        unsafe {
            let table = (pml4.as_u64() + phys_offset) as *mut PageTableEntry;
            core::ptr::write(table.add(idx), huge);
        }

        assert_eq!(
            space.map(va, pa, flags, &mut fa),
            Err(MapError::HugePagePresent),
            "map walked through a huge entry instead of refusing it"
        );
    }

    /// The same guard on the UNMAP side, and it needs a sharper setup than the one above.
    ///
    /// Asserting `unmap == None` against a huge entry planted at the TOP level passes whether
    /// or not the guard is there: without it the walk descends into the entry's target frame,
    /// finds it zeroed, and returns `None` anyway — the right answer for the wrong reason, in a
    /// test written to catch exactly that. So the frame walked into has to hold a PRESENT leaf.
    /// Here the huge bit is set on the last table entry of a mapping that already exists, so a
    /// walk that ignores it lands on a real leaf and returns `Some`.
    #[test]
    fn unmap_refuses_a_huge_entry_rather_than_walking_into_a_live_table() {
        let mut fa = MockAlloc::new(16);
        let phys_offset = fa.phys_offset();
        let pml4 = fa.alloc_frame().unwrap();
        let mut space = AddressSpace::new(pml4, phys_offset);

        let va = VirtAddr(0x0000_7F00_1234_5000);
        let pa = fa.alloc_frame().unwrap();
        let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;
        space.map(va, pa, flags, &mut fa).unwrap();
        assert_eq!(
            space.translate(va),
            Some(pa),
            "the mapping must exist first"
        );

        // SAFETY: every table below is a live mock frame reachable at `phys + phys_offset`,
        // which is how `AddressSpace` reaches them too.
        unsafe {
            let read_e = |t: PhysAddr, i: usize| -> PageTableEntry {
                core::ptr::read(((t.as_u64() + phys_offset) as *const PageTableEntry).add(i))
            };
            let pdpt = read_e(pml4, ((va.as_u64() >> 39) & 0x1FF) as usize).addr();
            let pd = read_e(pdpt, ((va.as_u64() >> 30) & 0x1FF) as usize).addr();
            let idx = ((va.as_u64() >> 21) & 0x1FF) as usize;
            let e = read_e(pd, idx);
            // Same address, HUGE now set: a walk that ignores the bit reaches the live leaf.
            core::ptr::write(
                ((pd.as_u64() + phys_offset) as *mut PageTableEntry).add(idx),
                PageTableEntry::new(e.addr(), e.flags() | PageFlags::HUGE),
            );
        }

        assert_eq!(
            space.unmap(va),
            None,
            "unmap treated a huge entry as a table and walked into a live one"
        );
    }

    #[test]
    fn map_rejects_unaligned() {
        let mut fa = MockAlloc::new(8);
        let phys_offset = fa.phys_offset();
        let pml4 = fa.alloc_frame().unwrap();
        let mut space = AddressSpace::new(pml4, phys_offset);

        let bad_va = VirtAddr(0x1001);
        let pa = PhysAddr(0x2000);
        assert_eq!(
            space.map(bad_va, pa, PageFlags::PRESENT, &mut fa),
            Err(MapError::UnalignedVirt)
        );

        let va = VirtAddr(0x1000);
        let bad_pa = PhysAddr(0x2001);
        assert_eq!(
            space.map(va, bad_pa, PageFlags::PRESENT, &mut fa),
            Err(MapError::UnalignedPhys)
        );
    }

    #[test]
    fn map_reports_out_of_frames() {
        // Capacity 2: one frame becomes the PML4, one is the target frame — none
        // left for the three intermediate tables the first map needs.
        let mut fa = MockAlloc::new(2);
        let phys_offset = fa.phys_offset();
        let pml4 = fa.alloc_frame().unwrap();
        let mut space = AddressSpace::new(pml4, phys_offset);

        let va = VirtAddr(0x0000_0000_0040_0000);
        let pa = fa.alloc_frame().unwrap();
        assert_eq!(
            space.map(va, pa, PageFlags::PRESENT, &mut fa),
            Err(MapError::OutOfFrames)
        );
    }

    #[test]
    fn two_mappings_share_intermediate_tables() {
        let mut fa = MockAlloc::new(16);
        let phys_offset = fa.phys_offset();
        let pml4 = fa.alloc_frame().unwrap();
        let mut space = AddressSpace::new(pml4, phys_offset);
        let flags = PageFlags::PRESENT | PageFlags::WRITABLE;

        // Two VAs in the same 2 MiB region ⇒ share PML4/PDPT/PD, differ in PT.
        let va0 = VirtAddr(0x0000_0000_0020_0000);
        let va1 = VirtAddr(0x0000_0000_0020_1000);
        let pa0 = fa.alloc_frame().unwrap();
        let pa1 = fa.alloc_frame().unwrap();

        space.map(va0, pa0, flags, &mut fa).unwrap();
        space.map(va1, pa1, flags, &mut fa).unwrap();

        assert_eq!(space.translate(va0), Some(pa0));
        assert_eq!(space.translate(va1), Some(pa1));

        // Unmapping one leaves the other intact.
        assert_eq!(space.unmap(va0), Some(pa0));
        assert_eq!(space.translate(va0), None);
        assert_eq!(space.translate(va1), Some(pa1));
    }

    #[test]
    fn create_allocates_zeroed_pml4() {
        let mut fa = MockAlloc::new(16);
        let phys_offset = fa.phys_offset();
        let mut space = AddressSpace::create(phys_offset, &mut fa).unwrap();

        let va = VirtAddr(0x0000_1000_0000_0000);
        let pa = fa.alloc_frame().unwrap();
        assert_eq!(space.translate(va), None);
        space
            .map(va, pa, PageFlags::PRESENT | PageFlags::WRITABLE, &mut fa)
            .unwrap();
        assert_eq!(space.translate(va), Some(pa));
    }
}
