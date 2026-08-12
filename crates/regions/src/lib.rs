#![cfg_attr(not(test), no_std)]
//! regions — the lifetime plan for shared memory: who still maps a region, and in what order
//! its teardown must happen.
//!
//! Pure bookkeeping over "which regions exist, who owns them, who has them mapped". It
//! performs nothing: it produces an ordered [`Plan`] and the kernel carries it out.
//!
//! ## Why the ORDER is the whole point
//!
//! A shared region is the first object in this kernel owned by a *process* rather than by the
//! kernel, so it is the first whose frames can outlive their owner while another process
//! still has them mapped. Review confirmed the resulting defect: frames went back to the pool
//! with the shared data intact and came straight back as another process's `USER_RW` stack.
//!
//! The fix is an ordering — unmap every holder, *then* release the frames — and an ordering
//! is exactly the kind of thing a comment cannot enforce. Reviewers said as much: teardown
//! ordering here was "load-bearing and nothing enforces it but a comment". So the plan is a
//! sequence, and [`Plan::well_ordered`] is a property checked over every configuration this
//! module can be handed: **no region's frames are released while any step still to come would
//! unmap it.**
//!
//! ## Identity
//!
//! Regions are named by a monotonic id that is never reused, and owners by process identity
//! rather than table slot. Both exist for the same reason as everywhere else in this kernel:
//! slots are recycled, and matching on a slot alone lets whoever occupies it next inherit
//! authority — or lose memory — that was never theirs.

/// A region table entry, reduced to what a lifetime decision depends on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Region {
    pub live: bool,
    /// Monotonic, never reused. `0` means "no region".
    pub id: u64,
    /// Identity (not slot) of the process that created it and may destroy it.
    pub owner: u64,
}

impl Region {
    pub const EMPTY: Region = Region {
        live: false,
        id: 0,
        owner: 0,
    };
    pub const fn new(id: u64, owner: u64) -> Region {
        Region {
            live: true,
            id,
            owner,
        }
    }
}

/// One process's mapping slots: the region id in each, `0` for an empty slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Holder<const S: usize> {
    pub live: bool,
    /// Identity of the process occupying this slot.
    pub id: u64,
    pub slots: [u64; S],
}

impl<const S: usize> Holder<S> {
    pub const FREE: Holder<S> = Holder {
        live: false,
        id: 0,
        slots: [0; S],
    };
    pub fn new(id: u64, slots: [u64; S]) -> Holder<S> {
        Holder {
            live: true,
            id,
            slots,
        }
    }
}

/// One action in a teardown plan. The ORDER of these matters; see [`Plan::well_ordered`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// Remove process `proc`'s mapping in share slot `slot` (of region `region`).
    Unmap {
        proc: usize,
        slot: usize,
        region: u64,
    },
    /// Drop every capability naming `region`, in every process. Per region rather than per
    /// process: the kernel sweeps the table anyway, and a step per (process, region) pair
    /// makes the worst-case plan several times larger for no extra information.
    ForgetCaps { region: u64 },
    /// Release region `region`'s frames back to the pool. Must come after every `Unmap` of it.
    Release { region: u64 },
}

/// A bounded, ordered teardown plan.
#[derive(Clone, Copy, Debug)]
pub struct Plan<const N: usize> {
    steps: [Option<Step>; N],
    len: usize,
    /// True if a step could not be recorded. The caller must treat a truncated plan as a
    /// refusal rather than executing it: a partial teardown is how frames get released while
    /// something still maps them.
    pub truncated: bool,
}

impl<const N: usize> Default for Plan<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Plan<N> {
    pub const fn new() -> Self {
        Plan {
            steps: [None; N],
            len: 0,
            truncated: false,
        }
    }

    fn push(&mut self, s: Step) {
        if self.len < N {
            self.steps[self.len] = Some(s);
            self.len += 1;
        } else {
            self.truncated = true;
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn steps(&self) -> impl Iterator<Item = Step> + '_ {
        self.steps[..self.len].iter().filter_map(|s| *s)
    }

    /// The safety property: no region is released while a later step would still unmap it.
    ///
    /// This is the cross-process use-after-free, stated as an order. If a `Release` for a
    /// region appears before any `Unmap` of that region, the frames go back to the pool while
    /// an address space still points at them — and this kernel hands recycled frames straight
    /// out as another process's stack.
    pub fn well_ordered(&self) -> bool {
        for (i, s) in self.steps().enumerate() {
            if let Step::Release { region } = s {
                for later in self.steps().skip(i + 1) {
                    if let Step::Unmap { region: r, .. } = later {
                        if r == region {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }
}

/// Every holder slot that currently maps `region`.
fn holders_of<const S: usize>(
    holders: &[Holder<S>],
    region: u64,
) -> impl Iterator<Item = (usize, usize)> + '_ {
    holders.iter().enumerate().flat_map(move |(p, h)| {
        h.slots
            .iter()
            .enumerate()
            .filter(move |(_, &r)| h.live && r == region && r != 0)
            .map(move |(s, _)| (p, s))
    })
}

/// The plan to destroy one region: unmap every holder and drop their capabilities, then
/// release the frames.
///
/// `region` must be a live id; a dead or zero id yields an empty plan, which is the correct
/// answer for a capability that outlived what it named.
pub fn destroy<const S: usize, const N: usize>(
    regions: &[Region],
    holders: &[Holder<S>],
    region: u64,
) -> Plan<N> {
    let mut plan = Plan::new();
    if region == 0 || !regions.iter().any(|r| r.live && r.id == region) {
        return plan;
    }
    for (p, s) in holders_of(holders, region) {
        plan.push(Step::Unmap {
            proc: p,
            slot: s,
            region,
        });
    }
    plan.push(Step::ForgetCaps { region });
    plan.push(Step::Release { region });
    plan
}

/// The plan for a dying process: destroy the regions it OWNS, then drop its own mappings of
/// everyone else's.
///
/// Owning is by identity. A process that merely borrowed a region must not take that region
/// down with it, and a process that owns one must not leave it mapped in a borrower after its
/// frames are gone.
pub fn teardown<const S: usize, const N: usize>(
    regions: &[Region],
    holders: &[Holder<S>],
    proc: usize,
    id: u64,
) -> Plan<N> {
    let mut plan: Plan<N> = Plan::new();
    for r in regions.iter() {
        if !r.live || r.owner != id {
            continue;
        }
        let sub: Plan<N> = destroy(regions, holders, r.id);
        for step in sub.steps() {
            plan.push(step);
        }
        if sub.truncated {
            plan.truncated = true;
        }
    }
    // Then this process's own mappings of regions it does not own. Their frames belong to
    // their owners and must not be touched — only the mapping goes.
    if let Some(h) = holders.get(proc) {
        if h.live && h.id == id {
            for (s, &r) in h.slots.iter().enumerate() {
                if r == 0 {
                    continue;
                }
                let owned = regions.iter().any(|x| x.live && x.id == r && x.owner == id);
                let already = plan.steps().any(
                    |st| matches!(st, Step::Unmap { proc: p, slot, .. } if p == proc && slot == s),
                );
                if !owned && !already {
                    plan.push(Step::Unmap {
                        proc,
                        slot: s,
                        region: r,
                    });
                }
            }
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    // The shapes the kernel actually deploys (crates/kernel/src/lib.rs:41, :91, :622):
    // MAX_PROCS = 6, SHARE_SLOTS = 4, PLAN_STEPS = MAX_PROCS*SHARE_SLOTS + 2*MAX_REGIONS +
    // SHARE_SLOTS = 52. These searches ran at P=3, S=2, N=32 — half the process count and
    // half the share slots — so a scan that stopped after two slots, or after three holders,
    // was invisible to every property below.
    const P: usize = 6; // processes        (kernel MAX_PROCS)
    const S: usize = 4; // share slots each (kernel SHARE_SLOTS)
    const N: usize = 52; // plan capacity   (kernel PLAN_STEPS)

    const A: u64 = 10; // owner identities
    const B: u64 = 20;
    const C: u64 = 30;

    fn h(id: u64, slots: [u64; S]) -> Holder<S> {
        Holder::new(id, slots)
    }

    #[test]
    fn destroying_a_region_unmaps_every_holder_before_releasing_it() {
        let regions = [Region::new(1, A)];
        let holders = [h(A, [1, 0, 0, 0]), h(B, [1, 0, 0, 0]), Holder::FREE];
        let plan: Plan<N> = destroy(&regions, &holders, 1);
        assert!(plan.well_ordered());
        let unmaps: Vec<_> = plan
            .steps()
            .filter(|s| matches!(s, Step::Unmap { .. }))
            .collect();
        assert_eq!(unmaps.len(), 2, "both holders must be unmapped");
        assert!(plan.steps().any(|s| s == Step::Release { region: 1 }));
    }

    #[test]
    fn a_stale_region_id_plans_nothing() {
        // The capability outlived its region. There is nothing to resolve to, which is the
        // property monotonic never-reused ids exist to give.
        let regions = [Region::EMPTY];
        let holders = [Holder::<S>::FREE; P];
        let plan: Plan<N> = destroy(&regions, &holders, 7);
        assert!(plan.is_empty());
    }

    #[test]
    fn a_dying_owner_takes_its_region_down_and_unmaps_the_borrower() {
        let regions = [Region::new(1, A)];
        let holders = [h(A, [1, 0, 0, 0]), h(B, [1, 0, 0, 0]), Holder::FREE];
        let plan: Plan<N> = teardown(&regions, &holders, 0, A);
        assert!(plan.well_ordered());
        assert!(plan.steps().any(|s| matches!(
            s,
            Step::Unmap {
                proc: 1,
                region: 1,
                ..
            }
        )));
        assert!(plan.steps().any(|s| s == Step::Release { region: 1 }));
    }

    #[test]
    fn a_dying_borrower_does_not_release_the_owners_frames() {
        let regions = [Region::new(1, A)];
        let holders = [h(A, [1, 0, 0, 0]), h(B, [1, 0, 0, 0]), Holder::FREE];
        let plan: Plan<N> = teardown(&regions, &holders, 1, B);
        assert!(
            !plan.steps().any(|s| matches!(s, Step::Release { .. })),
            "a borrower must not take the owner's memory with it"
        );
        assert!(plan.steps().any(|s| matches!(
            s,
            Step::Unmap {
                proc: 1,
                slot: 0,
                region: 1
            }
        )));
        // And it must not unmap the OWNER's mapping.
        assert!(!plan
            .steps()
            .any(|s| matches!(s, Step::Unmap { proc: 0, .. })));
    }

    #[test]
    fn ownership_is_by_identity_not_slot() {
        // Slot 0 is now a different process. It owns nothing, so its teardown releases
        // nothing, even though the previous occupant of its slot owned region 1.
        let regions = [Region::new(1, A)];
        let holders = [h(C, [0, 0, 0, 0]), h(B, [1, 0, 0, 0]), Holder::FREE];
        let plan: Plan<N> = teardown(&regions, &holders, 0, C);
        assert!(!plan.steps().any(|s| matches!(s, Step::Release { .. })));
    }

    #[test]
    fn well_ordered_rejects_a_release_before_its_unmap() {
        // The negative control the oracle lacked. Every plan the suite builds is well ordered,
        // so `well_ordered` was asserted constantly and never required to say NO — gutting it
        // to `return true` passed the whole suite, and combined with a reversed `destroy` that
        // is a green run emitting a genuine use-after-free.
        let mut bad: Plan<8> = Plan::new();
        bad.push(Step::Release { region: 1 });
        bad.push(Step::Unmap {
            proc: 0,
            slot: 0,
            region: 1,
        });
        assert!(!bad.well_ordered());

        let mut good: Plan<8> = Plan::new();
        good.push(Step::Unmap {
            proc: 0,
            slot: 0,
            region: 1,
        });
        good.push(Step::Release { region: 1 });
        assert!(good.well_ordered());

        // A Release of a DIFFERENT region before an unrelated Unmap is fine.
        let mut other: Plan<8> = Plan::new();
        other.push(Step::Release { region: 2 });
        other.push(Step::Unmap {
            proc: 0,
            slot: 0,
            region: 1,
        });
        assert!(other.well_ordered());
    }

    #[test]
    fn a_truncated_plan_is_flagged() {
        // A plan that does not fit must be refused, not executed: a partial teardown is how
        // frames get released while something still maps them.
        let regions = [Region::new(1, A)];
        let holders = [h(A, [1, 1, 0, 0]), h(B, [1, 1, 0, 0]), h(C, [1, 1, 0, 0])];
        let plan: Plan<2> = destroy(&regions, &holders, 1);
        assert!(plan.truncated);
    }

    #[test]
    fn teardown_propagates_truncation_from_a_sub_plan() {
        // Only `destroy` truncation was exercised. `teardown` appends sub-plans, so it has its
        // own way to overflow — and a teardown that silently dropped steps would release
        // frames while a holder kept them mapped, which is the whole hazard.
        let regions = [Region::new(1, A), Region::new(2, A)];
        let holders = [h(A, [1, 2, 0, 0]), h(B, [1, 2, 0, 0]), h(C, [1, 2, 0, 0])];
        let plan: Plan<4> = teardown(&regions, &holders, 0, A);
        assert!(
            plan.truncated,
            "an overflowing teardown must be flagged, not silently short"
        );
    }

    // ---------------------------------------------------------------- exhaustive checks
    //
    // Every configuration of a small table is enumerated. The universe deliberately contains
    // the discriminating cases the last three reviews showed a search must have: a free
    // holder carrying a STALE slot value, two processes with DIFFERENT identities, and a
    // region id that no longer exists.

    /// Region ids that may appear in a holder slot: none, a live one, another live one, and
    /// one that names nothing.
    const SLOT_VALUES: [u64; 4] = [0, 1, 2, 9];

    /// Every region table worth testing: which of ids 1 and 2 exist, and who owns them.
    fn region_tables() -> Vec<Vec<Region>> {
        let mut out = Vec::new();
        for r1 in [None, Some(A), Some(B)] {
            for r2 in [None, Some(A), Some(B)] {
                let mut t = Vec::new();
                if let Some(o) = r1 {
                    t.push(Region::new(1, o));
                }
                if let Some(o) = r2 {
                    t.push(Region::new(2, o));
                }
                out.push(t);
            }
        }
        out
    }

    /// Every holder table: two processes over identities A/B/C plus a free slot that still
    /// carries a stale region id, crossed with every pair of slot values.
    fn holder_tables() -> Vec<Vec<Holder<S>>> {
        let mut out = Vec::new();
        let ids = [A, B, C];
        // A free holder whose slots are NOT clean: nothing may act on it. Its POSITION varies,
        // because pinning it to the last index lets an implementation that assumes a compacted
        // table — stopping at the first dead entry, say — pass every property here. Review
        // found exactly that: the axis was present but held constant.
        let stale = Holder {
            live: false,
            id: C,
            slots: [1, 2, 9, 1],
        };
        for dead_at in 0..P {
            for id0 in ids {
                for a in SLOT_VALUES {
                    for b in SLOT_VALUES {
                        for c in SLOT_VALUES {
                            // Holders 1, 3 and 5 carry their references ONLY in the TAIL
                            // slots, and holder 0 keeps one in slot 2. A scan that stops
                            // after the first two slots therefore misses live shares
                            // entirely, rather than merely seeing them in a different order.
                            let mut t = vec![
                                h(id0, [a, b, c, 0]),
                                h(B, [0, 0, a, b]),
                                h(A, [c, 0, 0, a]),
                                h(C, [0, 0, b, c]),
                                h(B, [a, 0, c, 0]),
                                h(A, [0, 0, 0, b]),
                            ];
                            t[dead_at] = stale;
                            out.push(t);
                        }
                    }
                }
            }
        }
        out
    }

    fn for_every_config(mut f: impl FnMut(&[Region], &[Holder<S>])) {
        for rt in region_tables() {
            for ht in holder_tables() {
                f(&rt, &ht);
            }
        }
    }

    #[test]
    fn exhaustive_plans_are_always_well_ordered() {
        // THE property: no region's frames are released while a later step still unmaps it.
        let mut n = 0u64;
        for_every_config(|rt, ht| {
            for id in [0u64, 1, 2, 9] {
                let plan: Plan<N> = destroy(rt, ht, id);
                assert!(plan.well_ordered(), "destroy({id}) out of order");
                assert!(!plan.truncated);
                n += 1;
            }
            for (p, holder) in ht.iter().enumerate() {
                let plan: Plan<N> = teardown(rt, ht, p, holder.id);
                assert!(plan.well_ordered(), "teardown({p}) out of order");
                assert!(!plan.truncated);
                n += 1;
            }
        });
        assert!(n > 1000, "expected a real search, only did {n}");
    }

    #[test]
    fn exhaustive_destroy_unmaps_exactly_the_live_holders() {
        // Exactly: missing one leaves a mapping onto freed frames; inventing one unmaps a
        // process that never had it. A free holder's stale slots must be ignored entirely.
        for_every_config(|rt, ht| {
            for id in [1u64, 2, 9] {
                let plan: Plan<N> = destroy(rt, ht, id);
                let mut want: Vec<(usize, usize)> = Vec::new();
                if id != 0 && rt.iter().any(|r| r.live && r.id == id) {
                    for (p, holder) in ht.iter().enumerate() {
                        if !holder.live {
                            continue;
                        }
                        for (s, &r) in holder.slots.iter().enumerate() {
                            if r == id {
                                want.push((p, s));
                            }
                        }
                    }
                }
                let got: Vec<(usize, usize)> = plan
                    .steps()
                    .filter_map(|st| match st {
                        Step::Unmap { proc, slot, .. } => Some((proc, slot)),
                        _ => None,
                    })
                    .collect();
                assert_eq!(got, want, "destroy({id}) unmap set");
            }
        });
    }

    #[test]
    fn exhaustive_release_happens_iff_the_region_exists() {
        for_every_config(|rt, ht| {
            for id in [0u64, 1, 2, 9] {
                let plan: Plan<N> = destroy(rt, ht, id);
                let released = plan.steps().any(|s| s == Step::Release { region: id });
                let exists = id != 0 && rt.iter().any(|r| r.live && r.id == id);
                assert_eq!(released, exists, "release({id})");
            }
        });
    }

    #[test]
    fn exhaustive_teardown_unmaps_exactly_the_right_holders() {
        // The gap review found, and it is the important one: `well_ordered` forbids a Release
        // FOLLOWED BY an Unmap of the same region, but passes vacuously when the Unmap is
        // simply ABSENT — which is the actual shape of the confirmed use-after-free. Every
        // other teardown property reads the Release set, the ordering, or emptiness; none
        // reads the UNMAP set, so a teardown that dropped unmaps produced a plan that was
        // "well ordered" while a live holder still mapped the freed frames.
        //
        // Note for the next time: the fix is a PROPERTY, not a wider universe. The slot axis
        // was already varied; nothing read the answer.
        for_every_config(|rt, ht| {
            for (p, holder) in ht.iter().enumerate() {
                let plan: Plan<N> = teardown(rt, ht, p, holder.id);
                let mut want: Vec<(usize, usize)> = Vec::new();
                // Every live holder of every region this process owns.
                for r in rt.iter().filter(|r| r.live && r.owner == holder.id) {
                    for (hp, hh) in ht.iter().enumerate() {
                        if !hh.live {
                            continue;
                        }
                        for (hs, &v) in hh.slots.iter().enumerate() {
                            if v == r.id {
                                want.push((hp, hs));
                            }
                        }
                    }
                }
                // Plus this process's own mappings of regions it does not own.
                if holder.live {
                    for (sl, &v) in holder.slots.iter().enumerate() {
                        if v == 0 {
                            continue;
                        }
                        let owned = rt
                            .iter()
                            .any(|x| x.live && x.id == v && x.owner == holder.id);
                        if !owned && !want.contains(&(p, sl)) {
                            want.push((p, sl));
                        }
                    }
                }
                let mut got: Vec<(usize, usize)> = plan
                    .steps()
                    .filter_map(|st| match st {
                        Step::Unmap { proc, slot, .. } => Some((proc, slot)),
                        _ => None,
                    })
                    .collect();
                got.sort_unstable();
                want.sort_unstable();
                assert_eq!(got, want, "teardown({p}, {}) unmap set", holder.id);
            }
        });
    }

    #[test]
    fn exhaustive_teardown_releases_exactly_what_the_dying_process_owns() {
        // A borrower must never release; an owner must always release, once per region.
        for_every_config(|rt, ht| {
            for (p, holder) in ht.iter().enumerate() {
                let plan: Plan<N> = teardown(rt, ht, p, holder.id);
                let mut want: Vec<u64> = rt
                    .iter()
                    .filter(|r| r.live && r.owner == holder.id)
                    .map(|r| r.id)
                    .collect();
                let mut got: Vec<u64> = plan
                    .steps()
                    .filter_map(|s| match s {
                        Step::Release { region } => Some(region),
                        _ => None,
                    })
                    .collect();
                want.sort_unstable();
                got.sort_unstable();
                assert_eq!(got, want, "teardown({p}, {}) release set", holder.id);
            }
        });
    }

    #[test]
    fn exhaustive_teardown_of_a_stale_identity_touches_nothing() {
        // The dying identity and the slot's CURRENT occupant are separate facts. Every other
        // property here passes `holders[p].id`, so they always agree — and with the axis held
        // constant the identity check in `teardown` could be deleted with the suite green.
        // That is the fourth time in this kernel that an exhaustive search proved nothing
        // about an axis its universe never varied, so this one varies it: tear down slot `p`
        // as an identity that does NOT occupy it, and require the plan to be empty. A
        // recycled slot must not have its new occupant's mappings dropped by its predecessor.
        const GHOST: u64 = 77;
        for_every_config(|rt, ht| {
            for p in 0..ht.len() {
                assert!(ht[p].id != GHOST);
                let plan: Plan<N> = teardown(rt, ht, p, GHOST);
                assert!(
                    plan.is_empty(),
                    "tearing down slot {p} as a stale identity planned {:?}",
                    plan.steps().collect::<Vec<_>>()
                );
            }
        });
    }

    #[test]
    fn exhaustive_teardown_never_unmaps_another_process_from_a_region_it_does_not_own() {
        // A dying BORROWER may only touch its own mappings.
        for_every_config(|rt, ht| {
            for (p, holder) in ht.iter().enumerate() {
                if !holder.live {
                    continue;
                }
                let owns_any = rt.iter().any(|r| r.live && r.owner == holder.id);
                if owns_any {
                    continue; // an owner legitimately unmaps others
                }
                let plan: Plan<N> = teardown(rt, ht, p, holder.id);
                for s in plan.steps() {
                    if let Step::Unmap { proc, .. } = s {
                        assert_eq!(proc, p, "a borrower unmapped someone else");
                    }
                }
            }
        });
    }

    #[test]
    fn exhaustive_a_dead_holders_stale_slots_are_never_acted_on() {
        // The recurring lesson: the universe contains a NON-live holder whose slots still
        // name regions. Nothing may unmap it — its address space is gone, and its slot is
        // about to belong to somebody else.
        for_every_config(|rt, ht| {
            for id in [1u64, 2, 9] {
                let plan: Plan<N> = destroy(rt, ht, id);
                for s in plan.steps() {
                    if let Step::Unmap { proc, .. } = s {
                        assert!(ht[proc].live, "unmapped a dead holder");
                    }
                }
            }
        });
    }
}
