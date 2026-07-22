# docs/verification.md — Verus verification ladder & proof-engineering plan

Status: design sketch, nothing here verifies yet. The Verus fragments below are **illustrative** — they name the right idioms, specs, and proof obligations so a Verus-literate engineer can start typing, but they will not pass `verus` as-is (missing lemmas, trigger tuning, and vstd glue are elided). Treat this as the map, not the territory.

Scope reminder (do not re-litigate): the guarantee we verify is **isolation / DMA-containment**. GPU compute correctness is permanently out of scope. The C++ `lite::` gfx1201 driver and its GPUVM are **untrusted user processes** that emit only IOVAs; the nucleus owns the AMD-Vi IOMMU tables. Under plain VFIO (M0–M2) the *host* programs the physical IOMMU, so the nucleus IOMMU proof is not load-bearing until **M3 (emulated vIOMMU)** and **M4 (bare-metal AMD-Vi)**.

---

## 1. The invariant ladder V1–V7

Each rung is a machine-checked property of the nucleus. Lower rungs are lemmas the upper rungs consume. "Load-bearing at" is the milestone where a *failure* of that invariant would actually breach isolation on real hardware (before that, the host IOMMU or the absence of the feature covers us).

| ID | Property | Load-bearing at | Technique | State |
|----|----------|-----------------|-----------|-------|
| **V1** | Nucleus-core memory safety: no UB in safe nucleus code; every `unsafe` sits behind a trusted, spec'd boundary | M1 | Verus default (ownership + `PointsTo` permissions), big-lock | not started |
| **V2** | `reachable(AS) == capabilitied(AS)` for every address space, **preserved** across `map/unmap/grant/revoke` | M2 | Flat permission map + ghost `path`/`subtree` fields to de-recursify the radix tree | not started |
| **V3** | `dma_reach(GPU_domain) ⊆ authorized(GPU_domain)` — the crux | M3, hardened M4 | Same flat-map machinery over the **IO** page tables; `authorized` = GPU domain's frame caps | not started |
| **V4** | DTE-config invariant: for the GPU's BDF, `V=1 ∧ TV=1 ∧ translation-on ∧ bypass-off ∧ ATS-off ∧ root==our-tables` | M3/M4 | Struct invariant on the trusted DTE model; bridges V3 to hardware axioms A1/A3 | not started |
| **V5** | Reclaim / stale-IOTLB safety: a frame is returned to the free pool only after it is unmapped from **all** IO/CPU tables **and** the IOTLB/DTE cache is invalidated to completion | M4 | Ghost "in-flight invalidation" token; frame free-list disjoint from any live `dma_reach` | not started |
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

## 2. Actual Verus specs for the three that matter (V2, V3, V4)

Shared ghost vocabulary (in `nucleus/src/model/mod.rs`, `spec`-only, compiled away):

```rust
verus! {

pub struct Frame { pub base: nat, pub order: nat }   // 2^order * 4KiB, base 4KiB-aligned
pub struct VPage(pub nat);                            // 4KiB virtual page index
pub struct IOVA(pub nat);                             // device virtual address
pub struct PAddr(pub nat);

pub struct Rights { pub r: bool, pub w: bool, pub x: bool }

// Every 4KiB physical page covered by a frame.
pub open spec fn frame_pages(f: Frame) -> Set<PAddr> {
    Set::new(|pa: PAddr| f.base <= pa.0 < f.base + (pow2(f.order) * 4096))
}

}
```

### 2.1 V2 — reachable == capabilitied, preserved across map/unmap

The **flat permission map** is the whole trick. Instead of a `spec fn reachable(tree)` that recurses over four levels of radix nodes (which forces `decreases` on tree height and makes every map/unmap proof re-walk the tree), we keep a ghost `Map<VPage, Mapping>` that *is* the set of leaf mappings, and prove **once** that the concrete radix tree refines it.

```rust
verus! {

pub struct Mapping { pub frame: Frame, pub rights: Rights }

pub struct AddrSpaceModel {
    pub mappings: Map<VPage, Mapping>,   // flat: the leaves, de-recursified
    pub caps: Set<Frame>,                // frames this AS legitimately holds
}

// THE V2 INVARIANT.
pub open spec fn reachable_is_capabilitied(m: AddrSpaceModel) -> bool {
    forall|vp: VPage| #[trigger] m.mappings.dom().contains(vp)
        ==> m.caps.contains(m.mappings[vp].frame)
}

// map preserves V2 iff you already own the frame and aren't clobbering.
pub proof fn lemma_map_preserves(m: AddrSpaceModel, vp: VPage, map: Mapping)
    requires
        reachable_is_capabilitied(m),
        m.caps.contains(map.frame),               // must hold the cap FIRST
        !m.mappings.dom().contains(vp),           // no silent overwrite
    ensures
        reachable_is_capabilitied(AddrSpaceModel {
            mappings: m.mappings.insert(vp, map), ..m
        }),
{ }  // Z3 closes: only key `vp` is new, its frame is in `caps` by requires.

// revoke removes a frame from caps; V2 forces every mapping of it to be gone.
pub proof fn lemma_revoke_requires_unmapped(m: AddrSpaceModel, f: Frame)
    requires
        reachable_is_capabilitied(m),
        forall|vp: VPage| m.mappings.dom().contains(vp) ==> m.mappings[vp].frame != f,
    ensures
        reachable_is_capabilitied(AddrSpaceModel { caps: m.caps.remove(f), ..m }),
{ }

}
```

The concrete side — the radix tree carries **ghost linking fields** so each node's invariant is *local* (references only its children one level down), never the whole tree:

```rust
verus! {

// One 512-entry node of the concrete 4-level tree.
pub struct PtNode {
    pub entries: [PtEntry; 512],
    pub ghost path: Ghost<Seq<u16>>,       // 9-bit indices from root to here
    pub ghost subtree: Ghost<Set<Frame>>,  // EXACTLY the leaves reachable below
}

// Local node invariant: subtree = union of children's subtrees (one hop).
pub open spec fn node_inv(node: PtNode, level: nat, children: Map<u16, PtNode>) -> bool {
    &&& forall|i: u16| present(node.entries[i as int]) && level > 0 ==>
            children.dom().contains(i)
            && children[i].path@ == node.path@.push(i)
            && children[i].subtree@.subset_of(node.subtree@)
    &&& forall|i: u16| present(node.entries[i as int]) && level == 0 ==>   // leaf
            node.subtree@.contains(leaf_frame(node.entries[i as int]))
}

}
```

The **refinement lemma** — proved once, by induction on `level` with `decreases 4 - level` — establishes `root.subtree@ == flatten(mappings)`, i.e. the flat map faithfully models the tree. After that, `map_page`/`unmap_page` reason **only** against the flat map (constant work per op) and cite the refinement lemma to update the concrete tree. This is the Atmosphere technique verbatim: pay the recursion tax once, then stay flat.

Exec entry point:

```rust
verus! {
impl AddrSpace {
    pub fn map_page(&mut self, vp: VPage, frame: Frame, rights: Rights)
        requires
            old(self).inv(),                                  // includes V2
            old(self).g@.caps.contains(frame),
            !old(self).g@.mappings.dom().contains(vp),
        ensures
            self.inv(),
            self.g@.mappings == old(self).g@.mappings.insert(vp, Mapping{frame, rights}),
    {
        // 1. walk/allocate L3..L0 nodes (updates ghost path on each new node)
        // 2. write the leaf PTE (unsafe MMIO/DRAM store, behind trusted stub §3)
        // 3. proof: lemma_map_preserves + refinement lemma re-link subtree ghosts
    }
}
}
```

### 2.2 V3 — DMA-reach ⊆ authorized (the crux, load-bearing at M3/M4)

Structurally identical to V2 but over the **IO** page tables the nucleus owns for the GPU domain. GPUVM (untrusted) hands the device IOVAs; the device DMAs through them; AMD-Vi translates via *our* tables. The theorem: whatever the untrusted stack does, the device cannot reach a physical page outside the GPU domain's cap set.

```rust
verus! {

pub struct IommuDomainModel {
    pub leaves: Map<IOVA, IoLeaf>,    // flat IO leaves (same de-recursify trick)
    pub table_frames: Set<Frame>,     // frames holding the IO page tables themselves
}
pub struct IoLeaf { pub frame: Frame, pub rights: Rights, pub present: bool }

pub open spec fn translate(d: IommuDomainModel, iova: IOVA) -> Option<PAddr> {
    if d.leaves.dom().contains(iova) && d.leaves[iova].present {
        Some(pa_of(d.leaves[iova].frame, iova))
    } else { None }
}

// Everything the device can physically touch through the IOMMU.
pub open spec fn dma_reach(d: IommuDomainModel) -> Set<PAddr> {
    Set::new(|pa: PAddr| exists|iova: IOVA| #[trigger] translate(d, iova) == Some(pa))
}

// THE V3 INVARIANT (with the self-protection clause).
pub open spec fn dma_confined(d: IommuDomainModel, authorized: Set<PAddr>) -> bool {
    &&& dma_reach(d).subset_of(authorized)
    // the IOMMU's own tables must be UNREACHABLE by the device it governs,
    // else a compromised GPUVM could DMA-rewrite its own translations:
    &&& forall|f: Frame| d.table_frames.contains(f)
            ==> frame_pages(f).disjoint(dma_reach(d))
}

// Installing a mapping keeps confinement iff the frame is authorized.
pub proof fn lemma_iommu_map_confined(
    d: IommuDomainModel, iova: IOVA, leaf: IoLeaf, authorized: Set<PAddr>)
    requires
        dma_confined(d, authorized),
        frame_pages(leaf.frame).subset_of(authorized),   // ONLY authorized frames
        leaf.present,
    ensures
        dma_confined(IommuDomainModel { leaves: d.leaves.insert(iova, leaf), ..d },
                     authorized),
{ /* insert adds only pa's already in `authorized`; disjointness preserved */ }

}
```

**Why this is load-bearing and where.** The nucleus is the *sole writer* of `leaves`. Every write goes through an exec `iommu_map` whose `requires` is `frame_pages(leaf.frame).subset_of(authorized)`, and `authorized` is derived from the GPU domain's capability set (tied back to V2's `caps`). So a fully compromised `lite::`/GPUVM — feeding arbitrary IOVAs, arbitrary descriptors — still cannot enlarge `dma_reach` beyond `authorized`. At M0–M2 the host IOMMU already enforces this, so the proof is "just" future-proofing; at **M3** (we emulate the vIOMMU) and **M4** (we drive real AMD-Vi) it is the *only* thing standing between the GPU and other guests' RAM.

### 2.3 V4 — DTE-config invariant (bridges V3 to silicon)

V3 is a statement about our *model* of translation. It only corresponds to reality if the AMD-Vi Device Table Entry actually routes the device through those tables — translation on, no bypass, no ATS pre-translated shortcut. V4 is the struct invariant that makes that link, and it is exactly where hardware axioms A1/A3 (§4) get consumed.

```rust
verus! {

pub enum PagingMode { Passthrough, Level4, Level5 }  // AMD-Vi Mode field

pub struct Dte {
    pub v: bool,               // entry Valid
    pub tv: bool,              // Translation Valid (page tables present)
    pub mode: PagingMode,      // must NOT be Passthrough
    pub ats_enabled: bool,     // must be false
    pub root_ptr: PAddr,       // must equal our domain root
    // IntCtl/PASID/PRI fields modeled but pinned to "off" for the GPU BDF
}

pub open spec fn dte_confining(dte: Dte, domain_root: PAddr) -> bool {
    &&& dte.v && dte.tv                       // valid + translating
    &&& dte.mode != PagingMode::Passthrough   // translation ON, no identity/bypass
    &&& !dte.ats_enabled                       // ATS OFF: no pre-translated TLPs
    &&& dte.root_ptr == domain_root            // points at OUR owned tables
}

}
```

The device-table write path has `ensures dte_confining(self.dte@, self.domain_root@)`, and the M5 assurance case reads: *given A1 (hardware honors a confining DTE) and A3 (ATS-off blocks bypass), `dte_confining` + V3 ⇒ every physical request the gfx1201 emits lands in `authorized`.*

---

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

**CI job** (`.github/workflows/verus.yml`), runs on **every** PR touching `nucleus/`:

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
