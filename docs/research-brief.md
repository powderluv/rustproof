<!-- Companion research brief for verified-gpu-host-os.md. Generated 2026-07-21 by a multi-agent research fan-out (seL4, formal-OS landscape, Atmosphere, Redox, fe2o3, Rust verification tooling, GPU-kernel verification, IOMMU/DMA) with per-topic adversarial verification. -->

# Research Brief: A Formally-Verifiable Host OS Dispatching Verified GPU Kernels on gfx1201

*Basis document for a follow-up implementation-planning phase. Synthesizes verified research on seL4, the broader formal-OS landscape, Atmosphere, fe2o3, GPU-kernel verification, the IOMMU/DMA trust boundary, and minimal GPU-driver TCB. Where raw research and its adversarial verification conflict, this brief follows the verification.*

---

## 1. Executive summary

1. **The goal decomposes into two guarantees of very different strength, and they must never be conflated.** "Verified host OS" is a strong, achievable machine-checked property about *CPU-side code*. "Formally-verified GPU kernel" is a *strictly weaker, orthogonal* property — today it credibly means "proven data-race-free and barrier-divergence-free (sometimes functionally correct) at source/IR level under an *assumed* GPU memory model," not verified machine code on verified silicon. Neither guarantee reaches the GPU silicon or its firmware. (seL4 assumptions; gpu-kernel-verification synthesis.)

2. **The GPU + its firmware (PSP/SOS/SMU/MES/CP/RLC microcode) is a permanently untrusted, unverifiable device.** The only sound stance is to confine it behind an IOMMU and prove properties only about the host code that programs it. This is not a limitation to work around — it is the load-bearing architectural decision, and every credible verified-OS result (seL4, Ironclad, SeKVM, Atmosphere) already treats DMA-capable devices this way. ([sel4.systems/About/FAQ](https://sel4.systems/About/FAQ.html))

3. **The single biggest formal-methods gap in this project is not the kernel — it is verified IOMMU/DMA confinement.** No shipping verified kernel has a machine-checked proof that covers an untrusted DMA device behind an IOMMU. seL4's proof *assumes DMA is off or trusted*; its VT-d/SMMU support is explicitly outside every verified configuration. ([verified-configurations](https://docs.sel4.systems/projects/sel4/verified-configurations.html)) Closing this requires *new* proof work: a device-DMA model plus verified IOMMU-management code. Scope it as first-class, not as a config flag.

4. **On x86-64 — the gpu-host target — seL4 is at its weakest.** It has only C-level functional correctness: **no** binary/translation-validation proof (so the compiler is trusted) and **no** integrity, availability, or confidentiality proofs. All of seL4's strong security guarantees exist on AArch64/RISC-V. Worse, seL4's x86 hypervisor path is tested on Intel VT-x; AMD-V is documented as "wasn't tested." ([verified-configurations](https://docs.sel4.systems/projects/sel4/verified-configurations.html); [libsel4vm](https://docs.sel4.systems/projects/virtualization/docs/libsel4vm.html))

5. **Atmosphere (Rust + Verus, x86-64, SOSP'25) is the closest ready-made match to this project's exact threat model.** It already verifies multi-process isolation and confines untrusted devices behind an IOMMU whose *page tables* are managed by verified code, at a **3.32:1 proof-to-code ratio and ~2–2.5 person-years** — roughly an order of magnitude cheaper than seL4. ([2025-sosp-atmo.pdf](https://mars-research.github.io/doc/2025-sosp-atmo.pdf)) *(Note: a 7.5:1 figure appearing in some summaries is an error corrected against the SOSP'25 paper.)*

6. **The lite:: driver's correct home is a small trusted confinement component whose only security-critical duty is bounding the GPU's DMA reach** — not functional correctness of scheduling/dispatch. The realistic, tractable verification target is a *safety invariant*: every physical page the driver ever makes device-reachable lies within a statically-bounded, IOMMU-permitted set disjoint from kernel-private memory, and no firmware/GPU-supplied value is ever used to index host memory. This mirrors Ironclad's IOMMU-confinement proof. ([Ironclad OSDI'14](https://www.usenix.org/system/files/conference/osdi14/osdi14-paper-hawblitzel.pdf))

7. **fe2o3 is a philosophically-aligned experiment, not an adoptable component.** It is a ~5-day-old, single-author proof-of-concept that compiles only f32/f64 elementwise 1-D kernels, has no atomics/LDS/barriers/reductions, an unimplemented general-lowering path, and — critically — depends on the *HIP runtime*, the exact stack lite:: bypasses. Its Kani/Verus "verifiability" is hypothetical and not wired in. ([github.com/powderluv/fe2o3](https://github.com/powderluv/fe2o3))

8. **Because gpu-host uses VFIO passthrough, the untrusted host hypervisor programs the physical IOMMU and sits silently inside the verified guest's TCB.** Removing it requires either (a) trusting/verifying the host, (b) a guest vIOMMU (achievable via a software shadow page table even without hardware nested translation), or (c) confidential computing (SEV-SNP + SEV-TIO/TDISP) — which a *consumer RDNA4 gfx1201 card almost certainly does not support*. This is a first-milestone scoping decision. ([vfio-internals](https://kernel-internals.org/iommu/vfio-internals/))

---

## 2. The verification-scope reality: what the goal can and cannot mean

### 2.1 The trust boundary is the PCIe interface

Everything on the device side of the PCIe link — DMA engines, and the PSP/SOS/SMU/MES/CP/RLC firmware microcontrollers — is a large, closed, proprietary blob that executes *on the GPU*, is not host-visible, and cannot be formally verified. AMD ships only machine-*readable* XML (encoding/decoding, not execution semantics) for its ISA, refuses to open-source the PSP, and the AMD secure processor family has a documented history of exploitable bugs and voltage-glitch attacks. ([machine-readable-isa](https://gpuopen.com/machine-readable-isa/); [One Glitch to Rule Them All, arXiv 2108.04575](https://arxiv.org/pdf/2108.04575)) The GPU's own secure-boot chain protects *AMD's firmware integrity*, not the host.

**Consequence:** the GPU-plus-firmware must be modeled as an untrusted, possibly-adversarial bus master. It can only be *contained* (by an IOMMU), never *verified*.

### 2.2 The honest TCB

For a verified guest OS on gpu-host, the trusted computing base includes, at minimum:

- **The verifier stack itself:** the theorem prover / SMT solver (Isabelle, Coq, or Z3), and — for any Rust approach — the Rust compiler and the verifier's frontend and axioms. Atmosphere's TCB explicitly lists Verus frontend, Z3, rustc, ~700 lines of assumed axioms, and 300+1,900 lines for tracked-permission setters. ([2025-sosp-atmo.pdf](https://mars-research.github.io/doc/2025-sosp-atmo.pdf))
- **Unverified boot code and in-kernel assembly** (true of every verified kernel).
- **The IOMMU-management code and its device/page/interrupt-remap tables** — these are TCB whether or not they are verified; the question is only whether they are *also* proven correct.
- **On x86 without a binary proof:** the compiler.
- **Under VFIO passthrough:** the host KVM/QEMU stack that programs the physical IOMMU, plus the BMC, unless evicted via confidential computing.
- **The CPU silicon/microcode, SMM, DRAM, and the platform** (assumed correct hardware).
- **The entire GPU and its firmware** (assumed adversarial-but-confined).

### 2.3 The guarantee boundary — what can honestly be claimed

**Achievable (with the required new IOMMU proof work):**
> *"A machine-checked host OS that cannot be corrupted outside the specific physical pages it explicitly grants the GPU; a compromised or malicious GPU/firmware can at worst read or corrupt data already placed in those shared buffers, and can never reach the verified kernel's private memory or another tenant's memory — modulo correct CPU/IOMMU hardware."*

**Not achievable, and must not be claimed:**
- Verified GPU *behavior* or verified firmware.
- Confidentiality/integrity of data *inside* GPU-shared buffers against a malicious device (needs application-level encryption/attestation; the IOMMU cannot provide it).
- Freedom from side channels (DDIO/LLC cache, Rowhammer-over-DMA) or PCIe-error DoS — all outside any IOMMU model and any OS proof. ([DMA Security survey](https://dl.gi.de/bitstreams/1a7e69c5-eb7b-4ccb-b270-a119557a62e1/download))
- That a "verified GPU kernel" runs faithfully — the untrusted MES/CP must execute *exactly* that kernel under the *assumed* machine model, and neither is enforced by silicon.

---

## 3. seL4 vs the formal-OS landscape: what is genuinely transferable

### 3.1 seL4 — foundation-grade, but x86 is its weak flank

seL4 is the gold standard: a capability microkernel (canonically 8.7 KLOC C / ~600 lines asm; current verified configs are larger — ~16k SLOC on x86-64) with a machine-checked Isabelle/HOL refinement proof. All drivers run in user mode outside the TCB — an exact structural fit for treating lite:: as a user-level component. ([sel4.systems/About/FAQ](https://sel4.systems/About/FAQ.html))

But the guarantees are sharply architecture-dependent, and **the deployment target lands on the weakest configuration**:

| Property | AArch32 | AArch64 | RISC-V RV64 | **x86-64 (gpu-host)** |
|---|---|---|---|---|
| Functional correctness (to C) | ✅ (+fast path) | ✅ (+fast path) | ✅ | ✅ (no fast path) |
| Binary/translation validation | ✅ | ❌ | ✅ | **❌ (compiler trusted)** |
| Integrity + availability | ✅ | ✅ | ✅ | **❌** |
| Confidentiality (info-flow) | ✅ | in progress | ✅ | **❌** |

([verified-configurations](https://docs.sel4.systems/projects/sel4/verified-configurations.html))

Additional seL4 constraints for this project:
- **DMA assumed off/trusted; IOMMU/VT-d unverified.** The one mechanism you'd use to cage the GPU is outside every proof. ([FAQ](https://sel4.systems/About/FAQ.html))
- **AMD-V hypervisor path "wasn't tested"** (Intel VT-x is the tested path). gpu-host is AMD. ([libsel4vm](https://docs.sel4.systems/projects/virtualization/docs/libsel4vm.html))
- **Microkit** (the natural way to lay out a static multi-process userland: ≤63 protection domains, memory regions, channels) is itself unverified and rides the MCS config, which has **no x86 verification roadmap** (RISC-V 2026, AArch64 2027 only). ([microkit manual](https://docs.sel4.systems/projects/microkit/manual/latest/))
- WCET/timing analysis has lapsed (old ARM cores only; the *first* such analysis was Blackham et al. RTSS 2011, not Sewell 2017).

### 3.2 The one most transferable pattern: SeKVM-style microverification

SeKVM (IEEE S&P'21) proved VM confidentiality+integrity for real multiprocessor KVM by **verifying only a tiny core (KCore, ~3.8K SLOC Coq) that enforces isolation, while leaving a large untrusted body (KServ) merely *confined*.** ([ieeesp2021_kvm.pdf](https://www.cs.columbia.edu/~nieh/pubs/ieeesp2021_kvm.pdf)) This is the direct template for this project: prove a small host core that enforces address-space/IOMMU/process isolation; treat the lite:: driver + GPU firmware as a "KServ-analog" whose arbitrary misbehavior is bounded by proven invariants.

### 3.3 Cost anchors and the finite-interface constraint

- **Interactive proof (Coq/Isabelle):** strongest properties, highest cost. seL4 ~20 person-years for functional correctness (~200k lines Isabelle); CertiKOS is the reference for *verified fine-grained concurrency* but at many person-years. ([CACM seL4](https://cacm.acm.org/research/sel4-formal-verification-of-an-operating-system-kernel/))
- **Push-button SMT (Hyperkernel, Serval, Nickel):** cheap, but **require finite interfaces — no unbounded loops or recursion.** GPU ring/MES-scheduler/HQD-MQD state machines are full of unbounded control loops that do not naturally finitize, so the cheap automation does not transfer to the driver's hot path for free. ([Hyperkernel](https://unsat.cs.washington.edu/papers/nelson-hyperkernel.pdf))
- **Rust + Verus (Atmosphere, ~2 py, 3.32:1):** the realistic cost anchor for a fresh verified microkernel, and the only one that verifies isolation *and* fits the team's Rust direction.
- **Language-safety-only (RedLeaf, Theseus, baseline Tock):** cheap isolation from the Rust compiler, but **not machine-checked** — do not conflate "written in safe Rust" with "verified."

---

## 4. Base-OS decision inputs (decision deferred to design phase)

Four candidate foundations, laid out on the verifiability/effort axis. **No selection is made here.**

### Option A — Build on seL4 (verified kernel) + Microkit/CAmkES userland
- **Verifiability:** highest kernel pedigree, but on x86-64 you inherit only C-level functional correctness with no binary/security proofs; Microkit and its MCS config are unverified on x86. Real deployment history (DARPA HACMS). ([CACM seL4-in-Australia](https://cacm.acm.org/research/sel4-in-australia/))
- **GPU fit:** clean — drivers are already user-mode PDs with MMIO frames + IRQ channels + DMA regions. But IOMMU confinement is unverified, and you'd be adding the IOMMU/DMA proof onto a C kernel proven in Isabelle (not Rust).
- **Effort:** you don't re-verify the kernel, but the new IOMMU/DMA proof work is in Isabelle/HOL, and integrating a Rust driver world sits awkwardly against a C/Isabelle base.
- **Risk:** x86 is seL4's weakest arch; AMD-host hypervisor untested; MCS-on-x86 has no verification roadmap.

### Option B — Extend/adopt Atmosphere (fresh Rust + Verus microkernel)
- **Verifiability:** machine-checked refinement + well-formedness + memory-safety + leak-freedom + per-configuration isolation/non-interference, on x86-64, in Rust. ([2025-sosp-atmo.pdf](https://mars-research.github.io/doc/2025-sosp-atmo.pdf))
- **GPU fit:** best of the four — *already* treats physical devices as untrusted and confines them behind an IOMMU with verified page-table structures; already has verified multi-process (processes/threads/containers) + endpoint IPC.
- **Effort:** lowest verification cost anchor (~2 py, 3.32:1, <20s re-verify). Same language/tooling as a Rust lite:: port and fe2o3.
- **Risk:** research prototype (open TODOs: containers, hugepages, IPI wiring); big-lock concurrency (serializes multi-process GPU dispatch, acknowledged timing channel); **Intel/VT-d-centric — the ~465-line trusted IOMMU-register-config code would need an AMD-Vi rewrite**; isolation proof is per-configuration and must be re-derived for an N-tenant GPU topology; IOMMU *register programming* is trusted, not verified (only the tables are verified).

### Option C — Redox OS (Rust microkernel, Unix-like)
- **⚠️ Research gap:** dedicated research on Redox returned only a placeholder stub — this option is under-informed and must be properly researched in the design phase before it can be weighed. What is reliably known: **Redox is NOT formally verified**; its safety story rests on Rust type/memory-safety and a microkernel design, not machine-checked proofs. It has no AMD gfx1201 support.
- **Verifiability:** language-based only, categorically weaker than seL4/Atmosphere unless a verification effort is grafted on (large, unproven).
- **GPU fit / effort / risk:** unknown pending real research; likely provides userland/driver-framework ergonomics but no proof foundation.

### Option D — Verified separation kernel / hypervisor (SeKVM-style host-eviction)
- **Verifiability:** verify a small hypervisor core (à la SeKVM/KCore) that isolates a guest and confines the passed-through GPU, evicting the untrusted host from the guest TCB. Strong isolation/non-interference precedent.
- **GPU fit:** directly addresses the VFIO "host is in the TCB" problem (§8) by making the confining layer itself verified.
- **Effort:** SeKVM used Coq (~2 py for KCore) — interactive-proof cost, and re-targeting from Arm to x86/AMD is substantial.
- **Risk:** you're building a verified hypervisor *and* still need a guest OS + driver on top; largest total surface. Overlaps with, rather than replaces, Options A/B.

**Decision inputs to resolve in the design phase:** (i) Is a machine-checked *security* property (isolation/non-interference) in scope for milestone 1, or only functional correctness + a DMA-reach safety invariant? (ii) Rust-native (B/C) vs Isabelle/Coq (A/D)? (iii) Is the host hypervisor trusted, or must it be verified/evicted (drives A/B vs D)? (iv) Fill the Redox research gap before comparing.

---

## 5. Rust verification tooling recommendation

**Primary recommendation: Verus**, with **RefinedRust as the higher-assurance fallback** for the isolation-critical module.

- **Verus** (SMT/Z3, linear ghost types, permission reasoning; ghost code erased at compile time) verifies **both safe and unsafe Rust** that manipulates raw pointers, MMIO, memory, and concurrency — exactly what a driver's DMA-buffer construction needs. It is proven at systems scale (Atmosphere kernel, VeriSMo, CapybaraKV). It is the same tool Atmosphere uses, so choosing Atmosphere (Option B) and Verus is one coherent decision. ([Verus, arXiv 2303.05491](https://arxiv.org/pdf/2303.05491))
- **RefinedRust** produces foundational Coq/Rocq proofs for safe+unsafe Rust — higher assurance, less automation. Reserve for the single most safety-critical module if Verus's TCB (frontend + Z3 + assumed axioms) is judged too large.

**Hard limits to design around:**
1. **Verus/Kani model CPU/sequential semantics only.** Neither models the SIMT execution model, GPU shared/global memory, warp divergence, or the GPU weak memory model. Verifying a Rust GPU *kernel* body with Verus captures the sequential algorithm — **not** GPU races/divergence. ([Kani, arXiv 2607.01504](https://arxiv.org/html/2607.01504v1))
2. **The verifier and its axioms are in the TCB.** ~700 lines of assumed Verus axioms and the Z3/rustc chain are trusted, not proven.
3. **Verus is annotation-heavy deductive verification, not push-button.** Verifying an *unsafe*, pointer/MMIO/doorbell-manipulating submission path is materially harder than the safe-Rust majority and far harder than the NIC-driver precedents (see §7.3). Budget accordingly; do not extrapolate the "3 person-months, no prior experience" NIC number to the GPU path.
4. **SMT struggles with unbounded loops.** The flat-permission technique (§6) is the mitigation for recursive/inductive structures; genuinely unbounded driver loops may need loop invariants or must be pushed into the untrusted-confined body.

---

## 6. Atmosphere & fe2o3 concrete learnings

### 6.1 Atmosphere — reusable verification patterns (directly applicable)

1. **Microverification split done in Rust:** verified core enforces isolation; drivers (ixgbe, NVMe) run as *unverified* user-space processes atop it. Map lite:: onto this exactly.
2. **Untrusted-device-behind-IOMMU is already the model.** VM subsystem owns page-table *and* IOMMU-page-table memory with a proven invariant that each table's page-closure is pairwise disjoint. This is the seed of the DMA-reach invariant this project needs. ([2025-sosp-atmo.pdf](https://mars-research.github.io/doc/2025-sosp-atmo.pdf))
3. **"Flat" permission storage:** hold all pointer permissions for a recursive structure (page-table nodes, threads, containers) in one global flat map with ghost `path`/`subtree` fields — converts recursive/inductive proofs into non-recursive ones the SMT solver can discharge. Directly transferable to proving properties over GPU ring/MQD/doorbell data structures.
4. **Manual (non-borrow-checker) memory management** to get a system-wide view of memory — required for global leak-freedom and isolation proofs. Pointer-centric but *proven*, not trusted.
5. **Modular separation of structural vs non-structural invariants** via closed spec functions; kernel modeled as a state machine over abstract ghost state with pre/post-condition specs; refinement proven against the MMU walk.
6. **Design choices that cut proof cost:** big-lock synchronization (no fine-grained concurrency proof); *prohibit fine-grained capability revocation* (resources return only on container termination), which guarantees memory can't be revoked out from under verified user code mid-execution.

**Adaptation cost / caveats:** the ~465-line IOMMU register-config is Intel VT-d and trusted — **needs an AMD-Vi rewrite** and remains unverified; isolation/non-interference is proven for one specific 3-container (A, B, V) configuration and must be re-derived for an N-tenant GPU topology (a verified "GPU multiplexer" container is the natural analog of their verified shared container V); big-lock serializes multi-process GPU dispatch and leaks timing.

### 6.2 fe2o3 — what to borrow, what to avoid

**Borrow (the front-half idea):** single-source, all-Rust, cargo-native, C++-free authoring — `#[kernel]` device functions as `no_std` safe Rust, collected at MIR exactly where Kani/Verus operate. Philosophically, a Rust host OS and Rust GPU kernels could share one toolchain and (in principle) one source-level verification pass.

**Avoid / do not adopt as-is:**
- **Maturity:** ~5-day single-author PoC; only f32/f64 elementwise 1-D kernels; **no atomics, LDS, real barriers (`syncthreads` is an `unreachable!` stub), 2D/3D grids, generics, or reductions**; the Pliron general-lowering path is unimplemented (hand-written MIR recognizer). ([github.com/powderluv/fe2o3](https://github.com/powderluv/fe2o3))
- **Runtime coupling:** depends on **HIP** (`hipModuleLoad`/`hipModuleLaunchKernel`), dragging in the full ROCm userspace + amdgpu driver — **the exact stack lite:: bypasses and a verified guest cannot trust.** A lite::/AQL dispatch back-end does not exist yet.
- **Verification is hypothetical:** nothing in fe2o3 wires in Kani or Verus; even if wired, they cover only sequential source-level properties of individual kernel bodies, not GPU concurrency, and certify nothing about the emitted HSACO.

**Sensible integration (future):** keep fe2o3's (or cuda-oxide's more-complete Pliron) *front-half* compiler to produce HSACO/AQL; replace the HIP *back-half* with lite:: dispatch, preserving the `(ptr, len)` kernarg ABI; let the verified host confine the GPU behind the IOMMU. Treat fe2o3 as a design reference for kernel-authoring ergonomics, not a dependency.

---

## 7. GPU path: IOMMU-confined device + minimal lite:: driver + verified kernels

### 7.1 The three layers and their guarantee levels

| Layer | Guarantee | Method |
|---|---|---|
| **Host IOMMU confinement** | GPU DMA restricted to explicitly-granted, kernel-disjoint pages | **Verify** (new proof work — the crux) |
| **lite:: submission driver** | Never widens device-reachable set beyond IOMMU-permitted region; never indexes host memory with a device/firmware value | **Verify** the isolation-critical unsafe module (Verus/RefinedRust) |
| **GPU compute kernel** | Data-race-/divergence-freedom (sound-incomplete); optionally source-level functional correctness | **Best-effort verify** at source/IR under assumed memory model |
| **GPU silicon + firmware** | none | **Assume** (adversarial-but-confined) |

### 7.2 What to VERIFY

**(a) The DMA-reach confinement invariant (highest priority).** Prove that every physical page the driver ever makes device-reachable — via an IOMMU map, a GPUVM PTE it writes, or the base of a ring/MQD/doorbell/IH region it programs — lies within a statically-bounded, IOMMU-permitted set disjoint from kernel-private memory. This mirrors Ironclad's instruction-level proof that non-device memory operations only touch private memory. ([Ironclad OSDI'14](https://www.usenix.org/system/files/conference/osdi14/osdi14-paper-hawblitzel.pdf)) Prefer a **static, small "compute arena"** (fixed set of guest-physical pages) over dynamic per-dispatch mapping — it makes the invariant far simpler.

**(b) No device-value-indexes-host-memory.** Enumerate every channel the GPU/MES firmware writes back into host-visible memory — completion signals, HQD/MQD fields, IH ring entries, WPTR/status — and prove none is used to index host memory. This must be *complete*, not best-effort; the IH ring base and any GPU-writable status region are as security-critical as compute buffers.

**(c) Mapping minimality and promptness.** Verify mappings are minimal and that IOTLB invalidation is *strict/synchronous* — Thunderclap-class attacks and deferred-invalidation use-after-unmap windows exploit exactly the absence of these disciplines. ([Thunderclap NDSS'19](https://www.ndss-symposium.org/ndss-paper/thunderclap-exploring-vulnerabilities-in-operating-system-iommu-protection-via-dma-from-untrustworthy-peripherals/); [IOMMU deferred-invalidation, DATE'24](https://bu-icsg.github.io/publications/2024/iommu_date_2024.pdf))

### 7.3 What to ASSUME (and why verifying it is impractical)

- **PSP/SOS/SMU/MES/CP/RLC firmware** — closed blobs on-die; assume correct-or-adversarial-but-confined.
- **Bring-up sequencing** (PSP firmware load, SMU handshakes, ring/MES init) — highly device-state-dependent and asynchronous, entangled with firmware behavior. Audit it; do not attempt to prove functional correctness of it. Scope creep here is a real risk.
- **The GPU machine model** underlying any kernel-level proof.

### 7.4 Minimal driver is genuinely tractable

The mainline amdgpu driver is ~5.9M lines, but **~4.4M are auto-generated register headers**, and a compute-only single-ASIC path drops display/KMS/DC, codecs, the graphics pipeline, multi-GPU, and dynamic power management. ([Phoronix](https://www.phoronix.com/news/Linux-6.16-AMDGPU-Driver-Size)) A well-factored driver can be ~10× smaller than its Linux counterpart (sDDF Ethernet: <600 LOC vs ~5000). ([Trustworthy Systems drivers](https://trustworthy.systems/projects/drivers/)) **Port lite:: to Rust, pushing all volatile-MMIO and DMA-buffer construction into a thin, explicitly-`unsafe` module (the verification target) and keeping the rest in safe Rust** (RedLeaf/Tock model). Note the honest caveat: every verified-driver precedent (Ironclad, Pancake, sDDF) is a *simple NIC*; the GPU submission path is unsafe-heavy, so this is harder than the NIC results suggest.

### 7.5 Verified GPU kernels — scope precisely, don't over-promise

- **Near-term automated win:** race + barrier-divergence freedom via GPUVerify/Faial/PUG on HIP source/IR — **sound but incomplete** (false positives possible), and says nothing about correctness of the result. ([GPUVerify OOPSLA'12](https://nchong.github.io/papers/oopsla12.pdf))
- **Functional correctness:** only via heavy manual separation-logic annotation (VerCors/Kuiper) or narrow-domain equivalence checking (Volta: matmul/conv/attention). Not a general capability. ([VerCors](https://vercors.ewi.utwente.nl/))
- **AMD is the worst-supported target:** **no machine-checked GCN/RDNA ISA semantics, no verified compiler to GCN machine code.** A kernel "verified" at source is *not* verified as the code gfx1201 runs; the LLVM AMDGPU backend + silicon/firmware stay in the TCB. AMD has HRF/HSA memory-model formalizations at the *virtual-ISA* level, but nothing tool-checked at the RDNA4 hardware level. ([HRF ASPLOS'14](https://research.cs.wisc.edu/multifacet/papers/asplos14_hrf.pdf))
- **Optional complementary layer:** Honeycomb-style static validation of GPU binaries (OSDI'23, prototyped on RX6900XT/RDNA2, 18× smaller TCB, still trusts firmware) — a separate, weaker guarantee that could layer on top, but portability to gfx1201/RDNA4 is unproven. ([Honeycomb OSDI'23](https://www.usenix.org/system/files/osdi23-mai.pdf))

### 7.6 Hardening rules (bake into lite:: and the OS IOMMU policy)

- Map only minimal ring/kernarg/IH/doorbell/I-O pages — **never whole guest RAM.**
- Strict/synchronous IOTLB invalidation (`iommu.strict=1` equivalent).
- **Disable ATS/PRI** on the passthrough path (or mark the GPU untrusted) — ATS lets a device present pre-translated addresses that bypass the IOMMU. ([DMA survey](https://dl.gi.de/bitstreams/1a7e69c5-eb7b-4ccb-b270-a119557a62e1/download))
- Interrupt remapping on.
- **Dedicated pages / bounce buffers** — never let host data share a page with a device-mapped buffer (IOMMU is page-granular → sub-page exposure).
- Verify the passed-through GPU sits in a clean, singleton IOMMU group (**no ACS-override**).

---

## 8. gpu-host VM bring-up: first-milestone shape

### 8.1 Baseline topology

```
gpu-host (x86_64, AMD)
 ├─ Host: KVM/QEMU + VFIO  ── programs the PHYSICAL AMD-Vi IOMMU (GPA→HPA)
 │                             ⚠ in the guest's isolation TCB (milestone-1 decision)
 ├─ BMC (out-of-band power cycle for GPU/firmware resets)
 └─ Guest VM: the verifiable host OS
      ├─ verified core (isolation + multi-process + IOMMU tables)
      ├─ lite::-derived driver process (unsafe MMIO/DMA module = verify target)
      │     PSP/SMU bring-up · GMC/GPUVM · IH ring · CP ring · MES ADD_QUEUE · AQL · doorbell/MMIO-WPTR
      └─ tenant processes (GPU compute clients)
      gfx1201 (Radeon AI PRO R9700) — PCIe passthrough, IOMMU-confined
```

### 8.2 Where lite:: plugs in

lite:: already does exactly the compute-only bring-up subset needed (PSP/SOS load — the workspace's "Phase 8 PSP SOS load" confirms gfx1201 loads a signed SOS; SMU; GMC/GPUVM page tables; IH ring; CP ring; MES ADD_QUEUE; AQL dispatch; doorbell/MMIO-WPTR). It becomes an **unverified user-space driver process** (Atmosphere ixgbe/NVMe model) whose isolation-critical mapping/submission logic is the one module carried into the verified TCB.

### 8.3 Milestone-1 decisions to resolve up front

1. **VFIO trust posture.** Plain passthrough maps *all* guest RAM into the GPU's IOMMU domain — the GPU is confined to the guest but can DMA anywhere *within* it unless the guest runs its own vIOMMU. ([DMA survey](https://dl.gi.de/bitstreams/1a7e69c5-eb7b-4ccb-b270-a119557a62e1/download)) Choose one:
   - **(a) Trust the host** for milestone 1 (fastest; honestly documents the host in the TCB).
   - **(b) Guest vIOMMU** so the verified guest owns stage-1 confinement — **achievable via a software shadow page table synced through VFIO MAP/UNMAP even without hardware nested translation** (at a performance cost); hardware 2-stage nesting is the performant path, not a hard prerequisite. ([KVM-Forum vIOMMU](https://www.linux-kvm.org/images/a/a6/KVM_Forum_2018_viommu_vfio.pdf))
   - **(c) Confidential computing** (SEV-SNP + SEV-TIO/TDISP) to evict the host — **almost certainly unavailable on a consumer RDNA4 gfx1201 card**; treat as out of scope for gpu-host. ([GPU-CC analysis, arXiv 2507.02770](https://arxiv.org/pdf/2507.02770))
2. **Static compute arena vs dynamic mapping** — commit to a fixed, small set of GPU-reachable guest-physical pages to keep the DMA-reach proof simple.
3. **Minimal page set** — enumerate precisely which pages lite:: must map (AQL ring, MES/HQD/MQD, doorbells, IH ring, kernargs, I/O buffers) and confirm each can be a dedicated page with strict invalidation without breaking gfx1201 bring-up.
4. **Confirm** the gfx1201 is in a singleton IOMMU group and ATS/peer-to-peer is disable-able in both host and guest.
5. **Firmware measurement/pinning** — measure and pin PSP/SOS/MES/SMU versions at bring-up to *detect* tampering, even though it can never be *verified*.

### 8.4 Suggested milestone sequencing (for the design phase to refine)

- **M0:** boot the chosen base OS as a KVM guest on gpu-host with gfx1201 passed through; lite:: (C++, unverified) dispatches one compute kernel end-to-end. Proves the plumbing; nothing verified yet.
- **M1:** Rust port of lite:: with the isolation-critical MMIO/DMA logic isolated in one `unsafe` module; audited, not yet proven. Document the honest TCB including the host.
- **M2:** verify the DMA-reach confinement invariant on that module with Verus; add the AMD-Vi IOMMU-table proof (the crux — new proof work).
- **M3:** guest vIOMMU (shadow-page-table) to remove the host from the guest's isolation TCB, if that posture is chosen.
- **M4+:** best-effort GPU-kernel verification (race/divergence-freedom via a GPUVerify/Faial path on HIP; source-level Verus on kernel bodies) as a separate, weaker guarantee layer.

---

## 9. Biggest risks & open questions, ranked

**R1 — Verified IOMMU/DMA confinement is unsolved territory (highest).** No shipping verified kernel proves isolation for an untrusted DMA device behind an IOMMU; seL4 assumes DMA off, Atmosphere leaves IOMMU *register config* trusted. This new proof is the project's crux, not a config flag. *Open:* how large/tractable is a device-DMA model + verified AMD-Vi driver on the chosen base, and is any prior verified-IOMMU work reusable?

**R2 — The GPU/firmware is permanently unverifiable; guarantees stop at confinement.** PSP/SOS/SMU/MES/CP are closed blobs with an exploit history. "Verified host OS" can never mean "verified GPU." Messaging and threat model must state this plainly. *Open:* can firmware at least be measured/pinned at bring-up?

**R3 — Under VFIO the untrusted host is in the guest's TCB.** Confidential-computing eviction is likely physically unavailable on consumer gfx1201, forcing "trust the host" or a software-shadow vIOMMU. *Open:* which posture for milestone 1, and does gpu-host's platform support a guest vIOMMU for a passthrough device?

**R4 — x86-64 is the weakest arch for the strongest existing kernel.** If Option A (seL4) is chosen, you get only C-level functional correctness on x86, no binary/security proofs, untested AMD-V hypervisor, no MCS-on-x86 roadmap. This pushes toward a Rust base (Atmosphere) but that too has an Intel-centric trusted IOMMU layer needing an AMD rewrite. *Open:* Rust-native vs Isabelle/Coq base; trust-host vs verify-host.

**R5 — Verifying the unsafe GPU submission path is far harder than the NIC precedents.** The "3 person-months" and "1:4.8" figures are simple polled NICs; the lite:: hot path is unsafe-heavy, asynchronous, firmware-entangled. Scope the verification target tightly (DMA-reach safety invariant only) and resist scope creep into dispatch/scheduling functional correctness. *Open:* actual line count and unsafe footprint of current lite::; how much is isolation-critical vs bring-up sequencing?

**R6 — Atmosphere isolation is per-configuration; multi-tenant GPU sharing needs new invariants.** The non-interference proof covers one 3-container topology; an N-tenant GPU-sharing design requires re-derived invariants (a verified "GPU multiplexer" container). *Open:* incremental proof effort for the target topology.

**R7 — Big-lock concurrency limits parallel GPU dispatch and leaks timing.** Atmosphere's cheap-verification choice serializes syscalls/interrupts. Fine-grained concurrent multi-process GPU submission is CertiKOS-scale interactive-proof cost. *Open:* accept serialized dispatch or invest in fine-grained concurrency proofs?

**R8 — "Verified GPU kernel" on AMD is the least-supported verification target.** No GCN ISA semantics, no verified GCN compiler; source-level proofs don't cover the HSACO. The credible near-term deliverable is race/divergence-freedom, not functional correctness. *Open:* is there any in-progress formal RDNA semantics derivable from AMD's XML? Do GPUVerify/Faial's assumed memory models align with AMD's HRF/HSA scoped model, and has anyone litmus-tested gfx1201 to *validate* (not assume) it?

**R9 — Side channels and DoS are entirely out of scope of any proof.** DDIO/LLC cache channels, Rowhammer-over-DMA, PCIe-error DoS all survive perfect IOMMU confinement. State this as a known, unaddressed boundary.

**R10 — The Redox option is under-researched.** The dedicated research returned a stub; it cannot be fairly compared until properly investigated. *Open:* a real Redox assessment (architecture, driver/IOMMU model, verification-graftability, gfx support) is a prerequisite for the base-OS decision.

---

*Prepared as the research basis for the follow-up implementation-plan design phase. Base-OS selection (§4), concurrency model (R7), and VFIO trust posture (§8.3) are deliberately left open for that phase.*
