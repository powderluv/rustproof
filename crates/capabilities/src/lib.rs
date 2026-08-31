#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
//! capabilities — a fixed-capacity capability space for the Rustproof nucleus.
//!
//! VERIFIED TCB crate. A `CapSpace<N>` owns a flat array of `N` `CapSlot`s (no heap;
//! the array is inline storage). Capabilities are addressed by their slot index
//! (`abi::CapId`). Free slots are marked by `CapType::Null`.
//!
//! ONE rule is enforced structurally here, and it is worth being exact about which:
//!
//! - **Coarse revocation only.** The sole removal primitive is [`CapSpace::revoke`], which
//!   empties exactly one slot. There is no fine-grained mid-execution rights downgrade, and
//!   no subtree: a space is FLAT.
//!
//! **Authority monotonicity is NOT a property of this crate.** Rights are attenuated by
//! `abi::CapRights::intersect` at the kernel sites that mint capabilities — the `SPAWN`
//! delegation and `make_region` — and asserted exhaustively over the rights lattice in
//! `abi`'s own tests. This crate stores whatever rights it is handed; it enforces nothing
//! about them. Transitive teardown likewise is not here: it lives in `deleg`, whose edges
//! carry process IDENTITY rather than a slot index.
//!
//! This page used to claim both of those as this crate's rules, on the strength of a
//! `derive` operation and a revocation fixpoint that the kernel never once exercised. They
//! were deleted; a reader — in particular a future Verus author picking a first target —
//! should not be handed a proof obligation with nothing to discharge it against.
//!
//! See docs/nucleus-design.md and docs/verification.md.

use abi::{CapId, CapRights, CapType};

/// One capability slot. A slot is *free* iff `cap_type == CapType::Null`.
///
/// `object` is an opaque handle to the referenced kernel object (e.g. a physical
/// address / frame number / TCB index); this crate never interprets it. Every live slot is
/// a ROOT — there is no `parent` link and no derivation relation inside a space.
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
/// - A slot's authority depends on NO OTHER SLOT. Slots are independent roots; there is no
///   derivation relation inside a space, so there is nothing here to keep consistent and
///   nothing whose termination or completeness needs an argument.
/// - `lookup(c)` is `Some` exactly when `c < N` and `slots[c].cap_type != Null`.
/// - `revoke(c)` restores `slots[c]` to `CapSlot::EMPTY` exactly, so a recycled slot carries
///   nothing of its predecessor.
///
/// The invariant this block used to state — "every live slot with `parent == Some(p)` has
/// `slots[p]` live" — was VACUOUS: no live slot ever had a parent, because the only writer of
/// a live space is `insert`, which mints roots. A proof obligation discharged by emptiness is
/// worse than none, since it reads as coverage.
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
    /// wrong shape to keep. And intra-space derivation is not coming back as a `deleg` edge
    /// either: `MAKE_REGION` mints a `Region` from an `Untyped` in one space, but an `Untyped`
    /// names no extent, so that relation is depth-1, terminal, and carries no authority to
    /// revoke through. See docs/nucleus-design.md §1.2.
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
    fn revoke_frees_exactly_the_named_slot() {
        // Nothing pinned this once the fixpoint went. `revoke` must empty the slot it is
        // given and touch no other — the whole of what the primitive now promises.
        let mut cs: CapSpace<4> = CapSpace::new();
        let a = cs.insert(CapType::Untyped, CapRights::ALL, 1).unwrap();
        let b = cs.insert(CapType::Endpoint, CapRights::READ, 2).unwrap();
        let c = cs.insert(CapType::Mmio, CapRights::WRITE, 3).unwrap();
        cs.revoke(b);
        assert!(cs.lookup(b).is_none(), "the named slot must be freed");
        assert_eq!(cs.lookup(a).unwrap().object, 1, "a neighbour was disturbed");
        assert_eq!(cs.lookup(c).unwrap().object, 3, "a neighbour was disturbed");
        // And the freed slot carries nothing of its predecessor.
        let reused = cs.insert(CapType::Irq, CapRights::READ, 9).unwrap();
        assert_eq!(reused, b, "insert should reuse the freed slot");
        let slot = cs.lookup(reused).unwrap();
        assert_eq!(slot.cap_type, CapType::Irq);
        assert_eq!(slot.object, 9);
    }

    #[test]
    fn insert_stores_the_rights_it_was_given() {
        // The other half of attenuation: `intersect` computes the rights (asserted in abi),
        // and this crate must store them verbatim. With `derive` gone, nothing else pinned
        // that a mint cannot quietly widen what it was handed.
        for bits in 0u8..8 {
            let mut cs: CapSpace<2> = CapSpace::new();
            let r = CapRights(bits);
            let id = cs.insert(CapType::Untyped, r, 0x77).unwrap();
            assert_eq!(cs.lookup(id).unwrap().rights, r, "rights altered on insert");
        }
    }

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
        // And the BOUNDARY, exactly one past the end. `99` is far outside and passes any
        // plausible off-by-one; `N` itself is the value that distinguishes `>= N` from
        // `> N`, and it is the one an out-of-range id from a syscall argument would hit
        // first. Found by tools/mutate.py: widening the bound to `>= N + 1` survived the
        // whole suite, because nothing ever asked for exactly this slot.
        cs.revoke(CapId(4));
        assert_eq!(cs.len(), 1);
        cs.revoke(CapId(usize::MAX));
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

    /// The capacity the kernel actually deploys: `CapSpace<CAP_SLOTS>` with CAP_SLOTS = 16
    /// (crates/kernel/src/lib.rs:65, :146). Every other test in this file uses N in {2, 4, 8},
    /// so the deployed width was never instantiated — and because `first_free` is a linear
    /// scan, no test could distinguish it from a scan that stops early. Truncating that scan
    /// to the first 8 slots leaves the rest of this suite green.
    const DEPLOYED: usize = 16;

    #[test]
    fn every_slot_of_the_deployed_width_is_reachable() {
        let mut cs: CapSpace<DEPLOYED> = CapSpace::new();
        // Fill it completely; each insert must land in the next free slot, including the
        // ones past the widths the other tests use.
        for i in 0..DEPLOYED {
            let id = cs
                .insert(CapType::Endpoint, CapRights::ALL, i as u64)
                .unwrap_or_else(|| panic!("space full at slot {i} of {DEPLOYED}"));
            assert_eq!(id, CapId(i), "insert skipped to the wrong slot");
        }
        assert_eq!(cs.len(), DEPLOYED);
        assert!(cs.insert(CapType::Endpoint, CapRights::ALL, 99).is_none());

        // Every slot round-trips by index, and carries its own object rather than a
        // neighbour's.
        for i in 0..DEPLOYED {
            assert_eq!(cs.lookup(CapId(i)).unwrap().object, i as u64);
        }

        // Freeing any single slot -- including one only the deployed width has -- makes
        // exactly that slot the next one handed out.
        for i in 0..DEPLOYED {
            cs.revoke(CapId(i));
            assert!(cs.lookup(CapId(i)).is_none());
            let back = cs.insert(CapType::Irq, CapRights::READ, 0xAA).unwrap();
            assert_eq!(back, CapId(i), "first_free did not find the hole at {i}");
            // restore the original occupant so the next iteration starts from a full space
            cs.revoke(CapId(i));
            cs.insert(CapType::Endpoint, CapRights::ALL, i as u64)
                .unwrap();
        }
    }

    /// A capability carrying NO rights still OCCUPIES its slot.
    ///
    /// Possession is not authority, but it is still possession — and the two are easy to
    /// conflate in `first_free`, which asks only whether a slot is free. Measured: relaxing it
    /// to `s.is_free() || s.rights == CapRights::NONE` leaves this crate at 11/11, the kernel at
    /// 17/17, and the whole host suite green.
    ///
    /// It is reachable, not hypothetical. The kernel's worker role begins with
    /// `NO_AUTHORITY = (Endpoint, NONE, 0)` at entry 0 precisely so that entry `i` becomes
    /// `CapId(i)`, and `load_process` fills the space by inserting the table in order. Under
    /// that mutation the placeholder's slot is handed to the NEXT insert — the worker's
    /// `(Mmio, ALL, ...)` — so `CapId(0)` silently stops being the shared endpoint and becomes
    /// full device authority. The kernel's own tests check the grant TABLE; none of them builds
    /// a `CapSpace` from one, so nothing noticed.
    ///
    /// What this constructs that no other test here does: a live NONE-rights slot with a
    /// SECOND insert behind it. The rights loop above makes a fresh space per iteration and
    /// inserts exactly one capability, so it pins "a NONE-rights cap can be looked up" and
    /// cannot pin "a NONE-rights cap keeps its slot".
    #[test]
    fn a_rights_less_capability_still_occupies_its_slot() {
        let mut cs: CapSpace<4> = CapSpace::new();
        let placeholder = cs.insert(CapType::Endpoint, CapRights::NONE, 0).unwrap();
        assert_eq!(placeholder, CapId(0));
        let device = cs
            .insert(CapType::Mmio, CapRights::ALL, 0xE000_0000)
            .unwrap();
        assert_eq!(
            device,
            CapId(1),
            "the next insert took the rights-less slot instead of the free one"
        );
        let kept = cs.lookup(placeholder).expect("placeholder was evicted");
        assert_eq!(kept.cap_type, CapType::Endpoint);
        assert_eq!(kept.rights, CapRights::NONE);
        assert_eq!(cs.len(), 2);
    }

    /// `revoke` empties the slot it is NAMED, not every slot naming the same object.
    ///
    /// Measured: replacing it with "empty every slot whose `object` matches" leaves this crate
    /// at 11/11 and the kernel at 17/17, because every hand-written scenario in this file gives
    /// each slot a DISTINCT object — so nothing can tell "the named slot" from "everything
    /// naming that object".
    ///
    /// Two capabilities on one object is the normal shape, not a corner case: the kernel's
    /// worker holds an `Mmio` with ALL rights and a second `Mmio` on the SAME window with only
    /// WRITE, and the whole point of the pair is that revoking one leaves the other. If revoke
    /// took both, `holds_mmio` would go false at the first revocation and a mapping would be
    /// torn down early.
    #[test]
    fn revoke_spares_a_sibling_naming_the_same_object() {
        const WINDOW: u64 = 0xE000_0000;
        let mut cs: CapSpace<4> = CapSpace::new();
        let full = cs.insert(CapType::Mmio, CapRights::ALL, WINDOW).unwrap();
        let weak = cs.insert(CapType::Mmio, CapRights::WRITE, WINDOW).unwrap();
        cs.revoke(full);
        assert!(cs.lookup(full).is_none(), "the named slot survived revoke");
        let survivor = cs
            .lookup(weak)
            .expect("revoke took a sibling naming the same object");
        assert_eq!(survivor.rights, CapRights::WRITE);
        assert_eq!(survivor.object, WINDOW);
        assert_eq!(cs.len(), 1);
    }
}
