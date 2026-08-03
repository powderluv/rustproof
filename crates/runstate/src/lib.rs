#![cfg_attr(not(test), no_std)]
//! runstate — what the kernel should do when nothing is runnable, and who an IPC
//! rendezvous pairs with.
//!
//! Pure decisions over the process-state vector, separated from the kernel so they can be
//! checked exhaustively on the host. It performs nothing: it answers *given these states,
//! what happens next?* and the kernel carries it out.
//!
//! This logic earned its own crate. Two confirmed kernel hangs lived here, and both were
//! properties of the state vector alone:
//!
//! * A process parked in `WAIT_IRQ` whose capability was revoked could never be credited
//!   again, but the kernel still treated the machine as *idle waiting for hardware* rather
//!   than finished — so it parked forever. No reclamation, no failure, no output.
//! * The mirror image: a process blocked with no possible counterpart was reported as a
//!   deadlock, failing a boot that had actually completed.
//!
//! The distinction between those two outcomes — park, or declare deadlock — is exactly what
//! [`classify`] computes, and it is decided by nothing more than which slots are live, which
//! are blocked, and whether a blocked waiter can still be credited.

/// Why a process is not runnable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Blocked {
    /// Runnable (or running).
    No,
    /// Blocked sending on an endpoint, waiting for a receiver.
    Send(u64),
    /// Blocked receiving on an endpoint, waiting for a sender.
    Recv(u64),
    /// Blocked waiting for an interrupt line.
    Irq(u64),
}

/// One process-table slot, reduced to what a scheduling decision depends on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slot {
    /// False for a free slot: nothing about it matters.
    pub live: bool,
    pub blocked: Blocked,
}

impl Slot {
    pub const FREE: Slot = Slot {
        live: false,
        blocked: Blocked::No,
    };
    pub const fn ready() -> Slot {
        Slot {
            live: true,
            blocked: Blocked::No,
        }
    }
    pub const fn blocked(b: Blocked) -> Slot {
        Slot {
            live: true,
            blocked: b,
        }
    }
}

/// What the kernel should do when its run queue has emptied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Next {
    /// At least one waiter can still be credited: park the CPU with interrupts enabled and
    /// let the hardware wake it. NOT a failure — this is what an idle machine looks like.
    Park,
    /// Live processes remain, all blocked, and nothing can ever wake them.
    Deadlock,
    /// Every process has exited. A clean finish.
    AllDone,
}

/// Interrupt waiters that can never be credited, and so must be woken with an error instead
/// of waited for.
///
/// Both halves matter, and the kernel has been bitten by each: a waiter may hold no
/// authority for the line any more (its capability was revoked), or the kernel may not
/// deliver that line at all. Either way the event cannot arrive, and a waiter left in that
/// state does not merely stall itself — it makes the machine look idle rather than finished,
/// so the kernel parks for an event that will never come.
pub fn uncreditable<'a>(
    slots: &'a [Slot],
    creditable: &'a dyn Fn(usize, u64) -> bool,
) -> impl Iterator<Item = usize> + 'a {
    slots
        .iter()
        .enumerate()
        .filter_map(move |(i, s)| match s.blocked {
            Blocked::Irq(line) if s.live && !creditable(i, line) => Some(i),
            _ => None,
        })
}

/// What to do when nothing is runnable.
///
/// `creditable(slot, line)` answers whether that waiter could still be credited — it folds
/// together "does it still hold the capability" and "does the kernel deliver that line",
/// both of which are the kernel's business rather than this crate's.
///
/// The caller must first wake everything [`uncreditable`] reports; this function assumes
/// that has happened, which is why an uncreditable waiter is *not* a reason to park.
pub fn classify(slots: &[Slot], creditable: &dyn Fn(usize, u64) -> bool) -> Next {
    let mut any_live = false;
    let mut any_park = false;
    for (i, s) in slots.iter().enumerate() {
        if !s.live {
            continue;
        }
        any_live = true;
        if let Blocked::Irq(line) = s.blocked {
            if creditable(i, line) {
                any_park = true;
            }
        }
    }
    if any_park {
        Next::Park
    } else if any_live {
        Next::Deadlock
    } else {
        Next::AllDone
    }
}

/// The process blocked receiving on `ep`, if any — the peer a sender rendezvous with.
pub fn find_recv(slots: &[Slot], ep: u64) -> Option<usize> {
    slots
        .iter()
        .position(|s| s.live && s.blocked == Blocked::Recv(ep))
}

/// The process blocked sending on `ep`, if any — the peer a receiver rendezvous with.
pub fn find_send(slots: &[Slot], ep: u64) -> Option<usize> {
    slots
        .iter()
        .position(|s| s.live && s.blocked == Blocked::Send(ep))
}

/// Is this state vector one the kernel is allowed to be in?
///
/// The load-bearing part is the endpoint rule: **no endpoint may have a blocked sender and a
/// blocked receiver at the same time.** The kernel maintains it by construction — `SEND`
/// blocks only when no receiver is waiting and `RECV` blocks only when no sender is, and
/// nothing re-runs the matcher outside those two calls — so a state that breaks it is one
/// the matcher will never resolve: both sides wait forever and the boot ends reporting a
/// deadlock that an unprivileged process caused.
///
/// A design that grafted capability transfer onto the IPC rendezvous was rejected for
/// exactly this: a transfer that failed to validate would have bounced a receiver past a
/// stale sender, leaving both parked on one endpoint with nothing able to pair them.
pub fn well_formed(slots: &[Slot]) -> bool {
    for s in slots.iter() {
        if !s.live {
            continue;
        }
        if let Blocked::Send(ep) = s.blocked {
            if find_recv(slots, ep).is_some() {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const YES: &dyn Fn(usize, u64) -> bool = &|_, _| true;
    const NO: &dyn Fn(usize, u64) -> bool = &|_, _| false;

    #[test]
    fn all_exited_is_a_clean_finish() {
        assert_eq!(classify(&[Slot::FREE; 4], YES), Next::AllDone);
    }

    #[test]
    fn a_creditable_waiter_parks_rather_than_failing() {
        let s = [Slot::blocked(Blocked::Irq(0)), Slot::FREE];
        assert_eq!(classify(&s, YES), Next::Park);
    }

    #[test]
    fn an_uncreditable_waiter_is_not_a_reason_to_park() {
        // The confirmed hang: this state used to park forever. The waiter must be woken
        // (see `uncreditable`), and if it is the only thing left the machine is deadlocked,
        // not idle.
        let s = [Slot::blocked(Blocked::Irq(0)), Slot::FREE];
        assert_eq!(classify(&s, NO), Next::Deadlock);
        assert_eq!(uncreditable(&s, NO).collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn uncreditable_ignores_free_slots_and_other_blocks() {
        let s = [
            Slot::FREE,
            Slot::blocked(Blocked::Send(1)),
            Slot::blocked(Blocked::Irq(0)),
        ];
        assert_eq!(uncreditable(&s, NO).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn blocked_on_ipc_with_no_peer_is_a_deadlock_not_a_park() {
        let s = [Slot::blocked(Blocked::Send(0)), Slot::FREE];
        assert_eq!(classify(&s, YES), Next::Deadlock);
    }

    #[test]
    fn a_free_slot_never_makes_a_decision() {
        // A free slot's `blocked` field is meaningless; a stale value must not be read.
        let stale_irq = Slot {
            live: false,
            blocked: Blocked::Irq(0),
        };
        let stale_recv = Slot {
            live: false,
            blocked: Blocked::Recv(0),
        };
        let stale_send = Slot {
            live: false,
            blocked: Blocked::Send(0),
        };
        assert_eq!(classify(&[stale_irq], YES), Next::AllDone);
        assert_eq!(uncreditable(&[stale_irq], NO).count(), 0);
        // Each matcher probed with the stale value IT looks for — probing `find_recv` with a
        // stale `Irq` proves nothing, which is how this check first passed while the liveness
        // test it was written to cover could be deleted.
        assert_eq!(find_recv(&[stale_recv], 0), None);
        assert_eq!(find_send(&[stale_send], 0), None);
    }

    #[test]
    fn matchers_pair_only_the_opposite_direction_on_the_same_endpoint() {
        let s = [
            Slot::blocked(Blocked::Recv(7)),
            Slot::blocked(Blocked::Send(9)),
        ];
        assert_eq!(find_recv(&s, 7), Some(0));
        assert_eq!(find_recv(&s, 9), None);
        assert_eq!(find_send(&s, 9), Some(1));
        assert_eq!(find_send(&s, 7), None);
    }

    #[test]
    fn the_endpoint_rule_rejects_a_pair_nothing_could_resolve() {
        let bad = [
            Slot::blocked(Blocked::Send(3)),
            Slot::blocked(Blocked::Recv(3)),
        ];
        assert!(!well_formed(&bad));
        let ok = [
            Slot::blocked(Blocked::Send(3)),
            Slot::blocked(Blocked::Recv(4)),
        ];
        assert!(well_formed(&ok));
    }

    // ---------------------------------------------------------------- exhaustive checks
    //
    // The state vector is tiny, so every property below is checked over EVERY state a
    // 3-slot table can be in, against every capability predicate that matters. Both hangs
    // this crate exists to prevent were single states in this space.

    /// Every `Slot` value worth distinguishing: free, ready, and each block over two
    /// endpoints / two lines so "same endpoint" and "different endpoint" both occur.
    fn alphabet() -> [Slot; 9] {
        [
            Slot::FREE,
            // A free slot still carrying a stale block. Without this every `s.live` test in
            // the crate is vacuous — `Slot::FREE` is `Blocked::No`, so a matcher that forgot
            // liveness would still never match it. Mutation-testing found exactly that: the
            // liveness check in `find_recv` could be deleted with the suite green, for the
            // same reason the ledger's identity checks could be — the search universe did
            // not contain the case that distinguishes them.
            Slot {
                live: false,
                blocked: Blocked::Recv(0),
            },
            Slot::ready(),
            Slot::blocked(Blocked::Send(0)),
            Slot::blocked(Blocked::Send(1)),
            Slot::blocked(Blocked::Recv(0)),
            Slot::blocked(Blocked::Recv(1)),
            Slot::blocked(Blocked::Irq(0)),
            Slot::blocked(Blocked::Irq(1)),
        ]
    }

    /// Every 3-slot state vector, passed to `f`.
    fn for_every_state(mut f: impl FnMut(&[Slot; 3])) {
        let a = alphabet();
        for i in a.iter() {
            for j in a.iter() {
                for k in a.iter() {
                    f(&[*i, *j, *k]);
                }
            }
        }
    }

    /// The predicates a kernel can actually present.
    ///
    /// The last two are the point. The kernel's predicate is
    /// `creditable(proc, line) = delivers_irq(line) && holds_irq(proc, line)` — per PROCESS,
    /// because REVOKE strips one process's capability and leaves its neighbours' intact. With
    /// only index-blind predicates the whole suite cannot tell `creditable(i, line)` from
    /// `creditable(0, line)`, so the argument that decides which waiter is unanswerable goes
    /// unchecked — and per-process authority is exactly the axis of the revoked-capability
    /// hang. Both polarities are present so neither "always the first slot" nor "always the
    /// others" can pass.
    fn predicates() -> [&'static dyn Fn(usize, u64) -> bool; 5] {
        [
            YES,
            NO,
            &|_, line| line == 0,
            &|i, line| line == 0 && i == 0,
            &|i, line| line == 0 && i != 0,
        ]
    }

    #[test]
    fn exhaustive_never_parks_for_a_waiter_that_cannot_be_credited() {
        // THE property. Parking on behalf of an event that can never arrive is the hang:
        // no output, no reclamation, no failure — indistinguishable from a working machine.
        let mut n = 0u64;
        for cred in predicates() {
            for_every_state(|s| {
                if classify(s, cred) == Next::Park {
                    let parkable = s.iter().enumerate().any(|(i, sl)| match sl.blocked {
                        Blocked::Irq(line) => sl.live && cred(i, line),
                        _ => false,
                    });
                    assert!(parkable, "parked with nothing creditable: {s:?}");
                }
                n += 1;
            });
        }
        assert!(n > 1000, "expected a real search, only did {n}");
    }

    #[test]
    fn exhaustive_never_reports_deadlock_when_something_can_still_be_woken() {
        // The mirror hang: failing a boot that had actually finished, or that hardware was
        // about to rescue.
        for cred in predicates() {
            for_every_state(|s| {
                if classify(s, cred) == Next::Deadlock {
                    for (i, sl) in s.iter().enumerate() {
                        if let Blocked::Irq(line) = sl.blocked {
                            assert!(
                                !(sl.live && cred(i, line)),
                                "declared deadlock with a creditable waiter: {s:?}"
                            );
                        }
                    }
                    assert!(s.iter().any(|sl| sl.live), "deadlock with nothing alive");
                }
            });
        }
    }

    #[test]
    fn exhaustive_alldone_means_nothing_is_alive() {
        for cred in predicates() {
            for_every_state(|s| {
                if classify(s, cred) == Next::AllDone {
                    assert!(
                        !s.iter().any(|sl| sl.live),
                        "reported a clean finish with a live process: {s:?}"
                    );
                }
            });
        }
    }

    #[test]
    fn exhaustive_every_state_gets_exactly_one_answer() {
        // Total and deterministic: there is no state the kernel can reach for which this
        // returns nothing, and none where it would have to choose.
        for cred in predicates() {
            for_every_state(|s| {
                let a = classify(s, cred);
                let b = classify(s, cred);
                assert_eq!(a, b);
                assert!(matches!(a, Next::Park | Next::Deadlock | Next::AllDone));
            });
        }
    }

    #[test]
    fn exhaustive_uncreditable_reports_exactly_the_unanswerable_waiters() {
        // EXACTLY, in both directions. Reporting too few leaves the kernel parked on an
        // event that cannot arrive; reporting too many wakes a process that was legitimately
        // waiting, handing it NO_CAP for a wait that should still be pending. A property
        // that only checked one direction missed the second entirely.
        for cred in predicates() {
            for_every_state(|s| {
                let got: Vec<usize> = uncreditable(s, cred).collect();
                let want: Vec<usize> = s
                    .iter()
                    .enumerate()
                    .filter(|(i, sl)| match sl.blocked {
                        Blocked::Irq(line) => sl.live && !cred(*i, line),
                        _ => false,
                    })
                    .map(|(i, _)| i)
                    .collect();
                assert_eq!(got, want, "wrong wake set for {s:?}");
            });
        }
    }

    #[test]
    fn exhaustive_waking_the_uncreditable_never_leaves_a_reason_to_park() {
        // The kernel wakes everything `uncreditable` reports and then classifies. After
        // that, a Park decision must rest on a waiter that survived the wake.
        for cred in predicates() {
            for_every_state(|s| {
                let woken: Vec<usize> = uncreditable(s, cred).collect();
                let mut after = *s;
                for &i in &woken {
                    after[i] = Slot::ready();
                }
                if classify(&after, cred) == Next::Park {
                    assert!(
                        after.iter().enumerate().any(|(i, sl)| match sl.blocked {
                            Blocked::Irq(line) => sl.live && cred(i, line),
                            _ => false,
                        }),
                        "parked after waking: {s:?} -> {after:?}"
                    );
                }
                // Waking is idempotent: nothing is left to wake afterwards.
                assert_eq!(uncreditable(&after, cred).count(), 0);
            });
        }
    }

    #[test]
    fn exhaustive_the_matchers_agree_with_the_endpoint_rule() {
        // In a well-formed state, no endpoint has peers on both sides — so a sender never
        // finds a receiver already blocked on its endpoint, which is the invariant that
        // makes the rendezvous resolvable at all.
        for_every_state(|s| {
            if !well_formed(s) {
                return;
            }
            for ep in 0..2u64 {
                assert!(
                    find_send(s, ep).is_none() || find_recv(s, ep).is_none(),
                    "well-formed state has both directions blocked on {ep}: {s:?}"
                );
            }
        });
    }

    #[test]
    fn exhaustive_matchers_are_complete_and_pick_the_lowest_index() {
        // Asserting only "whatever it returns is in the right state" lets a matcher return
        // None whenever it likes, and lets it pick any peer it likes. Both are real: a false
        // None blocks a caller whose partner was waiting (a rendezvous that should have
        // happened does not), and the choice of peer is the kernel's queue discipline. The
        // oracle here is an explicit scan, sharing no code with `position`.
        for_every_state(|s| {
            for ep in 0..2u64 {
                let mut want_recv = None;
                let mut want_send = None;
                for (i, sl) in s.iter().enumerate() {
                    if !sl.live {
                        continue;
                    }
                    if want_recv.is_none() && sl.blocked == Blocked::Recv(ep) {
                        want_recv = Some(i);
                    }
                    if want_send.is_none() && sl.blocked == Blocked::Send(ep) {
                        want_send = Some(i);
                    }
                }
                assert_eq!(find_recv(s, ep), want_recv, "find_recv({ep}) on {s:?}");
                assert_eq!(find_send(s, ep), want_send, "find_send({ep}) on {s:?}");
            }
        });
    }

    #[test]
    fn exhaustive_matchers_only_ever_name_a_live_slot_in_the_right_state() {
        for_every_state(|s| {
            for ep in 0..2u64 {
                if let Some(i) = find_recv(s, ep) {
                    assert!(s[i].live && s[i].blocked == Blocked::Recv(ep));
                }
                if let Some(i) = find_send(s, ep) {
                    assert!(s[i].live && s[i].blocked == Blocked::Send(ep));
                }
            }
        });
    }
}
