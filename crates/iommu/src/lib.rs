#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
//! iommu — the containment logic behind an `IommuDomain` capability.
//!
//! **THIS IS NOT AN IOMMU DRIVER, AND THIS CRATE TOUCHES NO HARDWARE.** It writes no Device
//! Table Entry, pokes no register, and issues no invalidation. A crate in this repo was
//! deleted on 2026-08-12 for calling itself "VERIFIED TCB (+ Kani for register pokes) … the
//! DMA-reach CRUX proof" over seven lines of doc comment and no code, so the distinction is
//! worth stating first rather than last: what is here is the *decision* half — which frames a
//! device domain may reach, and whether the mappings it holds are covered by the capabilities
//! that authorized them. The *effect* half — programming AMD-Vi so the silicon agrees — does
//! not exist.
//!
//! # The property
//!
//! docs/nucleus-design.md states the crux as `device_reachable(dom) ⊆ granted(dom)`: every
//! address the I/O page tables let a device reach must be covered by a capability that
//! authorized it, with rights no wider than that capability's. [`Domain::contained`] is that
//! predicate, and every operation here preserves it.
//!
//! The reason it is worth extracting rather than writing inline is the same reason `deleg`,
//! `regions` and `runstate` were extracted: the property is decidable over small state, so it
//! can be searched EXHAUSTIVELY on the host, and the sequences that break it are exactly the
//! ones a scripted boot would never think to try.
//!
//! # Why revocation is the interesting operation
//!
//! Granting and mapping are easy to get right. The defect this exists to prevent is a grant
//! that goes away while a mapping survives it — a device that keeps DMAing into memory whose
//! authorization was revoked, which is the reclaim/stale-IOTLB hazard (V5). So [`Domain::revoke`]
//! withdraws the mappings BEFORE the grant, in that order, mirroring `regions::Plan`'s
//! unmap-before-release discipline.
//!
//! That ordering is what makes [`Domain::contained`] non-vacuous rather than true by
//! construction: an implementation that dropped the grant and left the mapping would violate
//! it, and the exhaustive search below is what notices.

use abi::CapRights;

/// A frame the domain's capabilities authorize, and the rights they confer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grant {
    pub live: bool,
    /// Physical frame number. `0` is a legal frame here; `live` decides occupancy.
    pub frame: u64,
    pub rights: CapRights,
}

impl Grant {
    pub const EMPTY: Grant = Grant {
        live: false,
        frame: 0,
        rights: CapRights::NONE,
    };
}

/// An address the I/O page tables translate for the device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mapping {
    pub live: bool,
    /// Device virtual address (IOVA), page-numbered.
    pub iova: u64,
    pub frame: u64,
    pub rights: CapRights,
}

impl Mapping {
    pub const EMPTY: Mapping = Mapping {
        live: false,
        iova: 0,
        frame: 0,
        rights: CapRights::NONE,
    };
}

/// Why a `map` was refused. A refusal is the interesting outcome, so it is named.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapErr {
    /// No live grant covers the frame — the device may not reach it at all.
    NotGranted,
    /// A grant covers the frame but confers less than was asked for.
    RightsExceedGrant,
    /// That IOVA already translates. Re-pointing it silently would let a device keep an
    /// address whose meaning changed underneath it.
    IovaInUse,
    /// No free mapping slot.
    Full,
}

/// One device's DMA domain: what it is allowed to reach, and what it can currently reach.
///
/// `G` grants and `M` mappings, both fixed-capacity and heap-free, like every other table in
/// this nucleus.
pub struct Domain<const G: usize, const M: usize> {
    grants: [Grant; G],
    maps: [Mapping; M],
}

impl<const G: usize, const M: usize> Default for Domain<G, M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const G: usize, const M: usize> Domain<G, M> {
    pub const fn new() -> Self {
        Domain {
            grants: [Grant::EMPTY; G],
            maps: [Mapping::EMPTY; M],
        }
    }

    /// The rights a live grant confers on `frame`, if any.
    pub fn granted(&self, frame: u64) -> Option<CapRights> {
        self.grants
            .iter()
            .find(|g| g.live && g.frame == frame)
            .map(|g| g.rights)
    }

    /// Authorize `frame` with `rights`.
    ///
    /// Re-granting a frame already granted REPLACES its rights rather than adding a second
    /// entry, so `granted` cannot depend on which duplicate is found first. Narrowing a grant
    /// this way must not leave a wider mapping behind, so any mapping the new rights no longer
    /// cover is withdrawn — the same ordering `revoke` uses, for the same reason.
    pub fn grant(&mut self, frame: u64, rights: CapRights) -> bool {
        if let Some(i) = self.grants.iter().position(|g| g.live && g.frame == frame) {
            self.grants[i].rights = rights;
            self.withdraw_uncovered(frame, rights);
            return true;
        }
        match self.grants.iter().position(|g| !g.live) {
            Some(i) => {
                self.grants[i] = Grant {
                    live: true,
                    frame,
                    rights,
                };
                true
            }
            None => false,
        }
    }

    /// Withdraw the authorization for `frame`.
    ///
    /// Mappings go FIRST. A grant that disappears while the I/O page tables still translate to
    /// its frame is a device with authority nobody issued — the stale-mapping hazard this
    /// module exists to prevent. Returns how many mappings were withdrawn.
    pub fn revoke(&mut self, frame: u64) -> usize {
        let mut n = 0;
        for m in self.maps.iter_mut() {
            if m.live && m.frame == frame {
                *m = Mapping::EMPTY;
                n += 1;
            }
        }
        for g in self.grants.iter_mut() {
            if g.live && g.frame == frame {
                *g = Grant::EMPTY;
            }
        }
        n
    }

    /// Install an I/O translation, if the capabilities authorize it.
    pub fn map(&mut self, iova: u64, frame: u64, rights: CapRights) -> Result<(), MapErr> {
        let Some(have) = self.granted(frame) else {
            return Err(MapErr::NotGranted);
        };
        if !have.contains(rights) {
            return Err(MapErr::RightsExceedGrant);
        }
        if self.maps.iter().any(|m| m.live && m.iova == iova) {
            return Err(MapErr::IovaInUse);
        }
        match self.maps.iter().position(|m| !m.live) {
            Some(i) => {
                self.maps[i] = Mapping {
                    live: true,
                    iova,
                    frame,
                    rights,
                };
                Ok(())
            }
            None => Err(MapErr::Full),
        }
    }

    /// Remove an I/O translation. Removing what is not there is a no-op, not an error.
    pub fn unmap(&mut self, iova: u64) -> bool {
        match self.maps.iter().position(|m| m.live && m.iova == iova) {
            Some(i) => {
                self.maps[i] = Mapping::EMPTY;
                true
            }
            None => false,
        }
    }

    /// Every live mapping, as (iova, frame, rights).
    pub fn reachable(&self) -> impl Iterator<Item = (u64, u64, CapRights)> + '_ {
        self.maps
            .iter()
            .filter(|m| m.live)
            .map(|m| (m.iova, m.frame, m.rights))
    }

    /// Plant a mapping straight into slot `i`, bypassing every check.
    ///
    /// Tests only, and load-bearing. [`Domain::contained`] exists to DETECT a state the public
    /// API cannot produce, so it is only ever called where the answer should be `true` — which
    /// made `fn contained() -> bool { true }` pass all 221 tests in the tree, the exhaustive
    /// 21,952-sequence search included. A checker that cannot be shown to reject anything is
    /// not a checker. Producing the bad state is the only way to test the thing.
    #[cfg(test)]
    pub fn force_mapping(&mut self, i: usize, iova: u64, frame: u64, rights: CapRights) {
        self.maps[i] = Mapping {
            live: true,
            iova,
            frame,
            rights,
        };
    }

    /// THE INVARIANT: `device_reachable ⊆ granted`.
    ///
    /// Every address the device can reach is covered by a live grant, with rights no wider
    /// than that grant confers. Checked after every operation by the exhaustive search below,
    /// and cheap enough for the kernel to assert at a quiescent point.
    pub fn contained(&self) -> bool {
        self.maps.iter().filter(|m| m.live).all(|m| {
            self.granted(m.frame)
                .is_some_and(|have| have.contains(m.rights))
        })
    }

    /// Live grant count, for the caller's own bookkeeping assertions.
    pub fn grant_count(&self) -> usize {
        self.grants.iter().filter(|g| g.live).count()
    }

    /// Live mapping count.
    pub fn mapping_count(&self) -> usize {
        self.maps.iter().filter(|m| m.live).count()
    }

    fn withdraw_uncovered(&mut self, frame: u64, rights: CapRights) {
        for m in self.maps.iter_mut() {
            if m.live && m.frame == frame && !rights.contains(m.rights) {
                *m = Mapping::EMPTY;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: CapRights = CapRights::READ;
    const W: CapRights = CapRights::WRITE;
    const RW: CapRights = CapRights(0b011);
    const NONE: CapRights = CapRights::NONE;

    /// Deployed shape, so the searches run at the width the kernel would use rather than a
    /// convenient one. Kept small deliberately: the point is exhaustion, not scale.
    const NG: usize = 3;
    const NM: usize = 3;

    fn dom() -> Domain<NG, NM> {
        Domain::new()
    }

    #[test]
    fn a_frame_nobody_granted_cannot_be_mapped() {
        let mut d = dom();
        assert_eq!(d.map(0, 7, R), Err(MapErr::NotGranted));
        assert_eq!(d.mapping_count(), 0);
        assert!(d.contained());
    }

    #[test]
    fn a_mapping_cannot_carry_more_rights_than_its_grant() {
        let mut d = dom();
        assert!(d.grant(7, R));
        assert_eq!(d.map(0, 7, W), Err(MapErr::RightsExceedGrant));
        assert_eq!(d.map(0, 7, RW), Err(MapErr::RightsExceedGrant));
        assert!(d.map(0, 7, R).is_ok());
        assert!(d.contained());
    }

    #[test]
    fn revoking_a_grant_withdraws_the_mappings_that_rested_on_it() {
        // THE defect this module exists to prevent: a device still translating an address
        // whose authorization is gone.
        let mut d = dom();
        assert!(d.grant(7, RW));
        assert!(d.map(0, 7, RW).is_ok());
        assert!(d.map(1, 7, R).is_ok());
        assert_eq!(d.mapping_count(), 2);
        assert_eq!(d.revoke(7), 2, "revoke must report what it withdrew");
        assert_eq!(d.mapping_count(), 0, "a mapping outlived its grant");
        assert_eq!(d.grant_count(), 0);
        assert!(d.contained());
    }

    #[test]
    fn narrowing_a_grant_withdraws_a_mapping_it_no_longer_covers() {
        let mut d = dom();
        assert!(d.grant(7, RW));
        assert!(d.map(0, 7, RW).is_ok());
        assert!(d.grant(7, R), "re-granting narrows rather than duplicating");
        assert_eq!(d.grant_count(), 1, "re-grant must not add a second entry");
        assert_eq!(
            d.mapping_count(),
            0,
            "a WRITE mapping survived a grant narrowed to READ"
        );
        assert!(d.contained());
    }

    #[test]
    fn an_iova_cannot_be_silently_repointed() {
        let mut d = dom();
        assert!(d.grant(7, RW));
        assert!(d.grant(8, RW));
        assert!(d.map(0, 7, R).is_ok());
        assert_eq!(d.map(0, 8, R), Err(MapErr::IovaInUse));
        // The original still stands.
        assert_eq!(d.reachable().next(), Some((0, 7, R)));
    }

    // ------------------------------------------------------------ exhaustive search
    //
    // Every operation sequence over a small universe, checking the invariant after EVERY step.
    // The universe is deliberately tiny and the depth deliberately real: the sequences that
    // break containment are orderings (grant, map, revoke, re-grant) that no scripted boot
    // would think to try, not large tables.

    const FRAMES: [u64; 2] = [7, 8];
    const IOVAS: [u64; 2] = [0, 1];
    const RIGHTSET: [CapRights; 4] = [NONE, R, W, RW];

    #[derive(Clone, Copy, Debug)]
    enum Op {
        Grant(u64, CapRights),
        Revoke(u64),
        Map(u64, u64, CapRights),
        Unmap(u64),
    }

    fn alphabet() -> alloc_vec::Vec<Op> {
        let mut v = alloc_vec::Vec::new();
        for f in FRAMES {
            for r in RIGHTSET {
                v.push(Op::Grant(f, r));
            }
            v.push(Op::Revoke(f));
        }
        for i in IOVAS {
            for f in FRAMES {
                for r in RIGHTSET {
                    v.push(Op::Map(i, f, r));
                }
            }
            v.push(Op::Unmap(i));
        }
        v
    }

    /// `contained` must REJECT, and from any slot.
    ///
    /// Two mutants motivate this: `contained() { true }` passed every test in the repository,
    /// and `contained()` truncated to the first two slots passed them too. Both are alive
    /// against a suite that only ever asks the question where the answer is yes.
    #[test]
    fn contained_rejects_an_ungranted_frame_in_every_slot() {
        const NM: usize = 8;
        for slot in 0..NM {
            let mut d: Domain<48, NM> = Domain::new();
            assert!(d.grant(500, RW));
            assert!(d.map(0, 500, RW).is_ok());
            assert!(d.contained(), "the legitimate state must pass");
            d.force_mapping(slot, 900 + slot as u64, 777, RW);
            assert!(
                !d.contained(),
                "a frame nobody granted, mapped in slot {slot}, went unnoticed"
            );
        }
    }

    /// The other half of the invariant: rights no wider than the grant confers.
    #[test]
    fn contained_rejects_rights_wider_than_the_grant_in_every_slot() {
        const NM: usize = 8;
        for slot in 0..NM {
            let mut d: Domain<48, NM> = Domain::new();
            assert!(d.grant(500, R));
            d.force_mapping(slot, 10 + slot as u64, 500, RW);
            assert!(
                !d.contained(),
                "a WRITE mapping over a READ-only grant in slot {slot} went unnoticed"
            );
        }
    }

    /// The DEPLOYED shape, with the violation in the LAST slot.
    ///
    /// The exhaustive search runs `Domain<3, 3>` and never fills more than two of either
    /// table, while the kernel runs `Domain<48, 8>` and its boot assertion calls `contained`.
    /// Every scan in this module was therefore only ever exercised at index 0 and 1: a scan
    /// that stopped early — `.take(2)` — passed all 21,952 sequences, and would have been
    /// blind to a mapping that outlived its grant in any slot from 2 up.
    #[test]
    fn every_scan_reaches_the_last_deployed_slot() {
        const NG_DEPLOYED: usize = 48;
        const NM_DEPLOYED: usize = 8;
        let mut d: Domain<NG_DEPLOYED, NM_DEPLOYED> = Domain::new();

        // Fill every mapping slot, and a grant slot for each.
        for i in 0..NM_DEPLOYED as u64 {
            assert!(d.grant(100 + i, RW), "grant {i} did not take");
            assert!(d.map(i, 100 + i, RW).is_ok(), "map {i} did not take");
        }
        assert!(d.contained());
        assert_eq!(d.reachable().count(), NM_DEPLOYED);

        // `granted` must see the LAST grant: a short scan reports NotGranted.
        assert_eq!(d.granted(100 + NM_DEPLOYED as u64 - 1), Some(RW));
        // `map` must see the LAST mapping when rejecting a duplicate IOVA.
        assert_eq!(
            d.map(NM_DEPLOYED as u64 - 1, 100, RW),
            Err(MapErr::IovaInUse)
        );
        // `revoke` must withdraw a mapping held in the LAST slot, not just the early ones.
        let last = 100 + NM_DEPLOYED as u64 - 1;
        assert_eq!(
            d.revoke(last),
            1,
            "revoke did not reach the last mapping slot"
        );
        assert!(
            d.contained(),
            "a mapping outlived its grant in the last slot"
        );
        assert_eq!(d.reachable().count(), NM_DEPLOYED - 1);
        // `unmap` must reach it too.
        assert!(d.unmap(NM_DEPLOYED as u64 - 2));
    }

    /// The same width question on the GRANT table, which is six times wider than the map table.
    #[test]
    fn the_last_grant_slot_is_usable_and_enforced() {
        const NG_DEPLOYED: usize = 48;
        let mut d: Domain<NG_DEPLOYED, 8> = Domain::new();
        for i in 0..NG_DEPLOYED as u64 {
            assert!(d.grant(200 + i, RW), "grant slot {i} unusable");
        }
        assert!(!d.grant(999, RW), "a full grant table must refuse");
        // A frame parked in the very last grant slot must still authorize a mapping...
        let last = 200 + NG_DEPLOYED as u64 - 1;
        assert_eq!(d.granted(last), Some(RW));
        assert!(d.map(0, last, RW).is_ok());
        // ...and narrowing THAT grant must withdraw the mapping it no longer covers.
        assert!(d.grant(last, R));
        assert!(
            d.contained(),
            "narrowing a grant in the last slot left a wider mapping behind"
        );
        assert_eq!(d.reachable().count(), 0);
    }

    // `std` is available under cfg(test); the crate itself is no_std.
    mod alloc_vec {
        pub use std::vec::Vec;
    }

    fn apply(d: &mut Domain<NG, NM>, op: Op) {
        match op {
            Op::Grant(f, r) => {
                d.grant(f, r);
            }
            Op::Revoke(f) => {
                d.revoke(f);
            }
            Op::Map(i, f, r) => {
                let _ = d.map(i, f, r);
            }
            Op::Unmap(i) => {
                d.unmap(i);
            }
        }
    }

    #[test]
    fn exhaustive_containment_survives_every_operation_sequence() {
        let ops = alphabet();
        let mut checks = 0u64;
        // Depth 3 over a 28-symbol alphabet = 21,952 sequences, each checked after every step.
        // (2 frames x (4 rights + revoke) = 10, plus 2 iovas x (2 frames x 4 rights + unmap)
        // = 18. It was documented as 26 symbols / 17,576 for as long as it existed; the count
        // was written down rather than derived, and never recomputed when the alphabet grew.)
        //
        // What this search covers is INTERLEAVING, not width: two frames and two IOVAs mean at
        // most two grants and two mappings are ever live, so no table slot above index 1 is
        // ever occupied no matter how large NG and NM are. Width is covered separately, by the
        // deployed-shape tests below, because a truncated scan passes everything here.
        for a in ops.iter() {
            for b in ops.iter() {
                for c in ops.iter() {
                    let mut d = dom();
                    for op in [*a, *b, *c] {
                        apply(&mut d, op);
                        assert!(
                            d.contained(),
                            "device_reachable outgrew granted after {:?} in {:?}",
                            op,
                            [*a, *b, *c]
                        );
                        checks += 1;
                    }
                }
            }
        }
        assert!(checks > 1000, "expected a real search, only did {checks}");
    }

    #[test]
    fn exhaustive_no_mapping_ever_outlives_its_grant() {
        // Containment says rights are covered. This says something stronger and separate: a
        // frame with NO live grant must have NO live mapping. A single predicate covering both
        // would let one hold vacuously while the other failed.
        let ops = alphabet();
        let mut checks = 0u64;
        for a in ops.iter() {
            for b in ops.iter() {
                for c in ops.iter() {
                    let mut d = dom();
                    for op in [*a, *b, *c] {
                        apply(&mut d, op);
                        for (iova, frame, _) in d.reachable() {
                            assert!(
                                d.granted(frame).is_some(),
                                "iova {iova:#x} still translates to ungranted frame {frame} \
                                 after {op:?}"
                            );
                        }
                        checks += 1;
                    }
                }
            }
        }
        assert!(checks > 1000, "expected a real search, only did {checks}");
    }
}
