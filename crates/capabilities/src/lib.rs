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
//!    [`CapSpace::revoke_subtree`], which drops a whole derivation subtree (e.g. on
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
    pub parent: Option<usize>,
}

impl CapSlot {
    /// The canonical empty/free slot.
    const EMPTY: CapSlot = CapSlot {
        cap_type: CapType::Null,
        rights: CapRights::NONE,
        object: 0,
        parent: None,
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
///   [`revoke_subtree`](CapSpace::revoke_subtree), which removes an entire subtree so
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
            parent: None,
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

    /// Derive a child capability from `parent`, restricting its rights.
    ///
    /// The child refers to the same object/type as the parent, records `parent`'s
    /// slot as its derivation parent, and is given rights
    /// `parent.rights ∩ new_rights`.
    ///
    /// Returns the child's [`CapId`], or `None` if `parent` is not a live cap or the
    /// space is full.
    ///
    /// PROOF(later): AUTHORITY-MONOTONIC — the returned child's `rights` satisfy
    /// `parent.rights.contains(child.rights)` in every reachable state; a derived cap
    /// can never gain a right the parent lacks because `intersect` only clears bits.
    /// This is the core capability-safety invariant of the nucleus.
    pub fn derive(&mut self, parent: CapId, new_rights: CapRights) -> Option<CapId> {
        let pidx = parent.0;
        if pidx >= N || self.slots[pidx].is_free() {
            return None;
        }
        // Snapshot the parent (Copy) before we mutate the array.
        let parent_slot = self.slots[pidx];
        // Monotonic restriction: intersection can only *drop* rights, never add.
        let child_rights = parent_slot.rights.intersect(new_rights);

        let idx = self.first_free()?;
        self.slots[idx] = CapSlot {
            cap_type: parent_slot.cap_type,
            rights: child_rights,
            object: parent_slot.object,
            parent: Some(pidx),
        };
        Some(CapId(idx))
    }

    /// Revoke `root` and its entire derivation subtree (coarse revocation).
    ///
    /// Removes the cap at `root` and, transitively, every cap derived from it (direct
    /// and indirect). A no-op if `root` is out of range or already free. This is the
    /// ONLY removal primitive — per the design we prohibit fine-grained
    /// mid-execution rights revocation.
    pub fn revoke_subtree(&mut self, root: CapId) {
        let ridx = root.0;
        if ridx >= N || self.slots[ridx].is_free() {
            return;
        }
        // Free the subtree root first.
        self.slots[ridx] = CapSlot::EMPTY;

        // Fixpoint sweep: repeatedly free any live slot whose parent slot has become
        // free. By the space invariant (a live slot's parent is live *before* this
        // call), a live slot acquires a freed parent only when that parent was itself
        // removed as part of *this* subtree — so exactly the descendants are dropped.
        //
        // PROOF(later): terminates — every non-final sweep frees ≥1 slot and there
        // are ≤ N slots — and is complete: at the fixpoint no live slot has a freed
        // parent, i.e. the whole subtree is gone.
        loop {
            let mut progress = false;
            for i in 0..N {
                if self.slots[i].is_free() {
                    continue;
                }
                if let Some(p) = self.slots[i].parent {
                    if p < N && self.slots[p].is_free() {
                        self.slots[i] = CapSlot::EMPTY;
                        progress = true;
                    }
                }
            }
            if !progress {
                break;
            }
        }
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
    fn insert_then_lookup_roundtrips() {
        let mut cs = CapSpace::<4>::new();
        let id = cs
            .insert(CapType::Frame, CapRights::READ, 0xdead_beef)
            .expect("insert into empty space");
        let slot = cs.lookup(id).expect("lookup live cap");
        assert_eq!(slot.cap_type, CapType::Frame);
        assert_eq!(slot.rights, CapRights::READ);
        assert_eq!(slot.object, 0xdead_beef);
        assert_eq!(slot.parent, None); // inserted caps are roots
        assert_eq!(cs.len(), 1);
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
    fn derive_shrinks_rights_never_grows() {
        let mut cs = CapSpace::<8>::new();
        // Parent holds READ|WRITE (not GRANT).
        let rw = CapRights(CapRights::READ.0 | CapRights::WRITE.0);
        let parent = cs.insert(CapType::Frame, rw, 0x1000).unwrap();

        // Requesting ALL cannot grow beyond the parent's RW.
        let child = cs.derive(parent, CapRights::ALL).unwrap();
        let cslot = *cs.lookup(child).unwrap();
        assert_eq!(cslot.rights, rw, "child clamped to parent rights");
        // Authority-monotonic: parent contains child.
        let pslot = *cs.lookup(parent).unwrap();
        assert!(pslot.rights.contains(cslot.rights));
        // And specifically the child did NOT gain GRANT.
        assert!(!cslot.rights.contains(CapRights::GRANT));

        // Requesting a strict subset drops further.
        let child2 = cs.derive(parent, CapRights::READ).unwrap();
        let c2 = *cs.lookup(child2).unwrap();
        assert_eq!(c2.rights, CapRights::READ);
        assert!(pslot.rights.contains(c2.rights));

        // Child inherits type + object, and records its parent slot.
        assert_eq!(cslot.cap_type, CapType::Frame);
        assert_eq!(cslot.object, 0x1000);
        assert_eq!(cslot.parent, Some(parent.0));
    }

    #[test]
    fn derive_requesting_a_missing_right_never_grants_it() {
        let mut cs = CapSpace::<4>::new();
        // Parent has READ only.
        let parent = cs.insert(CapType::Endpoint, CapRights::READ, 7).unwrap();
        // Ask for WRITE|GRANT — parent lacks both, so child gets NONE.
        let want = CapRights(CapRights::WRITE.0 | CapRights::GRANT.0);
        let child = cs.derive(parent, want).unwrap();
        assert_eq!(cs.lookup(child).unwrap().rights, CapRights::NONE);
    }

    #[test]
    fn derive_is_monotonic_down_a_chain() {
        let mut cs = CapSpace::<8>::new();
        let root = cs.insert(CapType::Frame, CapRights::ALL, 0).unwrap();
        let mut cur = root;
        let mut prev_rights = cs.lookup(root).unwrap().rights;
        // Each link may only restrict; verify parent ⊇ child at every step.
        for req in [CapRights::ALL, CapRights::READ, CapRights::NONE] {
            let next = cs.derive(cur, req).unwrap();
            let nr = cs.lookup(next).unwrap().rights;
            assert!(prev_rights.contains(nr));
            prev_rights = nr;
            cur = next;
        }
    }

    #[test]
    fn derive_on_invalid_parent_is_none() {
        let mut cs = CapSpace::<4>::new();
        // Out of range.
        assert!(cs.derive(CapId(3), CapRights::ALL).is_none());
        // Free slot.
        assert!(cs.derive(CapId(0), CapRights::ALL).is_none());
    }

    #[test]
    fn derive_capacity_exhaustion() {
        let mut cs = CapSpace::<2>::new();
        let parent = cs.insert(CapType::Frame, CapRights::ALL, 0).unwrap();
        // One slot left → first derive ok, next has nowhere to go.
        assert!(cs.derive(parent, CapRights::ALL).is_some());
        assert!(cs.derive(parent, CapRights::ALL).is_none());
    }

    #[test]
    fn revoke_subtree_removes_all_descendants() {
        let mut cs = CapSpace::<16>::new();
        //        root
        //       /    \
        //     c1      c2
        //    /  \      \
        //  g1a  g1b     g2
        let root = cs.insert(CapType::Untyped, CapRights::ALL, 0).unwrap();
        let c1 = cs.derive(root, CapRights::ALL).unwrap();
        let c2 = cs.derive(root, CapRights::ALL).unwrap();
        let g1a = cs.derive(c1, CapRights::ALL).unwrap();
        let g1b = cs.derive(c1, CapRights::ALL).unwrap();
        let g2 = cs.derive(c2, CapRights::ALL).unwrap();
        assert_eq!(cs.len(), 6);

        // Revoke a mid-tree node c1: c1, g1a, g1b vanish; root, c2, g2 survive.
        cs.revoke_subtree(c1);
        assert!(cs.lookup(c1).is_none());
        assert!(cs.lookup(g1a).is_none());
        assert!(cs.lookup(g1b).is_none());
        assert!(cs.lookup(root).is_some());
        assert!(cs.lookup(c2).is_some());
        assert!(cs.lookup(g2).is_some());
        assert_eq!(cs.len(), 3);

        // Revoke the whole tree from the root: everything goes.
        cs.revoke_subtree(root);
        assert!(cs.is_empty());
    }

    #[test]
    fn revoke_subtree_independent_trees_untouched() {
        let mut cs = CapSpace::<8>::new();
        let a = cs.insert(CapType::Frame, CapRights::ALL, 1).unwrap();
        let a_child = cs.derive(a, CapRights::ALL).unwrap();
        let b = cs.insert(CapType::Frame, CapRights::ALL, 2).unwrap();
        let b_child = cs.derive(b, CapRights::ALL).unwrap();

        cs.revoke_subtree(a);
        assert!(cs.lookup(a).is_none());
        assert!(cs.lookup(a_child).is_none());
        // The unrelated tree B is fully intact.
        assert!(cs.lookup(b).is_some());
        assert!(cs.lookup(b_child).is_some());
        assert_eq!(cs.len(), 2);
    }

    #[test]
    fn revoke_frees_slots_for_reuse() {
        let mut cs = CapSpace::<2>::new();
        let a = cs.insert(CapType::Frame, CapRights::ALL, 1).unwrap();
        let _b = cs.insert(CapType::Frame, CapRights::ALL, 2).unwrap();
        assert!(cs.insert(CapType::Frame, CapRights::ALL, 3).is_none()); // full
        cs.revoke_subtree(a);
        // Freed slot is now reusable.
        assert!(cs.insert(CapType::Frame, CapRights::ALL, 3).is_some());
    }

    #[test]
    fn revoke_invalid_or_free_is_noop() {
        let mut cs = CapSpace::<4>::new();
        let a = cs.insert(CapType::Frame, CapRights::ALL, 1).unwrap();
        // Out of range: no panic, no change.
        cs.revoke_subtree(CapId(99));
        assert_eq!(cs.len(), 1);
        // Already-free slot.
        cs.revoke_subtree(CapId(2));
        assert_eq!(cs.len(), 1);
        // Double revoke is harmless.
        cs.revoke_subtree(a);
        cs.revoke_subtree(a);
        assert!(cs.is_empty());
    }

    #[test]
    fn null_slot_handling_after_removal() {
        let mut cs = CapSpace::<4>::new();
        let a = cs.insert(CapType::Frame, CapRights::ALL, 1).unwrap();
        cs.revoke_subtree(a);
        // A removed slot reads back as free/None.
        assert!(cs.lookup(a).is_none());
        assert!(cs.slots[a.0].is_free());
    }
}
