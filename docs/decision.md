# ADR: Rustproof base-OS and architecture decision

- **Status:** accepted (2026-07-21).
- **Context:** we need a formally-verifiable, multi-process host OS that dispatches
  GPU compute on AMD gfx1201, first as a KVM guest on `gpu-host`.

## Decision

Build a purpose-built **~6-8K-SLOC Rust + Verus isolation nucleus** that hosts the
existing C++ `lite::` gfx1201 driver as an **untrusted user process**. Verify the
isolation boundary (address-space + AMD-Vi DMA containment); treat the GPU, its
firmware, and the driver as untrusted-but-confined. GPU compute correctness is out
of scope.

- Verification tool: **Verus** (Kani for the unsafe hardware stub).
- Seed proofs from **Atmosphere** (Rust+Verus microkernel) as a template.
- Borrow **Redox** userland ergonomics only for the untrusted layer, later.

## Rejected alternatives (summary; full reasoning in the decision doc)

- **Build on / verify Redox's kernel** — unverified ~32K-SLOC moving target; "verify
  Redox" is a rewrite; zero head start on the GPU half. (Redox kept only as a
  possible userland parts donor.)
- **seL4 base** — highest pedigree but C + Isabelle/HOL a Rust team can't self-serve;
  x86-64 is its weakest config (no FC proof); IOMMU is VT-d-only.
- **SeKVM-style separation kernel** — ARM/SMMU Coq proofs don't port to x86/AMD-Vi.
- **Extend Atmosphere as the base** — unconfirmed external-artifact/build risk; kept
  as a reference instead.

Full analysis: the decision doc and research brief live in the
`the internal workspace` repo (`plans/verified-gpu-host-os.md`) and a copy of the
brief is at [`docs/research-brief.md`](research-brief.md).
