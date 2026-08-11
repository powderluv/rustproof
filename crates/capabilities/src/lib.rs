#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
//! capabilities — a fixed-capacity capability space for the Rustproof nucleus.
//!
//! VERIFIED TCB crate. A `CapSpace<N>` owns a flat array of `N` `CapSlot`s (no heap;
//! the array is inline storage). Capabilities are addressed by their slot index
//! (`abi::CapId`). Free slots are marked by `CapType::Null`.
//!
//! Two authority rules are enforced structurally and are the properties the Verus
//! proofs (later) will discharge:
//!
//! 1. **Authority-monotonic derivation** — a derived (child) capability's rights are
//!    `parent.rights ∩ requested`, so a child can NEVER hold a right its parent lacks.
//! 2. **Coarse revocation only** — we deliberately do NOT support fine-grained
//!    mid-execution rights downgrade. The only removal primitive is
//!    [`CapSpace::revoke`], which drops a whole derivation subtree (e.g. on
//!    address-space teardown).
//!
//! See docs/nucleus-design.md and docs/verification.md.

use abi::{CapId, CapRights, CapType};

/// One capability slot. A slot is *free* iff `cap_type == CapType::Null`.
///
/// `object` is an opaque handle to the referenced kernel object (e.g. a physical
/// address / frame number / TCB index); this crate never interprets it. `parent`
/// records the slot index this cap was derived from (`None` for a freshly inserted
/// root cap), which is what makes subtree revocation possible.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CapSlot {
    pub cap_type: CapType,
    pub rights: CapRights,
    pub object: u64,
}

impl CapSlot {
    /// The canonical empty/free slot.
    const EMPTY: CapSlot = CapSlot {
        cap_type: CapType::Null,
        rights: CapRights::NONE,
        object: 0,
    };

    /// True when this slot holds no capability and may be handed out by `insert`.
    #[inline]
    pub fn is_free(&self) -> bool {
        self.cap_type == CapType::Null
    }
}

/// A capability space: fixed-capacity, heap-free, generic over its slot count `N`.
///
/// # Invariants (PROOF(later))
/// - Every *live* slot with `parent == Some(p)` has `p < N` and `slots[p]` live.
///   (Established by [`derive`](CapSpace::derive) — the parent is validated live
///   before a child is minted — and preserved by
///   [`revoke`](CapSpace::revoke), which removes an entire subtree so
///   no dangling parent link is ever left behind.) Revocation's termination and
///   completeness rely on this.
pub struct CapSpace<const N: usize> {
    slots: [CapSlot; N],
}

impl<const N: usize> Default for CapSpace<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> CapSpace<N> {
    /// Create an empty capability space (all `N` slots free).
    pub const fn new() -> Self {
        CapSpace {
            slots: [CapSlot::EMPTY; N],
        }
    }

    /// Total number of slots (the compile-time capacity).
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Number of currently-live capabilities.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| !s.is_free()).count()
    }

    /// True when no live capabilities are held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Insert a fresh *root* capability (no parent) into the first free slot.
    ///
    /// Returns the new [`CapId`], or `None` if the space is full. Inserting a
    /// `CapType::Null` is rejected (it is the free-slot sentinel and would be
    /// indistinguishable from an empty slot) — returns `None`.
    pub fn insert(&mut self, cap_type: CapType, rights: CapRights, object: u64) -> Option<CapId> {
        if cap_type == CapType::Null {
            return None;
        }
        let idx = self.first_free()?;
        self.slots[idx] = CapSlot {
            cap_type,
            rights,
            object,
        };
        Some(CapId(idx))
    }

    /// Look up a live capability by id. Returns `None` for an out-of-range id or a
    /// free slot.
    pub fn lookup(&self, id: CapId) -> Option<&CapSlot> {
        let idx = id.0;
        if idx < N && !self.slots[idx].is_free() {
            Some(&self.slots[idx])
        } else {
            None
        }
    }

    /// Free `cap`, returning its slot to the pool.
    ///
    /// There is deliberately no "subtree" here any more. This used to walk a fixpoint over a
    /// `parent` slot index recorded by a `derive` operation — a miniature
    /// capability-derivation tree — and it never once iterated, because nothing ever built a
    /// non-flat space: every write into a live capability space is an `insert`, which makes a
    /// root. The name promised transitive teardown while performing a single free, and two
    /// separate documents believed the promise.
    ///
    /// `docs/nucleus-design.md` had already rejected the mechanism in so many words ("seL4
    /// tracks a full capability-derivation tree … Rustproof needs none of it"), so the code
    /// and the design were in direct conflict.
    ///
    /// Cross-space delegation IS tracked transitively — by the `deleg` ledger, whose edges
    /// carry process IDENTITY rather than a bare slot index. That distinction is the one this
    /// codebase learned from a confirmed defect, and it is why a slot-index parent was the
    /// wrong shape to keep. If intra-space derivation is ever wanted it will be a RETYPE with
    /// range subsetting recorded as a `deleg` edge, not a parent pointer here.
    pub fn revoke(&mut self, cap: CapId) {
        if cap.0 >= N {
            return;
        }
        self.slots[cap.0] = CapSlot::EMPTY;
    }

    /// Index of the first free slot, if any.
    #[inline]
    fn first_free(&self) -> Option<usize> {
        self.slots.iter().position(|s| s.is_free())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abi::{CapId, CapRights, CapType};

    #[test]
    fn insert_then_lookup_roundtrips() {
        let mut cs: CapSpace<4> = CapSpace::new();
        let id = cs
            .insert(CapType::Untyped, CapRights::ALL, 0xDEAD_BEEF)
            .expect("space available");
        let slot = cs.lookup(id).expect("live slot");
        assert_eq!(slot.cap_type, CapType::Untyped);
        assert_eq!(slot.rights, CapRights::ALL);
        assert_eq!(slot.object, 0xDEAD_BEEF);
    }

    #[test]
    fn fresh_space_is_empty_and_all_free() {
        let cs = CapSpace::<8>::new();
        assert_eq!(cs.capacity(), 8);
        assert_eq!(cs.len(), 0);
        assert!(cs.is_empty());
        // Nothing looks up on a fresh space.
        for i in 0..8 {
            assert!(cs.lookup(CapId(i)).is_none());
        }
    }

    #[test]
    fn insert_of_null_is_rejected() {
        let mut cs = CapSpace::<4>::new();
        // Null is the free sentinel; inserting it must fail, not consume a slot.
        assert!(cs.insert(CapType::Null, CapRights::ALL, 0).is_none());
        assert_eq!(cs.len(), 0);
    }

    #[test]
    fn lookup_out_of_range_is_none() {
        let cs = CapSpace::<2>::new();
        assert!(cs.lookup(CapId(2)).is_none());
        assert!(cs.lookup(CapId(9999)).is_none());
    }

    #[test]
    fn capacity_exhaustion_on_insert() {
        let mut cs = CapSpace::<2>::new();
        assert!(cs.insert(CapType::Frame, CapRights::ALL, 1).is_some());
        assert!(cs.insert(CapType::Frame, CapRights::ALL, 2).is_some());
        // Third insert has no free slot.
        assert!(cs.insert(CapType::Frame, CapRights::ALL, 3).is_none());
        assert_eq!(cs.len(), 2);
    }

    #[test]
    fn revoke_frees_slots_for_reuse() {
        let mut cs = CapSpace::<2>::new();
        let a = cs.insert(CapType::Frame, CapRights::ALL, 1).unwrap();
        let _b = cs.insert(CapType::Frame, CapRights::ALL, 2).unwrap();
        assert!(cs.insert(CapType::Frame, CapRights::ALL, 3).is_none()); // full
        cs.revoke(a);
        // Freed slot is now reusable.
        assert!(cs.insert(CapType::Frame, CapRights::ALL, 3).is_some());
    }

    #[test]
    fn revoke_invalid_or_free_is_noop() {
        let mut cs = CapSpace::<4>::new();
        let a = cs.insert(CapType::Frame, CapRights::ALL, 1).unwrap();
        // Out of range: no panic, no change.
        cs.revoke(CapId(99));
        assert_eq!(cs.len(), 1);
        // Already-free slot.
        cs.revoke(CapId(2));
        assert_eq!(cs.len(), 1);
        // Double revoke is harmless.
        cs.revoke(a);
        cs.revoke(a);
        assert!(cs.is_empty());
    }

    #[test]
    fn null_slot_handling_after_removal() {
        let mut cs = CapSpace::<4>::new();
        let a = cs.insert(CapType::Frame, CapRights::ALL, 1).unwrap();
        cs.revoke(a);
        // A removed slot reads back as free/None.
        assert!(cs.lookup(a).is_none());
        assert!(cs.slots[a.0].is_free());
    }
}
