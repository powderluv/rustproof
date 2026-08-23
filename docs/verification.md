# docs/verification.md — Verus verification ladder & proof-engineering plan

## DECISION 2026-08-11 — Verus is DECLINED for now, on measured grounds

Nothing in this tree verifies, and after actually running the tool, nothing should yet. Everything
below in this line-item list was measured on 2026-08-11, not estimated.

1. **Verus works here.** Release `0.2026.08.09.92f466f`. It pins **stable 1.97.1**, not a nightly —
   the claim in `rust-toolchain.toml` that the proof track "will pin its own nightly" was false.
   It ships its own driver and ignores this repo's rustup pin entirely.
2. **`no_std` is not an obstacle.** A `no_std` crate verifies with `--no-vstd` (3/3), and `vstd`'s
   `Seq` is usable from `no_std` as well.
3. **Unbounded proofs are within reach.** "insert disturbs no neighbouring slot", over an
   arbitrary-length `Seq` rather than the `N ∈ {2,4,8}` the tests sample, verified 4/4 in about
   twenty lines and two lemmas.
4. **But adoption is not incremental — it is a whole-repo toolchain move.** `rustc 1.95.0` cannot
   load a Verus-built rlib: `error[E0514]: found crate verus_builtin compiled by an incompatible
   version of rustc`. Any crate containing a `verus!{}` block must be built by 1.97.1, and every
   TCB crate links into the bare-metal kernel. There is no feature gate that avoids this; the
   alternative — a second plain-Rust copy behind a `cfg` — is two copies of the TCB, which is the
   drift trap this project has paid for before.
5. **The decisive finding: a Verus pass count is not a measure of content.** Replacing every
   `ensures` clause in a candidate spec with `ensures true` produced *byte-identical* output —
   `21 verified, 0 errors`. "N obligations verified" is exactly the kind of number this project
   calls theatre. Any future adoption must gate on **spec mutation** (corrupt the `ensures`, watch
   it go red), never on a pass count.
6. **The three candidate targets did not survive scrutiny.** `CapSpace<N>` "for all N" is a
   property of the *type*: the tree monomorphises one N (16) and the tests sample {2,4,8}, so the
   real gap is "the deployed N is untested" — a test-matrix line, not a proof. `mm` frame
   conservation was falsified: `free` has four consumers tree-wide, all of them log lines or the
   leak verdict, while allocation reads the *bitmap*; a divergent counter is a wrong log line, not
   a double-mapped page. Proposed mutants for both die under the existing suite already.

**Reversal condition.** Revisit when a property is (a) load-bearing for isolation, (b) beyond
exhaustive search, and (c) *not* already killed by a host mutation. `deleg`'s revocation closure
over arbitrary ledgers is the standing candidate — no one has yet run a spec-mutation test against
it, so it is untried rather than rejected. The first real IO page-table code would also qualify.

What was done instead, the same day: `crates/mm` gained `partition_holds_for_every_dma_top`, which
widens the one axis every other `mm` test held constant. See §0 below.

---

Status of the rest of this document, updated 2026-08-22: **partly built, none of it PROVEN.**
The DMA-containment half is no longer a sketch. `crates/iommu-amdvi` — the seven-line stub this
paragraph used to point at — does not exist; what exists is `crates/iommu` (the authority model,
exhaustively tested) plus AMD-Vi programming in `crates/kernel`: a device-table entry per device,
per-device I/O page tables, a command buffer with invalidation waited to completion, and
default-deny for every function the scan reaches. An untrusted ring-3 process holds a real
bus-mastering device and reaches only what `MAP_DMA` gave it, demonstrated on the rig.

What is still a sketch: everything about SILICON and everything about PROOF. All of the above is
QEMU with an emulated AMD-Vi and a toy DMA device; no gfx1201, no VFIO, no real GPU. And
`toolchain/verus.lock` / `toolchain/fetch-verus.sh` remain placeholders (`REPLACE_ME`, `exit 1`) —
in-tree Verus was DECLINED on 2026-08-11 with a written reversal condition. Nothing below is
machine-PROVEN; the strongest word this tree has earned is TESTED, and for the hardware half,
DEMONSTRATED. Read the milestone framing as intent.

Scope reminder (do not re-litigate): the guarantee we verify is **isolation / DMA-containment**. GPU compute correctness is permanently out of scope. The C++ `lite::` gfx1201 driver and its GPUVM are **untrusted user processes** that emit only IOVAs; the nucleus owns the AMD-Vi IOMMU tables. Under plain VFIO (M0–M2) the *host* programs the physical IOMMU, so the nucleus IOMMU proof is not load-bearing until **M3 (emulated vIOMMU)** and **M4 (bare-metal AMD-Vi)**.

---

## 2026-08-23 — a domain bound to a BDF the unit never consults

A PCI-to-PCI bridge forwards its children's transactions as ITSELF. So for a function behind a
bridge, the requester id the IOMMU sees is not the BDF the scan read out of config space — which
is why Linux has IOMMU groups and `pci_for_each_dma_alias`. The nucleus indexed the device table
by the enumerated BDF and bound domains there, so a domain bound to a behind-bridge device
programmed an entry the unit never consults for it. Measured: a domain bound at `0x0108` while
every translation that device performed arrived under a different id, and the granted IOVA did
not reach its frame. Several functions behind one bridge share a requester id, so "one table per
device" would silently have become one table for all of them.

Fixed by declining: a domain is bound only to a ROOT-BUS device, where requester id and BDF
coincide. Anything behind a bridge is DENIED like every other unbound function — and so is the
bridge, so it reaches nothing, which is the safe direction. An assertion refuses to publish a
domain whose BDF is not a root-bus one, and the bridged rig now MOVES the second DMA-capable
device behind the bridge (with `-net none`, since QEMU's default NIC would otherwise fill the
slot and the question would never arise) so that mutant is caught in CI.

**And the justification I wrote for crossing bridges was wrong.** The entry above dated
2026-08-21 says a DMA-capable function behind a bridge "got no device-table entry, and was
therefore PASSED THROUGH". On this emulator it was not: its requester id aliases to the host
bridge at `00:00.0`, which the bus-0 scan DID enumerate and therefore did deny. The refutation
ran that case at the pre-fix commit and the device reached nothing. Crossing bridges is still
right — the functions need entries, and on hardware whose aliasing differs the passthrough case
is real — but the specific thing I claimed to have closed was not open here. The commit did
something worth doing for a reason that did not hold.

Two smaller consequences, both about assertions that had quietly become claims about the RIG:

- The demo asserted a second device domain exists. How many domains a machine has is a property
  of its device list, so the demo now says when there is only one, and the runner requires the
  second-domain checks only when the boot reports a second domain bound. It also prefers domain 2
  for the mapping left live at exit and falls back to domain 1, so the teardown path is exercised
  on both topologies.
- `find_dma_device` — which picks the one device with a payload oracle — still scans bus 0 only.
  That is now CORRECT rather than an oversight, since only root-bus devices are bindable, but the
  commit message for 602891b said "Three bus-0 scans became one" and there were four.

## 2026-08-22 (fifth) — checking a claim I had just written down

Writing the V5 row forced a question the code had not been asked: is "invalidated to completion"
actually established? `iommu_invalidate` returns whether COMPLETION_WAIT came back, and EVERY
call site discarded it. On a timeout the caller carried on regardless — so the acknowledgement
that is the whole of the reclaim story was hoped for rather than checked, in the one place where
being wrong means frames reissued while the unit still holds a translation for them.

Now counted and asserted: the boot fails on any invalidation the unit did not acknowledge. The
mutant that never observes the acknowledgement reports 24 of them and fails.

Also checked rather than assumed, since the same row claims it: the shutdown walk really does
cover EVERY domain's table, not just domain 1's. A planted present leaf in domain 2 is caught
(`STALE HARDWARE MAPPING … first at IOVA 0x12c000`).

And the CI steps added earlier: `tools/run-qemu.sh` exits nonzero on `RESULT: FAIL`, so those
steps gate rather than decorate — verified by breaking a gated property and reading the exit
code. Worth noting the shape of the result: the traced step failed and the untraced one did NOT,
which is correct, because that property is gated only under `QEMU_TRACE`. A step passing is only
meaningful together with knowing which properties it is in a position to see.

The general point, and the reason this entry exists: rewriting the accounting was not a
documentation task. Two of the sentences I wrote turned out to be claims the code had never been
asked to support, and one of them was false.

## 2026-08-22 (fourth) — none of the containment work ran in CI

CI ran `IOMMU=1` and never `FIRMWARE=1`. Booting through SeaBIOS is what assigns PCI BARs, and
a DMA-capable device needs a BAR before it can be told to transfer — so without it the device
probe bails and every gate it feeds is skipped. Which means the whole containment story
(CONTAINED, TRANSLATED, RIGHTS ENFORCED, UNREACHABLE, REVOKED, PER-DEVICE, DENY WORKS, the real
BAR, ring 3 driving the device, default-deny, and every `dma:` gate) was verified by hand, on
this machine, and by nothing else. Weeks of gates, none of them automated.

Added: the containment rig, the bridged topology, the traced run — the only witness that an
invalidation names the domain it is flushing, since the second device cannot be driven — and a
step asserting that `run-qemu.sh` REFUSES a non-x86 `ARCH` rather than quietly booting x86 with
four gate blocks switched off.

**And CI had never once been green.** Not "was failing recently" — forty runs back, to
2026-08-14, not one success. Every run died on step ONE: `cargo fmt --all --check` against a pin
that says `profile = "minimal"`, which installs no rustfmt. Both date to the initial scaffold.

So the entire DMA/IOMMU arc was pushed against automation that had never reported on it, while I
read my own local runs as though they were the automated signal. The gates were real; the thing
that was supposed to run them was not, and I did not look until now. Naming `rustfmt` in the pin
fixes it everywhere the pin is honoured.

Run 32606071609 is the first green one, and it is green including the three new rigs: the
containment rig, the bridged topology, and the traced run all pass on `ubuntu-latest`, as does
the step asserting a non-x86 `ARCH` is refused. The caveat this entry originally carried — that
the bridged and traced steps were reasoned about rather than run on Ubuntu — is now discharged by
measurement.

## 2026-08-22 (later still) — a gate that fired at random, and a property the boot cannot pin

A `(bug)` line could fire on a healthy boot. `poll_irq(6); poll_irq(7)` then "if not (ticks > 0
and bytes == 0) it leaked" — but `ticks == 0` merely means no timer tick landed between the
preceding blocking wait and the poll, which is a matter of timing. Three distinct facts shared
one message, so the run failed announcing a leak that had not happened.

A gate that fires at random is worse than no gate: it teaches you to discount failures, and every
"all modes PASS" line above was reported against a suite that could cry wolf. The timer is now
waited on rather than polled, and each condition says which one it is. Six consecutive
firmware-rig runs give the same verdict.

**Then the sharper half.** Mutating the kernel so a capability reads a FIXED line — authority
still checked, only the count taken from the wrong place — did NOT trip this assertion. Reading
DRAINS, so a capability wrongly reading the timer finds zero moments after the wait emptied it,
and the check passes for the wrong reason. The property "a capability for one line can never read
or clear another's" is not pinnable from ring 3 for that reason, and the boot comment now says so
instead of implying otherwise.

The draining that makes it untestable from outside makes it trivial from inside. `collect_irq` is
extracted, and a host test loads every line with a distinct count and collects each in turn: a
collector that reads a fixed line, or clears more than one, is caught whichever line is asked
for. The fixed-line mutant that survives the boot fails that test.

Two mutants misled me on the way there, both by tripping an EARLIER assertion than the one under
test, which looks like success and is not. A mutation that fails the run has not necessarily
exercised the check you are aiming at; read which assertion actually fired.

Also fixed: `load_process` reset `shares` for a recycled slot but not the new `dma` table. Stale
records are harmless today — region ids are monotonic, so one names a dead region and the
withdrawal clears it — but that is an argument two hops from the code, and the reset is one line.

## 2026-08-22 (later) — the review's other findings, including one about my own reporting

**`ARCH=riscv64 tools/run-qemu.sh` never ran riscv64.** It built `x86_64-unknown-none` and booted
`qemu-system-x86_64`. The script has no dispatch on `ARCH` at all — the variable exists only in
four gate conditions this DMA arc added, so setting it produced a SECOND x86 run with four gate
blocks silently switched off: weaker than the plain run, and reported as riscv coverage.
`git log -S ARCH` shows the variable arriving with those commits. Every "all five modes PASS"
line in the entries above should be read as four x86 modes and one x86 rerun.

The riscv64 port itself is fine — `tools/run-qemu-riscv.sh` builds `nucleus-riscv`, boots
`qemu-system-riscv64` under OpenSBI, and passes. The port was never in doubt; the EVIDENCE cited
for it was wrong. `run-qemu.sh` now refuses a non-x86 `ARCH` before doing any work, and the dead
conditions are gone.

**A released region leaked its attribution slot.** `Process::dma` records `(region, domain)`, and
`Release` cleared the mappings but not the records — while `UNMAP_DMA` cannot clear them either,
because it resolves the region first and a freed region resolves to nothing. Four map-then-free
cycles filled the table with records for regions that no longer existed and `MAP_DMA` answered
`NO_MEM` for ever after, with nothing mapped anywhere. Reachable from the unprivileged ABI with
capabilities the demo role already holds. The demo now runs six cycles; against the old code it
stops after four.

**Teardown could ignore the domain half of the record.** Everything in the demo mapped into
domain 1, so `(region, domain)` was over-determined and a teardown that looked the domain up
wrongly still withdrew from the right table by luck — the mutant survived every gate. The
mapping deliberately left live at exit is now in the SECOND domain, so getting the domain wrong
leaves domain 2's leaves behind and the shutdown walk finds them.

**"0 still passed through" was satisfied by an enumeration that found nothing.** No functions
examined, none counted, the line printed over an empty set. A machine with an IOMMU has PCI
functions, so finding none means the scan failed — the one case where reporting success is worst.

One process note: the cycle probe was wrong on its first run, in the same way as the last two
probes. It treated "no unit, so MAP_DMA refuses" as failure, and broke every boot without an
IOMMU. A refusal on the FIRST attempt is a different fact from a table that fills up, and the
probe now distinguishes them.

## 2026-08-22 — "0 still passed through" was two bits of a sixty-four-bit word

The default-deny read-back checked `V | TV`. That says an entry EXISTS; it says nothing about
what the entry DOES. The review found two ways to satisfy it while handing every unbound
function unrestricted DMA, and demonstrated both with a payload:

- **Mode 0 is passthrough.** `V | TV | root` without `MODE_3_LEVEL` means translation disabled.
  The boot printed `9 present, 0 still passed through` and `RESULT: PASS`, and a device holding
  that entry wrote a kernel frame no capability granted.
- **A root aimed at a LIVE table.** Point every "deny" entry at domain 1's page table — the one
  ring 3's `MAP_DMA` writes leaves into — and the read-back is just as satisfied.

Two fixes, because the check was weak in two different ways.

**The read-back is now the WHOLE word.** A bound device is compared against its own entry, an
unbound one against the deny word exactly. But note what that alone cannot do: it compares the
table against what the code INTENDED to write, so mutating the intention passes trivially. A
read-back can only catch a store that did not take.

**So the deny entry is now aimed at a device.** Nothing in the boot had ever done that — the
sweep wrote an entry it BELIEVED reached nothing, and no device ever held one. The driveable
device is now pointed at that exact word, the unit invalidated, and the transfer attempted
against the frame `TRANSLATED` had reached moments earlier; then it is put back. Only the
device-table entry changed.

That probe was wrong on its first attempt, in a way worth recording. It aimed only at the
translated IOVA, and the mode-0 mutant PASSED it — because under passthrough an IOVA is a
physical address, so the device wrote to physical `0x1000` and left the watched frame alone.
"Untouched" was true for the wrong reason. It now aims at BOTH the translated IOVA and the
frame's own address, since the two failure directions are each invisible to the other's address.
Both mutants die.

The general lesson, third time in this arc: a check written against one way of being wrong is
silent about the others, and the way to find out is to break the mechanism in each direction and
watch which breakages the check sleeps through.

## 2026-08-21 (fifth) — default-deny was a claim about a FLAT bus

> CORRECTION (2026-08-23): the passthrough this entry claims to have closed was not open on this
> emulator — a behind-bridge function's requester id aliases to the host bridge, which the bus-0
> scan already denied. Crossing bridges remains right for enumeration coverage; see the
> 2026-08-23 entry, which also fixes the unsoundness this change introduced by BINDING domains
> to behind-bridge BDFs.

The sweep that gave every unbound PCI function an empty table enumerated bus 0 and stopped
there. Three separate scans all did. So a DMA-capable function behind a bridge was in none of the
answers, got no device-table entry, and was therefore PASSED THROUGH — the same hole that sweep
had just closed, one topology over.

The rig could not show it, so the rig grew a `BRIDGE=1` mode that puts a DMA-capable function
behind a PCI bridge. With one enumeration that follows bridges:

```
[iommu] 11 PCI function(s) across 2 bus(es), 4 DMA-capable; room to bound 2
[iommu] 9 other function(s) given an EMPTY table; 11 present, 0 still passed through
```

**The read-back could not have caught this, and that is the part worth keeping.** It checks the
functions the scan FOUND, so a walk that stops early reports "0 still passed through" over a set
that excludes exactly what it missed — a check whose subject is chosen by the thing it is
checking. The mutant that stops at bus 0 shows it precisely: `10 PCI function(s) across 1
bus(es), 3 DMA-capable`, with the bridge itself enumerated and the device behind it invisible,
and the read-back still perfectly satisfied.

What moves is the BUS COUNT, so that is what the boot reports and what the bridged rig gates on.
An enumeration that truncates now also refuses to publish any domain at all, rather than
reporting success over the part it managed to see.

Three bus-0 scans became one: the kernel enumerates once, follows every bridge breadth-first
through a bounded worklist (a looping topology cannot run away), and derives from that single
list which devices to bind and which to deny. It also stops probing functions 1..7 of a
single-function device, which the old scans did not.

## 2026-08-21 (fourth) — a proxy published before the property it stood for

Whether ring 3 may hold a real bus-mastering BAR, and whether `MAP_DMA` may hand out an IOVA,
were both decided by "is a domain slot populated". That slot was published where the page tables
were WRITTEN — before the unit was enabled. And the enable can fail: its branch printed a line
and the boot carried on.

So a boot where `CTRL` did not take handed an untrusted process a live bus master and DMA
addresses from a unit that was passing everything through. Demonstrated with two mutations,
publish-early plus never-enable:

```
[proc 2] dma: the device can now reach our region by DMA
[proc 2] dev: mapped the REAL device BAR and read its identification register
```

Domains are now STAGED and committed only once `CTRL` reads back with `IommuEn` set. The same
mutation against the fixed tree gives:

```
[iommu] CTRL write did not take: 0x…1004 — no domain is published, so no process can be
        handed a bus master or a DMA mapping
[proc 2] dma: no IOMMU on this machine, so MAP_DMA refuses to hand out reach
[proc 2] dev: no bounded device on this machine, so no BAR to map
```

The `domain N bound to …` line moved with it, from where the tables were written to after the
commit: "bound" is a statement about a unit that translates, and until that store none of them
did. A log line that runs ahead of the fact it reports is how a proxy gets mistaken for its
property in the first place.

This was the review's remaining PARTIAL finding, and it is the same shape as the arc's others:
the check was real, the thing it checked was not quite the thing claimed.

## 2026-08-21 (later still) — the default was PASSTHROUGH, and most of the bus was outside the claim

A device-table entry with `V = 0` is **passthrough**, not deny. The nucleus programmed entries
for the devices it had domains for, enabled the unit, and left every other function at zero — so
they had UNRESTRICTED DMA while the boot reported containment.

Not a corner case on this rig. Measured, before the change:

```
[iommu] 3 DMA-capable function(s) present; room to bound 2
[iommu] 7 other function(s) given an EMPTY table; 9 present, 7 still passed through
```

Nine PCI functions, two bounded, **seven passed through** — including a third DMA-capable
function the enumeration silently dropped past `MAX_DOMAINS`. Every claim this arc made about
bounding DMA was a claim about the two devices that happened to fit.

Every remaining function now gets a VALID entry pointing at an EMPTY page table, with a DomainID
of its own so a flush for a bounded device never speaks for it: the walk reaches a not-present
level and the transfer is refused. Reaching nothing is the right default for a device nobody has
asked to use.

The check is a READ-BACK, not a count of writes: a store that did not take leaves `V = 0`, which
is the passthrough being removed, and counting writes would report success either way. The boot
now requires `0 still passed through` and the mutant that skips the sweep reports 7 and fails.

This is the same shape as the arc's other findings, one level up. Containment was demonstrated
for the device we can drive, and generalised — silently — to "the nucleus bounds DMA". What the
generalisation skipped over was every device the enumeration never reached.

## 2026-08-21 (later) — the adversarial review of the DMA arc: four real ones

Twenty agents over the seven-commit arc. Four findings survived refutation and were acted on;
the sharpest was a live containment breach reachable from an unprivileged process.

**1. `MAP_DMA`'s rollback stranded hardware mappings — a device reaching reclaimed memory.**
`Domain::revoke(frame)` is frame-scoped and withdraws EVERY model mapping of that frame,
including ones earlier calls installed at other IOVAs, while the rollback zeroed only the single
leaf it was mid-write on. The surviving leaves then became invisible to every withdrawal path in
the tree, because `clear_io_mappings_in` was driven by `domain.reachable()` — the model the
rollback had just emptied. `UNMAP_DMA` returned OK having cleared nothing, `FREE_REGION` returned
the frames to the allocator, and the device could write into a region minted afterwards.
`contained()` stayed true throughout: the model was self-consistent and only the hardware
disagreed, which is precisely the shape a model-only check cannot see.

Reachable from the shipped ABI: `Domain<_, 8>` and `REGION_MAX_PAGES = 4` mean two 4-page
mappings fill the domain exactly, and nothing refuses a re-map of an already-mapped region. The
per-process cap of 4 added earlier the same day does NOT mask it — the domain's 8 slots fill
first. Two independent oracles agreed: the tree's own shutdown scan reported the stale leaves,
and a ring-3 probe drove the real device into a freshly-minted region through them.

Fixed twice over, and measured to be independently sufficient:
  - the rollback now undoes exactly what that call installed, revoking a grant only where no
    mapping still rests on it;
  - `clear_io_mappings_in` is now driven by the TABLE, scanning the level the device actually
    walks, so a model that has forgotten cannot strand anything.
Reverting either one alone still passes; reverting BOTH reproduces the breach. Belt and braces,
stated as such rather than dressed up as one fix with two mutants.

**2. Invalidation named the wrong domain.** Every `iommu_invalidate` hardcoded DomainID 0 and
the first device's BDF, while `program_dte` gives each device-table entry its own DomainID. So
withdrawing from domain 2 cleared its leaves and then flushed domain 1's caches. The emulator's
trace showed it exactly: seven page-invalidations, all `domain 0x0`, seven device-table
invalidations, all `00:05.0`. Now `0x0` ×7 AND `0x1` ×4, both BDFs.

There is no payload oracle for this — the second device cannot be driven from here — so the
witness is the emulator's own report, gated only when `QEMU_TRACE` is set. That is the honest
scope: with tracing off, this property is unverified, and the gate says so by only existing
there.

**3. The rights oracle could not fail.** `wider_refused` attempted the wider mapping at the SAME
IOVA already mapped read-only, so `IovaInUse` refused it whatever its rights were. Deleting
`Domain::map`'s rights check left all five modes PASS with byte-identical output — "wider rights
refused" and "RIGHTS ENFORCED" included. The attempt now uses a fresh IOVA, and the device is
aimed at it, so the same mutant fails with `(bug) WRITE-THROUGH: a READ-only mapping accepted a
device write`.

**4. The `stray` witness was computed and discarded.** `let _ = stray` threw away the one value
that separates "the device REFUSED it" from "the command never ran" — the device reports
completion in its own register either way. The end-to-end assertion now requires it.

Also corrected: §0's `iommu` row still said "there is no hardware half — nothing maps", which
this arc is precisely what closed.

Refuted, and worth recording as such: a claimed vacuity in the rights→PTE derivation (the mutant
survives but is EQUIVALENT — every call site passes the same rights, so both versions emit
identical words), and a claimed MAP_BAR index defect (real but vestigial: edu exposes one BAR).

## 2026-08-21 — DMA reach was the one authority revocation did not withdraw

`revoke_delegations` states its doctrine three times in its own body — "a capability going away
must take the AUTHORITY it conferred with it, not merely the slot" — and withdraws the MMIO
device window, shared-region CPU mappings, and interrupt credits. It said nothing about DMA,
which is the newest and by far the most powerful authority in the tree: a bus master writing
memory directly. Revoking a `Region` capability tore down the holder's CPU window and left the
device's reach to those same frames.

Underneath that: `MAP_DMA` recorded nothing about who asked. Mappings were ANONYMOUS, so nothing
could withdraw them per process even in principle. Process teardown appeared to handle it only
because a process happens to OWN the regions it maps here — destroying those regions clears their
entries as a side effect. Map a BORROWED region and the reach outlives the process. That is the
"works by luck" shape this project keeps finding in itself.

Fixed by attribution: `Process::dma` records `(region, domain)` per mapping, `MAP_DMA` REFUSES
rather than install one it cannot track, and one withdrawal routine now serves `UNMAP_DMA`,
process teardown and `REVOKE`. Teardown withdraws what the process asked for, whoever owns the
memory; `REVOKE` withdraws any mapping whose holder no longer has both the `Region` and the
`IommuDomain` capability it was made through.

**Reachability, stated plainly.** The borrowed-region case cannot be reached today: `SPAWN`
delegates exactly ONE capability, a spawned process's role grants nothing, and `MAP_DMA` needs
TWO — so no process can both hold the pair and be a revocation target. This is therefore
HARDENING, not a live defect, and it is not claimed as one. What it buys is that the property
holds by construction instead of by an ownership coincidence, and that the doctrine no longer has
an exception carved out for its most dangerous member.

The path is shown to RUN rather than merely to exist: the demo deliberately exits still holding a
mapping, and the boot reports `teardown withdrew 1 DMA mapping(s) by attribution`. Removing the
teardown call takes that to 0 and fails the run — a count is asserted, not the absence of a
complaint, because "the probe did not run" and "the path does not work" look identical from
outside.

One consequence worth recording: DMA mappings are now capped per process at `SHARE_SLOTS` (4),
because an untracked mapping is one nothing can withdraw. `MAP_DMA` returns `NO_MEM` past that.

## 2026-08-20 — an untrusted process DRIVES the device, and reaches only what it was granted

The whole story, end to end, in one process that holds no privilege of any kind:

```
[proc 2] dev: mapped the REAL device BAR and read its identification register
         dev: WE drove the device and our data came back through the IOVA we were granted
         dev: a transfer aimed at an IOVA we were never granted changed nothing we own
```

It creates a region, asks `MAP_DMA` for an IOVA, writes a pattern through its own CPU mapping,
programs the device's source/destination/count/command registers to move those bytes out to the
device and back, clobbers its copy first so anything that returns must have come from the device,
and reads the pattern back. Every address it touches came from a capability it holds.

**What the negative establishes, stated precisely.** A transfer aimed at an IOVA nobody granted
it changed nothing it owns. It is NOT a claim that the device could not reach that memory by some
other route — the process cannot even NAME the memory to try, which is the point. From ring 3 an
address is an IOVA, and the only IOVAs that resolve are the ones `MAP_DMA` handed back.

Containment is exact at page granularity: the mutant that aims the round-trip ONE PAGE past the
single-page grant fails with "the device did not return our data".

The bug worth recording is the observer, not the mechanism. The first version borrowed the
demo's mailbox region and its address — but the demo had already `UNMAP_REGION`'d it, so the
address was stale, and a later `MAP_REGION` handed that freed share slot to a different region.
Both windows became the same page. The transfer had been working the whole time; the check was
reading the wrong memory, and it reported "the device reached a region we never offered it",
which is about as alarming as a false negative gets. Diagnostics printing the two addresses
side by side settled it in one run — `mva == qva`. The test is self-contained now: it makes its
own regions and maps them itself, borrowing no state from the demo around it.

## 2026-08-19 (sixth) — an untrusted process holds a REAL bus-mastering device

`MAP_BAR` mapped a kernel RAM frame with a signature in it. That stand-in was deliberate:
docs/host-contract.md §5 states that a bus-mastering BAR must not reach an untrusted process
until that device's DMA is bounded, because otherwise the process holds a DMA engine that can
write the process table, the capability spaces and the delegation ledger — at which point every
other gate in the contract is advisory. Per-device domains satisfied that precondition, so the
stand-in can go.

A ring-3 process now maps `edu`'s real register aperture through an `Mmio` capability and reads
`0x010000ed` back out of its identification register. A RAM frame cannot forge that. The window
is mapped UNCACHED, which the stand-in never needed and which `Perms::device` existed to express.

The grant table carries a LOGICAL selector resolved at mint time, so the capability the process
holds names the real physical base and delegation, attenuation and `map_device` are unchanged.
(Every `Mmio` grant previously resolved to the stand-in regardless of what the table said, which
made that object decorative — the same shape as the domain object two commits ago.)

**The precondition is now enforced rather than stated, and that was not hypothetical.** QEMU's
default machine carries an e1000, so a boot with NO IOMMU still has a real bus master for the
scan to find — and the first version of this handed it straight over. The capability now
resolves to nothing unless that device has a domain; the default boot reports "no bounded device
on this machine, so no BAR to map", and both directions are gated on whether the boot bound a
domain. The mutant that skips the check fails the no-IOMMU boot.

## 2026-08-19 (fifth) — per-device containment, with both halves on hardware

There was one domain, so "a capability for device A's domain cannot grant reach into device B's"
had nothing to be tested against and was not claimed. There are now two, one per DMA-capable
function, **each with its own I/O page table** — separate tables are what makes this a fact about
the machine rather than bookkeeping, since two devices sharing a table would have identical reach
whatever their models said. The device-table entries carry distinct DomainIDs so the unit's own
caching and invalidation treat them as separate.

The hardware result, driven by `edu` (the device this nucleus can actually drive):

```
[iommu] domain 1 bound to 0x0028 with its own page table 0x1206000
[iommu] domain 2 bound to 0x0010 with its own page table 0x1209000
[iommu] cross-domain: mapped in 2 yes / in 1 yes | through 2 only the frame reads
        0x5e17..(sentinel), once 1 maps it 0xd1ce..
[iommu] PER-DEVICE: a frame mapped in another device's domain stayed UNREACHABLE, and became
        reachable only when this device's own domain mapped it
```

Both halves are the same transfer by the same device with exactly ONE thing changed — which
table the leaf was written into. Without the second half a wall would look like containment: a
device that reaches nothing is not contained, it is broken. The mutant that gives both devices
the same table fails with `CROSS-DOMAIN REACH`.

`domain_lookup` is the pure naming rule, and its test covers the case that keeps recurring: `0`
is what an unclaimed slot carries AND what a zeroed capability names, and that coincidence must
never become authority — over an empty table or a partly filled one.

What the sweeps had to learn: `FREE_REGION` clears mappings across EVERY domain, because a frame
may be mapped by more than one device and the region being destroyed knows nothing about which
domains took it. `UNMAP_DMA` deliberately does NOT — tearing down another device's mapping
through your own capability is the mirror of installing one in its table.

A capability that named domain 2 as "nonexistent" started failing the moment a second device got
a domain. That is the assertion working: it was pinned to a fact that changed. It now names 3,
and the worker also holds a real capability for domain 2, so the second domain is exercised as
authority rather than only as a refusal.

## 2026-08-19 (fourth) — allocating memory stopped being authority to reach it

`MAKE_REGION` granted every page it allocated into the device domain. So every region was
DMA-authorized whether or not anyone had asked, which is authority nobody requested and nobody
could decline — and it made `grant_count` a restatement of the region table rather than a record
of what had been handed out. Grants are now issued by `MAP_DMA`, from the rights of the
capability that asks, and withdrawn by `UNMAP_DMA` and `FREE_REGION`.

That invalidated the boot check, which asserted `grants == the page count of every live region`.
It held only because of the thing being removed, and it could not distinguish a domain holding
the right NUMBER of grants from one holding the right ONES. Replaced with the property that
actually matters, checked frame by frame: **no grant may outlive the memory it names.**
`crates/iommu` gained a `grants()` iterator for it — the mapping side has had `reachable()` since
the beginning while the grant side had only a count, and a count answers "how many", not "which".

**Where the mutants landed is the interesting part.** Restoring the old grant-at-allocation
survived BOTH new shutdown checks:

- not an orphan — the regions granting at allocation are live, so their grants are legitimate;
- not a containment failure — nothing is mapped, and `contained()` only compares mappings with
  grants;
- and a third check added specifically for it, "no grant without a mapping under it", ALSO
  passed: by the time the shutdown checks run every region has been freed and its grants revoked
  along with it, so there is nothing left to find.

Three checks, none of which could see it. What catches it is a TRIPWIRE at the site — an
`assert!` in `MAKE_REGION` that no frame it allocates is already granted. Some properties are
about a moment, not a final state, and a shutdown check is structurally blind to them however
many of them you add.

Operational note: shark-a went unreachable mid-run, and it turns out this Mac runs the full rig
— QEMU 8.2.1 with both `amd-iommu` and `edu`, and SeaBIOS supplies an RSDP on the multiboot
path. All five modes, including `IOMMU=1 FIRMWARE=1`, validate locally. shark-a is a
cross-check, not a dependency.

## 2026-08-19 (later still) — the device kept reaching memory the nucleus had reclaimed

`MAP_DMA` writes real I/O page-table entries. `FREE_REGION` revoked the domain's grant and
returned the frames to the allocator — and never touched the table. So a process that mapped a
region for DMA and then freed it left a PRESENT leaf pointing at a frame on its way back into
the pool, to be reissued to someone else while the device could still write it.

Every check in the tree missed it, and the reason is worth stating plainly: `contained()`
compares the domain's mappings with the domain's own grants. Revoking both at once leaves that
comparison perfectly satisfied. The model agreed with itself while the hardware disagreed with
both, which is the one shape a model-only invariant cannot see.

Measured before fixing, on the rig: a present entry at IOVA `0x100000` naming a frame no grant
covered. Nothing in the ABI obliges a caller to `UNMAP_DMA` first, and a killed process cannot
be relied on to have done anything, so `FREE_REGION` is where it has to close.

Fixed, hardware FIRST — the model is the index that finds the hardware, so revoking it first
would leave the entries with nothing remaining that knows where they are — then the unit is
invalidated. `UNMAP_DMA` and `FREE_REGION` now share one helper so the two cannot drift.

**The new check is the interesting part.** The boot now WALKS THE REAL I/O PAGE TABLE and
requires every present leaf to be covered by a live grant — the hardware analogue of
`contained()`, and the first check here that is not the model talking to itself. A probe in the
demo exercises exactly the case that produced the bug (map for DMA, then free without
unmapping), and the runner requires that probe to have run: the scan only reports a stale entry
if something created one, so a missing probe is a quietly weaker boot rather than a failing one.

## 2026-08-19 (later) — the capability named a domain and the name was ignored

Found by mutating what had just shipped. `caps_iommu_domain` returned the capability's `object`
and `map_dma` discarded it — it did not take a domain parameter at all, and always used the one
global domain. A capability granting DMA reach into **domain 999** mapped into the real domain
and the boot passed. The type half and the rights half of that gate were both real; the thing
the capability actually NAMES was decorative.

Fixed: domains have an identity (`DEVICE_DOMAIN_ID`, a logical id because the role grant tables
are static while the device's BDF is discovered at runtime), set only once a device table entry
AND a table under it exist. `MAP_DMA`/`UNMAP_DMA` take the named domain and refuse otherwise.
The rule is a pure predicate, `domain_named_is_live`, so it is checkable off-target — including
the case that matters most: before any domain exists, NO object names one, and in particular the
`0` that a zeroed slot carries must not become authority by coincidence.

Two refusals are now kept apart because they mean different things: no unit programmed at all is
`NO_MEM` (nothing here could bound DMA), while a domain that exists but is not the one named is
`NO_CAP` (an authority question).

A third under-powered capability is GRANTED to the worker — full rights, naming a domain that
does not exist — for the reason `grants_for` already records: a refusing branch nothing can
reach is not a check. That probe runs only where a domain EXISTS, because on a machine with no
unit every domain capability is refused for that reason alone and the object is never consulted,
which would make the assertion pass without testing anything.

**What this does NOT establish.** There is one domain. The property that matters eventually —
a capability for device A's domain cannot grant reach into device B's — has no second domain to
be tested against, and is not claimed. The rig has two DMA-capable functions, so it is testable
when per-device domains exist; that is the next step, and it needs the grant to move from
`MAKE_REGION` (which today grants every region page to *the* device) to `MAP_DMA`.

The mutant that ignores the object dies on the BOOT, not in the host suite: the suite tests the
predicate, not whether `map_dma` calls it. Worth keeping separate — a checker existing and a
checker being invoked are different claims, and only the second is a property of the system.

Two self-inflicted breakages, both caught by existing assertions:
- Adding one capability to the worker role made CAPABILITY SPACE, not the per-owner quota, the
  limit that bound the region-quota demo — which is precisely the confusion that demo's comment
  says it exists to detect. `CAP_SLOTS` is now sized with headroom and says why.
- The first ordering returned `NO_CAP` where there is no unit, so the informative "no IOMMU"
  refusal stopped being reachable.

## 2026-08-19 — DMA reach becomes a CAPABILITY

`CapType::IommuDomain` had a referent since the IOMMU work but no ABI: the nucleus programmed
the I/O page tables from its own boot path, so "the driver is an untrusted process that reaches
hardware only through capabilities" had nothing behind it for the one thing a driver
fundamentally needs. Syscalls stopped at `FREE_REGION`.

`MAP_DMA`/`UNMAP_DMA` close that. Two capabilities are required, because two separate
authorities are involved: an `IommuDomain` carrying WRITE (handing a device the ability to reach
memory is GRANTING authority, not observing it) and a `Region` carrying READ. The kernel picks
the IOVA — no user-supplied address reaches the I/O page tables, the same rule `MAP_REGION`
follows for virtual addresses. The device may WRITE the region only if the caller's own region
capability carries WRITE, so a READ-only loan produces a read-only I/O mapping; `Domain::map`
enforces that, and a refusal writes no page-table entry.

The grant half already existed — `make_region` grants each page into the device domain as it
allocates it, which is what the boot's `grants == live region pages` check has been asserting.
What was missing was the MAPPING half and any way to ask for it.

**With no unit programmed, `MAP_DMA` refuses** (`NO_MEM`) rather than quietly succeeding. On such
a machine a "granted" mapping and unrestricted access to all of memory are the same thing, and
the caller cannot tell them apart. The mutant that returns a plausible IOVA instead dies.

Three refusals are asserted on every x86 boot, and all three are reachable because the demo
holds the capabilities needed to reach them: a domain cap WITHOUT WRITE (the rights half), no
domain cap at all (the type half, from the producer role), and the no-unit case. The rights half
is exercised from a process holding BOTH caps — which the note in `grants_for` records the
`Mmio` case as NOT doing, leaving its check vacuous on hardware. Under-powered capabilities of
the right type are granted rather than merely described, for the same reason.

Also hoisted: the I/O page table's lower two levels are now built where the DTE is programmed
rather than inside the containment demo, since a syscall has to write a leaf into a table that
exists whether or not the demo ran. Both levels stay empty, so what can be REACHED is unchanged.

Two process notes, both the same lesson twice:
- A hand-run `cargo build -p init` failed to link (`R_X86_64_32S out of range`). I "fixed" the
  code model globally, which broke the nucleus into an empty serial log. The runner had been
  passing `-C code-model=large` for init all along, with a comment saying exactly why. Nothing
  was wrong; I had built it out-of-band.
- The first version of the new gate keyed on `IOMMU=1 && FIRMWARE=1` to decide whether a unit
  exists. `IOMMU=1` alone also finds IVRS and enables the unit, so the run meant to demonstrate
  the refusal was demonstrating the mapping, and the gate failed a working boot. It now keys on
  what the boot REPORTS about the machine rather than on what a flag implies about it.

## 2026-08-18 (later) — the adversarial review, and the checker that could not fail

Six items, mostly of the same family: a mechanism whose check could not fail, or a claim written
into a comment instead of measured. Two were found before the review returned, four by it. Every
fix below is mutation-tested — the mutant is named, and it died. The review's refute phase earned
its keep in both directions: it confirmed four, correctly downgraded one to hardening, and got
one wrong (see 4), which is why a refuter that cannot run the rig does not get the last word over
a measurement that can.

**1. `contained()` was never verified at all.** `fn contained() -> bool { true }` passed all 221
tests in the repository, the 21,952-sequence exhaustive search included. The invariant is only
ever evaluated in states the public API cannot corrupt, so "always true" is indistinguishable
from the real predicate: the search verifies the API and never the checker. `contained` is what
the kernel asserts at boot and what `crates/iommu` exists for. Fixed with a `#[cfg(test)]`
`force_mapping` that plants the state the API cannot produce, plus tests that require rejection
from EVERY slot. Both the `{ true }` mutant and a `.take(2)` truncation now die.

**2. The exhaustive search never occupied more than two slots.** It runs `Domain<3, 3>` over two
frames and two IOVAs while the kernel deploys `Domain<48, 8>`, so no table index above 1 was
ever exercised, at any N — widening the constants would not have helped, because the universe
caps live occupancy. Added deployed-shape tests that put the violation in the LAST slot; a
truncated `granted` scan dies against them. Separately the search size was documented as
"26 symbols / 17,576 sequences" for as long as it existed. The alphabet is 28 symbols and 21,952
sequences: the number was written down once and never recomputed when the alphabet grew.

**3. Granted rights never reached the hardware.** `Domain::map` refuses rights wider than the
grant, and the page-table leaf was then written with a constant `IR | IW` regardless. A READ-only
grant produced a WRITABLE mapping. Nothing caught it because every grant in the demo was RW, so
the constant was accidentally correct in every case exercised — the model's authority covered
which FRAME the device could reach but not what it could DO to it. The PTE bits are now derived
from the grant, and a read-only page is proved unwritable on the rig: the device is told to write
it, reports the transfer complete, and the page still reads its sentinel.

**4. Withdrawal did not withdraw.** Clearing a page-table entry is not revocation while the unit
still holds a cached translation. Two independent refuters argued this one away on the reasoning
that the unit populates its cache only from a successful walk — correct, and beside the point,
since two walks HAD succeeded by then. Both said they could not run the rig. Measured rather than
argued: re-aiming the device at the withdrawn IOVA returned `0xd1ce…`. Correction to an earlier
draft of this entry: the code's own comment had called this exactly right — "the moment a mapping
is CHANGED rather than added, this needs the command buffer" — so it was a warning that went
unheeded, not a false justification. Describing it as licensing the omission was wrong.
Fixed by implementing the command buffer (INVALIDATE_IOMMU_PAGES + INVALIDATE_DEVTAB_ENTRY +
COMPLETION_WAIT with a store, so completion is observed rather than assumed). The same probe now
reads back the sentinel, and removing the invalidation puts `0xd1ce…` back.

**5. "CONTAINED" could not distinguish a blocked write from a write of zeros.** HARDENING, not a
live defect — the refuter established that the failure is unreachable in the current structure,
because the measurement is deliberately taken against a freshly-zeroed root before any leaf
exists, so both legs are refused by the same absent entry and no allowed write is possible at
that instant. The oracle's discrimination is still weak on its own terms. The verdict was
`wrote != PATTERN` over a pre-zeroed frame — but the inbound leg is refused too, so the device's
buffer is empty and a transfer the unit ALLOWED would deposit zeros into a frame already reading
zero. The line claimed "NOTHING reached memory"; what it established was "the pattern did not
arrive". Frames are now pre-filled with a sentinel and all 64 transferred bytes are checked, so
any write is visible, including a write of zeros or one that starts at byte 8.

**6. The hardware gates were self-disabling.** All four containment gates sat behind
`grep -q 'edu ident='`, a line printed AFTER five places where the probe can bail out quietly.
Any regression that stopped the nucleus reaching the device skipped every gate and reported PASS.
It is now required rather than a condition; a mutant that bails early fails the run.

Also fixed: the AMD-Vi capability walk did `u8` arithmetic on offsets that legally reach 0xFC
(silent wrap in release, where overflow checks are off, composing the aperture base from the
Vendor/Device ID registers); and `PageFlags::NO_CACHE` was pinned by no test, so zeroing it left
every suite green while the boot still printed "aperture mapped uncached".

**Still not proven: the event log records nothing — and the cause is UNRESOLVED.** Correcting
the previous entry, which said the silence was ours: that was asserted on no more evidence than
the silence itself, which is the same move this document exists to catch.

`QEMU_TRACE='amdvi_*'` (new hook in `tools/run-qemu.sh`, output to its own file) shows the unit
DOES detect the errors — `amdvi_invalid_dte` sixteen times, `amdvi_unhandled_command` once — and
upstream 8.2.2's source has every one of those paths call `amdvi_log_event`. Yet no event appears.
Measured, and therefore ruled out: logging disabled (the unit's own STATUS reports EventLogRun=1),
overflow (EventOverflow=0, and that path would set it), a failed write (`amdvi_evntlog_fail` never
fires), reading the wrong entry (the whole 4 KiB ring is scanned), unmapped registers (four
aperture pages mapped, STATUS reads sensibly), and "the unit cannot write our memory"
(COMPLETION_WAIT's store lands every boot). Observation and upstream source disagree; the next
step is the distro build's actual sources rather than another guess.

Two positive controls now exist for it — a DTE corrupted with a reserved bit, and an illegal
command opcode — because reported silence means nothing until the log is shown capable of
speaking. Neither speaks. The refusals themselves are real and separately demonstrated by the
payload; what is missing is the unit REPORTING them.

One process note worth keeping: turning the trace on made the run FAIL. QEMU writes trace lines
to stderr, the runner merges that into the serial stream, and 392 interleaved lines chopped a
gate string in half — the assertion was present in the output and the gate missed it. Test
through the harness: an unintended difference is indistinguishable from the bug being hunted.
The hook now sends trace output to its own file.

The lesson worth keeping: an exhaustive search over an API proves things about the API. It says
nothing about a predicate that the API is designed never to falsify. Testing a checker means
constructing the state it exists to reject.

## 2026-08-18 — the MODEL now governs the MACHINE

`crates/iommu`'s `Domain` has been exhaustively host-tested since it was written, and until now
it ran BESIDE the hardware: the model said what was authorized while separate stores said what
the device could reach, and nothing tied them together. A model with no authority over the thing
it models is documentation.

Every I/O page-table leaf now goes through `Domain::map`, which refuses a frame no capability
granted and refuses rights wider than the grant. A refusal writes NO entry, which is what makes
the tie observable rather than structural — an ungranted frame is left UNREACHABLE by the
device, not merely unrecorded in a table. In one boot:

```
[iommu] domain: dst mapped src mapped ungranted-frame refused (no PTE written)
[iommu] TRANSLATED: the same device reached exactly the frame it was granted
[iommu] withdrew both mappings and grants; domain holds 0 grant(s)
[iommu] CONTAINED: the transfer completed at the device and the target frame is UNTOUCHED
```

The withdrawal is not tidiness. The proof's grants are not a standing authority, and the boot's
own consistency check — grants must equal live DMA pages — FAILED when two were left
outstanding with no region behind them. That check catching this is the check working. Mappings
are withdrawn before grants and the PTE is cleared as well as the model entry: clearing only the
model would leave the device able to reach a frame nothing said it could, which is the exact
stale-mapping hazard the crate's exhaustive search exists to prevent.

The runner gates on the refusal as well as the translation. Without it, "translated" shows only
that the table works, not that anything decides what goes into it.

## 2026-08-18 — THE LOOP IS CLOSED: refused where unmapped, delivered where granted

In one boot, on one unit, with one device:

```
[iommu] CONTAINED:  the transfer completed at the device and the target frame is UNTOUCHED
[iommu] TRANSLATED: the same device reached exactly the frame it was granted
```

The second line is what makes the first mean anything. Blocking every transfer is also what a
broken IOMMU does; delivering exactly the granted frame — the pattern `0xd1ce…` arriving at the
frame behind IOVA `0x1000` and nowhere else — is what distinguishes enforcing a POLICY from
enforcing a WALL. This is `dma_reach ⊆ authorized` demonstrated on hardware rather than argued,
and it is the property `crates/iommu` has been host-testing in the abstract since it was written.

The I/O page table is built by hand for now: a 3-level walk (root -> L2 -> L1 -> page) with each
entry carrying its NEXT LEVEL in bits [11:9] and a leaf marked next-level 0. Writing the level
of the table you point AT rather than the one you are IN is the obvious error, so the levels are
named constants.

**Both directions had to be mapped, and finding that out was the fourth oracle failure in this
sequence.** The first attempt mapped only the destination and reported NOT TRANSLATED — because
the inbound RAM->device transfer that loads the pattern was itself refused, so the device
faithfully delivered an empty buffer. Exactly the zeroed-buffer trap one level up.

No invalidation is issued, and that is only sound because nothing was ever cached for this
domain: the unit had refused every transfer, so there is no stale entry. The moment a mapping is
CHANGED rather than added, this needs the command buffer.

The runner gates on both lines.

## 2026-08-18 — CONTAINMENT PROVEN ON HARDWARE

A bounded device's DMA is refused, and it is refused in the only way that means anything: the
SAME code, the SAME device and the SAME two transfers, differing only in whether the unit was
translating.

| translation | target frame reads |
|---|---|
| OFF (control) | `0xd1ced1ced1ced1ce` — the transfer lands |
| ON | `0x0000000000000000` — nothing reaches memory |

Both runs report `transfers: RAM->dev done dev->RAM done`, so the device really did perform
them; "contained" means nothing arrived, not that nothing was attempted. That distinction is the
whole result, and it took three attempts to be able to state it:

1. First run reported "no event logged", which reads like a refusal. It was IMPATIENCE — QEMU's
   `edu` defers its transfer on a 100 ms timer and the code spun for a few milliseconds. Now it
   polls the RUN bit.
2. Then the transfer completed and the target still read zero WITH TRANSLATION OFF. `edu`'s
   internal buffer starts zeroed, so a successful device->RAM transfer writes zeros — identical
   to a blocked one. Now a known pattern is pushed in first and read back out, and the pattern
   is the oracle.
3. The device targeted at first was the rig's e1000, not `edu`, because the scan matched on
   CLASS. Caught by an identification-register check before trusting the mapping
   (`ident=0x00140241`, an e1000, not `edu`'s `0x010000ed`) — without which DMA commands would
   have gone into a NIC's registers and produced silent nonsense.

Every one of those three would have produced a confident "contained" that measured nothing. The
positive control is what turned each of them up.

**What is NOT proven: the event log records nothing.** Tail stays at 0 across a refused
transfer, so the unit is not reporting the refusal even though it is performing it. Event-log
setup is unfinished, and the boot line says so rather than leaving the silence to be read as
"no faults occurred". (Followed up above with a positive control that rules out the emulator
as the explanation.)

The runner gates on the payload: the transfers must complete AND `CONTAINED:` must appear.

## 2026-08-18 — the firmware rig BOOTS. The hang was an orphan `.got`.

`IOMMU=1 FIRMWARE=1 tools/run-qemu.sh` now boots through SeaBIOS end to end, on both hosts:
RSDP found, IVRS walked, AMD-Vi located and mapped, device table installed, event log armed.

**The bug was a linker-script orphan.** `.got` was never named in linker.ld, so the linker
placed it AFTER everything the script mentions — at `0x158BD8`, exactly `__bss_end`, which is
where `load_end_addr` stopped. The GOT was therefore never loaded. Associated consts like
`Arch::NAME` resolve through it, so the first one used jumped through a zeroed slot to
`0x159000` — one page past the image — and ran zeros into a triple fault before `init_traps()`
could produce a dump.

It is invisible on the ELF path because the program headers cover `.got` regardless, which is
why the same kernel boots fine under PVH. Naming `.got` in the script and setting
`load_end_addr = __data_end` fixes it, and that is also the textbook multiboot arrangement:
the loader reads to the end of the FILE and zeroes `.bss` itself.

**What made it findable was bisecting a symptom, not reading the spec harder.** A string
literal formats fine (PC-relative `lea`, no GOT entry) while `Arch::NAME` faults — that
asymmetry is what pointed at the GOT. Three earlier hypotheses were all wrong and all
plausible: the loaded extent, the em-dash in the format string, and `core::fmt` itself. Each
was killed by an experiment rather than by argument.

The firmware path also cannot use PVH's `rsdp_paddr`, because multiboot never hands one over —
firmware placed the tables itself. `acpi::scan_for_rsdp` looks in the BIOS window and VALIDATES
rather than signature-matches; its test plants a decoy with the right eight bytes and a wrong
checksum ahead of the real one, because a signature match alone would return the decoy.

## OPEN DECISION 2026-08-17 — proving containment needs the first PCI config WRITE

The AMD-Vi unit is enabled with a DTE for a real device and an event log armed, so a refused
DMA would be recorded. What is missing is a DMA to refuse, and getting one has hit a decision
that must not be crossed silently.

**PCI BARs are unassigned on this boot path.** Measured with QEMU's monitor on shark-a: the
`edu` device (1234:11e8, a trivial register-driven DMA engine) sits at 00:02.0 with
`BAR0: 32 bit memory at 0xffffffffffffffff` — i.e. unmapped, exactly like the AMD-Vi capability
base was. Assigning BARs is firmware's job and this boot runs none.

So triggering a DMA requires WRITING a BAR, and a config write is the thing the 2026-08-14
ruling deferred with "it needs its own decision rather than being smuggled in here". The options:

1. **Assign the BAR from the nucleus at a hardcoded address**, marked rig-scaffolding. Needs the
   size, which is only discoverable by SIZING (write all-ones, read the mask back) — itself a
   config write with the decode bit cleared, which is a standing no in the kernel. Hardcoding
   `edu`'s known 1 MiB avoids the sizing but is a constant that is true of one device on one rig.
2. **Do proper BAR sizing in the kernel.** Previously ruled out, and the reasons have not
   changed: it transiently unmaps a live device.
3. **Boot under firmware** so BARs are assigned before the nucleus runs. Changes the boot path
   every gate in both runners was calibrated against.
4. **Have the DMA come from something already addressable.** Nothing qualifies: a device needs
   its registers reachable to be told to transfer.

Not chosen here. The evidence is in place so the choice is made on facts rather than rediscovered.

## 2026-08-14 (later still) — the nucleus can SEE an IOMMU

The effect half starts with locating the unit, which the nucleus must do itself and no one else
may: the untrusted-driver story turns on the driver never holding an `Mmio` capability for the
IOMMU aperture.

`tools/run-qemu.sh` gained an OPT-IN rig — `IOMMU=1` boots q35 with an emulated AMD-Vi unit
(`-device amd-iommu`, matching the design's target rather than VT-d). The default path is
untouched, so a regression in the rig cannot become a regression in the boot everyone runs. The
nucleus reports `[iommu] AMD-Vi at 00:03.0 vendor=1022 …` on the rig and `no IOMMU on this
machine` otherwise, and the runner REQUIRES the line matching the rig it launched — both
directions, because either failure reads as success on its own: a scan that silently finds
nothing looks like a machine without an IOMMU, and a scan that matches anything "finds" one
where there is none. Both gates were mutation-checked.

`crates/kernel/src/pci.rs` is read-only, kernel-only, scans bus 0, and looks for exactly one
thing. The 2026-08-14 ruling against a config-space accessor stands and is not contradicted:
that ruling was about handing config authority to a DRIVER, which is authority over every
function's BARs. There is deliberately no config WRITE — BAR sizing needs writes with the decode
bit cleared, which remains a standing no in the kernel.

The capability block is read too, and it produced the first real hardware finding: **the AMD-Vi
register base is UNPROGRAMMED on this rig.** `lo=0x00000000, hi=0x0000fed8`, enable bit clear.
Firmware normally assigns it; this nucleus boots `-kernel`/PVH with none, so nothing has. The
obvious composition of those halves yields `0xfed800000000` — a plausible-looking address that is
not where anything lives, and exactly what the code reported until the enable bit was consulted.

The obvious next move — read IVRS through the PVH `rsdp_paddr` — was tried on 2026-08-15, and the
first conclusion drawn from it was WRONG and is corrected here.

**Availability is a property of the QEMU BUILD, not of PVH.** Same nucleus, same flags:
QEMU 8.2.1 (homebrew, macOS) gives `rsdp_paddr = 0x0` and no ACPI at all; QEMU 8.2.2 (Ubuntu,
shark-a) gives `rsdp_paddr = 0xf52c0` on q35 and `0xf5290` on i440fx. It was recorded as "there
is no ACPI on this boot path" on the strength of one host. A negative result from a single
environment is a claim about that environment, and this project keeps two on purpose.

So the IVRS route IS available on shark-a — the x86 validation host, the one with real AMD-Vi
silicon — and is not available on the macOS dev box. Below is what the missing-ACPI case means
where it does apply. QEMU does build the tables, but delivers them over **fw_cfg**
(`etc/acpi/tables`, `etc/table-loader`, `etc/acpi/rsdp` appear as fw_cfg ROMs) for FIRMWARE to
fetch, link and place. A `-kernel`/PVH boot runs no firmware, so nobody ever places them and no
RSDP exists in memory.

That leaves four routes to the AMD-Vi register base, none free:

1. **Implement a fw_cfg client and the ACPI table-loader in the nucleus.** This is precisely
   what SeaBIOS/OVMF do; it makes the nucleus into firmware, in ring 0, parsing an
   externally-supplied linker script. Large, and the wrong shape for a microkernel.
2. **Boot under real firmware** (SeaBIOS) instead of direct `-kernel`, so ACPI exists. Changes
   the whole boot path, which every gate in both runners was calibrated against.
3. **Have the nucleus program the capability's base register itself** — firmware's job, and the
   register is writable. This would be the first config WRITE in this tree, and it needs an
   address known not to collide, which means trusting the PVH memory map for something it was
   not written to answer.
4. **Hardcode QEMU's AMD-Vi base for the rig only**, clearly marked as rig-scaffolding and never
   a discovery mechanism.

Recorded rather than chosen: picking one is a decision, and the evidence for it is now in place
instead of being rediscovered.

The composition rule is now a pure function with host tests covering the case that produced the
wrong answer, so a mistake made once against real hardware is checked forever after without one.

Still absent: no Device Table Entry, no I/O page tables, nothing programmed. The unit is located
and reported, and the boot line says so in as many words.

## 2026-08-14 (later) — `IommuDomain` has a referent, for the half that can fail

`abi::CapType::IommuDomain` was a bare enum variant with nothing behind it. `crates/iommu` now
holds the DECISION half of a domain: which frames a device may reach, with which rights, and
whether the mappings it holds are covered by the capabilities that authorized them —
`device_reachable ⊆ granted`, the crux stated at docs/nucleus-design.md.

**It touches no hardware.** No Device Table Entry, no register, no invalidation. That is stated
first in the crate's own doc comment, because a crate was deleted from this repo two days
earlier for calling itself "VERIFIED TCB … the DMA-reach CRUX proof" over zero code, and the
distinction between deciding and effecting is the whole difference.

What makes it not theatre: the invariant is preserved by construction ONLY if the operations are
right, and four mutants prove it can fail — a `revoke` that drops the grant while leaving the
mapping (the V5 stale-mapping bug), a `map` that skips the rights check (amplification), a `map`
that skips the grant check, and a grant narrowed without withdrawing the mapping it no longer
covers. Each is caught by the exhaustive 3-op search.

It is wired, not shelved: `make_region` grants each DMA frame with the minting capability's
rights, the `Release` step withdraws them BEFORE the frames return to the pool, and the boot
asserts the grant set tracks the live DMA regions exactly. Measured — deleting the withdrawal
gives `[iommu] domain grants 13 frame(s) for 0 live DMA page(s)` and fails the boot, so 13 real
frames pass through the grant path per run.

What is still absent, plainly: no IOMMU driver, no DTE, no I/O page tables, no device. Nothing
maps, so the containment half of the boot assertion is trivially satisfied there; only the
crate's own search exercises it. The §1.2 revisit is NOT yet triggered — that needs
`MAKE_REGION` minting from a constrained extent, which still does not happen.

---

## DECISION 2026-08-14 — device discovery is DECLINED; the IOMMU is the blocker

A design pass asked how a process should discover and map a REAL device (PCI enumeration in the
kernel, a config-space capability for userland, or a boot-protocol device list). The verdict was
BUILD NO PCI CODE, for a reason that is not caution:

**There is no IOMMU.** `docs/nucleus-design.md` states the premise the untrusted-driver story
rests on — the nucleus grants the driver an `Mmio` capability for the GPU aperture but never for
the IOMMU aperture, so it can command arbitrary DMA but cannot touch the tables that bound it.
Grep finds exactly one referent for `IommuDomain`: a bare `abi::CapType` variant. So on a
bus-mastering function **MMIO is unbounded DMA authority** — one store programs the device to
write the process table, the capability spaces and the delegation ledger. Discovery is not the
blocker; it is the step after the blocker.

That is why `DEVICE_PHYS` is a kernel-allocated RAM frame with no bus master behind it. The
stand-in is load-bearing, not laziness, and it stays.

Second, independent reason: MAP_BAR's "uncached / device-memory" precondition is not expressible
in this tree (no PCD/PWT in `vspace::PageFlags`, no PBMT in `vspace_riscv::PageFlags`), so any
real BAR mapped today would be a CACHED mapping — and QEMU TCG cannot tell. The property that
matters most on real silicon is the one the only available rig cannot falsify.

**Ruling on `Untyped`.** Real device DMA DOES force `Untyped` to name an extent, but there is no
real device DMA and cannot be until an IOMMU exists — so §1.2 may be inherited now, on a stated
principle rather than by luck, and must be revisited at the IOMMU commit. The tripwire and the
IOMMU blocker fire together. What was built instead is the generalization of that tripwire from
the TYPE `Untyped` to the PROPERTY `is_mint_source`, so a second mint source cannot slip past it.

Explicitly NOT built: no config-space accessor on either arch, no bus scan, no BAR decode, no BAR
sizing in the kernel (that needs config WRITES with decode disabled — a standing ruling, not a
deferral), no new syscalls, no referent for `IommuDomain`, no FDT/DTB or RSDP/MCFG parsing.

Also recorded: neither runner can answer a PCI question as invoked — `tools/run-qemu.sh` passes
no `-machine`, so it gets the default i440fx with no ECAM, and `tools/run-qemu-riscv.sh` boots
`-machine virt` with no `-device` at all.

---

## 0. What is actually checked today (and how to prove the checks can fail)

**The kernel was exempt from all of this until 2026-08-11**, and not by decision.
`crates/kernel` — 2360 lines, the whole syscall surface — had zero `#[test]`, so it could
never trip `tools/host-tests.sh`'s guard, which fires on the PRESENCE of `#[test]`. It was
believed unable to build for the host; in fact only a missing off-target `sched::Context`
stood in the way. "Cannot build for the host" and "has nothing worth testing" are different
claims and neither had been checked. It now builds and is tested on every host.

Measured, and the reason this mattered: granting the least-authority producer a full
`Untyped`/`ALL` capability — allocate and spawn, to the process the isolation story rests on
— produced a clean `BOOT OK` and `RESULT: PASS` on x86. The QEMU boot cannot see a privilege
escalation in the boot grant tables, because the demo only exercises what a process CAN do.
Seven host properties now cover the tables; each was mutation-checked against a distinct
authority change.

**What else the boot cannot see.** Rather than guess, the same probe was run against the
kernel's authority gates one at a time, deleting the RIGHTS half of each and booting x86:

| Gate | Boot verdict with the rights check deleted |
|---|---|
| IPC endpoint rights (`endpoint_of`) | FAIL — caught |
| `SPAWN` requires `Untyped` + WRITE | FAIL — caught |
| `holds_mmio` requires `Mmio` + READ | **PASS — invisible** |
| `MAP_REGION` requires `Region` + READ | **PASS — invisible** |
| `FREE_REGION` requires `Region` + WRITE | **PASS — invisible** |
| `WAIT_IRQ` requires `Irq` + READ | **PASS — invisible** |
| `POLL_IRQ` requires `Irq` + READ | **PASS — invisible** |
| `MAKE_REGION` requires `Untyped` + WRITE | FAIL — caught |

Five of eight were vacuous on hardware while `grants_for` claimed otherwise in a comment
(now corrected). Two DISTINCT reasons, which the fix depends on telling apart: for `Mmio` the
discriminating capability exists in the grant tables and the scenario never reaches it; for
`Irq` and `Region` no under-powered capability is granted to anyone, so the case does not
exist to be reached. Only the first looks like a testing problem. The
grant tables *do* contain the discriminating capability — the worker holds an `Mmio` without
READ — but the revocation teardown the demo checks runs in the CHILD, which holds no second
`Mmio` at all. The case existed in the tables and was never reached. That is a sharper
version of the recurring defect: not an axis held constant, but a case present in the fixture
and unreachable by the scenario.

The fix separates the DECISION from the process table: `caps_hold_mmio` / `caps_hold_irq` /
`caps_hold_endpoint` / `caps_endpoint_object` are pure functions over a capability space, and
the `unsafe fn`s reading `PROCS` are thin wrappers. That also collapsed a duplicate — the
`WAIT_IRQ` credit path open-coded its own copy of the Irq check instead of calling it, so the
two could drift and only one would be fixed.

Assurance in this tree is host tests plus one scripted QEMU boot per arch. Several of the host
suites are *exhaustive searches* rather than samples, and where a search covers its whole universe
it is a proof — that is why Verus buys nothing on those axes. The honest qualifier is that a search
proves nothing about an axis its universe holds constant, which is a defect this project has found
repeatedly, and the searches below still hold real axes constant:

| Crate | Search | Axis still held constant |
|---|---|---|
| `abi` | all 64 rights pairs over the 3-bit lattice, + `from_user` over every u8 | — (the mask that makes `0..8` exhaustive now lives in `abi::CapRights::from_user` and is pinned by its own test) |
| `deleg` | all forests of ≤5 edges on 6 endpoints, in `Ledger<16>`; ≤3-edge forests in every insertion order | universe size (3 procs / 2 caps vs kernel 6 × 16) — measured NOT to be a gap: both proc/cap-confusion mutants die on the current universe |
| `runstate` | every state vector of length 1..=6 × 7 predicates | endpoint/line values ∈ {0,1}; 7 of the boolean functions over the reachable domain |
| `regions` | 20,736 configs × 7 plans at `P=6`/`S=4`/`N=52` (tables both compacted and with a dead entry first), plus the worst-case teardown at `MAX_REGIONS=12` | region ARITY (≤2 live) — but measured: `take(1)`/`take(2)` on the teardown loop is already caught, so arity is covered and it was the COMPACTION that was not |
| `capabilities` | rights bits, every slot of `CapSpace<16>`, occupancy under a NONE-rights slot, two slots on one object | cap TYPE is thin here (5 of 11 variants) — but measured: covered at the repo level by `kernel`'s Region tests |
| `iommu` | every 3-op sequence over grant/revoke/map/unmap (21,952 sequences, invariant checked after EVERY step; plus deployed-shape tests that reach the LAST slot, which the search itself never occupies, and rejection tests so `contained` is shown to REFUSE) | the hardware half now EXISTS and is exercised: the boot programs a device-table entry per device, `MAP_DMA` writes real I/O page-table leaves, and the shutdown walks the real table requiring every present leaf to be grant-covered. What the model still cannot see is anything the page tables do that no leaf records |
| `mm` | `partition_holds_for_every_dma_top` (1,040 configs), every **3**-region map shape over unaligned starts/lengths/kinds asserting the exact allocatable SET (110,592 configs), and 5×4000-step arbitrary alloc/free interleavings | maps are 3 regions, not arbitrary-length |
| `kernel` | boot grant tables, every authority predicate, the PVH map bound (17 properties) | everything else in ~2400 lines — a foothold, not coverage |

**Closed 2026-08-11 — the deployed-shape gap.** Every crate above is generic over an `N`, and the
kernel monomorphises each to a value the tests never instantiated: `CapSpace<16>`, `Ledger<16>`,
`Holder<4>`, `Plan<52>`, and a 6-slot state vector. The searches ran at half those widths or less.
That is a test-matrix gap, and it was cheap — but it was not cosmetic, because in every case a
scan that simply *stopped early* was invisible. Measured, each of these mutants left the old suite
fully green and fails now:

| Mutation | Old suite | Now |
|---|---|---|
| `classify` ignores slots past the third | 17 passed | 2 fail |
| `uncreditable` ignores slots past the third | 17 passed | 1 fails |
| `first_free` scans only the first 8 cap slots | 10 passed | 1 fails |
| `revoke_from` scans only the first 4 ledger slots | 17 passed | 3 fail |
| `holders_of` reads only the first 2 share slots | 16 passed | 2 fail |
| `holders_of` reads only the first 3 holders | 16 passed | 2 fail |

Two candidate mutants were *rejected* as evidence because they died under the old suite too — a
`revoke_from` fixpoint capped at two rounds, and `classify` collapsing per-process authority to
index 0. The widening did not buy those, and the first of them had already been written into a
comment as justification before being run; the comment was corrected rather than kept.

**A NEW AXIS, found 2026-08-13 and CLOSED the same day.** `mm`'s map search was a ONE-SIDED
oracle: it asserted that every frame handed out is legitimate (fully inside a Usable region,
touched by no non-Usable one) and never that a frame which SHOULD be allocatable actually is. An
allocator that marked everything USED would have satisfied it — the `frames_seen > 0` guard rules
out only the totally-degenerate case, and `alloc_until_exhausted_then_none` compares `free_count()`
against the drained count, both derived from the SAME bitmap, so it is self-consistent under any
uniform under-freeing.

Demonstrated rather than argued, and it took three tries: the first two candidate defects were
caught after all (by `construction_counts_are_correct` and
`unaligned_region_bounds_only_yield_whole_frames`, which ARE two-sided on the shapes they use).
The one that survived was `if end < PAGE_SIZE` -> `if end < 2 * PAGE_SIZE` in the Usable pass — a
plausible off-by-one in the minimum-size guard that silently discards every Usable region under
two pages. The hardcoded map's regions are megabytes wide; only the search has shapes small enough
to notice, and it was not looking. The search now recomputes the expected allocatable SET from the
region list with plain arithmetic and asserts set equality, so under- and over-freeing both fail.
Three under-freeing mutants that previously survived now fail.

**Some listed axes are gaps and some are not, and only measurement tells them apart.** Of six
axes probed by applying a real single-expression mutation and running the existing suite:

| Axis | Verdict | Evidence |
|---|---|---|
| `deleg` insertion order | NOT a gap | 3 order-dependence mutants, all already caught |
| `mm` alloc/free sequencing | NOT a gap | 4 mutants incl. the cursor/floor interleaving, all already caught |
| `mm` map length (2 regions) | **REAL** | `.take(2)`, `continue`→`break`, and a kind-filter on the non-Usable pass ALL survived |
| `capabilities` occupancy | **REAL** | `first_free` reusing a NONE-rights slot survived; reachable via the kernel's `NO_AUTHORITY` placeholder |
| `capabilities` revoke scope | **REAL** | revoke emptying every slot naming the same object survived |
| `regions` table compaction | **REAL** | `take_while(\|r\| r.live)` on teardown AND destroy both survived |

Note the shape of the two that were NOT gaps versus the four that were: the misses were about
ORDER and SEQUENCE, dimensions the existing searches already varied incidentally. The real gaps
were about a second entity existing at all — a third region, a second capability on one object, a
dead entry before a live one. A search that varies one thing thoroughly does not thereby vary how
many things there are.

**Two of the listed axes turned out not to be gaps at all**, and the pattern is worth stating
because this table is what makes them look like gaps. `deleg`'s insertion order and `mm`'s
alloc/free sequencing were both named here as held constant; searches were written for both;
neither caught a single mutant the suite did not already catch. Varying which edges a forest
contains already varies which lands in slot 0, and the one interleaving that matters for the
allocator was already written down by hand. **An axis is unexplored only if the search cannot
REACH the case — not merely if no loop iterates over it.** Both searches were kept, each with a
comment saying plainly that it closed no gap, because a passing test that reads as coverage is
the failure mode this whole document exists to avoid.

**The `mm` test, and why it is not theatre.** Before it existed, every `BitmapAllocator::new` call
in the suite passed `DMA_TOP = 16 MiB`, which pins two axes at their most forgiving values at once:
`dma_top` is page-aligned, so the round-up in `general_floor` is a no-op, and `general_floor` lands
on a bitmap *word* boundary, so `first_free`'s `start_bit` mask is a no-op. Both are load-bearing
off those values. Measured: with either of these single-expression mutations applied, the other 14
`mm` tests stay **green**, and the new test is the only one that fails.

```
mm/src/lib.rs:76   ((dma_top + PAGE_SIZE - 1) >> PAGE_SHIFT)  ->  (dma_top >> PAGE_SHIFT)
mm/src/lib.rs:215  word | ((1u64 << start_bit) - 1)           ->  word
```

Either mutant hands a device-reachable frame to the general pool. The failure is concrete rather
than abstract — `dma_top=0x200001: general allocation 0x200000 is device-reachable` — i.e. a
`dma_top` one byte off a page boundary is enough. Re-run the two mutations rather than trusting
this paragraph; that is the point of naming them.

---

## 1. The invariant ladder V1–V7

Each rung is a machine-checked property of the nucleus. Lower rungs are lemmas the upper rungs consume. "Load-bearing at" is the milestone where a *failure* of that invariant would actually breach isolation on real hardware (before that, the host IOMMU or the absence of the feature covers us).

| ID | Property | Load-bearing at | Technique | State |
|----|----------|-----------------|-----------|-------|
| **V1** | Nucleus-core memory safety: no UB in safe nucleus code; every `unsafe` sits behind a trusted, spec'd boundary | M1 | Verus default (ownership + `PointsTo` permissions), big-lock | not started |
| **V2** | `reachable(AS) == capabilitied(AS)` for every address space, **preserved** across `map/unmap/grant/revoke` | M2 | Flat permission map + ghost `path`/`subtree` fields to de-recursify the radix tree | not started |
| **V3** | `dma_reach(GPU_domain) ⊆ authorized(GPU_domain)` — the crux | M3, hardened M4 | Same flat-map machinery over the **IO** page tables; `authorized` = GPU domain's frame caps | subsystem EXISTS, property TESTED not proven: `Domain::contained` is the predicate, exhaustively searched over every 3-op sequence AND shown to REJECT from every slot; the boot walks the real I/O tables requiring every present leaf to be grant-covered; a device reaches its granted frame and not an ungranted one, on the rig |
| **V4** | DTE-config invariant: for the GPU's BDF, `V=1 ∧ TV=1 ∧ translation-on ∧ bypass-off ∧ ATS-off ∧ root==our-tables` | M3/M4 | Struct invariant on the trusted DTE model; bridges V3 to hardware axioms A1/A3 | subsystem EXISTS, partly CHECKED not proven: every entry is read back as a WHOLE WORD (V\|TV alone admitted mode-0 passthrough and a root aimed at a live table — both measured), domains are published only once `CTRL` reads back with `IommuEn`, and the deny entry is shown to deny by aiming a device at it. ATS-off is NOT checked; the field is untouched and unexamined |
| **V5** | Reclaim / stale-IOTLB safety: a frame is returned to the free pool only after it is unmapped from **all** IO/CPU tables **and** the IOTLB/DTE cache is invalidated to completion | M4 | Ghost "in-flight invalidation" token; frame free-list disjoint from any live `dma_reach` | subsystem EXISTS, property TESTED not proven: command buffer with INVALIDATE_IOMMU_PAGES + INVALIDATE_DEVTAB_ENTRY + COMPLETION_WAIT, and the acknowledgement is now CHECKED at every call site — it was returned and discarded everywhere, so "to completion" was hoped for rather than established; the boot fails on any invalidation the unit did not acknowledge; `FREE_REGION` clears hardware across every domain BEFORE the frames are reissued, and the boot fails on any present leaf no grant covers. Measured both ways — without the invalidation the device kept reaching a withdrawn frame. The second domain's invalidation has no payload oracle (its device cannot be driven); the emulator's trace is the only witness |
| **V6** | No IPC authority amplification: a message transfer never yields the receiver a capability the sender did not already hold (grant is monotone-down) | M2/M5 | Cap-set monotonicity lemma over the IPC transition relation | not started |
| **V7** | *(optional)* host-submission well-formedness: ring/doorbell descriptors the nucleus forwards are bounds- and type-checked | M5 | Bounded structural predicate; mostly Kani-territory | optional |

**Composition (M5).** The confinement assurance case is `V1 ∧ V2 ∧ V3 ∧ V4 ∧ V5 ∧ V6 ⇒ inter-guest & guest↔nucleus DMA/memory isolation`, discharged as one top-level Verus theorem plus a prose assurance argument citing the hardware axioms (§4) for the steps no Rust tool can close.

### 1.1 Concurrency model: big lock

The nucleus takes a **single big lock (BKL)** around all page-table, IOMMU-table, cap-set, and free-list mutation. Consequences for the proof:

- No interleaving reasoning. Every mutating `exec fn` runs to completion holding the lock, so invariants need only hold at lock release. This is the difference between a 6-month proof and a multi-year one.
- The protected ghost state is a `Tracked<NucleusGhost>` token handed out by the lock (`vstd::rwlock` / a hand-rolled `Tracked` guard). The lock's `inv` closure asserts `V1..V6` on the guarded state.
- Cost: no fine-grained concurrency inside the nucleus. Acceptable — the nucleus is an isolation nucleus, not a throughput kernel; the untrusted `lite::` driver keeps the parallelism.

If big-lock throughput ever bites, the escape hatch is per-address-space locks with a fixed lock order, but that is explicitly out of scope until after M5.

---

## 2. Specs for V2/V3/V4 — REMOVED 2026-08-11

This section held ~200 lines of illustrative Verus for V2 (reachable == capabilitied), V3
(DMA-reach ⊆ authorized) and V4 (the DTE-config invariant). It has been deleted rather than
updated, for two reasons stated in the section's own former preamble and in the DECISION block
above:

- It **did not verify**, and said so: "they will not pass `verus` as-is (missing lemmas, trigger
  tuning, and vstd glue are elided)". Unverifiable code in a document is indistinguishable from
  verified code to a reader skimming for status, and this project has already paid a full design
  cycle for exactly that confusion (`CapType::Untyped` importing seL4's contract by name alone).
- It specified **page tables and IOMMU device tables that did not exist in this tree**. V3 and V4
  were properties of `crates/iommu-amdvi`: seven lines of doc comment, no functions, no tests, no
  dependents. A spec for absent code cannot be wrong, which is precisely what made it worthless —
  there was nothing it could fail against. (That crate is gone; the code those properties describe
  now exists in `crates/iommu` and `crates/kernel`, and the ladder's V3/V4/V5 rows say what is
  tested and what is not. The lesson stands unchanged: write the spec against code that can fail
  it.)

When there is IO page-table code, write the spec against that code, and gate it on spec mutation
(§DECISION item 5) rather than on an obligation count. Until then the honest statement is that
these three rungs are unstarted, which the table in §1 now says plainly.


## 3. The unsafe stub and Kani (bug-finding, not proof)

Verus cannot reason about volatile MMIO, the physical-frame allocator's raw pointer arithmetic, or the exact bit-layout of PTEs/DTEs against the silicon. We concentrate **all** such code into one tiny crate, `nucleus-stub/`, and treat it two ways:

1. **Trusted spec boundary (Verus side).** Each stub fn is `#[verifier::external_body]` with a hand-written `requires`/`ensures` that the rest of the nucleus verifies against. These signatures are **TCB** (§4) — a lie here is unsound. Example:

```rust
#[verifier::external_body]
pub fn write_io_pte(slot: *mut u64, encoded: u64)
    requires slot_owned(slot), well_formed_pte(encoded)
    // ensures: the abstract leaf at this slot now decodes to `encoded`
{ unsafe { core::ptr::write_volatile(slot, encoded); fence(Release); } }
```

2. **Kani bug-finding (independent of Verus).** Bounded model checking (CBMC backend) over the pure logic *inside* the stub — the parts that are ordinary computation, not hardware effects. This finds bugs; it is **not** a proof (unwinding bounds ⇒ incomplete). Harnesses in `nucleus-stub/tests/kani/`:

```rust
#[kani::proof]
fn pte_roundtrip() {
    let f: Frame = kani::any();
    let r: Rights = kani::any();
    kani::assume(f.base % 4096 == 0);
    let (frame2, rights2) = decode_pte(encode_pte(f, r));
    assert_eq!(f, frame2);
    assert_eq!(r, rights2);
}

#[kani::proof]
#[kani::unwind(64)]
fn cmdbuf_index_never_oob() {
    let head: u32 = kani::any();  let tail: u32 = kani::any();
    kani::assume(head < CMDBUF_ENTRIES && tail < CMDBUF_ENTRIES);
    let idx = advance(head, tail);       // ring math for AMD-Vi command buffer
    assert!(idx < CMDBUF_ENTRIES);       // no OOB store into the ring
}
```

Kani targets: PTE/DTE encode↔decode round-trips, the frame-allocator bitmap set/clear/find-first, command-buffer & event-log ring index math, and IOVA/PA alignment arithmetic. These are exactly the places where a silent off-by-one would corrupt an otherwise-verified invariant.

---

## 4. Honest TCB of the proof itself

A green `verus` run does **not** mean "the nucleus is safe." It means "the nucleus is safe *modulo* everything below." Everyone reading the assurance case must see this list.

**Software TCB (trust the tool):**
- **rustc** — the exact vendored nightly Verus ships with; its frontend (up to the point Verus intercepts) and its codegen. A codegen bug is unsound.
- **Verus** — the Rust→VIR→AIR→SMT translation, its encoding of the ownership/permission model, and its trust that the compiled binary is the same source it verified (mitigated by building the *verified crate itself* via `cargo verus`, not a separate copy).
- **Z3** — the pinned SMT solver. Solver soundness bugs are rare but real; drift between Z3 versions can also *hide* a regression, hence the hard pin (§5).
- **vstd** axioms — the standard-library specs we build on.
- **Every hole we open:** `#[verifier::external_body]`, `assume_specification`, `admit()`, `assume()`, `#[verifier::external]`. CI greps for these and fails on any un-annotated addition (§5).
- **The trusted spec.** Our models of the AMD-Vi DTE/IO-PTE format and CPU page tables must **faithfully match silicon**. A wrong spec yields a proof that is *vacuously* green. This is the most likely place to be wrong and the hardest to catch — it is reviewed against the AMD I/O Virtualization Technology (IOMMU) spec by hand.

**Hardware axioms A1–A6 (no Rust tool can discharge these):**

- **A1 — Enforcement.** With `dte_confining` true for a BDF, AMD-Vi translates *every* upstream memory request from that requester-ID through the domain's IO page tables; there is no undocumented bypass path.
- **A2 — Requester-ID integrity.** The gfx1201 device issues DMA only under its assigned PCIe requester-ID (BDF) and cannot spoof another source-id to select a different (weaker) DTE.
- **A3 — No pre-translated bypass.** With ATS disabled in the DTE, the device cannot present already-"translated" TLPs that skip the IOMMU. (PASID/PRI assumed off for the GPU BDF and modeled as such.)
- **A4 — Invalidation completeness.** After `INVALIDATE_IOTLB_PAGES` / `INVALIDATE_DEVTAB_ENTRY` followed by a `COMPLETION_WAIT` the nucleus observes complete, the IOMMU uses **no** stale cached translation or DTE for subsequent requests. (This is what makes V5 real.)
- **A5 — Register/queue semantics.** MMIO reads/writes to IOMMU registers and the command-buffer/event-log rings behave per the AMD IOMMU spec — i.e., our trusted register model matches the silicon.
- **A6 — Walker coherence & DRAM integrity.** After the nucleus writes a PTE/DTE and executes the required store fence + invalidation, the IOMMU page-table walker observes the new value and never reads a torn/stale entry; and the DRAM frames holding the tables are not corrupted by any other agent — which holds precisely because those `table_frames` are outside `dma_reach` (V3's self-protection clause). A6 is thus partly *discharged by* V3 and partly a raw physical assumption about DRAM.

These six are the honest edge of the guarantee. M5's assurance case states them explicitly and does not pretend they are proven.

---

## 5. Version pinning + proof-in-CI

**Pinning (non-negotiable — the proof is only reproducible if the toolchain is frozen):**

- Pin one **Verus release tag** (a specific `release/0.YYYY.MM.DD` or git SHA). Verus vendors its own rustc nightly and its own Z3 build; adopting a release pins all three at once.
- Commit the `rust-toolchain.toml` that the chosen Verus release dictates (a specific `nightly-YYYY-MM-DD`). Do **not** float it — a nightly bump can change what Verus accepts.
- Pin the **exact Z3** binary the release ships (historically the 4.12.x line; take whatever `tools/get-z3.*` fetches for your tag). Record its version and SHA-256 in `toolchain.lock`. Z3 patch bumps change proof search and can flip a green proof red (or, worse, mask a real failure).
- Record all three (Verus SHA, rustc nightly, Z3 version+hash) in `docs/toolchain.lock` and assert them in CI. Upgrades are a deliberate, reviewed event with a full re-verify, never incidental.

**CI job** (planned as `.github/workflows/verus.yml`; DOES NOT EXIST — the only workflow is `ci.yml`, two jobs, no proof gate):

```yaml
- run: cargo verus verify -p nucleus --release
        -- --rlimit 50 --num-threads 8 --no-report-long-running
```

- **Determinism / flakiness.** Z3 is nondeterministic under time pressure. Mitigate: set a generous `--rlimit` per function, split proofs into small modules (`--verify-module`) so no single query is huge, prefer `broadcast` lemmas and explicit trigger annotations over letting Z3 guess, and fail the build on *any* function that needs a retry. Keep a `--log-all` trigger dump artifact for debugging trigger loops.
- **TCB-growth gate.** A second CI step greps the diff for new `external_body`, `assume`, `admit`, `assume_specification`, `#[verifier::external]` and fails unless the PR description whitelists each with a rationale. This keeps §4 from silently expanding.
- **Expected re-verify time.** For a 6–8K-SLOC nucleus with the flat-map page-table proofs, budget **~2–10 min** for the full crate on a modern CI runner, with the V3/V5 IOMMU modules dominating. The one-time refinement lemma is the slowest single query; cache-friendly module splitting keeps incremental PRs under a couple of minutes.

---

## 6. THE STAFFING GATE (read this before promising any M1 date)

**There is no in-house proof engineer.** This is the single largest risk to the plan, and it is a *hard gate*, not a footnote.

- **M0 is reachable by the systems engineer alone.** Booting as a KVM guest (`start-gpu-vm.sh`) and getting `lite::` to dispatch one wave is ordinary systems work; verification is not load-bearing yet (host IOMMU covers isolation).
- **M1 and everything above it require Verus capacity that does not currently exist.** The crux proofs (V3–V5) are research-grade SMT/proof-engineering, not something a strong Rust generalist closes cold. **M1+ is unstaffed and therefore unfunded until this gate is cleared.** Do not put M1–M5 dates on any roadmap before then.

**The plan to clear the gate (pursue in parallel):**

1. **Hire a proof engineer with Verus/SMT experience.** Small talent pool; it overlaps CMU (Parno group alumni), Microsoft Research (Hawblitzel/Lattuada lineage), and Utah's Mars Research. Expect a long search.
2. **Research partnership — the fastest realistic path.** Engage **Mars Research (Anton Burtsev's group, University of Utah)**, the authors of Atmosphere and its Verus page-table proofs. They built the exact flat-map/ghost-subtree technique this plan leans on. A funded collaboration (sponsored student/postdoc or a consulting arrangement) buys both the expertise *and* the head start of their proofs. Co-authoring/upstreaming the IOMMU extension is a plausible incentive for them.
3. **Verus community.** The verus-lang Zulip has active office hours and responsive maintainers. Good for unblocking, spec review, and recruiting; not a substitute for a committed engineer.
4. **Grow in-house (the ramp).** A strong Rust systems engineer can reach productivity on Verus in **~3–6 months**: work the Verus tutorial and `vstd`, then **reproduce Atmosphere's page-table proof from scratch** (§7) as the ramp exercise, then extend it to the IO page tables and the `authorized` confinement theorem. Budget this ramp explicitly; it is real headcount time, not slack.

**Concrete gate condition:** *do not begin M1 proof work until either (a) a proof engineer is hired, or (b) a funded Mars Research collaboration is signed.* Use the Atmosphere reproduction as the hiring test / ramp deliverable so the first month of paid proof work also de-risks the technique on our toolchain.

---

## 7. Seeding from Atmosphere (with Asterinas OSTD fallback)

**Primary path — reuse Atmosphere's open Verus page-table proofs.** Atmosphere (Mars Research) verifies an x86-64 4-level CPU page table in Verus using exactly the flat-map + ghost `path`/`subtree` de-recursification this plan specifies. Our IO page tables (AMD-Vi, multi-level, 4KiB pages, present/RW bits) are structurally close enough to reuse the machinery almost directly.

Ramp/reproduction steps:

1. Clone Atmosphere and build its page-table module **against our pinned Verus** (§5). If it verifies unchanged, you have a working reference proof on our toolchain.
2. Extract the page-table crate + its proof harness into `nucleus/vendor/atmosphere-pt/` and re-verify in isolation.
3. **Re-parametrize** the entry/rights types: swap x86-64 PTE bits for AMD-Vi IO-PTE bits and the DTE root pointer. The tree shape and the refinement lemma carry over.
4. **Add the new content** (this is the part Atmosphere doesn't have): the `caps` / `authorized` set, `dma_confined` (§2.2 including the self-protection clause), and `lemma_iommu_map_confined`.
5. **Add the DTE model + `dte_confining`** (§2.3) and wire the M5 assurance case to hardware axioms A1–A6.

Reuse the same machinery a **second** time for the CPU-side V2 (guest/nucleus address spaces) — that use is the closest to Atmosphere's original and should port with the least change.

**Verify before trusting:** confirm the Atmosphere repo URL, license, and that its proofs actually build on our pinned Verus *before* committing to this path — Verus moves fast and open research proofs bit-rot against toolchain drift. If it doesn't build, that discovery belongs in the ramp exercise, not in M1.

**Fallback — Asterinas OSTD (structure, not proofs).** If Atmosphere won't build or its proofs prove un-portable, fall back to **Asterinas** (`github.com/asterinas/asterinas`) for the *unsafe-boundary structure only*. Asterinas is a Rust framekernel whose **OSTD** crate concentrates all unsafe code into one small, soundness-audited TCB with safe abstractions on top — exactly the shape our `nucleus-stub/` (§3) wants. Asterinas is **not** Verus-verified, so this fallback gives us the architecture (one audited unsafe crate; safe kernel above) but **no proof to copy** — we hand-roll the page-table proofs from the Verus tutorial + `vstd` `PPtr`/`PointsTo` primitives. That is materially more work and pushes the staffing gate (§6) harder, so it is the fallback, not the plan.

---

## Open questions / where this plan is weakest

- **The trusted spec (§4) is the real risk, not the proofs.** A faithful model of AMD-Vi DTE/IO-PTE semantics is doing more work than any lemma, and it is checked by human reading of the spec, not by a machine. Worth a dedicated review pass with someone who has driven AMD-Vi bare-metal.
- **A4 (invalidation completeness) and V5 are tightly coupled and the least de-risked rung.** Stale-IOTLB bugs are subtle on real silicon; the model may need refinement once M4 hardware bring-up exposes actual completion-wait behavior.
- **Atmosphere portability is unconfirmed.** The whole seeding strategy assumes its proofs build on a Verus we can also live with. That assumption is checkable in a week and should be checked first.
- **Big-lock throughput** is fine for an isolation nucleus but forecloses concurrency work until post-M5; flag it if the nucleus ever grows scope.
