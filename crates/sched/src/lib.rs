#![cfg_attr(not(test), no_std)]
//! sched — a cooperative round-robin scheduler with a per-arch context switch.
//!
//! [`Scheduler`] is a portable, allocation-free round-robin run queue of [`abi::ThreadId`]
//! — pure index logic, fully host-testable. The machine [`Context`] + [`switch`] (and
//! [`Context::prepare`]) are arch-specific (x86-64 / riscv64) and exercised under QEMU by
//! the integrator, never in host tests.
use abi::ThreadId;

#[cfg(target_arch = "x86_64")]
mod context_x86;
#[cfg(target_arch = "x86_64")]
pub use context_x86::{switch, Context};

#[cfg(target_arch = "riscv64")]
mod context_riscv;
#[cfg(target_arch = "riscv64")]
pub use context_riscv::{switch, Context};

// ============================================================ run queue

/// A fixed-capacity, allocation-free round-robin run queue of ready [`ThreadId`]s.
///
/// The ready threads occupy `slots[0..len]` in insertion order; `cur` (valid only while
/// `len > 0`) indexes the currently-scheduled one. [`Scheduler::next`] advances `cur`
/// modulo `len`, so every ready thread is visited once per lap — no starvation.
///
/// Capacity `N` is fixed at compile time; there is no heap. `add` past capacity fails
/// rather than allocating.
pub struct Scheduler<const N: usize> {
    slots: [Option<ThreadId>; N],
    /// Number of live entries, packed into `slots[0..len]`.
    len: usize,
    /// Index of the current thread within `slots[0..len]`. Meaningful iff `len > 0`.
    cur: usize,
}

impl<const N: usize> Default for Scheduler<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Scheduler<N> {
    /// An empty run queue.
    pub const fn new() -> Self {
        Scheduler {
            slots: [None; N],
            len: 0,
            cur: 0,
        }
    }

    /// Fixed capacity of the run queue.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Number of ready threads currently queued.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// True when no thread is ready to run.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// True if `tid` is already in the run queue.
    pub fn contains(&self, tid: ThreadId) -> bool {
        self.slots[..self.len].iter().any(|&s| s == Some(tid))
    }

    /// Enqueue `tid` at the tail of the run queue.
    ///
    /// Returns `false` (and changes nothing) if the queue is full or already contains
    /// `tid` — a thread must never appear twice, or round-robin fairness breaks.
    pub fn add(&mut self, tid: ThreadId) -> bool {
        if self.len == N || self.contains(tid) {
            return false;
        }
        self.slots[self.len] = Some(tid);
        self.len += 1;
        // First thread added becomes current (cur is already 0).
        true
    }

    /// The currently-scheduled thread, or `None` if the queue is empty.
    pub fn current(&self) -> Option<ThreadId> {
        if self.len == 0 {
            None
        } else {
            self.slots[self.cur]
        }
    }

    /// Advance round-robin to the next ready thread and return it (the new current), or
    /// `None` if the queue is empty. With a single ready thread it returns that thread.
    ///
    /// PROOF(later): each ready thread is scheduled in round-robin order — over any `len`
    /// consecutive `next()` calls every queued `ThreadId` is returned exactly once (no
    /// starvation, no double-visit).
    pub fn next(&mut self) -> Option<ThreadId> {
        if self.len == 0 {
            return None;
        }
        self.cur = (self.cur + 1) % self.len;
        self.slots[self.cur]
    }

    /// Remove `tid` from the run queue, compacting the remaining threads and keeping the
    /// cursor pointing at a valid current thread. Returns `false` if `tid` was not queued.
    ///
    /// If the current thread is removed, `current()` advances to the thread that followed
    /// it (wrapping to the head when the tail was removed).
    pub fn remove(&mut self, tid: ThreadId) -> bool {
        let Some(idx) = self.slots[..self.len].iter().position(|&s| s == Some(tid)) else {
            return false;
        };

        // Compact: shift the tail down over the hole at `idx`.
        for i in idx..self.len - 1 {
            self.slots[i] = self.slots[i + 1];
        }
        self.len -= 1;
        self.slots[self.len] = None;

        // Fix up the cursor so it still names a live slot in [0, len).
        if self.len == 0 {
            self.cur = 0;
        } else if idx < self.cur {
            // Everything at/after cur shifted down one; follow the current thread.
            self.cur -= 1;
        } else if self.cur >= self.len {
            // We removed the tail while it was current — wrap to the head.
            self.cur = 0;
        }
        // idx == cur (non-tail): cur now names the thread that followed the removed one.
        // idx > cur: nothing before cur moved.

        true
    }
}

// ============================================================ tests (host / std)

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(n: usize) -> ThreadId {
        ThreadId(n)
    }

    #[test]
    fn empty_scheduler() {
        let mut s: Scheduler<4> = Scheduler::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.capacity(), 4);
        assert_eq!(s.current(), None);
        assert_eq!(s.next(), None);
        assert!(!s.remove(tid(0)));
    }

    #[test]
    fn add_and_current() {
        let mut s: Scheduler<4> = Scheduler::new();
        assert!(s.add(tid(10)));
        assert_eq!(s.len(), 1);
        assert_eq!(s.current(), Some(tid(10)));
        // A lone thread is scheduled to itself.
        assert_eq!(s.next(), Some(tid(10)));
        assert_eq!(s.current(), Some(tid(10)));
    }

    #[test]
    fn add_rejects_duplicate_and_overflow() {
        let mut s: Scheduler<2> = Scheduler::new();
        assert!(s.add(tid(1)));
        assert!(!s.add(tid(1))); // duplicate
        assert!(s.add(tid(2)));
        assert!(!s.add(tid(3))); // full
        assert_eq!(s.len(), 2);
        assert!(s.contains(tid(1)));
        assert!(s.contains(tid(2)));
        assert!(!s.contains(tid(3)));
    }

    #[test]
    fn round_robin_fairness() {
        let mut s: Scheduler<3> = Scheduler::new();
        s.add(tid(1));
        s.add(tid(2));
        s.add(tid(3));
        assert_eq!(s.current(), Some(tid(1)));

        // Two full laps: strict 1->2->3 order, every thread once per lap.
        let seen: [ThreadId; 6] = core::array::from_fn(|_| s.next().unwrap());
        assert_eq!(seen, [tid(2), tid(3), tid(1), tid(2), tid(3), tid(1)]);

        // Each of the three threads appears exactly twice across the two laps.
        for t in [tid(1), tid(2), tid(3)] {
            assert_eq!(seen.iter().filter(|&&x| x == t).count(), 2);
        }
    }

    #[test]
    fn remove_after_current() {
        // Removing a thread positioned after the cursor leaves current untouched.
        let mut s: Scheduler<4> = Scheduler::new();
        s.add(tid(1));
        s.add(tid(2));
        s.add(tid(3));
        // current = 1 (cur = 0)
        assert!(s.remove(tid(2)));
        assert_eq!(s.len(), 2);
        assert_eq!(s.current(), Some(tid(1)));
        assert_eq!(s.next(), Some(tid(3)));
        assert_eq!(s.next(), Some(tid(1)));
    }

    #[test]
    fn remove_before_current_follows_it() {
        let mut s: Scheduler<4> = Scheduler::new();
        s.add(tid(1));
        s.add(tid(2));
        s.add(tid(3));
        assert_eq!(s.next(), Some(tid(2))); // cur -> 1
        assert!(s.remove(tid(1))); // idx 0 < cur 1
                                   // current thread (2) is preserved.
        assert_eq!(s.current(), Some(tid(2)));
        assert_eq!(s.next(), Some(tid(3)));
        assert_eq!(s.next(), Some(tid(2)));
    }

    #[test]
    fn remove_current_non_tail_advances_to_next() {
        let mut s: Scheduler<4> = Scheduler::new();
        s.add(tid(1));
        s.add(tid(2));
        s.add(tid(3));
        assert_eq!(s.next(), Some(tid(2))); // current = 2, cur = 1 (not tail)
        assert!(s.remove(tid(2)));
        // current becomes the thread that followed 2, i.e. 3.
        assert_eq!(s.current(), Some(tid(3)));
        assert_eq!(s.next(), Some(tid(1)));
    }

    #[test]
    fn remove_current_tail_wraps_to_head() {
        let mut s: Scheduler<4> = Scheduler::new();
        s.add(tid(1));
        s.add(tid(2));
        s.add(tid(3));
        assert_eq!(s.next(), Some(tid(2)));
        assert_eq!(s.next(), Some(tid(3))); // current = tail 3, cur = 2
        assert!(s.remove(tid(3)));
        // Removed the tail-while-current: wrap to head.
        assert_eq!(s.current(), Some(tid(1)));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn remove_down_to_empty() {
        let mut s: Scheduler<4> = Scheduler::new();
        s.add(tid(7));
        assert!(s.remove(tid(7)));
        assert!(s.is_empty());
        assert_eq!(s.current(), None);
        assert_eq!(s.next(), None);
        // Re-add works and cursor is sane again.
        assert!(s.add(tid(8)));
        assert_eq!(s.current(), Some(tid(8)));
    }

    #[test]
    fn remove_missing_is_noop() {
        let mut s: Scheduler<4> = Scheduler::new();
        s.add(tid(1));
        s.add(tid(2));
        assert!(!s.remove(tid(99)));
        assert_eq!(s.len(), 2);
        assert_eq!(s.current(), Some(tid(1)));
    }

    // ---- Context::prepare — x86-specific; skipped on hosts where `Context` is not
    // compiled (only x86_64 / riscv64 kernel targets have a machine context). ----
    #[cfg(target_arch = "x86_64")]
    use abi::VirtAddr;

    #[cfg(target_arch = "x86_64")]
    extern "C" fn dummy_entry() -> ! {
        loop {}
    }

    #[cfg(target_arch = "x86_64")]
    #[repr(align(16))]
    struct AlignedStack([u8; 256]);

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn prepare_lays_out_return_address() {
        let mut stack = AlignedStack([0u8; 256]);
        let base = stack.0.as_mut_ptr() as u64;
        let stack_top = VirtAddr(base + 256); // 16-aligned exclusive top

        // SAFETY: `stack` is a live, exclusively-owned, 16-aligned 256-byte buffer; the
        // slot prepare writes to sits well within it.
        let ctx = unsafe { Context::prepare(stack_top, dummy_entry) };

        // rsp is 16-aligned and strictly inside the buffer.
        assert_eq!(ctx.rsp % 16, 0);
        assert!(ctx.rsp >= base && ctx.rsp < stack_top.as_u64());

        // After `ret` pops the slot, entry would observe rsp % 16 == 8 (ABI-correct).
        assert_eq!((ctx.rsp + 8) % 16, 8);

        // The slot holds the entry address that `switch`'s `ret` will jump to.
        // SAFETY: ctx.rsp points at the u64 prepare just wrote inside `stack`.
        let written = unsafe { core::ptr::read(ctx.rsp as *const u64) };
        assert_eq!(written, dummy_entry as *const () as u64);

        // All other saved registers start zeroed.
        assert_eq!(ctx.rbx, 0);
        assert_eq!(ctx.rbp, 0);
        assert_eq!(ctx.r12, 0);
        assert_eq!(ctx.r15, 0);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn prepare_handles_unaligned_top() {
        // Even if the caller hands us a non-16-aligned top, rsp comes out 16-aligned and
        // below it.
        let mut stack = AlignedStack([0u8; 256]);
        let base = stack.0.as_mut_ptr() as u64;
        let stack_top = VirtAddr(base + 250); // deliberately not 16-aligned

        let ctx = unsafe { Context::prepare(stack_top, dummy_entry) };
        assert_eq!(ctx.rsp % 16, 0);
        assert!(ctx.rsp < stack_top.as_u64());
        let written = unsafe { core::ptr::read(ctx.rsp as *const u64) };
        assert_eq!(written, dummy_entry as *const () as u64);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn context_default_is_zeroed() {
        let c = Context::new();
        assert_eq!(c, Context::default());
        assert_eq!(c.rsp, 0);
    }
}
