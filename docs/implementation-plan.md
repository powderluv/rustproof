# docs/implementation-plan.md — Rustproof Master Implementation Plan

> Status: program plan, 2026-07-21. This is the top-level document that ties the six section docs together into one buildable program. It restates no decided facts to re-litigate them; it sequences the work, names the gates, and points at the detailed docs. Every "verified" claim below is qualified by the trusted computing base (TCB) it rests on — a green proof is never "the system is safe," it is "the system is safe *modulo* the listed trusted stub, the trusted spec, the pinned toolchain, and the hardware axioms."

Cross-links (read these for detail; this doc is the map, not the territory):
- [`docs/repo-structure.md`](repo-structure.md) — repo/Cargo workspace, toolchain pinning, boot path, how the untrusted C++ `lite::` driver is vendored and hosted.
- [`docs/milestone-M0.md`](milestone-M0.md) — the near-term critical path: boot the nucleus as a KVM guest and dispatch one gfx1201 wave through untrusted `lite::` (tasks T0.0–T0.11).
- [`docs/host-contract.md`](host-contract.md) — the nucleus ↔ untrusted-driver ABI (the 9 operations, the capability model, the message-passing layer, the DMA-reach crux argument).
- [`docs/verification.md`](verification.md) — the Verus invariant ladder V1–V7, the honest proof TCB, the toolchain pin, the staffing gate, the Atmosphere seeding plan.
- [`docs/nucleus-design.md`](nucleus-design.md) — the internal architecture of the verified nucleus (capabilities, address spaces, IPC, scheduling, the AMD-Vi domain manager, the trusted unsafe stub).
- [`docs/dev-infra.md`](dev-infra.md) — dev loop, CI jobs, the red-team DMA harness, gpu-host integration, the gpu-host facts to confirm first.
- [`docs/ai-proof-writer.md`](ai-proof-writer.md) — the AI-agent proof-writer harness that restructures Gate G1 (§4): what the agent writes vs. what a human must own, the reward-hacking guardrails, and the per-invariant-rung verdict.

---

## 1. Orientation

**What Rustproof is.** A new, small (~6–8K SLOC) Rust + Verus **isolation nucleus** — not a full OS, not Redox's kernel, not seL4. It runs first as a KVM guest on the x86_64 box `gpu-host`, and its one job is to safely host an **untrusted** GPU compute stack: the existing C++ `lite::` gfx1201 (RDNA4) driver runs as an ordinary user process, drives the GPU directly, and is confined so that whatever it does — buggy or malicious — it cannot reach memory it was not granted.

**Why it exists.** The `lite::` driver plus GPUVM is a large, fast-moving body of untrusted C++. Rather than trust it, we put it behind a boundary we can *prove* things about. The nucleus owns the address-space page tables and (from M3) the AMD-Vi IOMMU tables; the driver owns everything about *making the GPU compute*, and produces only IOVAs. The nucleus never reasons about what a wave computes — only about what physical memory the device can touch.

**The two-guarantees split (permanent, not a phase).**
1. **Isolation / DMA-containment — in scope, verifiable.** The device's reachable physical memory is bounded by the frames its owner was granted.
2. **GPU compute correctness — permanently out of scope.** Rustproof says nothing about whether a kernel computes the right answer, only about where its DMA can land.

**The honest scope one-liner:** *Rustproof aims to verifiably isolate an untrusted GPU driver, not to verify GPU computation.*

**What "verified" will and will not mean here.** When we say a property is verified, we mean Verus (with a pinned Z3) discharges it over the safe-Rust nucleus, and the guarantee is conditional on four things that are **not** machine-checked: (a) the **trusted unsafe stub** — a few hundred SLOC of asm/MMIO/TLB/IOMMU-invalidate primitives, each `#[verifier::external_body]` with a hand-audited contract, some parts additionally Kani-checked; (b) the **trusted spec** — our models of x86-64 page tables and the AMD-Vi DTE/IO-PTE format, checked by human reading of the Intel SDM and the AMD I/O Virtualization spec, not by a tool; (c) the **pinned toolchain** — one Verus release, one rustc nightly, one Z3 binary, moved only as a unit; (d) the **hardware axioms A1–A6** — that the MMU and AMD-Vi engine interpret our tables per spec, with no undocumented bypass. This is the same shape of assurance seL4 offers (Isabelle over C, plus a hardware/spec assumption); the tooling differs, the honesty does not.

---

## 2. Milestones M0–M5

Isolation column is the load-bearing point: *who actually stops a bad DMA at that milestone.* Before M3 the answer is "the host," so the nucleus's IOMMU proof is present but dormant — a failure of it would not breach isolation because the host physical AMD-Vi (programmed by host Linux under plain VFIO, pinning all guest RAM) is the enforcer.

| Milestone | New capability | Machine-checked property (and the invariant ID) | TCB the property rests on | Isolation actually enforced by |
|---|---|---|---|---|
| **M0** | Boots as a KVM guest via `start-gpu-vm.sh`; untrusted `lite::` dispatches one real gfx1201 wave (VRAM verify `x[i]==N`) | **None.** M0 proves nothing; it is a physical-feasibility gate for the untrusted-driver architecture. | — | **Host** physical AMD-Vi (plain VFIO, axiom A7) |
| **M1** | — (proof work begins) | **V1** — nucleus-core memory safety / UB-freedom (cap + mm + ipc + sched well-formedness) | trusted stub + pinned toolchain | Host (A7) |
| **M2** | Multiple isolated address spaces | **V2** — inter-AS isolation: `reachable(AS) == capabilitied(AS)`, preserved across map/unmap/grant; + **V6** IPC no-authority-amplification | + trusted x86-64 page-table spec | Host (A7) |
| **M3** | Emulated vIOMMU in the guest (QEMU `amd-iommu`, or `virtio-iommu` fallback — see §7) | **V3** — DMA-reach: `dma_reach(GPU_domain) ⊆ authorized(GPU_domain)`; **V4** — DTE-config invariant. *First load-bearing proof.* | + trusted AMD-Vi DTE/IO-PTE spec + emulator fidelity (axiom A8) | **Nucleus** IOMMU proof (emulated) |
| **M4** | Bare-metal real AMD-Vi on hardware | **V5** — reclaim / stale-IOTLB safety; V3/V4 now over real silicon | + hardware axioms A1–A6 against real AMD-Vi | **Nucleus** IOMMU proof (bare-metal) |
| **M5** | — | **Composed confinement assurance case**: V1 ∧ V2 ∧ V3 ∧ V4 ∧ V5 ∧ V6 ⇒ storage + DMA confinement of the untrusted GPU stack; + prose argument citing A1–A6 | all of the above, stated explicitly | Nucleus (composed) |

Two caveats carried in every milestone write-up: **(1)** M0 proves nothing — isolation there is the host IOMMU, not the nucleus. **(2)** "Confinement" at M5 means *storage/DMA* confinement, **not** timing confinement — Verus proves functional/safety properties and does not cover timing side channels (cache, scheduler, TLB/IOTLB contention remain covert channels), exactly the boundary seL4 draws. **(3)** The first *load-bearing* proof (M3) assumes QEMU can present an emulated AMD-Vi that translates for the **passthrough** gfx1201 — historically weak/absent for VFIO-assigned devices (§7c). If gpu-host's QEMU can't, M3 degrades to *statically* verifying the AMD-Vi table-builder + corroborating fault-plumbing via `virtio-iommu`, and the first **hardware-enforced** end-to-end DMA-reach demonstration slides to **M4** (bare-metal). Plan for the first genuinely load-bearing result to be bare-metal, not emulated.

---

## 3. Work-breakdown structure and critical path

### 3.1 The two tracks

The program runs on **two tracks that proceed in parallel and only converge at M3**:

- **Track B — Bring-up (self-serve now).** Everything needed to boot the nucleus and dispatch a wave: the nucleus's boot/memory/AS/IPC/scheduler plumbing, the host-contract server, the no-POSIX C++ hosting of `lite::`, firmware provisioning, in-guest MES/IH bring-up. This is ordinary (hard) systems work; **a strong systems engineer can do all of M0 alone.** No verification is load-bearing here.
- **Track V — Verification (gated).** Everything Verus: the invariant ladder V1–V7, the trusted-stub contracts, the Kani harnesses, the reference model, the red-team oracle. **This track cannot start in earnest until the proof-engineer gate (§4) is cleared.** M1+ dates do not exist until then.

The tracks share one artifact discipline: the **nucleus state representations are fixed in Track B with Track V's needs in mind** (bounded single-level capability table, tracked-permission global frame view, append-only no-revocation derivation forest, reach-as-map-equality). The expensive mistake is discovering the state representation is unprovable *after* the exec code exists — so the design docs (`nucleus-design.md`, `verification.md`) pin those choices now, before Track V is staffed.

### 3.2 Dependency graph — today → M0

Nodes are the M0 tasks from [`docs/milestone-M0.md`](milestone-M0.md). Edges are hard dependencies. `[CP]` marks the critical path.

```
T0.0 baseline on host (0.5–1w) ───────────────────────────────┐
   (reproduce known-good lite:: dispatch on gpu-host; capture   │ (feeds T0.8 host-side FW load
    the strace syscall set for T0.5; get BOOTLOAD signature)    │  and is the M0 reference log)
                                                                ▼
T0.1 bootable nucleus ──► T0.2 AS/threads/caps ──► T0.3 userland loader
   (PVH boot, GDT/IDT,      (addr spaces, sched,      (static ELF64 loader,
    paging, serial,          ctx-switch stub,          ring-3, syscall demux)
    timer, frame alloc)      handle/cap table)               │
        [CP]                     [CP]                        │[CP]
                                                             ▼
                                       T0.4 host-contract server (ioctls) [CP]
                                       (PCIe enum, BAR map, VRAM/GTT alloc,
                                        MAP_GPU passthrough, RING_DOORBELL=MMIO-WPTR,
                                        SETUP_IRQ stub) — freeze the ABI here
                                                             │
                          ┌──────────────────────────────────┤
                          ▼                                   ▼
        T0.5 no-POSIX C++ hosting [CP, LONGEST]     T0.6 firmware provisioning
        (musl+libstdc++ static; Linux-syscall        (RO cpio of psp_14_0_3/
         personality: mmap/ioctl/futex/threads/       gc_12_0_1/sdma_7_0 blobs
         clock/dlopen-static; strace-driven set)      via -initrd) ── needs T0.5 files
                          │                                   │
                          └──────────────┬────────────────────┘
                                         ▼
                          T0.7 ROCr/HIP/lite:: attach [CP]
                          (static-link libhsa/libamdhip64; hsa_init;
                           agent=gfx1201; hipMalloc/Memset; NO dispatch yet)
                                         │
                                         ▼
                          T0.8 in-guest MES/IH bring-up [CP, UNCERTAIN]
                          (host loads FW to BOOTLOAD_COMPLETE, no-FLR handover;
                           guest re-establishes IH ring + MES/KIQ from guest GTT;
                           DECIDE EARLY: port bringup.py → C++/Rust vs CPython personality)
                                         │
                                         ▼
                          T0.9 single AQL dispatch + VRAM verify [CP] ── M0 PAYOFF
                          (MES-backed queue; inc kernel; poll EOP fence w/ HSA_ENABLE_INTERRUPT=0;
                           MMIO-poke CP_HQD_PQ_WPTR; assert x[0]==N)
                                         │
                          ┌──────────────┴───────────────┐
                          ▼                               ▼
              T0.10 real MSI-X IRQ (optional)   T0.11 one-command repro + FREEZE spec surface
              (not required to declare M0)       (tag each host-contract op VERIFIED/
                                                  TRUSTED-STUB/UNTRUSTED — this is the M1–M3 target)
```

**The critical path is T0.1 → T0.2 → T0.3 → T0.4 → T0.5 → T0.7 → T0.8 → T0.9 → T0.11.** Two nodes set the schedule and are the honest risk:
- **T0.5 (no-POSIX C++ hosting), ~8–16 weeks, highest risk** — hosting a large multithreaded C++ runtime (`std::thread`/futex/exceptions/static libstdc++) on a from-scratch kernel via a bounded Linux-syscall personality. De-risk with the T0.0 `strace` capture (fix the exact syscall set) and a personality prototype against a stock ROCr "hello agent" before the real stack.
- **T0.8 (in-guest MES/IH bring-up), ~3–6 weeks, moderate (downgraded on review 2026-07-21)** — the system-RAM ring state does not survive the no-FLR handover, so the guest must re-establish MES/KIQ/IH. **This logic already works in-guest today:** `lite_mes_ih_reference.py` runs `full_gpu_bringup` *after* the handover on the Linux guest and reaches a live MES with a climbing IH ring. So the *algorithm* is proven; the residual risk is **re-hosting** it under the no-POSIX nucleus — which is really T0.5, not new GPU bring-up. Decide **before T0.7** whether to port `bringup.py` to C++/Rust (recommended; keeps CPython out of the guest) or run it under a CPython personality (larger T0.5 surface).

**Honest M0 budget:** the mandatory path sums to **~33–63 engineer-weeks ≈ 8–15 months solo**; overlap (T0.1–T0.3 vs. an early strace-driven T0.5 spike; T0.4 vs. T0.6) pulls it toward **~6–12 months**. Do not present a shorter number. This is dominated by **C++ hosting of already-working bring-up (T0.5)** — not by re-solving GPU bring-up (the `lite::` dispatch + in-guest MES/IH already work on Linux/Windows/macOS) and not by the microkernel.

### 3.3 Dependency graph — M1 → M3 (Track V, once staffed)

```
[GATE: proof engineer hired OR Mars Research collaboration signed]  (§4)
        │
        ▼
Ramp / seed: reproduce Atmosphere's Verus page-table proof on OUR pinned toolchain
   (verification.md §7 — also the hiring test; confirms the flat-map + ghost-subtree
    technique builds before M1 relies on it; fallback = Asterinas OSTD structure, no proofs)
        │
        ▼
M1: V1 nucleus-core memory safety
   - big-lock concurrency model (no interleaving) established
   - all unsafe concentrated in trusted/ stub; each fn external_body + contract
   - Kani harnesses for the stub's non-asm logic (PTE/DTE roundtrip, ring index, bitmap)
   - CI: proof job green; TCB-growth gate (grep external_body/assume/admit/external) armed
        │
        ▼
M2: V2 inter-AS isolation  +  V6 IPC no-amplification
   - AddrSpaceModel flat map; lemma_map_preserves / lemma_revoke_requires_unmapped
   - refinement lemma (concrete radix tree ⟶ flat map) proved ONCE by induction on level
   - reachable == capabilitied maintained by every map/unmap
        │      (this is the machinery reused, re-parametrized, for V3)
        ▼
M3: V3 DMA-reach  +  V4 DTE-config   ── FIRST LOAD-BEARING PROOF
   - iommu/ domain manager added; IommuDomainModel over IO page tables (same flat-map trick)
   - dma_confined incl. self-protection clause (table_frames ∉ dma_reach)
   - dte_confining struct invariant (V=TV=1, mode≠passthrough, ATS off, root==our tables)
   - reference model/ walker golden-vector tested; red-team harness (dev-infra §3) enabled
   - emulated vIOMMU brought up (see §7 open question — amd-iommu vs virtio-iommu fallback)
```

M2's page-table machinery is deliberately built so M3 reuses it verbatim over the IO tables — that reuse is why M2→M3 is an extension, not a restart. **Prerequisite before M3 scheduling is real:** the gpu-host facts in §7 must be answered.

### 3.4 An architecture fork to record before T0.5: how the untrusted driver is hosted

T0.5 (hosting the C++ `lite::` stack with no POSIX) is the single largest cost and risk in the whole M0 path, so *how* the untrusted driver is hosted deserves to be an explicit, recorded decision rather than an implicit default:

- **(A) Driver-as-process (plan of record).** The driver runs as an ordinary userspace process on the nucleus via a bounded Linux-syscall personality. Smallest TCB, cleanest capability story, all-Rust host, crispest end-state guarantee. Cost: **T0.5** — hosting a large multithreaded C++ runtime (ROCr/HIP/libstdc++/threads) with no POSIX (8–16 weeks, highest risk).
- **(B) Driver-in-a-thin-Linux-guest, nucleus as a tiny confining hypervisor.** The nucleus runs the untrusted driver inside a minimal Linux VM and owns only the IOMMU/DMA boundary around it (the SeKVM / separation-kernel shape). This **removes the T0.5 C++-hosting problem entirely** — the driver keeps its native POSIX/Python environment — at the cost of (i) a Linux kernel inside the driver's confinement domain (still untrusted, still IOMMU-caged, but a larger confined blob) and (ii) building nucleus virtualization (NPT/EPT, VM-exit handling) instead of a process/loader model.

(A) is the better *end state* (smaller TCB, the DMA-reach guarantee is crisper). (B) is a materially faster route to a booting, GPU-dispatching system and directly retires the dominant unknown (T0.5). A sensible sequencing is **(B) → (A)**: stand up the isolation architecture and prove the DMA-reach invariant with the driver in a thin guest first, then collapse to driver-as-process once C++ hosting is solved. **Decide and record which path M0 takes before starting T0.5** — under (B), T0.5 largely disappears and is replaced by a virtualization-bring-up task, reshaping the near-term critical path.

---

## 4. Staffing and go/no-go gates

**Gate G1 — the verification-capacity gate (hard, blocks all of Track V).** There is no in-house proof engineer today. M0 is reachable by the systems engineer alone; **M1 and everything above are unstaffed and therefore unscheduled until this gate is cleared.** Do not put M1–M5 dates on any roadmap before then. The plan of record clears it **not** with a scarce full-time proof-engineer hire but with an **AI agent proof-writer harness + a lighter human spec-author/reviewer** — full design in [`ai-proof-writer.md`](ai-proof-writer.md).

- **Primary path — agent harness + spec-author/reviewer.** An AI agent writes the mechanical bulk of proof labor (loop invariants, `assert`/SMT-hint blocks, `#[trigger]`s, `proof fn` bodies) in a generate → run pinned Verus/Z3 → parse-typed-errors → repair loop (best-of-n, backtracking, retrieval over vstd + the in-repo proof corpus + Atmosphere/verified-nrkernel seeds), gated in CI by a hard accept-gate. A human **authors every top-level `requires`/`ensures`/invariant, owns the trusted x86-64 page-table + AMD-Vi models and the unsafe-stub contracts, and runs the final TCB audit.** This **lowers and reshapes** the gate — from "hand-write ~20K lines of proof" to "author specs + review agent proofs + audit the TCB" — but does **not** remove the human-expertise requirement. Compute is thousands-to-low-tens-of-thousands of dollars, dwarfed by any person-year of formal-methods labor; the binding constraint becomes spec-author/reviewer time.
- **Fallback for the crux — hire or Mars Research collaboration.** The agent plausibly closes most of **M1** under review and accelerates **M2** as a leaf-lemma discharger, but only *assists* at **M3** (the DMA-reach crux) and above ([`ai-proof-writer.md`](ai-proof-writer.md) §5). For M3+ keep open either (a) hiring a Verus/SMT proof engineer (small pool — CMU/Parno, MSR/Hawblitzel-Lattuada, Utah/Mars Research — long search), or (b) a funded Mars Research / Atmosphere collaboration (the flat-map/ghost-subtree technique this plan leans on).

**Gate condition:** begin M1 proof work only when the agent harness, a signed-off spec-author/reviewer, and the accept-gate ([`ai-proof-writer.md`](ai-proof-writer.md) §4) are in place. **Calibration precondition (Stage 0):** before budgeting any agent yield past M1, run the harness against the repo's *existing* proved corpus as a proof-hole regression (strip proof bodies, ask the agent to reconstruct) — if it can't reproduce proofs it already has the specs for, it won't write new ones; that result is the go/no-go, not a public benchmark. Also use the **Atmosphere-reproduction exercise** ([`verification.md`](verification.md) §7) to de-risk the flat-map/ghost-subtree technique on our pinned toolchain. **Soundness is never delegated to the agent:** a green Verus run is necessary, never sufficient — accept only on green-run AND human-approved spec AND unchanged-TCB (no new `assume`/`admit`/`assume_specification`/`external_body`/`external` without a human-whitelisted rationale) AND spec-mutation-suite-fails-as-expected AND non-vacuous AND seed-stable.

**Gate G2 — the gpu-host facts gate (blocks M3/M4 planning).** Answer §7's questions on the box before M3/M4 scheduling is anything but speculative. A red answer to ACS-without-override (a) or emulated-AMD-Vi-for-passthrough (c) changes the plan, not just the schedule.

**Gate G3 — M0 exit / spec freeze (blocks M1 starting on the right target).** M0 is declared only when a fresh-checkout engineer reproduces, from one command, `BOOTLOAD_COMPLETE → MES/direct-MEC → completed wave → expected VRAM value`, **and** the host-contract ioctl→capability mapping is frozen with each op tagged VERIFIED / TRUSTED-STUB / UNTRUSTED. That frozen surface is what M1–M3 verify against.

**Non-gating but continuous:** the pinned-toolchain discipline (`rust-toolchain.toml` + `verus.lock`/`verus-toolchain.toml` + `Cargo.lock` move as one unit; CI asserts the versions), and the TCB-growth CI gate (any new `external_body`/`assume`/`admit`/`assume_specification`/`#[verifier::external]` must be whitelisted with a rationale).

---

## 5. Start here — first two weeks

Concrete, self-serve, all on Track B (no proof engineer needed). Ordered.

1. **Reproduce the known-good baseline on gpu-host (T0.0).** Cold BMC power-cycle → `insmod amdgpu_lite.ko` → firmware staged → `LITE_MES_RECIPE=1` bring-up to `RLC_RLCS_BOOTLOAD_STATUS == 0x8000003f` → run the multi-dispatch test → `SURVIVED N dispatches; verify=PASS`. Capture that log; it is the M0 target signature.
2. **`strace` the real stack during step 1** and commit the syscall set as `docs/lite-syscall-surface.txt`. This is the authoritative input to T0.5 (the no-POSIX personality) and the single most schedule-relevant fact you can gather early.
3. **Answer the gpu-host facts (§7 / [`dev-infra.md`](dev-infra.md) §6)** and write `docs/gpuhost-facts.md`: ACS + IOMMU-group singleton *without* `pcie_acs_override` (a); ReBAR/BAR-size posture (b); whether QEMU can present `amd-iommu`/`virtio-iommu` that translates for the passthrough gfx1201 (c); the empirical per-POST bring-up budget (d).
4. **Stand up the repo skeleton** per [`repo-structure.md`](repo-structure.md): virtual Cargo workspace, both custom target JSONs, `-Zbuild-std` wired, `rust-toolchain.toml` copied verbatim from the chosen Verus release, `toolchain/verus.lock`, `cargo-deny`. Get the scaffold to the litmus state — everything compiles, TCB proofs are trivially green (crux proof present but `admit()`-guarded and non-load-bearing), even before there is a bootable image.
5. **Spike the bootable nucleus (T0.1 start).** *(Boot default for the gpu-host libvirt flow is PVH direct-boot — no ISO; Limine-on-ISO in `repo-structure.md` §3.2 is the standalone/dev alternative.)* PVH ELF note → long-mode trampoline → serial (`uart_16550` on `0x3F8`) → GDT/IDT with a double-fault IST → paging from the PVH memory map → LAPIC/TSC timer → bitmap frame allocator with a separate DMA-capable pool. Acceptance: `qemu-system-x86_64 -enable-kvm -kernel nucleus.elf -serial mon:stdio` prints boot progress and a forced page-fault dumps cleanly.
6. **Fork the gpu-host boot assets.** `libvirt/rustproof-gpu.xml` (clone of the working GPU-passthrough domain: same `<hostdev>` gfx1201 + `.1` audio, `managed='no'`, direct-boot the nucleus ELF, serial→host pty) and `libvirt/start-rustproof-vm.sh` (fork of `start-gpu-vm.sh` with `VM=rustproof-gpu`, preserving the `reset_method=""` no-FLR trick verbatim and swapping the SSH-wait for a serial-banner wait).
7. **Decide the T0.8 bring-up-port question now.** C++/Rust port of `bringup.py` (recommended) vs. CPython personality — write the one-paragraph decision into `docs/milestone-M0.md`, because it changes T0.5's surface and you are about to build T0.5.
8. **Set up the fast CI loop (build + test-qemu).** `cargo build` on the custom target, headless QEMU boot via `isa-debug-exit` (`0x10`→exit 33 pass), `cargo fmt --check`, `clippy -D warnings`. The `proof`/`test-kani` jobs are wired but inert until Track V starts.

Deliverables at end of week 2: the M0 reference log, the syscall surface, `docs/gpuhost-facts.md`, a compiling workspace, a nucleus that boots to serial under fast QEMU, and the forked libvirt assets. None of this is blocked on the hire.

---

## 6. Risk register (top items, honest)

| Risk | Where | Why it bites | Mitigation |
|---|---|---|---|
| No-POSIX C++ hosting under-scoped | T0.5 | Large multithreaded C++ runtime on a from-scratch kernel; thread/futex/static-libstdc++ fidelity | strace-driven exact syscall set; personality prototype vs. stock ROCr before the real stack; budget 8–16w |
| In-guest MES/IH doesn't re-establish from guest RAM | T0.8 | RAM ring state doesn't survive the no-FLR handover; today Python | keep fragile PSP load host-side; port bring-up native; `LITE_STOP_AFTER` staging |
| Proof-engineer hire is slow / never lands | Gate G1 | Tiny talent pool | pursue Mars Research collaboration in parallel; Atmosphere reproduction as hiring test |
| The **trusted spec** is wrong (AMD-Vi DTE/IO-PTE model doesn't match silicon) | V3/V4 | A wrong spec makes the proof *vacuously* green — the likeliest and hardest-to-catch failure | dedicated review pass against the AMD IOMV spec by someone who has driven AMD-Vi bare-metal; golden-vector the `model/` walker |
| Atmosphere proofs don't build on our pinned Verus | V2/V3 seed | Open research proofs bit-rot against toolchain drift | check it in a week (it's the first ramp task); Asterinas OSTD structural fallback (no proofs) |
| `amd-iommu` + VFIO passthrough unsupported on gpu-host QEMU | M3 (§7) | Emulated AMD-Vi translation for *assigned* devices historically weak | split M3: verify AMD-Vi table-builder statically (needs no emulator) + corroborate fault-plumbing via virtio-iommu fallback; AMD-Vi-format end-to-end fault moves to M4 |
| ACS faked via `pcie_acs_override` | A2 / M4 | P2P containment axiom empirically unfounded → proof boundary leaks | confirm singleton IOMMU group with no override (§7a); go/no-go input for M4 |
| Z3/Verus version drift flips proofs red or masks a regression | all of Track V | SMT is nondeterministic under time pressure | hard-pin all three; fixed seed + per-fn rlimit; timeout = red, never skip; upgrades are deliberate re-verify events |
| Big-lock concurrency serializes multi-tenant GPU dispatch | design (all milestones) | chosen to keep proofs tractable (Atmosphere-style); serializes concurrent submission + leaks a timing channel — tension with the multi-tenant serving direction (spur-k8s/sglang) | conscious, recorded tradeoff for M0–M5; revisit only with a CertiKOS-scale fine-grained-concurrency proof if multi-tenant throughput demands it |

---

## 7. gpu-host facts to confirm first (Gate G2 detail)

Commands and the go/no-go framing are in [`dev-infra.md`](dev-infra.md) §6. In brief, answer on the box before M3/M4 planning:
- **(a) ACS / IOMMU-group singleton, no `pcie_acs_override`** — gates the P2P-containment axiom A2. A faked ACS override is a no-go input for M4.
- **(b) ReBAR / BAR-size posture** — sizes the `MAP_BAR` aperture in the frozen host contract and decides whether `lite::`'s VRAM bump-allocator assumptions hold (contrast a constrained 256 MB / ReBAR-off BAR path).
- **(c) Can QEMU present an emulated AMD-Vi that translates for the passthrough gfx1201?** — decides the whole M3 shape. If yes, M3 is clean AMD-Vi end-to-end. If no, M3 = V3/V4 statically verified over AMD-Vi format + fault-plumbing corroborated via `virtio-iommu`, and the first AMD-Vi-format end-to-end fault demonstration becomes an M4 deliverable.
- **(d) Empirical per-POST bring-up budget** — is it really ~1 before the PSP wedges? Sets the HW-loop cadence and how the nightly job chunks isolated tests.

Record answers in a living `docs/gpuhost-facts.md` and gate the M3/M4 milestone entries on them.

---

## 8. What is trusted, one more time (so no reader over-claims)

Even at M5 with every proof green, the guarantee rests on: the **trusted unsafe stub** (asm context switch, TLB/IOMMU invalidate, MMIO accessors — hand-audited, partly Kani-checked, enumerated in `trusted/contracts.md`); the **trusted spec** (page-table and AMD-Vi table models, human-checked against the SDM/IOMV spec); the **pinned toolchain** (rustc + Verus + Z3); and the **hardware axioms A1–A6** (the MMU and AMD-Vi honor our tables, requester-ID integrity, ATS-off blocks bypass, invalidation completeness, register/queue semantics, walker/DRAM coherence). The guarantee is **storage + DMA confinement**, not timing confinement, and not GPU compute correctness. That is the whole and honest claim.

---

## 9. Change-log

**2026-07-21 — review pass + AI proof-writer.** Folded a review of this plan into the docs:
1. **T0.8 downgraded** (§3.2) — in-guest MES/IH re-establishment already works on Linux (`lite_mes_ih_reference.py` runs `full_gpu_bringup` post-handover); the residual risk is *re-hosting* under the no-POSIX nucleus (T0.5), not new bring-up. Budget line reweighted to "C++ hosting of already-working bring-up."
2. **New §3.4** — made the driver-hosting choice explicit: driver-as-process (plan of record, smallest TCB, pays the T0.5 cost) vs. driver-in-a-thin-Linux-guest with the nucleus as a tiny confining hypervisor (retires the T0.5 risk, larger confined blob). Decide before starting T0.5.
3. **M3→M4 caveat** (§2, §7c) — an emulated AMD-Vi that translates for a *passthrough* device is historically weak; the first *load-bearing* DMA-reach proof likely slides to bare-metal M4.
4. **Boot default reconciled** — PVH direct-boot is the gpu-host M0 default; `repo-structure.md` §3.2 updated to match (Limine-on-ISO demoted to standalone/dev).
5. **Big-lock tradeoff** added to the risk register (§6) — serializes multi-tenant GPU dispatch; conscious M0–M5 tradeoff.
6. **Gate G1 rewritten** (§4) to the **AI-agent proof-writer** approach (agent harness + spec-author/reviewer replaces the scarce full-time proof-engineer hire; hire / Mars-collab become the M3-crux fallback). New companion doc [`ai-proof-writer.md`](ai-proof-writer.md).
