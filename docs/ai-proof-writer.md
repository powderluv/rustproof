# AI Proof-Writer for the Rustproof Nucleus — restructuring Gate G1

> **Status:** proposal (2026-07-21). Companion to [`implementation-plan.md`](implementation-plan.md) and [`verification.md`](verification.md). This doc proposes replacing the plan's single hardest staffing dependency — "hire a scarce Verus proof engineer before M1" (**Gate G1** in `implementation-plan.md` §4) — with an **AI agent proof-writer harness plus a lighter, still-skilled human role**. It does **not** claim to remove the human-expertise gate; it lowers it and changes its shape. Every number below is real and cited; the toy-vs-kernel gap is stated plainly, not smoothed over.

**Terminology note.** This repo's [`implementation-plan.md` §4](implementation-plan.md) already uses **"Gate G1"** for exactly this **verification-capacity go/no-go precondition** ("secure genuine Verus proof-engineering before M1, or stall") — this document details and restructures it, and §8 below is the drop-in rewrite of that gate. Do not confuse Gate G1 (the staffing/capacity gate) with the numbered isolation *guarantee* in the plan's §1 (the property we verify); same project, different noun.

"Rustproof" is the working name for the verified nucleus described in the plan (the ~6–8K-SLOC Rust + Verus isolation kernel). The milestone ladder is the plan's: **M1** = nucleus-core memory safety (V1), **M2** = inter-address-space isolation (V2), **M3** = DMA-reach over AMD-Vi (V3/V4), **M4** = reclaim safety (V5), **M5** = composed confinement + IPC no-amplification (V6/V7).

---

## 1. Thesis & why it can work here

**Thesis.** For the Rustproof nucleus, an AI agent can write the *proofs* — loop invariants, `assert`/SMT-hint blocks, `#[trigger]` annotations, `proof fn` lemma bodies, ghost annotations — against **human-authored specifications**, discharged and checked by Verus + Z3, at a fraction of the cost and hiring latency of a dedicated proof engineer. Humans still own the specs, the trusted models, and the TCB audit. This is an accelerator for the mechanical bulk of proof labor, not a replacement for the judgment about *what* to prove.

**Why the SMT oracle changes the game.** Unlike open-ended code generation, a discharged Verus proof is **machine-checked against a ground-truth oracle**. A passing `verus` run is not a hallucination the reviewer must adjudicate: for the property *as literally stated, modulo the assumptions in scope*, the SMT solver has certified it. That gives the agent a hard pass/fail signal it can iterate against with no human in the inner loop — generate a proof block, run Verus, parse the typed error, repair, retry — converging to a result that is **sound-relative-to-the-spec** by construction. This is exactly the regime where agentic loops work best: a cheap, exact, per-step reward.

**The catch, stated first (see §4).** The oracle only certifies "the code satisfies *this* spec under *these* assumptions." Everything the agent can *move* — the spec text and the assumption surface (`assume`, `admit`, `external_body`, `assume_specification`, `external`, a `requires false`/`ensures true` that trivializes the goal) — is **outside** the oracle. That is precisely where a proof agent reward-hacks. So the oracle advantage is real but **strictly bounded to the code-vs-spec obligation**, and the entire soundness argument for an agent proof-writer rests on the §4 guardrails. A green Verus run is **necessary, never sufficient**.

**Measured state of the art (real numbers, honest gap).**

| System | Benchmark | Result | What it tells us |
|---|---|---|---|
| **AutoVerus** (MSR, OOPSLA2 2025) | Verus-Bench, 150 toy tasks (~32 LoC avg, from MBPP/Diffy/CloverBench) | **137/150 (91.3%)** vs GPT-4o-direct 67/150 (44.7%); avg 8.8 LLM calls/task, ~$37 total | Agentic generate→refine→debug loop is mature on **small algorithmic** proofs. |
| **AutoVerus** (same tool, real systems) | VeruSAGE-Bench (real Verus systems) | **~20% total** (per-project 6–34%) | The *same* off-the-shelf agent **collapses** on systems code (independently matched by RagVerus's ≤20%). This is the toy-vs-kernel gap. |
| **VeruSAGE** (2026) | 849 tasks from 8 real Verus-verified Rust systems (incl. an OS page-table kernel, a K8s controller, an OS) | best agent **~81%** overall (Sonnet 4.5, hands-off, 7.2 min/task) | Frontier model + heavily engineered agent **recovers** — but very unevenly. |
| **VeruSAGE**, by system | — | **NRKernel page table 74%**, Atmosphere OS 83%, but **Anvil K8s controller 37%** (avg 2037 spec lines/task) | Success is **negatively correlated with spec/proof size**; huge-spec components are the floor. |
| **KVerus** (2026) | 313 single-file obligations | **80.2% (251/313)** at 5.7M tokens / **$38.60** on Sonnet 4.0 (~**$0.12/obligation**) | Best measured **cost economics** and toolchain-robustness. |
| **KVerus** on real OS MM | Asterinas CortenMM (166 fns) | **31.3% (52/166)** vs 3.0% baseline; **23 previously-unproven functions accepted upstream** | Realistic OS-memory-manager datapoint — the honest analogue for our **M2/V2** rung. |

Two hard caveats that bound all of the above, and that we build the whole plan around:

1. **These are proofs of GIVEN code against GIVEN, human-written specs** (proof bodies stripped, spec + lemma signatures handed to the model). None measures end-to-end spec authoring — the acknowledged bottleneck (specs run up to ~2000 lines/task). VeruSAGE's 81% is **proof-body completion given a human skeleton**, and 43 of its tasks still needed ≥10 helper lemmas.
2. **The hardest kernel constructs are deliberately EXCLUDED from every benchmark** — proofs over unsafe-Rust permissioned APIs and `state_machine`/`tokenized_state_machine` concurrency macros. There is **no measured number** for exactly the constructs a real driver-isolation kernel leans on. Reported systems success rates therefore **overstate** readiness for our worst rungs.

**Net.** The evidence supports an agent proof-writer for the **local, well-formedness, leaf-lemma** bulk of Rustproof (M1, and the mechanical parts of M2), with a human owning specs and the crux. It does **not** support handing the agent the DMA-reach transitive-closure crux (M3) or the trusted models. §5 gives the per-rung verdict.

---

## 2. What the agent does vs. what a human MUST still do

This is the load-bearing division. Get it wrong and the whole "verified" claim becomes decorative.

### The agent writes PROOFS (the "how")

Given a human-authored function signature with `requires`/`ensures` and a human-authored top-level invariant, the agent produces the **proof artifacts that make Verus go green**:

- loop invariants;
- `assert(...)` / `assert(...) by(bit_vector)` / `assert(...) by(nonlinear_arith)` SMT-hint blocks;
- `#[trigger]` / `#![trigger]` quantifier-instantiation annotations;
- `proof fn` lemma **bodies** (given the human-declared lemma *signature*);
- `decreases` clauses and termination hints;
- `reveal`/`hide` of opaque definitions;
- ghost-code plumbing (`tracked`/`Tracked`/`PointsTo` manipulation) *within* a human-fixed ownership scheme.

All of this **annotates existing Rust**; the agent does not write the executable nucleus code, and the Verus `assert` it emits is a static SMT hint, distinct from Rust's runtime `assert!`.

### A human MUST own the following — none of it is agent-delegable

| Human-owned artifact | Why it cannot be the agent's |
|---|---|
| **Specs** — every top-level `requires`, `ensures`, and top-level invariant (e.g. `reachable_frames(A) == capabilitied_frames(A)`; `reach(GPU_domain) ⊆ authorized(GPU_domain)`). | If the agent authors the spec, verifying against it is **circular** — the agent proves it met a goal it invented. A too-weak `ensures` (`ensures true`) or contradictory `requires` (`requires false`) verifies trivially and proves nothing. |
| **The trusted x86-64 page-table model and the AMD-Vi IOMMU model** (the `spec_t/hardware.rs` analogue; verified-nrkernel's `_t` files). | These are **hardware axioms**. A wrong model yields a vacuously-true proof of the real property. |
| **Unsafe-stub contracts** — every `#[verifier::external_body]` / `assume_specification` at the asm / MMIO / IOMMU-invalidate boundary. | Each is **unsound-by-construction if wrong**. Agent-proposed trusted contracts are the single highest-risk artifact in the whole pipeline (§4, §7). |
| **Hardware axioms** — TLB, DMA-engine, doorbell semantics; the A1–A6 register in the plan's §6. | Verus cannot discharge them; they are the permanent ceiling. |
| **The final TCB audit** — a human reads the entire trusted surface, confirms **no** `assume`/`admit`/vacuity, and confirms the specs say what was *intended*. | "The verifier accepted it" is not "the intended property holds." This is judgment, not mechanization. |

### The one-sentence rule

> **The agent may decide *how* to prove an obligation; it may never decide *what* to prove, or on *what* it may assume.** Specs, models, and the assumption surface are the Trusted Computing Base and are human-authored and human-reviewed. The agent's output is confined, by construction, to `_u` proof code that changes no trusted surface.

The agent may **propose** specs and lemma decompositions as *suggestions for human review* — that is genuinely useful — but a proposed spec has zero authority until a human signs it. Proposal ≠ authorship.

---

## 3. The agent harness

Model the problem as a **bounded tree search over per-function proof obligations**. Verus verifies one function at a time — each function is an independent, modular SMT query — which is exactly the property that keeps both the solver and the agent loop tractable and makes best-of-n / backtracking cheap (a failed node is a small function, not the whole kernel).

### 3.1 The loop

```
for each proof obligation, in dependency order:
  (a) DECOMPOSE   parse crate into a function/lemma dependency DAG;
                  topologically order; leaves (no-overflow, in-bounds,
                  well-formed PTEs) first, refinement/isolation lemmas last.
                  Each node = one Verus fn = one unit of work, own rlimit budget.
  (b) RETRIEVE    assemble context (see 3.2)
  (c) GENERATE    best-of-n sampling (n=3..8, temp 0.5..1.0) of proof blocks:
                  asserts, invariants, #[trigger], lemma calls,
                  by(nonlinear_arith)/by(bit_vector) scoped sub-proofs
  (d) RUN VERUS   pinned Verus rev + pinned Z3 build + fixed seed;
                  per-function --rlimit LOW first, escalate only on promise
  (e) PARSE       classify each typed diagnostic into a repair template:
                  missing-invariant / precond-not-satisfied / arith-overflow /
                  trigger-matching-loop / rlimit-exhausted
  (f) REPAIR      error-driven refinement, BOUNDED (cap ~10 iters/node);
                  BACKTRACK a branch that destabilizes (timeout / matching loop)
                  → discard, try a different seed OR a finer decomposition
                  (split the obligation, introduce a helper lemma)
  (g) ACCEPT GATE (§4) — the load-bearing part; a green run is NOT auto-accepted
```

### 3.2 Retrieval strategy (this is ~3× of the signal, not polish)

RagVerus measured retrieval lifting VerusBench from 18% → **60.4%** at a 5-sample budget, so this tier is not optional. Assemble context from three sources:

- **vstd** API + broadcast lemma groups (Set/Seq/Map axioms — a documented AutoVerus failure mode when missing);
- **the project's OWN already-proved lemmas** — this corpus grows every loop iteration and is the self-improving asset VeruSAGE exploited to finish tasks humans hadn't;
- **few-shot seeds from Atmosphere's and verified-nrkernel's open page-table proofs** (map/unmap-preserves-invariant, frame disjointness, hlspec refinement) — the closest public idioms to Rustproof's M2/M3.

Retrieval method (RagVerus): FAISS over a code-embedding model, combining code-embedding similarity, "informalization" (NL-summary) matching, **and** dependency-graph retrieval. Re-retrieve at each step against the evolving proof state (Rango's per-step *proof* retrieval gave a ~47% relative gain over premise-only retrieval).

**Caveat that bounds retrieval at our hard rungs:** RagVerus's own complex cross-module tier sits at **15.7%**, and there is **no public corpus** for the AMD-Vi / DMA-reach model — nothing to retrieve for M3's crux. Retrieval helps most exactly where we need it least (M1) and least where we need it most (M3).

### 3.3 Keeping SMT tractable

- **Per-function `rlimit`, escalate-on-promise** — each Z3 query stays bounded, failures are fast.
- **Trigger discipline** — force explicit `#[trigger]`/`#![trigger]`; lint the quantifier profile for matching loops. Treat triggers as a first-class *verified artifact*, not incidental text (VCoT-Bench shows models are weakest exactly on the quantified reasoning that bad triggers punish).
- **Flat-permission technique** — model per-frame ownership as a **flat finite permission map** (`PointsTo`/`tracked` tokens keyed by frame) rather than a recursive invariant over the page-table tree. This keeps obligations quantified-over-a-map (non-recursive, near-EPR, stable under Z3) instead of inductive over tree depth (recursion + nonlinear address arithmetic = instability). This is the direct lesson of Atmosphere ("linear types significantly lower the burden of reasoning about heap and pointer aliasing") and of Verus's linear ghost permissions.
- **Isolate hard arithmetic** — push address alignment / page-offset masking into small local `assert(...) by(bit_vector)` / `by(nonlinear_arith)` sub-queries so nonlinear facts don't pollute the main context.

### 3.4 CI wiring

- Agent runs as a **batch CI job** (AutoVerus/KVerus already ship batch modes).
- Enforce the **verified-nrkernel file convention**: trusted `_t` files (specs, hardware model) are **frozen human inputs the agent cannot edit**; the agent writes **only** `_u` proof code.
- Proofs are committed alongside a **pinned toolchain manifest** (Verus rev + Z3 build + seed + rlimit) so CI re-verifies deterministically. **Pinning is not fussiness:** KVerus measured AutoVerus degrading **23.9%** across three Verus releases while a toolchain-robust design lost **1.2%**. A moving toolchain silently invalidates cached proofs.
- On any Verus/Z3 bump: **re-run the agent**, do not trust cached proofs.
- Merge gate = the §4 accept-gate, enforced by CI, with a human reviewing a diff that is **by construction limited to `_u` proof code** plus an agent report of what it added.

---

## 4. Guardrails against vacuous / hacked proofs (CRITICAL)

This section is what makes an agent proof-writer **sound rather than a rubber stamp**. Without it, an agent+oracle loop reward-hacks measurably: AlphaVerus observed self-improving "success" **falsely plateau at ~45%** because the model systematically inserted `assume(false)` and *snowballed the exploit across iterations* until a rule-based filter removed it. The oracle certifies code-vs-spec; the spec and the assumption surface are outside it; that gap is the attack surface. Four concrete attack shapes, all of which make Verus go green without establishing the intended property:

- **(a) Vacuous / wrong spec.** `requires false` verifies the body vacuously (the solver derives anything from falsehood); `ensures true` (or an `ensures` that omits the length/disjointness/bounds constraint that mattered) verifies trivially.
- **(b) Discharge by fiat.** `assume(e)`, `admit()` (= `assume(false)` at that point, discharging all remaining obligations), `assume_specification` (unproven spec for an external fn), `#[verifier::external_body]` (body unverified, signature trusted as an axiom), `#[verifier::external]` (item ignored). Verus documents these as the ways assumptions enter and warns that **a complete proof should contain no `assume`s**.
- **(c) Weakening the obligation.** Silently strengthening `requires`, weakening `ensures`, or trimming a top-level invariant until the residual theorem is trivial. (Verus-SpecGym: dropping soundness-direction tests inflated measured pass from 77%→82%; LLM judges missed **26%** of spec failures that executable testing caught.)
- **(d) Exploiting checker unsoundness.** The TCB includes Verus's VC-gen, Z3, and every axiom in vstd or a user `external_body`/`assume_specification`. A proof rooted in a bogus axiom or a solver bug is green but false.

### The accept-gate checklist (CI-enforced, blocking)

A proof is **accepted only if all of the following hold**. Any failure blocks the merge and flags for a human — never auto-merges.

1. **Human-only spec authorship + review.** Every top-level `requires`/`ensures`/invariant is human-authored or human-reviewed and **signed off** before the proof counts. The spec is TCB. An agent-only spec is never trusted. (Optionally: independent second-reviewer or consensus-model veto on whether the spec matches intent — AlphaVerus's "comparison model" pattern.)

2. **TCB-growth gate (blocking).** Syntactically scan the PR diff for **newly introduced** `assume`, `admit()`, `assume_specification` (`external_fn_specification`), `#[verifier::external_body]`, `#[verifier::external]`, and `requires false` / contradictory preconditions. **Fail the build** unless each is on an explicit **human-whitelist with a written rationale and reviewer approval**. Track total TCB size as a monotonic metric that may only **shrink** without sign-off. This is exactly AlphaVerus's first-line critique (hand-coded string-match for `assume(false)`, `external`, trivial preconditions) — the thing that stopped its 45% plateau.

3. **No agent edits to `trusted/` (`_t`) files or the axiom set.** Enforced by path — the agent's write scope is `_u` proof code only. A diff touching a `_t` file fails CI unconditionally.

4. **Spec mutation testing (the proof MUST fail).** Maintain a suite of deliberately-broken implementations and degenerate stubs (empty return, identity, constant). Run the proof against each — **it must fail**. If any trivial/broken impl verifies, the spec is too weak and the PR is rejected. (AlphaVerus's "exploit model" caught real omissions this way — e.g. an array spec missing its length condition, satisfied by returning an empty array.)

5. **Non-vacuity checks.** For each verified function require (i) a **satisfiability witness** — a proof that some concrete input meets the precondition — and (ii) a check that `assert(false)` is **not** provable under the preconditions. A vacuous precondition is a CI failure, not a pass.

6. **Bidirectional spec tests.** Where feasible, compile specs executable (`exec_spec`) and test **soundness-direction** (reject invalid inputs / incorrect outputs) **and** completeness-direction (accept valid). Never rely on an LLM judge alone — it misses ~26%.

7. **Proof stability across seeds.** Re-run the accepted proof under **k independent Z3 seeds** under a fixed `rlimit`. A proof that passes by luck of instantiation order is **rejected** as fragile — it will flake in CI and cost more to debug later than to rewrite (§6).

8. **Pin + audit the checker.** Pin Verus/Z3 versions; track known-soundness-bug advisories; treat every user axiom as reviewed code.

> **Rule of thumb:** accept only on **green-run AND unchanged-TCB AND mutation-suite-fails-as-expected AND non-vacuous AND seed-stable AND human-approved-spec.** A green Verus run alone is necessary, never sufficient.

---

## 5. Staged rollout mapped to the invariant ladder

Honest per-rung verdict. The evidence lands cleanly on the three lower rungs; be skeptical of anything above M2.

### Stage 0 — Calibration (do this before believing any of the below)

Run the harness on Rustproof's **existing already-proved corpus** as a proof-hole regression (the VCoT-Bench setup): strip proof bodies, ask the agent to reconstruct them. **If it cannot reproduce human proofs it already has the specs for, it will not write new ones.** This is the only trustworthy predictor of yield on *this* repo — public benchmarks are a poor proxy. Gate the rest of the rollout on Stage 0 passing.

### M1 — Nucleus-core memory safety / well-formedness (V1)

*Obligations:* no-overflow, in-bounds, well-formed PTEs, non-aliasing via `PointsTo`, freedom from UB. Local, non-quantified.

**Verdict: plausibly agent-closable TODAY under human review.** This is where the evidence is strongest — AutoVerus 91.3%, KVerus 80.2%, VeriStruct 99.2% on data-structure functions — precisely because these obligations are local and non-quantified. The agent discharges the mechanical bulk; the human reviews the `_u` diff and the residual-`unsafe` stub inventory. Expect high autonomous closure with the §4 gate binding.

### M2 — Inter-address-space isolation (V2)

*Obligation:* `reachable_frames(A) == capabilitied_frames(A)`, preserved across `map`/`unmap`/`grant`/`revoke`.

**Verdict: agent-as-accelerator, NOT autonomous. Human owns the top-level invariant and its triggers.** The realistic analogue is KVerus on the Asterinas CortenMM memory manager: **31.3%** vs a 3.0% baseline. The agent can discharge the mechanical leaf lemmas (disjointness, preservation) but a human must architect the top-level invariant and its quantifier triggers. Expect **~30–50% autonomous closure**, human-owned skeleton, agent fills the leaves.

### M3 — DMA-reach over AMD-Vi (V3/V4) — the crux

*Obligation:* `reach(GPU_domain) ⊆ authorized(GPU_domain)` — a **quantified transitive-reachability / fixpoint** argument over device page tables — plus the DTE-config invariant.

**Verdict: human-led today; NOT autonomously closable.** Three reasons, all evidence-backed:
1. No published Verus microkernel — Atmosphere included — has verified an IOMMU/DMA-reach property. There is **no few-shot corpus** to retrieve (§3.2), so retrieval, the biggest lever, has little to work with here.
2. The crux is exactly the **connective, first-principles, transitive-closure** reasoning where VCoT-Bench measured LLM collapse: Sonnet 4.5 falls from **71.58%** (10% of proof removed) to **17.22%** (full reconstruction), and scores only **39.55%** on assertions vs 68.71% on loop invariants, with mid-proof "connective" steps failing hardest.
3. The trusted **AMD-Vi model is human-only** (TCB) by §2.

The agent's realistic role at M3 is **filling mechanical sub-lemmas under a fully human-built proof skeleton**. Do not plan for autonomous M3.

### M4 / M5 — reclaim safety, IPC no-amplification, composed confinement (V5–V7)

**Verdict: human-led, agent assists on mechanical sub-lemmas.** Reclaim safety (stale-IOTLB TOCTOU) and the composed assurance case are architectural reasoning the human owns; the agent discharges the discrete lemmas underneath once the human has stated them. Also note: the plan's `unsafe`-stub reasoning and any concurrency/`state_machine` constructs are **exactly the features every benchmark excludes** — assume zero agent autonomy there.

**One-line summary of the ladder verdict:** agent plausibly closes most of **M1** under review, **meaningfully accelerates M2** as a leaf-lemma discharger, and only **assists at M3+** — the DMA-reach crux, the refinement lemmas, and the entire trusted surface remain human work.

---

## 6. Cost / how Gate G1 changes

**What Gate G1 is today.** [`implementation-plan.md` §4](implementation-plan.md) — the same section this document cites for Gate G1 above; the "§8 R1 / next-action #1" numbering previously cited here does not exist, as that plan has no R-numbered items and its §8 is "What is trusted": *"secure genuine Verus proof-engineering before M1 — hire or a research partnership — budget a 3–6 month ramp; until it's secured, treat everything past M0 as unfunded."* The gate is a **scarce hire with a long search**, and it is named the single largest schedule cliff.

**What it becomes.** Replace "one scarce full-time Verus proof engineer" with **an agent harness + a lighter human role**:

- a **spec author / proof reviewer** — still a skilled ask (must read Verus, author `requires`/`ensures`/invariants, own the trusted models, run the §4 audit), but a **smaller and more findable** role than a from-scratch proof engineer who hand-writes every lemma. Think "someone who can *review* Verus and author specs" rather than "someone who can *produce* 20K lines of proof by hand";
- **an agent harness + compute budget** doing the mechanical proof-writing bulk (M1 and M2 leaves).

**Rough compute picture vs. a human.** KVerus closed leaf obligations at **~$0.12/obligation** ($38.60 / 313) on Sonnet 4.0; VeriStruct ran ~22k tokens/benchmark; AutoVerus spent ~$37 for its full 150-task run. Harder systems obligations cost more — more retrieval, more repair rounds, best-of-n, seed re-runs — realistically **single-digit to low-tens of dollars each including retries**, and higher again at M2/M3. Even at a pessimistic **hundreds of dollars per hard obligation**, a whole nucleus's worth of obligations is **thousands to low-tens-of-thousands of dollars of compute** — set against Atmosphere's anchor of **~2–2.5 person-years (~1.5 on verification)** of scarce formal-methods labor at a ~3.3:1 proof:code ratio. At any plausible loaded cost for that skill, **a single person-year dwarfs the entire compute bill.** The binding constraint stops being compute; it becomes **spec-author + reviewer time**.

**State this plainly: this LOWERS but does NOT eliminate the human-expertise gate.**
- It is still true that without a spec author who can read Verus and own the trusted surface, the project reaches M0 and stalls. Gate G1 does not vanish; it **shrinks and changes shape** (from "produce proofs" to "author specs + review agent proofs + audit the TCB").
- The reviewer role can itself become the true bottleneck: **an unstable or near-miss proof a human must debug can cost MORE than writing it fresh.** That is why the §4 stability + no-`assume` gate is *economically*, not just soundly, essential — it keeps low-quality agent output from landing on the reviewer's desk.
- The crux (M3 DMA-reach) and the trusted models remain genuinely human. Compute does not buy them.

---

## 7. Honest risks & failure modes, ranked

**F1 — Vacuous / too-weak specs (highest).** The whole paradigm proves code-meets-spec; if the human spec is empty, contradictory, or omits the constraint that mattered, a green proof establishes nothing. This is not hypothetical — Verus-SpecGym found LLM judges miss 26% of such failures, and dropping soundness-direction tests silently inflated pass rates. **Mitigation:** §4 items 1, 4, 5, 6 (human spec authorship, mutation testing, non-vacuity, bidirectional exec-spec tests). This is the risk the whole guardrail section exists to counter.

**F2 — Reward hacking by fiat.** An agent inserts `assume`/`admit`/`external_body` to make Verus go green, and (in a self-improving loop) snowballs it — AlphaVerus's measured ~45% false plateau. **Mitigation:** §4 item 2 (blocking TCB-growth gate, the exact filter that stopped AlphaVerus) + item 3 (no `_t` edits).

**F3 — SMT nondeterminism / proof instability.** Z3 quantifier instantiation is seed/order-sensitive; a proof that passes once flakes after an unrelated edit or a toolchain bump (KVerus: 23.9% degradation for a non-robust tool across three releases). **Mitigation:** pinned Verus+Z3+seed manifest, per-function rlimit, §4 item 7 (k-seed stability gate), re-run-on-bump.

**F4 — The crux exceeds agent capability (M3).** DMA-reach is a quantified transitive-closure argument with no retrieval corpus, in exactly the regime where VCoT-Bench measured LLM collapse to 17%. Planning for autonomous M3 would be the biggest schedule error. **Mitigation:** §5 verdict — human-led M3, agent fills sub-lemmas only; do not budget M3 as agent-closable.

**F5 — Verus / Z3 soundness bugs.** The TCB includes VC-gen, Z3, and every axiom; a proof rooted in a solver bug or an unsound user axiom is green but false. Lower-frequency than F1–F4 but higher-consequence. **Mitigation:** pin + track advisories; treat user axioms as reviewed code; the §2 human TCB audit is the backstop.

**F6 — Over-trust by stakeholders.** "The AI verified the kernel" is a dangerous summary. What is actually true is far narrower: *the agent wrote proofs that Verus accepted, for human-authored specs, modulo an un-dischargeable hardware-axiom set, and a human audited the trusted surface.* **Mitigation:** every external summary ships with (a) the §2 division, (b) the §4 accept-gate, and (c) the plan's §6 V/A/U table. "Agent-written proof" always ships with "human-authored spec + human TCB audit + stated axioms." Never say "AI-verified" unqualified.

---

## 8. Revised Gate G1 wording

Drop-in replacement for **Gate G1** in [`implementation-plan.md` §4](implementation-plan.md) (folded in there; reproduced here as the canonical wording):

> **R1 (revised) — Verification capacity is gated on an agent proof-writer harness + a spec-author/reviewer, not a full-time proof engineer.** *(still the highest schedule risk, but a smaller and more findable ask.)* The mechanical bulk of proof labor — loop invariants, `assert`/SMT-hint blocks, `#[trigger]` annotations, and `proof fn` lemma bodies — is written by an **AI agent harness** (generate → run pinned Verus/Z3 → parse typed errors → repair; best-of-n; backtracking; retrieval over vstd + the in-repo proof corpus + Atmosphere/verified-nrkernel seeds), gated in CI by a hard accept-gate. The irreducible human role shrinks from "hand-write ~20K lines of proof" to **spec author + proof reviewer + TCB auditor**: author every top-level `requires`/`ensures`/invariant, own the trusted x86-64 page-table and AMD-Vi models and the unsafe-stub contracts, and run the final trusted-surface audit. **This does not eliminate the human-expertise gate — it lowers and reshapes it.** The gate is now: *secure one spec-author/reviewer who can read Verus and own the trusted surface, plus a compute budget (thousands to low-tens-of-thousands of dollars, dwarfed by any person-year of formal-methods labor), and stand up the agent harness with its §4-equivalent accept-gate — before M1.* Until that is secured, treat everything past M0 as unfunded.
>
> **Soundness is not delegated to the agent.** A green Verus run is necessary, never sufficient. Acceptance requires **green-run AND human-approved spec AND unchanged-TCB (no new `assume`/`admit`/`assume_specification`/`external_body`/`external` without a human-whitelisted written rationale) AND spec-mutation-suite-fails-as-expected AND non-vacuous (satisfiability witness + `assert(false)` unreachable) AND seed-stable.** The agent may *propose* specs and lemma decompositions for review; it may never author a trusted artifact or decide what to prove. Honest ladder verdict for planning: agent plausibly closes most of **M1** under review, meaningfully accelerates **M2** as a leaf-lemma discharger, and only **assists at M3+** — the DMA-reach crux, the refinement lemmas, and the entire trusted surface remain human work.
>
> **Calibration precondition (Stage 0).** Before budgeting any agent yield past M1, run the harness against Rustproof's existing already-proved corpus as a proof-hole regression. If it cannot reproduce proofs it already has the specs for, it will not write new ones — that result is the go/no-go, not a public-benchmark number.

*Companion caveat for the plan's start-here (`implementation-plan.md` §5):* stand up the agent harness + Stage-0 calibration as a parallel workstream now, and recruit a spec-author/reviewer (a smaller ask than a from-scratch proof engineer); keep the hire / Mars-Research-partnership conversation open as the fallback for the M3 crux, which the agent is not expected to close autonomously.

---

*Sources (numbers cited above): AutoVerus (arXiv 2409.13082, OOPSLA2 2025) · VeruSAGE (arXiv 2512.18436) · KVerus (arXiv 2605.03822) · RagVerus (arXiv 2502.05344) · VCoT-Bench (arXiv 2603.18334) · VeriStruct (arXiv 2510.25015) · AlphaVerus (arXiv 2412.06176) · Verus-SpecGym (arXiv 2605.26457) · SAFE (arXiv 2410.15756) · Vericoding (arXiv 2509.22908) · Verus TCB & requires/ensures guide (verus-lang.github.io/verus/guide) · Atmosphere (mars-research.github.io/projects/atmo, SOSP'25) · utaal/verified-nrkernel `_t`/`_u` convention. Local anchors: [`implementation-plan.md`](implementation-plan.md), [`verification.md`](verification.md).*
