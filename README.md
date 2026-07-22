# Rustproof

A small, formally-verifiable **Rust + Verus isolation nucleus** that safely hosts an *untrusted* GPU compute driver. The existing C++ `lite::` gfx1201 (AMD RDNA4) driver runs as an ordinary user process and drives the GPU directly; the nucleus owns the address-space page tables and (later) the AMD-Vi IOMMU tables, and confines the driver so that — buggy or malicious — it cannot touch memory it was not granted. First target: a KVM guest on the x86_64 box `shark-a`, dispatching one real gfx1201 wave through the untrusted driver.

> **Honest scope:** Rustproof aims to *verifiably isolate an untrusted GPU driver — it does **not** verify GPU computation.* Two guarantees, split permanently: **isolation / DMA-containment is in scope and verifiable; GPU compute correctness is permanently out of scope.** The nucleus never reasons about what a wave computes, only about what memory the device can reach.

**Status:** design + scaffold stage, **pre-M0**. Nothing verifies yet; nothing boots yet. No in-house proof engineer, so the verification track (M1+) is unstaffed and unscheduled until that gate is cleared — see the plan. The near-term work is ordinary (hard) systems work: boot the nucleus as a KVM guest and get the untrusted driver to dispatch one wave.

## What "verified" will mean here

A green proof is never "the system is safe." It is "safe *modulo*": a small **trusted unsafe stub** (asm/MMIO/TLB/IOMMU-invalidate primitives, hand-audited, partly Kani-checked); a **trusted spec** (our models of x86-64 page tables and the AMD-Vi table format, checked by human reading of the Intel SDM / AMD IOMMU spec); a **pinned toolchain** (one Verus release, one rustc nightly, one Z3); and **hardware axioms** (the MMU and AMD-Vi interpret our tables per spec). The confinement claim is *storage + DMA* confinement — **not** timing side channels. Same shape of assurance as seL4; the tooling differs, the honesty does not.

## Milestones

| M | Capability | Machine-checked property | Enforced by |
|---|---|---|---|
| **M0** | Boots as KVM guest; untrusted `lite::` dispatches one gfx1201 wave | none (feasibility gate) | host IOMMU (plain VFIO) |
| **M1** | — | nucleus-core memory safety (V1) | host |
| **M2** | multiple address spaces | inter-AS isolation `reachable == capabilitied` (V2) + IPC no-amplification (V6) | host |
| **M3** | emulated vIOMMU | **first load-bearing** DMA-reach `dma_reach ⊆ authorized` (V3) + DTE-config (V4) | nucleus (emulated) |
| **M4** | bare-metal AMD-Vi | reclaim / stale-IOTLB safety (V5); V3/V4 on real silicon | nucleus (bare-metal) |
| **M5** | — | composed confinement assurance case | nucleus (composed) |

Under plain VFIO (M0–M2) the *host* programs the physical IOMMU, so the nucleus's IOMMU proof is present but **not load-bearing** — it does real work only from M3.

## Repo layout

```
rustproof/
├── crates/
│   ├── nucleus-core/   capabilities/  vspace/  ipc/   # verified TCB (Verus, no_std)
│   ├── iommu-amdvi/                                    # AMD-Vi tables + the DMA-reach crux proof
│   ├── arch-x86_64/                                    # trusted unsafe stub (Kani-checked, Verus-external)
│   ├── abi/                                            # shared syscall/IPC ABI (emits a C header)
│   ├── nucleus/                                        # the bootable kernel image
│   ├── init/  userland-rt/                             # untrusted root task + userland runtime
│   └── driver-host/  driver-shim/                      # hosts the untrusted C++ lite:: driver
├── vendor/rocr-lite/   vendor/limine/                  # untrusted driver source; pinned bootloader
├── libvirt/                                            # shark-a domain + boot script (forks start-gpu-vm.sh)
├── tools/                                              # xtask, mkimage, run-vm, verify
└── docs/                                               # design docs (below)
```

The C++ `lite::` driver is **never linked into the nucleus** — it is a separate, untrusted process reached only over the capability/IPC ABI.

## Build & run (planned)

Nothing here works yet; this is the intended loop.

```bash
# fast inner loop — no GPU, any Linux; memory-safety/isolation work
cargo build -p nucleus --release          # custom bare-metal target, -Zbuild-std
cargo test  -p nucleus                     # boots under headless QEMU, exits via isa-debug-exit

# proof gate (once the verification track is staffed)
cargo xtask verify                         # pinned Verus + pinned Z3 over the TCB crates

# real loop — shark-a, gfx1201 passed through, dispatch a wave
cargo xtask run                            # build → image → VFIO-bind (no-FLR) → boot guest
                                           # success = "M0: WAVE OK" on the guest serial console
```

## Docs

- [`docs/implementation-plan.md`](docs/implementation-plan.md) — master plan: milestones, work-breakdown, critical path, staffing gates, first two weeks.
- [`docs/repo-structure.md`](docs/repo-structure.md) — workspace, toolchain pinning, boot, hosting the untrusted driver.
- [`docs/milestone-M0.md`](docs/milestone-M0.md) — the near-term critical path (tasks T0.0–T0.11).
- [`docs/host-contract.md`](docs/host-contract.md) — nucleus ↔ untrusted-driver ABI (9 ops, capabilities, IPC, the crux argument).
- [`docs/nucleus-design.md`](docs/nucleus-design.md) — verified nucleus internals (caps, address spaces, IPC, scheduling, AMD-Vi manager, trusted stub).
- [`docs/verification.md`](docs/verification.md) — Verus invariant ladder V1–V7, honest proof TCB, toolchain pin, staffing gate.
- [`docs/dev-infra.md`](docs/dev-infra.md) — dev loop, CI, red-team DMA harness, shark-a integration.

## License

Licensed under the **MIT License** — see [`LICENSE`](LICENSE).

*Unless you state otherwise, any contribution you submit is licensed under MIT.*
