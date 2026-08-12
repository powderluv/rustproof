#![cfg_attr(not(test), no_std)]
//! deleg — the delegation ledger: who handed which capability to whom, and what a
//! revocation reaches.
//!
//! This is pure index/identity bookkeeping, deliberately separated from the kernel so it can
//! be tested exhaustively on the host rather than by one scripted boot. It knows nothing
//! about capability spaces, address spaces or scheduling: it answers exactly one question —
//! *given this graph of delegations, which (process, capability) pairs does an operation
//! reach?* — and the kernel performs the effects.
//!
//! It is here because this is the most defect-dense logic in the kernel. Adversarial review
//! has confirmed four separate defects in it: authority derived from a recycled slot rather
//! than an identity (twice), a teardown that CUT delegation chains instead of splicing them
//! so an ancestor's revoke silently missed the grandchildren, and blocked processes left
//! stranded by a revocation that took their capability. Every one of those is a property of
//! the graph alone, which means every one of them is checkable here, with no QEMU involved.
//!
//! ## The identity rule
//!
//! Both endpoints of an edge are process SLOTS, and slots are recycled. So every edge also
//! records the IDENTITY of the process that occupied each end when the edge was made, and
//! every match tests both. Matching on the slot alone would let whoever occupies that slot
//! next inherit revocation authority over someone else's children, or have its own
//! capabilities stripped by an edge it has no relation to. Two of the four confirmed defects
//! were exactly this, at one end and then at the other — which is why [`Endpoint`] exists as
//! a single type rather than as loose pairs of fields.

/// One end of a delegation edge: a capability slot in a specific process INCARNATION.
///
/// `(slot, id)` together, never `slot` alone. The type exists so that the two ends cannot be
/// compared inconsistently — the bug shape that has appeared twice here is fixing one end
/// and leaving the other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Endpoint {
    /// Process table slot.
    pub proc: usize,
    /// Identity of the process occupying that slot when the edge was recorded.
    pub id: u64,
    /// Capability slot within that process's capability space.
    pub cap: usize,
}

impl Endpoint {
    pub const fn new(proc: usize, id: u64, cap: usize) -> Self {
        Endpoint { proc, id, cap }
    }
    /// Same process incarnation, ignoring which capability.
    #[inline]
    pub const fn same_proc(&self, proc: usize, id: u64) -> bool {
        self.proc == proc && self.id == id
    }
}

/// A recorded delegation: `parent` handed a capability to `child`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Edge {
    parent: Endpoint,
    child: Endpoint,
    live: bool,
}

const DEAD: Edge = Edge {
    parent: Endpoint::new(0, 0, 0),
    child: Endpoint::new(0, 0, 0),
    live: false,
};

/// A fixed-capacity delegation ledger. No heap, no allocation, bounded work.
pub struct Ledger<const N: usize> {
    edges: [Edge; N],
}

impl<const N: usize> Default for Ledger<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Ledger<N> {
    pub const fn new() -> Self {
        Ledger { edges: [DEAD; N] }
    }

    /// Capacity, and the bound on how much any operation below can do.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Live edges.
    pub fn len(&self) -> usize {
        self.edges.iter().filter(|e| e.live).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Record that `parent` delegated to `child`.
    ///
    /// Returns `false` if the ledger is full, and records nothing — the caller must refuse
    /// the delegation rather than proceed unrecorded, or the child would hold a capability
    /// no revocation could ever reach.
    pub fn record(&mut self, parent: Endpoint, child: Endpoint) -> bool {
        match self.edges.iter().position(|e| !e.live) {
            Some(i) => {
                self.edges[i] = Edge {
                    parent,
                    child,
                    live: true,
                };
                true
            }
            None => false,
        }
    }

    /// Every capability reachable from `root` by delegation, transitively, excluding `root`
    /// itself. The reached edges are marked dead; the reached ENDPOINTS are written into
    /// `out` and their count returned.
    ///
    /// The root is deliberately left alone: a process revoking what it handed out does not
    /// disarm itself.
    ///
    /// `out` must hold at least [`capacity`](Self::capacity) entries; anything beyond that
    /// cannot exist, since each entry corresponds to a distinct live edge.
    pub fn revoke_from(&mut self, root: Endpoint, out: &mut [Endpoint]) -> usize {
        let mut n = 0usize;
        // Sources whose derivations must die. Starts as {root} and grows as the fixpoint
        // discovers children, so a grandchild is reached through its parent.
        loop {
            let mut progress = false;
            for i in 0..N {
                let e = self.edges[i];
                if !e.live {
                    continue;
                }
                let from_root = e.parent == root;
                let from_reached = out[..n].iter().any(|&r| r == e.parent);
                if !from_root && !from_reached {
                    continue;
                }
                self.edges[i].live = false;
                if n < out.len() {
                    out[n] = e.child;
                    n += 1;
                }
                progress = true;
            }
            if !progress {
                break;
            }
        }
        n
    }

    /// Remove a dead process from the ledger, SPLICING rather than cutting.
    ///
    /// An edge *into* the dead process is re-parented: everything it delegated onward is
    /// re-attached to the source it received the capability from. Cutting instead would make
    /// an ancestor's later revocation silently miss those grandchildren and report success
    /// while they kept the capability — a confirmed defect, and the reason this is one
    /// function rather than two loops at a call site.
    ///
    /// Edges still rooted at the dead process after that derive from a capability it held in
    /// its own right, so no surviving process can revoke through them; they are dropped.
    ///
    /// # Precondition: the graph is a FOREST
    ///
    /// Re-parenting is only well defined when a capability has ONE source. The kernel
    /// guarantees this by construction: the single site that records an edge is the `SPAWN`
    /// delegation, whose child is a process created moments earlier, so an edge's child
    /// endpoint is always a fresh incarnation with no prior edges. Each child therefore has
    /// exactly one in-edge, and no cycle can form.
    ///
    /// This is not a stylistic note. The exhaustive tests below originally ran over ARBITRARY
    /// graphs and found a case — two edges into one capability slot, closing a cycle — where
    /// splicing loses a path an ancestor could previously reach. That graph is unreachable
    /// through the kernel's API, so the precondition is stated and the search restricted to
    /// graphs the kernel can actually build, rather than the code being armoured against an
    /// input it cannot receive. A malformed graph still terminates (see `a_cycle_terminates`);
    /// it simply gets no re-parenting guarantee.
    pub fn splice_out(&mut self, proc: usize, id: u64) {
        for i in 0..N {
            let inc = self.edges[i];
            if !inc.live || !inc.child.same_proc(proc, id) {
                continue;
            }
            for j in 0..N {
                let out = &mut self.edges[j];
                if out.live && out.parent == inc.child {
                    out.parent = inc.parent;
                }
            }
            self.edges[i].live = false;
        }
        for e in self.edges.iter_mut() {
            if e.live && e.parent.same_proc(proc, id) {
                e.live = false;
            }
        }
    }

    /// Drop every edge naming `end`, at BOTH ends.
    ///
    /// Used when a capability slot is emptied by something other than revocation (a region
    /// being destroyed, say). An edge left naming a freed slot would later be walked as
    /// though it still described a delegation, and the slot may by then hold something else.
    pub fn forget(&mut self, end: Endpoint) {
        for e in self.edges.iter_mut() {
            if e.live && (e.parent == end || e.child == end) {
                e.live = false;
            }
        }
    }

    /// Every live edge, as `(parent, child)`. For tests and invariant checks.
    pub fn live_edges(&self) -> impl Iterator<Item = (Endpoint, Endpoint)> + '_ {
        self.edges
            .iter()
            .filter(|e| e.live)
            .map(|e| (e.parent, e.child))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Identities. D is a SECOND incarnation of slot 0 — see `universe`.
    const A: u64 = 100;
    const B: u64 = 200;
    const C: u64 = 300;
    const D: u64 = 400;

    fn e(proc: usize, id: u64, cap: usize) -> Endpoint {
        Endpoint::new(proc, id, cap)
    }

    /// Assert two endpoints are equal WITHOUT going through `Endpoint`'s own comparison.
    ///
    /// Writing assertions in terms of the thing under test is how `same_proc` became
    /// untestable: every check agreed with it by construction. These compare raw fields.
    fn same(a: Endpoint, b: Endpoint) -> bool {
        a.proc == b.proc && a.id == b.id && a.cap == b.cap
    }

    #[test]
    fn record_until_full_then_refuses() {
        let mut l: Ledger<2> = Ledger::new();
        assert!(l.record(e(0, A, 0), e(1, B, 0)));
        assert!(l.record(e(0, A, 1), e(2, C, 0)));
        // Full: must refuse rather than silently drop, or the child would hold a capability
        // no revocation could reach.
        assert!(!l.record(e(0, A, 2), e(3, 500, 0)));
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn revoke_reaches_grandchildren() {
        let mut l: Ledger<8> = Ledger::new();
        l.record(e(0, A, 0), e(1, B, 0));
        l.record(e(1, B, 0), e(2, C, 0));
        let mut out = [e(0, 0, 0); 8];
        let n = l.revoke_from(e(0, A, 0), &mut out);
        assert_eq!(n, 2, "the grandchild must be reached through its parent");
        assert!(out[..n].iter().any(|&x| same(x, e(1, B, 0))));
        assert!(out[..n].iter().any(|&x| same(x, e(2, C, 0))));
        assert!(l.is_empty());
    }

    #[test]
    fn revoke_leaves_the_root_alone() {
        let mut l: Ledger<8> = Ledger::new();
        l.record(e(0, A, 0), e(1, B, 0));
        let mut out = [e(0, 0, 0); 8];
        let n = l.revoke_from(e(0, A, 0), &mut out);
        // A process revoking what it handed out does not disarm itself.
        assert!(!out[..n].iter().any(|&x| same(x, e(0, A, 0))));
    }

    #[test]
    fn splice_keeps_an_ancestor_able_to_reach_a_grandchild() {
        let mut l: Ledger<8> = Ledger::new();
        l.record(e(0, A, 0), e(1, B, 0)); // A -> B
        l.record(e(1, B, 0), e(2, C, 0)); // B -> C
        l.splice_out(1, B); // B dies
        let mut out = [e(0, 0, 0); 8];
        let n = l.revoke_from(e(0, A, 0), &mut out);
        assert_eq!(n, 1);
        assert!(
            same(out[0], e(2, C, 0)),
            "A must still reach C after B dies"
        );
    }

    #[test]
    fn splice_drops_edges_rooted_at_the_dead_process() {
        let mut l: Ledger<8> = Ledger::new();
        // B delegates a capability it holds in its OWN right (nobody delegated it to B).
        l.record(e(1, B, 5), e(2, C, 0));
        l.splice_out(1, B);
        assert!(l.is_empty(), "nothing may survive naming a recycled slot");
    }

    #[test]
    fn forget_drops_edges_at_both_ends() {
        let mut l: Ledger<8> = Ledger::new();
        l.record(e(0, A, 0), e(1, B, 0));
        l.record(e(1, B, 0), e(2, C, 0));
        l.forget(e(1, B, 0));
        assert!(
            l.is_empty(),
            "an edge naming a freed slot must go at BOTH ends"
        );
    }

    // ------------------------------------------------- the identity rule, checked directly
    //
    // Every operation is keyed on (slot, identity). These graphs deliberately contain TWO
    // incarnations of slot 0 — something the kernel itself never builds, because teardown
    // removes a dead process's edges before its slot is reused. That is exactly why the
    // checks must be tested here: they are the defence that makes a stale edge harmless if
    // one ever survives, and a defence nothing exercises is an assumption.

    #[test]
    fn revoke_ignores_a_different_incarnation_of_the_same_slot() {
        let mut l: Ledger<8> = Ledger::new();
        l.record(e(0, A, 0), e(1, B, 0));
        let mut out = [e(0, 0, 0); 8];
        // Slot 0, different process. It must not inherit revocation authority.
        let n = l.revoke_from(e(0, D, 0), &mut out);
        assert_eq!(n, 0);
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn revoke_transitively_ignores_a_different_incarnation() {
        // The child-END of the rule, which the root-end test above does not cover: the
        // fixpoint matches an edge's parent against endpoints it has already reached.
        let mut l: Ledger<8> = Ledger::new();
        l.record(e(1, B, 0), e(0, A, 0)); // reaches (0,A,0)
        l.record(e(0, D, 0), e(2, C, 0)); // parented at a DIFFERENT incarnation of slot 0
        let mut out = [e(0, 0, 0); 8];
        let n = l.revoke_from(e(1, B, 0), &mut out);
        assert_eq!(n, 1, "reaching (0,A,0) must not drag in (0,D,0)'s children");
        assert!(same(out[0], e(0, A, 0)));
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn splice_ignores_a_different_incarnation_of_the_same_slot() {
        let mut l: Ledger<8> = Ledger::new();
        l.record(e(0, D, 0), e(2, C, 0));
        l.record(e(1, B, 0), e(0, A, 0));
        // Slot 0 dies as incarnation A. Edges naming incarnation D must be untouched.
        l.splice_out(0, A);
        let live: Vec<_> = l.live_edges().collect();
        assert_eq!(live.len(), 1);
        assert!(same(live[0].0, e(0, D, 0)) && same(live[0].1, e(2, C, 0)));
    }

    #[test]
    fn forget_ignores_a_different_incarnation_of_the_same_slot() {
        let mut l: Ledger<8> = Ledger::new();
        l.record(e(0, D, 0), e(2, C, 0));
        l.forget(e(0, A, 0)); // same slot and cap, different process
        assert_eq!(
            l.len(),
            1,
            "forget must not drop another incarnation's edge"
        );
    }

    // ---------------------------------------------------------------- exhaustive checks
    //
    // The graphs are tiny, so the properties are checked over EVERY graph of that size rather
    // than over hand-picked examples. Hand-picked cases are how the confirmed defects got in.

    /// The endpoint universe. Note (0, A, _) and (0, D, _): the SAME SLOT under two different
    /// identities. Without that pair every `Endpoint` comparison in the search degenerates to
    /// a slot+cap comparison and the identity half of every check is inert — a review found
    /// five separate identity checks that could be deleted with the whole suite still green.
    fn universe() -> [Endpoint; 6] {
        [
            e(0, A, 0),
            e(0, A, 1),
            e(0, D, 0),
            e(1, B, 0),
            e(1, B, 1),
            e(2, C, 0),
        ]
    }

    /// What a revocation from `root` MUST reach, computed independently of the ledger.
    ///
    /// This shares no code with the thing it checks, which is the whole point. The previous
    /// oracle asked the ledger whether anything was still reachable AFTER the revocation —
    /// but revoking kills the root's direct out-edges, which disconnects the subtree from the
    /// root whether or not the descendants were actually reached. A revoke with no fixpoint
    /// at all passed it. Review demonstrated exactly that.
    fn expected_closure(made: &[(Endpoint, Endpoint)], root: Endpoint) -> Vec<Endpoint> {
        let mut reached: Vec<Endpoint> = Vec::new();
        loop {
            let mut grew = false;
            for (p, c) in made.iter() {
                let from = same(*p, root) || reached.iter().any(|r| same(*r, *p));
                if from && !reached.iter().any(|r| same(*r, *c)) {
                    reached.push(*c);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        reached
    }

    /// Does this edge set match what the kernel can build? Each child endpoint has at most one
    /// in-edge (a delegation lands in a fresh process's capability slot), and following edges
    /// forward from any node terminates rather than returning to it.
    fn is_forest(edges: &[(Endpoint, Endpoint)]) -> bool {
        for (i, (_, c)) in edges.iter().enumerate() {
            if edges.iter().skip(i + 1).any(|(_, c2)| same(*c2, *c)) {
                return false; // two sources for one capability
            }
        }
        // Acyclic: from every node, walking forward must not return to it. With at most one
        // in-edge per child the forward walk is deterministic, so a bounded walk suffices.
        let mut nodes: Vec<Endpoint> = Vec::new();
        for (p, c) in edges.iter() {
            for n in [*p, *c] {
                if !nodes.iter().any(|x| same(*x, n)) {
                    nodes.push(n);
                }
            }
        }
        for start in nodes {
            let mut cur = start;
            for _ in 0..=edges.len() {
                match edges.iter().find(|(p, _)| same(*p, cur)) {
                    Some((_, next)) => {
                        if same(*next, start) {
                            return false;
                        }
                        cur = *next;
                    }
                    None => break,
                }
            }
        }
        true
    }

    /// Every forest of exactly `k` edges over `universe()`, passed to `f`.
    fn for_every_ledger(k: usize, mut f: impl FnMut(&[(Endpoint, Endpoint)])) {
        let u = universe();
        let mut pairs: Vec<(Endpoint, Endpoint)> = Vec::new();
        for p in u.iter() {
            for c in u.iter() {
                if !same(*p, *c) {
                    pairs.push((*p, *c));
                }
            }
        }
        fn rec(
            depth: usize,
            k: usize,
            start: usize,
            pairs: &[(Endpoint, Endpoint)],
            acc: &mut Vec<(Endpoint, Endpoint)>,
            f: &mut impl FnMut(&[(Endpoint, Endpoint)]),
        ) {
            if depth == k {
                if is_forest(acc) {
                    f(acc);
                }
                return;
            }
            for i in start..pairs.len() {
                acc.push(pairs[i]);
                rec(depth + 1, k, i + 1, pairs, acc, f);
                acc.pop();
            }
        }
        let mut acc = Vec::new();
        rec(0, k, 0, &pairs, &mut acc, &mut f);
    }

    /// The ledger capacity the kernel actually deploys: `Ledger<MAX_DELEGATIONS>` with
    /// MAX_DELEGATIONS = 16, revoking into `[Endpoint; MAX_DELEGATIONS]`
    /// (crates/kernel/src/lib.rs:345, :358, :391). The searches below used `Ledger<4>`, which
    /// capped them at 3-edge forests for a structural reason rather than a chosen one: a
    /// 4-slot ledger cannot hold a 5-edge forest at all. Capacity and edge count are therefore
    /// ONE axis, and widening the capacity without the edge count would have changed nothing.
    const DEPLOYED: usize = 16;

    /// Longest forest the searches enumerate.
    ///
    /// What this buys, measured rather than assumed: truncating `revoke_from`'s scan to the
    /// first 4 ledger slots (`for i in 0..4.min(N)`) leaves the 3-edge/`Ledger<4>` suite
    /// entirely GREEN — it cannot notice, because its ledger only ever held 4 entries — and
    /// fails three tests here.
    ///
    /// What it does NOT buy, also measured: capping the revocation fixpoint at two rounds is
    /// caught either way. A first draft of this comment claimed otherwise; the mutant was run
    /// and refuted it.
    const MAX_EDGES: usize = 5;

    fn build(made: &[(Endpoint, Endpoint)]) -> Ledger<DEPLOYED> {
        let mut l: Ledger<DEPLOYED> = Ledger::new();
        for (p, c) in made {
            assert!(l.record(*p, *c));
        }
        l
    }

    #[test]
    fn exhaustive_revoke_reports_exactly_the_closure() {
        // The flagship property. Compared against an INDEPENDENT closure, so a revoke that
        // skips the fixpoint, over-reaches, reports duplicates, includes the root, or reports
        // nothing at all is caught — none of which the previous formulation could see.
        let mut checks = 0u64;
        for k in 0..=MAX_EDGES {
            for_every_ledger(k, |made| {
                for root in universe() {
                    let mut l = build(made);
                    let want = expected_closure(made, root);
                    let mut out = [e(0, 0, 0); DEPLOYED];
                    let n = l.revoke_from(root, &mut out);
                    assert_eq!(
                        n,
                        want.len(),
                        "wrong number reached from {root:?} in {made:?}"
                    );
                    for w in want.iter() {
                        assert!(
                            out[..n].iter().any(|x| same(*x, *w)),
                            "revoke from {root:?} missed {w:?} in {made:?}"
                        );
                    }
                    for got in out[..n].iter() {
                        assert!(
                            want.iter().any(|w| same(*w, *got)),
                            "revoke from {root:?} over-reached to {got:?} in {made:?}"
                        );
                    }
                    checks += 1;
                }
            });
        }
        assert!(checks > 1000, "expected a real search, only did {checks}");
    }

    #[test]
    fn exhaustive_revoke_removes_exactly_the_right_edges() {
        // The surviving ledger must be exactly the edges whose parent was never reached.
        for k in 0..=MAX_EDGES {
            for_every_ledger(k, |made| {
                for root in universe() {
                    let mut l = build(made);
                    let want = expected_closure(made, root);
                    let mut out = [e(0, 0, 0); DEPLOYED];
                    l.revoke_from(root, &mut out);
                    let survivors: Vec<_> = l.live_edges().collect();
                    for (p, c) in made.iter() {
                        let should_die = same(*p, root) || want.iter().any(|w| same(*w, *p));
                        let alive = survivors
                            .iter()
                            .any(|(sp, sc)| same(*sp, *p) && same(*sc, *c));
                        assert_eq!(!alive, should_die, "edge {p:?}->{c:?} in {made:?}");
                    }
                }
            });
        }
    }

    #[test]
    fn exhaustive_splice_preserves_what_an_ancestor_can_reach() {
        // The confirmed defect this crate exists to prevent: teardown CUT chains instead of
        // splicing, so an ancestor's revoke silently missed grandchildren.
        for k in 0..=MAX_EDGES {
            for_every_ledger(k, |made| {
                for dying in [(0usize, A), (0, D), (1, B), (2, C)] {
                    for root in universe() {
                        if root.proc == dying.0 && root.id == dying.1 {
                            continue; // the root itself is gone
                        }
                        let want: Vec<_> = expected_closure(made, root)
                            .into_iter()
                            .filter(|t| !(t.proc == dying.0 && t.id == dying.1))
                            .collect();
                        let mut l = build(made);
                        l.splice_out(dying.0, dying.1);
                        let mut out = [e(0, 0, 0); DEPLOYED];
                        let n = l.revoke_from(root, &mut out);
                        for w in want {
                            assert!(
                                out[..n].iter().any(|x| same(*x, w)),
                                "splice_out({dying:?}) lost {w:?} from {root:?} in {made:?}"
                            );
                        }
                    }
                }
            });
        }
    }

    #[test]
    fn exhaustive_no_edge_ever_names_a_dead_incarnation() {
        // After a process dies, no live edge may name that INCARNATION at either end — and
        // edges naming a different incarnation of the same slot must be left alone.
        for k in 0..=MAX_EDGES {
            for_every_ledger(k, |made| {
                for dying in [(0usize, A), (0, D), (1, B), (2, C)] {
                    let mut l = build(made);
                    l.splice_out(dying.0, dying.1);
                    for (p, c) in l.live_edges() {
                        assert!(
                            !(p.proc == dying.0 && p.id == dying.1),
                            "edge from a dead proc"
                        );
                        assert!(
                            !(c.proc == dying.0 && c.id == dying.1),
                            "edge into a dead proc"
                        );
                    }
                    // Nothing that named only OTHER incarnations may have been dropped.
                    for (p, c) in made.iter() {
                        let touches = (p.proc == dying.0 && p.id == dying.1)
                            || (c.proc == dying.0 && c.id == dying.1);
                        if !touches {
                            let alive = l.live_edges().any(|(sp, sc)| same(sp, *p) && same(sc, *c));
                            assert!(alive, "splice dropped unrelated edge {p:?}->{c:?}");
                        }
                    }
                }
            });
        }
    }

    #[test]
    fn exhaustive_forget_drops_exactly_the_named_endpoint() {
        for k in 0..=MAX_EDGES {
            for_every_ledger(k, |made| {
                for target in universe() {
                    let mut l = build(made);
                    l.forget(target);
                    for (p, c) in made.iter() {
                        let names = same(*p, target) || same(*c, target);
                        let alive = l.live_edges().any(|(sp, sc)| same(sp, *p) && same(sc, *c));
                        assert_eq!(alive, !names, "forget({target:?}) on {p:?}->{c:?}");
                    }
                }
            });
        }
    }

    #[test]
    fn exhaustive_operations_never_invent_or_lose_edges() {
        for k in 0..=MAX_EDGES {
            for_every_ledger(k, |made| {
                for root in universe() {
                    let mut l = build(made);
                    let mut out = [e(0, 0, 0); DEPLOYED];
                    let n = l.revoke_from(root, &mut out);
                    assert!(n <= made.len(), "revoke reported more edges than exist");
                    assert_eq!(l.len() + n, made.len(), "edges appeared or vanished");
                }
            });
        }
    }

    #[test]
    fn a_cycle_terminates() {
        // Not reachable through the kernel's API (see `splice_out`'s precondition), but the
        // ledger must not hang if one is ever built: the fixpoint stops when a round marks
        // nothing new.
        let mut l: Ledger<4> = Ledger::new();
        l.record(e(0, A, 0), e(1, B, 0));
        l.record(e(1, B, 0), e(0, A, 0));
        let mut out = [e(0, 0, 0); DEPLOYED];
        let n = l.revoke_from(e(0, A, 0), &mut out);
        assert_eq!(n, 2);
        assert!(l.is_empty());
    }

    /// How many edges the insertion-ORDER search permutes. Bounded below `MAX_EDGES` because
    /// the cost is forests x k! x roots; at 3 that is ~188k ledger builds, at 4 it is ~8M.
    const ORDER_EDGES: usize = 3;

    /// Every live edge, as raw field tuples rather than `Endpoint`s.
    ///
    /// Deliberately NOT compared through `Endpoint`'s own equality: writing assertions in
    /// terms of the thing under test is how `same_proc` once became untestable, and the same
    /// hazard applies to any derived comparison this crate might get wrong.
    fn live_edges<const M: usize>(l: &Ledger<M>) -> Vec<(usize, u64, usize, usize, u64, usize)> {
        let mut v: Vec<_> = l
            .edges
            .iter()
            .filter(|e| e.live)
            .map(|e| {
                (
                    e.parent.proc,
                    e.parent.id,
                    e.parent.cap,
                    e.child.proc,
                    e.child.id,
                    e.child.cap,
                )
            })
            .collect();
        v.sort();
        v
    }

    fn ends(out: &[Endpoint]) -> Vec<(usize, u64, usize)> {
        let mut v: Vec<_> = out.iter().map(|x| (x.proc, x.id, x.cap)).collect();
        v.sort();
        v
    }

    fn permutations(items: &[(Endpoint, Endpoint)]) -> Vec<Vec<(Endpoint, Endpoint)>> {
        fn rec(
            k: usize,
            cur: &mut Vec<(Endpoint, Endpoint)>,
            out: &mut Vec<Vec<(Endpoint, Endpoint)>>,
        ) {
            if k == cur.len() {
                out.push(cur.clone());
                return;
            }
            for i in k..cur.len() {
                cur.swap(k, i);
                rec(k + 1, cur, out);
                cur.swap(k, i);
            }
        }
        let mut out = Vec::new();
        let mut cur = items.to_vec();
        rec(0, &mut cur, &mut out);
        out
    }

    /// INSERTION ORDER — the last axis these searches held constant.
    ///
    /// `for_every_ledger` emits each edge set in one fixed ascending order and `build`
    /// records in that order, so until now every forest occupied exactly ONE array layout.
    /// `record` fills the first free slot and performs no validation, so a different
    /// insertion order is the same forest in a different layout — and both operations below
    /// walk the array BY INDEX. `splice_out` is the reason to care: it is a single forward
    /// pass that reparents edges it may already have walked past, so if any outcome depended
    /// on layout, that is where it would.
    ///
    /// The claim is that no outcome does. Reasoning says an edge's `child` never changes, so
    /// a forward pass visits every candidate whatever the order — but that argument is
    /// exactly what a search is for, and a prior review could not turn it into a divergence
    /// by hand either. This makes it falsifiable rather than argued.
    ///
    /// RESULT, stated plainly because it is not the flattering one: no divergence exists, and
    /// this test CLOSED NO GAP. Three real order-dependence mutants were run — reparenting
    /// only forward (`j in i..N`), only backward (`j in 0..i`), and stopping after the first
    /// match — and every one is caught by tests that already existed. A fourth, reversing the
    /// outer pass, is caught by nothing, which is correct: it is not a bug.
    ///
    /// The reason is worth keeping. Insertion order was flagged as an unexplored axis, but it
    /// is not INDEPENDENT of the axis already explored: varying which edges a forest contains
    /// already varies which edge lands in slot 0, so enumerating 2611 forests was producing
    /// layout diversity the whole time. An axis is only unexplored if the search cannot reach
    /// it, not merely if no loop is named after it.
    ///
    /// Retained anyway, at 0.5s, because it states the layout-independence invariant DIRECTLY
    /// rather than as a by-product — an implementation that later assumed a sorted or
    /// compacted array is the case the forest enumeration might not reach. It is a statement
    /// of intent, not evidence of coverage, and should not be counted as the latter.
    #[test]
    fn exhaustive_outcome_is_independent_of_insertion_order() {
        let mut checks = 0u64;
        for k in 0..=ORDER_EDGES {
            for_every_ledger(k, |made| {
                let perms = permutations(made);
                for victim in universe() {
                    let want = {
                        let mut l = build(made);
                        l.splice_out(victim.proc, victim.id);
                        live_edges(&l)
                    };
                    for p in perms.iter() {
                        let mut l = build(p);
                        l.splice_out(victim.proc, victim.id);
                        assert_eq!(
                            live_edges(&l),
                            want,
                            "splice_out depends on insertion order: {made:?} vs {p:?}"
                        );
                        checks += 1;
                    }
                }
                for root in universe() {
                    let (want_out, want_live) = {
                        let mut l = build(made);
                        let mut o = [e(0, 0, 0); DEPLOYED];
                        let n = l.revoke_from(root, &mut o);
                        (ends(&o[..n]), live_edges(&l))
                    };
                    for p in perms.iter() {
                        let mut l = build(p);
                        let mut o = [e(0, 0, 0); DEPLOYED];
                        let n = l.revoke_from(root, &mut o);
                        assert_eq!(
                            ends(&o[..n]),
                            want_out,
                            "revoke closure depends on insertion order: {made:?} vs {p:?}"
                        );
                        assert_eq!(
                            live_edges(&l),
                            want_live,
                            "revoke residue depends on insertion order: {made:?} vs {p:?}"
                        );
                        checks += 1;
                    }
                }
            });
        }
        assert!(checks > 1000, "expected a real search, only did {checks}");
    }
}
