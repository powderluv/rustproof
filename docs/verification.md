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

Status of the rest of this document: **design sketch for a system that does not exist yet.** The
ladder below is keyed to milestones M0–M5 (VFIO → vIOMMU → bare-metal AMD-Vi) that have no code in
this tree — `crates/iommu-amdvi` is seven lines of doc comment with no functions, no tests, and no
dependents, and `toolchain/verus.lock` / `toolchain/fetch-verus.sh` are placeholders (`REPLACE_ME`,
`exit 1`). Read §1 onward as intent, not as a description of the repo.

Scope reminder (do not re-litigate): the guarantee we verify is **isolation / DMA-containment**. GPU compute correctness is permanently out of scope. The C++ `lite::` gfx1201 driver and its GPUVM are **untrusted user processes** that emit only IOVAs; the nucleus owns the AMD-Vi IOMMU tables. Under plain VFIO (M0–M2) the *host* programs the physical IOMMU, so the nucleus IOMMU proof is not load-bearing until **M3 (emulated vIOMMU)** and **M4 (bare-metal AMD-Vi)**.

---

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
[iommu] CONTAINED: the transfer completed at the device and NOTHING reached memory
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
[iommu] CONTAINED:  the transfer completed at the device and NOTHING reached memory
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
"no faults occurred".

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
| `iommu` | every 3-op sequence over grant/revoke/map/unmap (17,576 sequences, invariant checked after EVERY step) | there is no hardware half — nothing maps, so the containment side is satisfied trivially IN THE KERNEL (the crate's own search does exercise it) |
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
| **V3** | `dma_reach(GPU_domain) ⊆ authorized(GPU_domain)` — the crux | M3, hardened M4 | Same flat-map machinery over the **IO** page tables; `authorized` = GPU domain's frame caps | **no subsystem exists** (`crates/iommu-amdvi` is a 7-line stub) |
| **V4** | DTE-config invariant: for the GPU's BDF, `V=1 ∧ TV=1 ∧ translation-on ∧ bypass-off ∧ ATS-off ∧ root==our-tables` | M3/M4 | Struct invariant on the trusted DTE model; bridges V3 to hardware axioms A1/A3 | **no subsystem exists** (same stub) |
| **V5** | Reclaim / stale-IOTLB safety: a frame is returned to the free pool only after it is unmapped from **all** IO/CPU tables **and** the IOTLB/DTE cache is invalidated to completion | M4 | Ghost "in-flight invalidation" token; frame free-list disjoint from any live `dma_reach` | **no subsystem exists** (no IO page tables, no IOTLB code) |
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
- It specified **page tables and IOMMU device tables that do not exist in this tree**. V3 and V4
  are properties of `crates/iommu-amdvi`, which is seven lines of doc comment with no functions,
  no tests, and no dependents. A spec for absent code cannot be wrong, which is precisely what
  makes it worthless: there is nothing it can fail against.

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
