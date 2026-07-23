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

        let mut alloc = BitmapAllocator {
            bitmap,
            total,
            free: 0,
            cursor: 0,
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
        let mut f = 0usize;
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
        let frame = self.first_free(self.cursor)?;
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

        // Frames 0..512 USED (reserve), so first free frame is 512 -> 2MiB.
        let first = a.alloc_frame().expect("has free frames");
        assert_eq!(first, PhysAddr(2 * MIB));
        assert!(first.is_page_aligned());
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

        let expected = a.free_count();
        let mut got = 0;
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
        while let Some(frame) = a.alloc_frame() {
            assert!(
                seen.insert(frame.as_u64()),
                "double-allocated {:#x}",
                frame.as_u64()
            );
        }
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
    fn dma_and_general_share_one_pool() {
        let regions = synthetic_regions();
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), RESERVE_BELOW, DMA_TOP);

        // A DMA alloc must remove the frame from the general pool too (no double hand-out).
        let dma = a.alloc_dma_frame().unwrap();
        let mut seen = std::collections::BTreeSet::new();
        seen.insert(dma.as_u64());
        while let Some(frame) = a.alloc_frame() {
            assert!(
                seen.insert(frame.as_u64()),
                "frame {:#x} handed out twice",
                frame.as_u64()
            );
        }
        assert_eq!(a.free_count(), 0);
    }

    #[test]
    fn reserve_below_zero_frees_low_usable_frames() {
        // With reserve_below = 0, the whole of r1 down to 1MiB is allocatable.
        let regions = synthetic_regions();
        let words = BitmapAllocator::bitmap_words_needed(&regions);
        let mut a = BitmapAllocator::new(&regions, leak_bitmap(words), 0, DMA_TOP);

        // r1 now free frames [256,4096) = 3840; r2 = 12032.
        assert_eq!(a.free_count(), 3840 + 12032);
        let first = a.alloc_frame().unwrap();
        assert_eq!(first, PhysAddr(1 * MIB)); // frame 256
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
        let f = a.alloc_frame().unwrap();
        assert_eq!(f, PhysAddr(1 * MIB + 4 * 1024));
        assert!(a.alloc_frame().is_none());
    }
}
