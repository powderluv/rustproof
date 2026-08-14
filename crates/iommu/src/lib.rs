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
        // Depth 3 over a 26-symbol alphabet = 17,576 sequences, each checked after every step.
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
