#![cfg_attr(not(test), no_std)]
//! mm — physical frame allocation for the Rustproof nucleus.
//!
//! A [`BitmapAllocator`] tracks one bit per 4 KiB physical frame (bit set = USED,
//! bit clear = FREE) over a caller-supplied `&'static mut [u64]` bitmap. There is
//! no heap: the integrator sizes the bitmap with [`BitmapAllocator::bitmap_words_needed`]
//! and hands it in. Frames outside every `Usable` region, frames in gaps of the boot
//! map, and frames below `reserve_below` (kernel image + low structures) are marked
//! USED at construction; everything else is free. Frames whose physical address is
//! `< dma_top` form a DMA-capable low pool served by [`BitmapAllocator::alloc_dma_frame`].
//!
//! Host-unit-testable: `#[cfg(test)]` runs on std with synthetic boot maps; the real
//! build is `no_std`.

use abi::{FrameAllocator, MemoryKind, MemoryRegion, PhysAddr, PAGE_SHIFT, PAGE_SIZE};

/// Bits tracked per bitmap word.
const BITS_PER_WORD: usize = 64;

/// A physical frame allocator backed by a fixed bitmap (bit set = used).
///
/// `bitmap` is caller-owned static storage; `total` is the number of frames the
/// bitmap actually tracks (frames `0..total`). Any tail bits `>= total` in the last
/// word are left permanently USED so they can never be handed out.
pub struct BitmapAllocator {
    /// One bit per 4 KiB frame; bit set = USED, bit clear = FREE.
    bitmap: &'static mut [u64],
    /// Number of frames tracked (frames `0..total`).
    total: usize,
    /// Count of currently-free frames within `0..total`.
    free: usize,
    /// Search hint: no free frame is known to exist below this frame index.
    cursor: usize,
    /// First frame the GENERAL pool may hand out: everything below it belongs to the DMA
    /// arena. `dma_top` and `reserve_below` stop being two independent limits and become a
    /// PARTITION — `[reserve_below, dma_top)` is the arena, `[dma_top, ..)` is everything
    /// else — so a frame that has been device-reachable never becomes a page table, and a
    /// page table never becomes device-reachable.
    general_floor: usize,
    /// Byte address ceiling of the DMA-capable pool (frame `f` is DMA-capable iff
    /// `(f << PAGE_SHIFT) < dma_top`).
    dma_top: u64,
    /// Byte address floor below which frames are permanently reserved. A legitimately
    /// allocated frame is always `>= reserve_below`, so `free_frame` rejects any lower
    /// address as a caller error (e.g. freeing the null / low frame).
    reserve_below: u64,
}

impl BitmapAllocator {
    /// Build an allocator over `regions`, using `bitmap` as tracking storage.
    ///
    /// The bitmap covers frame `0` up to the highest fully-`Usable` frame. Frames in
    /// non-`Usable` regions, frames not covered by any region, and frames below
    /// `reserve_below` are marked USED; every other frame is free. `dma_top` bounds
    /// the DMA-capable low pool.
    ///
    /// `bitmap` must have at least [`bitmap_words_needed`](Self::bitmap_words_needed)
    /// words; a larger buffer is fine (extra words are held USED). A smaller buffer is
    /// clamped defensively (the tracked frame count is reduced to fit).
    pub fn new(
        regions: &[MemoryRegion],
        bitmap: &'static mut [u64],
        reserve_below: u64,
        dma_top: u64,
    ) -> Self {
        // Highest usable frame -> tracked frame count, clamped to the bitmap capacity.
        let frames_from_regions = highest_usable_frame(regions).map_or(0, |h| h + 1);
        let capacity_frames = (bitmap.len() as u64).saturating_mul(BITS_PER_WORD as u64);
        let total = frames_from_regions.min(capacity_frames) as usize;

        // Start with everything USED; tail bits (>= total) stay USED forever this way.
        for word in bitmap.iter_mut() {
            *word = u64::MAX;
        }

        let general_floor = ((dma_top + PAGE_SIZE - 1) >> PAGE_SHIFT) as usize;
        let mut alloc = BitmapAllocator {
            bitmap,
            total,
            free: 0,
            cursor: 0,
            general_floor,
            dma_top,
            reserve_below,
        };

        // Clear (free) frames fully contained in a Usable region.
        if total > 0 {
            let last = total - 1;
            for region in regions {
                if region.kind != MemoryKind::Usable {
                    continue;
                }
                let end = region.end();
                if end < PAGE_SIZE {
                    continue;
                }
                // First frame whose start >= region.start (round up), and last frame
                // whose end <= region.end (round down): the fully-contained span.
                let f_lo = ((region.start + PAGE_SIZE - 1) >> PAGE_SHIFT) as usize;
                let f_hi = ((end >> PAGE_SHIFT) - 1) as usize;
                if f_lo > f_hi {
                    continue;
                }
                let f_hi = f_hi.min(last);
                let mut f = f_lo;
                while f <= f_hi {
                    alloc.set_free(f);
                    f += 1;
                }
            }
        }

        // Re-mark every NON-Usable region as USED.
        //
        // Without this, a `Reserved` (or ACPI / Unusable) span that OVERLAPS a `Usable` one is
        // handed out as ordinary memory: the pass above frees every frame inside the Usable
        // region and nothing ever took the reserved ones back. Measured on a map declaring
        // 0..16 MiB Usable with 8..9 MiB Reserved, the allocator handed out all 256 reserved
        // frames, which then become page tables and user stacks. On x86 this map is not ours —
        // it is parsed from the PVH `hvm_start_info` the hypervisor supplies
        // (crates/kernel/src/pvh.rs), an arbitrary-length list of arbitrary triples from
        // outside the TCB — and nothing anywhere validated that its regions are disjoint.
        //
        // Rounding here is OUTWARD, the opposite of the Usable pass above, and the asymmetry
        // is the point: a frame only PARTIALLY covered by a Usable region is not fully backed
        // so it must not be freed, while a frame only partially covered by a Reserved region
        // still contains bytes that must never be handed out. Both directions round toward
        // "not allocatable".
        if total > 0 {
            let last = total - 1;
            for region in regions {
                if region.kind == MemoryKind::Usable || region.len == 0 {
                    continue;
                }
                let f_lo = (region.start >> PAGE_SHIFT) as usize;
                let f_hi = (((region.end() + PAGE_SIZE - 1) >> PAGE_SHIFT).max(1) - 1) as usize;
                if f_lo > last {
                    continue;
                }
                let mut f = f_lo;
                let hi = f_hi.min(last);
                while f <= hi {
                    alloc.set_used(f);
                    f += 1;
                }
            }
        }

        // Re-mark low frames (kernel image + low structures) as USED. This runs after
        // the Usable pass so it always wins on overlap.
        let mut f = 0usize;
        while f < total && ((f as u64) << PAGE_SHIFT) < reserve_below {
            alloc.set_used(f);
            f += 1;
        }

        // Tally the free frames once.
        alloc.free = (0..total).filter(|&f| !alloc.is_used(f)).count();
        alloc
    }

    /// Number of `u64` words a bitmap must have to track `regions` — i.e. one bit per
    /// frame from 0 through the highest fully-`Usable` frame. Zero if nothing is usable.
    /// The integrator uses this to size the static bitmap before calling [`new`](Self::new).
    pub fn bitmap_words_needed(regions: &[MemoryRegion]) -> usize {
        match highest_usable_frame(regions) {
            Some(hi) => {
                let frames = hi + 1;
                ((frames + BITS_PER_WORD as u64 - 1) / BITS_PER_WORD as u64) as usize
            }
            None => 0,
        }
    }

    /// Frames free right now.
    #[inline]
    pub fn free_count(&self) -> usize {
        self.free
    }

    /// Total frames tracked by the bitmap (frames `0..total_frames`).
    #[inline]
    pub fn total_frames(&self) -> usize {
        self.total
    }

    /// Allocate one free frame whose physical address is `< dma_top`, or `None` if the
    /// DMA pool is exhausted. Scans from frame 0 (the DMA pool is small and low).
    pub fn alloc_dma_frame(&mut self) -> Option<PhysAddr> {
        // Bounded at both ends. The low bound is belt-and-braces and CANNOT be observed
        // through this API — frames below `reserve_below` are already marked USED at
        // construction, so a scan from zero finds nothing there either. It is here to state
        // the arena's extent rather than leave it holding incidentally; there is deliberately
        // no test claiming to verify it, because such a test could not fail.
        let mut f = (self.reserve_below >> PAGE_SHIFT) as usize;
        while f < self.total && ((f as u64) << PAGE_SHIFT) < self.dma_top {
            if !self.is_used(f) {
                self.set_used(f);
                self.free -= 1;
                return Some(Self::frame_addr(f));
            }
            f += 1;
        }
        None
    }

    // ---------------------------------------------------------------- internals

    #[inline]
    fn frame_addr(frame: usize) -> PhysAddr {
        PhysAddr((frame as u64) << PAGE_SHIFT)
    }

    #[inline]
    fn word_bit(frame: usize) -> (usize, u64) {
        (frame / BITS_PER_WORD, 1u64 << (frame % BITS_PER_WORD))
    }

    #[inline]
    fn is_used(&self, frame: usize) -> bool {
        let (w, mask) = Self::word_bit(frame);
        self.bitmap[w] & mask != 0
    }

    #[inline]
    fn set_used(&mut self, frame: usize) {
        let (w, mask) = Self::word_bit(frame);
        self.bitmap[w] |= mask;
    }

    #[inline]
    fn set_free(&mut self, frame: usize) {
        let (w, mask) = Self::word_bit(frame);
        self.bitmap[w] &= !mask;
    }

    /// First free frame at index `>= from`, or `None`. Scans a word at a time and uses
    /// `trailing_zeros` on the inverted word to find the first clear bit.
    fn first_free(&self, from: usize) -> Option<usize> {
        let mut f = from;
        while f < self.total {
            let w = f / BITS_PER_WORD;
            let word = self.bitmap[w];
            if word == u64::MAX {
                f = (w + 1) * BITS_PER_WORD;
                continue;
            }
            let start_bit = f % BITS_PER_WORD;
            // Force bits below `start_bit` to USED so we never return a frame < `from`.
            let masked = word | ((1u64 << start_bit) - 1);
            if masked != u64::MAX {
                let bit = (!masked).trailing_zeros() as usize;
                let frame = w * BITS_PER_WORD + bit;
                // Tail bits (>= total) are held USED, so a real find is always in range;
                // this guard is belt-and-suspenders for a clamped bitmap.
                return if frame < self.total {
                    Some(frame)
                } else {
                    None
                };
            }
            f = (w + 1) * BITS_PER_WORD;
        }
        None
    }
}

impl FrameAllocator for BitmapAllocator {
    fn alloc_frame(&mut self) -> Option<PhysAddr> {
        // ONE mechanism, deliberately. The floor is applied HERE, where a general frame is
        // chosen — not also by starting the cursor high and not also by refusing to lower it
        // on an arena free. Those three each enforce the partition independently, so with all
        // three present no single one can be shown to matter: removing any of them left the
        // whole suite green. The same masking hid whether this kernel scrubbed memory at all
        // until the scrub sites were collapsed to one.
        //
        // The cursor is only a rescan hint, so letting an arena free pull it low costs
        // nothing: `max` clamps where the scan actually starts.
        let frame = self.first_free(self.cursor.max(self.general_floor))?;
        // PROOF(later): the returned frame was FREE (bit clear), lies within a Usable,
        // non-reserved region, and is now marked USED — so it is handed out to exactly
        // one caller and cannot be double-allocated until it is freed.
        self.set_used(frame);
        self.free -= 1;
        self.cursor = frame + 1;
        Some(Self::frame_addr(frame))
    }

    fn free_frame(&mut self, frame: PhysAddr) {
        // Defensive (TCB): reject frees below the reserve floor (never a legit frame),
        // out-of-range frames, and double-frees, so `free` and the bitmap stay consistent.
        // A mid-map Reserved hole cannot be distinguished from an allocated frame by the
        // bitmap alone; per `abi::FrameAllocator`, callers must only free frames they
        // actually allocated.
        if frame.as_u64() < self.reserve_below {
            return;
        }
        let f = (frame.as_u64() >> PAGE_SHIFT) as usize;
        if f >= self.total || !self.is_used(f) {
            return;
        }
        self.set_free(f);
        self.free += 1;
        if f < self.cursor {
            self.cursor = f;
        }
    }
}

/// Highest frame number that is fully contained in some `Usable` region, or `None` if
/// no region contributes a whole frame.
fn highest_usable_frame(regions: &[MemoryRegion]) -> Option<u64> {
    let mut hi: Option<u64> = None;
    for region in regions {
        if region.kind != MemoryKind::Usable {
            continue;
        }
        let end = region.end();
        if end < PAGE_SIZE {
            continue;
        }
        let f_lo = (region.start + PAGE_SIZE - 1) >> PAGE_SHIFT;
        let f_hi = (end >> PAGE_SHIFT) - 1;
        if f_lo > f_hi {
            continue;
        }
        hi = Some(hi.map_or(f_hi, |h| h.max(f_hi)));
    }
    hi
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    /// Leak a zeroed bitmap of `words` u64s as `&'static mut [u64]` for tests.
    fn leak_bitmap(words: usize) -> &'static mut [u64] {
        Box::leak(vec![0u64; words].into_boxed_slice())
    }

    /// Standard synthetic map: Usable [1MiB,16MiB), Reserved hole [16MiB,17MiB),
    /// Usable [17MiB,64MiB). Highest usable frame = 16383 -> 16384 frames -> 256 words.
    fn synthetic_regions() -> [MemoryRegion; 3] {
        [
            MemoryRegion {
                start: 1 * MIB,
                len: 15 * MIB,
                kind: MemoryKind::Usable,
            },
            MemoryRegion {
                start: 16 * MIB,
                len: 1 * MIB,
                kind: MemoryKind::Reserved,
            },
            MemoryRegion {
                start: 17 * MIB,
                len: 47 * MIB,
                kind: MemoryKind::Usable,
            },
        ]
    }

    // Params reused across tests.
    const RESERVE_BELOW: u64 = 2 * MIB; // frames 0..512 forced USED
    const DMA_TOP: u64 = 16 * MIB; // DMA pool = frames 0..4096

    /// Is `addr` the base of a frame fully inside a Usable region of the synthetic map,
    /// above the reserve floor, and outside the reserved hole?
    fn is_allocatable_addr(addr: u64) -> bool {
        if addr < RESERVE_BELOW {
            return false;
        }
        let in_r1 = addr >= 1 * MIB && addr + PAGE_SIZE <= 16 * MIB;
        let in_r2 = addr >= 17 * MIB && addr + PAGE_SIZE <= 64 * MIB;
        in_r1 || in_r2
    }

    #[test]
    fn words_needed_matches_map() {
        assert_eq!(
            BitmapAllocator::bitmap_words_needed(&synthetic_regions()),
            256
        );
        assert_eq!(BitmapAllocator::bitmap_words_needed(&[]), 0);
    }

    #[test]
    fn construction_counts_are_correct() {
        let regions = synthetic_regions();
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let a = BitmapAllocator::new(&regions, leak_bitmap(words), RESERVE_BELOW, DMA_TOP);

        assert_eq!(a.total_frames(), 16384);
        // r1 free: frames [512,4096) = 3584 ; r2 free: frames [4352,16384) = 12032.
        assert_eq!(a.free_count(), 3584 + 12032);
    }

    #[test]
    fn first_alloc_is_first_free_frame_and_aligned() {
        let regions = synthetic_regions();
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), RESERVE_BELOW, DMA_TOP);

        // General allocation starts at the partition, not at the reserve floor: frames below
        // `dma_top` belong to the DMA arena. The lowest free general frame is 16 MiB + one
        // frame, since 16 MiB itself falls in the synthetic map's reserved hole.
        let first = a.alloc_frame().expect("has free frames");
        assert!(
            first.as_u64() >= DMA_TOP,
            "general alloc dipped into the arena"
        );
        assert_eq!(first, PhysAddr(17 * MIB));
        assert!(first.is_page_aligned());
        // The arena still hands out the low frames it owns.
        let dma = a.alloc_dma_frame().expect("arena has free frames");
        assert_eq!(dma, PhysAddr(2 * MIB));
    }

    #[test]
    fn all_allocations_are_valid_and_aligned() {
        let regions = synthetic_regions();
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), RESERVE_BELOW, DMA_TOP);

        while let Some(frame) = a.alloc_frame() {
            let addr = frame.as_u64();
            assert!(frame.is_page_aligned(), "unaligned: {:#x}", addr);
            assert!(addr >= RESERVE_BELOW, "below reserve: {:#x}", addr);
            assert!(
                is_allocatable_addr(addr),
                "outside usable / in reserved hole: {:#x}",
                addr
            );
        }
    }

    #[test]
    fn alloc_until_exhausted_then_none() {
        let regions = synthetic_regions();
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), RESERVE_BELOW, DMA_TOP);

        // Draining BOTH pools must account for every free frame: the partition splits where a
        // frame comes from, never how many there are.
        let expected = a.free_count();
        let mut got = 0;
        while a.alloc_dma_frame().is_some() {
            got += 1;
        }
        while a.alloc_frame().is_some() {
            got += 1;
        }
        assert_eq!(got, expected);
        assert_eq!(a.free_count(), 0);
        assert!(a.alloc_frame().is_none());
    }

    #[test]
    fn no_double_allocation() {
        let regions = synthetic_regions();
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), RESERVE_BELOW, DMA_TOP);

        // Drain and confirm every handed-out frame is distinct.
        let mut seen = std::collections::BTreeSet::new();
        while let Some(frame) = a.alloc_dma_frame() {
            assert!(
                seen.insert(frame.as_u64()),
                "double-allocated {:#x}",
                frame.as_u64()
            );
        }
        while let Some(frame) = a.alloc_frame() {
            assert!(
                seen.insert(frame.as_u64()),
                "double-allocated {:#x}",
                frame.as_u64()
            );
        }
        // Same total as before the partition: 3584 arena frames plus 12032 general ones.
        assert_eq!(seen.len(), 3584 + 12032);
    }

    #[test]
    fn free_then_realloc_returns_freed_frame() {
        let regions = synthetic_regions();
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), RESERVE_BELOW, DMA_TOP);

        // Allocate several, remember one from the middle, free it.
        let _a0 = a.alloc_frame().unwrap();
        let target = a.alloc_frame().unwrap();
        let _a2 = a.alloc_frame().unwrap();
        let after_three = a.free_count();

        a.free_frame(target);
        assert_eq!(a.free_count(), after_three + 1);

        // The freed frame is the lowest free frame now, so it comes back next.
        let re = a.alloc_frame().unwrap();
        assert_eq!(re, target);
        assert_eq!(a.free_count(), after_three);
    }

    #[test]
    fn free_is_idempotent_and_range_checked() {
        let regions = synthetic_regions();
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), RESERVE_BELOW, DMA_TOP);

        let f = a.alloc_frame().unwrap();
        let base = a.free_count();

        a.free_frame(f);
        assert_eq!(a.free_count(), base + 1);
        // Double free must not inflate the count.
        a.free_frame(f);
        assert_eq!(a.free_count(), base + 1);
        // Out-of-range free is ignored.
        a.free_frame(PhysAddr(1 << 40));
        assert_eq!(a.free_count(), base + 1);
        // Freeing a reserved (never-allocatable) low frame is ignored (already USED).
        a.free_frame(PhysAddr(0));
        assert_eq!(a.free_count(), base + 1);
    }

    #[test]
    fn dma_frames_stay_below_dma_top() {
        let regions = synthetic_regions();
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), RESERVE_BELOW, DMA_TOP);

        // First DMA frame is the lowest free frame below dma_top: frame 512 -> 2MiB.
        let first = a.alloc_dma_frame().expect("dma pool non-empty");
        assert_eq!(first, PhysAddr(2 * MIB));

        let before = a.free_count();
        let mut count = 1; // already took one
        while let Some(frame) = a.alloc_dma_frame() {
            let addr = frame.as_u64();
            assert!(addr < DMA_TOP, "dma frame {:#x} not below dma_top", addr);
            assert!(frame.is_page_aligned());
            assert!(is_allocatable_addr(addr));
            count += 1;
        }
        // DMA pool = free frames below 4096 = r1 free = frames [512,4096) = 3584.
        assert_eq!(count, 3584);
        // Every DMA alloc consumed a free frame.
        assert_eq!(a.free_count(), before - (3584 - 1));
    }

    #[test]
    fn dma_and_general_are_disjoint() {
        // The partition, in both directions. A frame that has been device-reachable must
        // never become a page table, and a page table must never become device-reachable.
        let regions = synthetic_regions();
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), RESERVE_BELOW, DMA_TOP);

        let mut seen = std::collections::BTreeSet::new();
        let mut dma = Vec::new();
        while let Some(f) = a.alloc_dma_frame() {
            assert!(
                f.as_u64() >= RESERVE_BELOW,
                "arena frame below the reserve floor"
            );
            assert!(f.as_u64() < DMA_TOP, "arena frame above dma_top");
            assert!(
                seen.insert(f.as_u64()),
                "frame {:#x} handed out twice",
                f.as_u64()
            );
            dma.push(f);
        }
        let mut general = Vec::new();
        while let Some(f) = a.alloc_frame() {
            assert!(
                f.as_u64() >= DMA_TOP,
                "general allocation dipped into the arena"
            );
            assert!(
                seen.insert(f.as_u64()),
                "frame {:#x} handed out twice",
                f.as_u64()
            );
            general.push(f);
        }
        assert!(
            !dma.is_empty() && !general.is_empty(),
            "both pools must be non-empty"
        );
        assert_eq!(a.free_count(), 0);
        for f in dma.into_iter().chain(general) {
            a.free_frame(f);
        }
    }

    #[test]
    fn general_never_returns_an_arena_frame_after_arena_churn() {
        // The case the partition exists for, and the one a cursor-only implementation fails:
        // free every arena frame, then require general allocation to stay above dma_top. A
        // freed arena frame must not drag the general cursor down into the arena.
        let regions = synthetic_regions();
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), RESERVE_BELOW, DMA_TOP);

        let mut taken = Vec::new();
        for _ in 0..8 {
            if let Some(f) = a.alloc_dma_frame() {
                taken.push(f);
            }
        }
        assert!(!taken.is_empty());
        for f in taken {
            a.free_frame(f);
        }
        for _ in 0..16 {
            let f = a.alloc_frame().expect("general pool exhausted");
            assert!(
                f.as_u64() >= DMA_TOP,
                "general allocation returned arena frame {:#x} after arena churn",
                f.as_u64()
            );
        }
    }

    #[test]
    fn reserve_below_zero_frees_low_usable_frames() {
        // With reserve_below = 0, the whole of r1 down to 1MiB is allocatable.
        let regions = synthetic_regions();
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), 0, DMA_TOP);

        // r1 now free frames [256,4096) = 3840; r2 = 12032.
        assert_eq!(a.free_count(), 3840 + 12032);
        // Those newly-freed low frames are ARENA frames — below `dma_top` — so the arena is
        // where the effect of a zero reserve floor shows up. General allocation is unaffected
        // by it, which is the partition working.
        let first = a.alloc_dma_frame().unwrap();
        assert_eq!(first, PhysAddr(1 * MIB)); // frame 256
        assert!(a.alloc_frame().unwrap().as_u64() >= DMA_TOP);
    }

    #[test]
    fn empty_map_is_empty_allocator() {
        let mut a = BitmapAllocator::new(&[], leak_bitmap(0), RESERVE_BELOW, DMA_TOP);
        assert_eq!(a.total_frames(), 0);
        assert_eq!(a.free_count(), 0);
        assert!(a.alloc_frame().is_none());
        assert!(a.alloc_dma_frame().is_none());
    }

    #[test]
    fn unaligned_region_bounds_only_yield_whole_frames() {
        // Region spanning [1MiB+2KiB, 1MiB+10KiB): only the single whole frame at
        // [1MiB+4KiB, 1MiB+8KiB) is usable (frame 257).
        let regions = [MemoryRegion {
            start: 1 * MIB + 2 * 1024,
            len: 8 * 1024,
            kind: MemoryKind::Usable,
        }];
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), 0, DMA_TOP);
        assert_eq!(a.free_count(), 1);
        // The one whole frame sits below `dma_top`, so it belongs to the arena; general
        // allocation must not see it at all.
        assert!(a.alloc_frame().is_none());
        let f = a.alloc_dma_frame().unwrap();
        assert_eq!(f, PhysAddr(1 * MIB + 4 * 1024));
        assert!(a.alloc_dma_frame().is_none());
    }

    /// The partition holds for EVERY `dma_top`, not just 16 MiB.
    ///
    /// The rest of this file passes `DMA_TOP = 16 MiB` to every `BitmapAllocator::new`,
    /// which pins two axes at their most forgiving values: `dma_top` is page-aligned (so
    /// the round-up in `general_floor` is a no-op) and `general_floor` lands on a bitmap
    /// WORD boundary (so `first_free`'s `start_bit` mask is a no-op). Both are load-bearing
    /// off those values. Mutation-checked: this test is the ONLY thing in the suite that
    /// fails when line 76 drops `+ PAGE_SIZE - 1`, or when line 215 drops
    /// `| ((1u64 << start_bit) - 1)`. Either mutant hands a device-reachable frame to the
    /// general pool while the other 14 tests stay green.
    #[test]
    fn partition_holds_for_every_dma_top() {
        let regions = synthetic_regions();
        let mut top = 2 * MIB;
        let mut n = 0;
        while top <= 15 * MIB {
            // deltas straddle a page boundary so `dma_top` is sometimes unaligned.
            for delta in [0u64, 1, 2048, PAGE_SIZE, PAGE_SIZE + 8] {
                let dma_top = top + delta;
                let words = BitmapAllocator::bitmap_words_needed(&regions);
                let mut a =
                    BitmapAllocator::new(&regions, leak_bitmap(words), RESERVE_BELOW, dma_top);
                for _ in 0..8 {
                    if let Some(f) = a.alloc_frame() {
                        assert!(
                            f.as_u64() >= dma_top,
                            "dma_top={:#x}: general allocation {:#x} is device-reachable",
                            dma_top,
                            f.as_u64()
                        );
                        n += 1;
                    }
                }
                for _ in 0..8 {
                    if let Some(f) = a.alloc_dma_frame() {
                        assert!(
                            f.as_u64() < dma_top,
                            "dma_top={:#x}: arena frame {:#x} above dma_top",
                            dma_top,
                            f.as_u64()
                        );
                        n += 1;
                    }
                }
            }
            top += 20 * PAGE_SIZE;
        }
        assert!(n > 1000, "universe too small: {}", n);
    }

    /// A `Reserved` span INSIDE a `Usable` one must never be allocatable.
    ///
    /// Measured before the fix: this map handed out all 256 reserved frames. The firmware map
    /// is not ours on x86 — it comes from the PVH `hvm_start_info` the hypervisor supplies —
    /// and nothing validated that its regions are disjoint.
    #[test]
    fn a_reserved_span_inside_a_usable_one_is_never_handed_out() {
        let regions = [
            MemoryRegion {
                start: 0,
                len: 16 * MIB,
                kind: MemoryKind::Usable,
            },
            MemoryRegion {
                start: 8 * MIB,
                len: MIB,
                kind: MemoryKind::Reserved,
            },
        ];
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), 0, 0);
        let mut handed = 0u64;
        let mut reserved_handed = 0u64;
        while let Some(f) = a.alloc_frame() {
            let p = f.as_u64();
            if (8 * MIB..9 * MIB).contains(&p) {
                reserved_handed += 1;
            }
            handed += 1;
        }
        assert_eq!(reserved_handed, 0, "handed out reserved frames");
        assert!(handed > 0, "the whole map became unusable");
    }

    /// Every frame the allocator hands out, for EVERY map shape in a small universe.
    ///
    /// The rest of this suite builds from one hardcoded 3-region map, which fixes the shape of
    /// the single input the nucleus does not control. This varies it: unaligned starts,
    /// zero-length regions, kinds in either order, and Usable/non-Usable overlap in both
    /// directions. Two rounding rules are asserted at once and they point opposite ways — a
    /// frame only partly Usable must not be freed, a frame only partly Reserved must not be
    /// handed out.
    #[test]
    fn every_handed_frame_is_backed_by_usable_memory_and_untouched_by_reserved() {
        // UNALIGNED values are load-bearing. With every start and length page-aligned, no
        // region ever partially covers a frame, and the rounding DIRECTION of the non-Usable
        // pass stops mattering: a mutant rounding it inward (like the Usable pass) survived a
        // page-aligned-only universe. `6000` straddles a frame boundary and a length of `1`
        // covers a single byte, so a region can now touch a frame without filling it.
        const STARTS: [u64; 4] = [0, 4096, 6000, 8192];
        const LENS: [u64; 4] = [0, 1, 4096, 12288];
        const KINDS: [MemoryKind; 3] = [
            MemoryKind::Usable,
            MemoryKind::Reserved,
            MemoryKind::AcpiNvs,
        ];
        let mut configs = 0u64;
        let mut frames_seen = 0u64;
        for s0 in STARTS {
            for l0 in LENS {
                for k0 in KINDS {
                    for s1 in STARTS {
                        for l1 in LENS {
                            for k1 in KINDS {
                                let regions = [
                                    MemoryRegion {
                                        start: s0,
                                        len: l0,
                                        kind: k0,
                                    },
                                    MemoryRegion {
                                        start: s1,
                                        len: l1,
                                        kind: k1,
                                    },
                                ];
                                let words = BitmapAllocator::bitmap_words_needed(&regions);
                                let mut a =
                                    BitmapAllocator::new(&regions, leak_bitmap(words), 0, 0);
                                let mut seen: Vec<u64> = Vec::new();
                                while let Some(f) = a.alloc_frame() {
                                    let lo = f.as_u64();
                                    let hi = lo + PAGE_SIZE;
                                    assert!(
                                        lo % PAGE_SIZE == 0,
                                        "unaligned frame {lo:#x} from {regions:?}"
                                    );
                                    assert!(
                                        !seen.contains(&lo),
                                        "frame {lo:#x} handed out twice from {regions:?}"
                                    );
                                    seen.push(lo);
                                    // Fully backed by some Usable region.
                                    let backed = regions.iter().any(|r| {
                                        r.kind == MemoryKind::Usable
                                            && r.len > 0
                                            && r.start <= lo
                                            && hi <= r.end()
                                    });
                                    assert!(
                                        backed,
                                        "frame {lo:#x} is not fully inside any Usable region: \
                                         {regions:?}"
                                    );
                                    // Touched by NO non-Usable region.
                                    let poisoned = regions.iter().any(|r| {
                                        r.kind != MemoryKind::Usable
                                            && r.len > 0
                                            && lo < r.end()
                                            && r.start < hi
                                    });
                                    assert!(
                                        !poisoned,
                                        "frame {lo:#x} overlaps a non-Usable region: {regions:?}"
                                    );
                                    frames_seen += 1;
                                }
                                configs += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(configs > 1000, "universe too small: {configs}");
        assert!(
            frames_seen > 0,
            "no configuration produced a single allocatable frame — the search proves nothing"
        );
    }

    /// Arbitrary alloc/free INTERLEAVINGS, checked against a model.
    ///
    /// Every other sequence in this file is drain-shaped — allocate a run, maybe free one,
    /// allocate again. The longest was 3 allocs, 1 free, 1 alloc. That shape cannot reach the
    /// state this checks: a frame handed out while it is ALREADY HELD, which is two address
    /// spaces sharing a page table.
    ///
    /// The sequence is a deterministic LCG rather than a random one, so a failure is
    /// reproducible from the seed printed in the assertion. Frees are drawn only from frames
    /// the model actually holds: `free_frame`'s contract (see its comment) is that callers
    /// free only what they allocated, because a mid-map Reserved hole is indistinguishable
    /// from an allocated frame in the bitmap. Freeing one anyway is a caller bug, not an
    /// allocator bug, so it is not asserted here.
    ///
    /// RESULT, and it is the unflattering one again: this CLOSED NO GAP. Four mutants were
    /// run — the double-free guard removed, the cursor rewind removed, the free counter not
    /// decremented on allocation, and the `general_floor` clamp dropped from `alloc_frame` —
    /// and every one is already caught by tests that existed. The last is the interesting
    /// case, because it is precisely an interleaving bug (an arena free pulls the cursor below
    /// the floor, then a general allocation scans from there) and
    /// `general_never_returns_an_arena_frame_after_arena_churn` already constructs exactly
    /// that sequence by hand.
    ///
    /// So "alloc/free sequences are drain-shaped" overstated the gap: no loop enumerates
    /// interleavings, but the specific interleaving that matters was already written down.
    /// That is the second axis named as held-constant which turned out to be covered — see
    /// deleg's insertion-order search. An axis is only unexplored if the search CANNOT REACH
    /// the case, not merely if nothing iterates over it.
    ///
    /// Kept at ~0.01s because it states the allocator's core safety property directly — a
    /// live frame is never handed out twice — over sequences no hand-written case fixes in
    /// advance, which is what a future free-list or cursor change would need. It is a
    /// statement of the invariant, not evidence of new coverage.
    #[test]
    fn arbitrary_alloc_free_interleavings_never_hand_out_a_live_frame() {
        for seed in [1u64, 12345, 0xDEAD_BEEF, 7, 99_991] {
            let regions = synthetic_regions();
            let words = BitmapAllocator::bitmap_words_needed(&regions);
            let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), RESERVE_BELOW, DMA_TOP);

            let at_start = a.free_count();
            let mut live: Vec<u64> = Vec::new();
            let mut expect_free = at_start;
            let mut rng = seed;
            let mut next = || {
                rng = rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (rng >> 33) as usize
            };

            for step in 0..4000 {
                match next() % 4 {
                    // general allocation
                    0 | 1 => {
                        if let Some(f) = a.alloc_frame() {
                            let p = f.as_u64();
                            assert!(
                                !live.contains(&p),
                                "seed {seed} step {step}: frame {p:#x} handed out while live"
                            );
                            assert!(
                                p >= DMA_TOP,
                                "seed {seed} step {step}: general frame {p:#x} is below dma_top"
                            );
                            live.push(p);
                            expect_free -= 1;
                        }
                    }
                    // DMA arena allocation
                    2 => {
                        if let Some(f) = a.alloc_dma_frame() {
                            let p = f.as_u64();
                            assert!(
                                !live.contains(&p),
                                "seed {seed} step {step}: dma frame {p:#x} handed out while live"
                            );
                            assert!(
                                p < DMA_TOP && p >= RESERVE_BELOW,
                                "seed {seed} step {step}: dma frame {p:#x} outside the arena"
                            );
                            live.push(p);
                            expect_free -= 1;
                        }
                    }
                    // free one we hold, then double-free it: the second must be a no-op
                    _ => {
                        if !live.is_empty() {
                            let i = next() % live.len();
                            let p = live.swap_remove(i);
                            a.free_frame(PhysAddr(p));
                            expect_free += 1;
                            assert_eq!(
                                a.free_count(),
                                expect_free,
                                "seed {seed} step {step}: free_count wrong after freeing {p:#x}"
                            );
                            a.free_frame(PhysAddr(p));
                        }
                    }
                }
                assert_eq!(
                    a.free_count(),
                    expect_free,
                    "seed {seed} step {step}: free_count diverged from the model"
                );
            }

            // Conservation over the WHOLE arbitrary sequence: give everything back, and the
            // allocator must be able to hand out exactly what it started with. This is the
            // property the drain-shaped tests cannot state, because they never interleave.
            for p in live.drain(..) {
                a.free_frame(PhysAddr(p));
            }
            assert_eq!(
                a.free_count(),
                at_start,
                "seed {seed}: frames were lost across an arbitrary alloc/free sequence"
            );
            let mut handed = 0usize;
            while a.alloc_frame().is_some() || a.alloc_dma_frame().is_some() {
                handed += 1;
            }
            assert_eq!(
                handed, at_start,
                "seed {seed}: drained {handed} frames but started with {at_start}"
            );
        }
    }
}
