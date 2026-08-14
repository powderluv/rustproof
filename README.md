# Rustproof

A small, formally-verifiable **Rust + Verus isolation nucleus** that safely hosts an *untrusted* GPU compute driver. The existing C++ `lite::` gfx1201 (AMD RDNA4) driver runs as an ordinary user process and drives the GPU directly; the nucleus owns the address-space page tables and (later) the AMD-Vi IOMMU tables, and confines the driver so that — buggy or malicious — it cannot touch memory it was not granted. First target: a KVM guest on the x86_64 box `gpu-host`, dispatching one real gfx1201 wave through the untrusted driver.

> **Honest scope:** Rustproof aims to *verifiably isolate an untrusted GPU driver — it does **not** verify GPU computation.* Two guarantees, split permanently: **isolation / DMA-containment is in scope and verifiable; GPU compute correctness is permanently out of scope.** The nucleus never reasons about what a wave computes, only about what memory the device can reach.

**Status:** M0 core running. Under QEMU the nucleus boots (PVH → long mode → COM1 serial), installs an IDT that dumps a clean register trace on any CPU fault, then exercises a working kernel core: it parses the PVH memory map and initializes a bitmap **frame allocator**, builds a fresh **address space** (page-table map / translate / unmap), derives **capabilities** with authority-monotonic rights, runs a synchronous **IPC** send/recv, and performs a real cooperative **context switch** into a second kernel thread and back. Those five subsystems (`mm`, `vspace`, `capabilities`, `ipc`, `sched`) are separate crates built against a shared `abi`. Not yet: userland / syscalls, the GPU driver (needs real hardware), or any verification (needs the Verus toolchain — see [`docs/ai-proof-writer.md`](docs/ai-proof-writer.md)). No in-house proof engineer, so the verification track (M1+) is unstaffed until that gate is restructured — see [`docs/ai-proof-writer.md`](docs/ai-proof-writer.md). The near-term work is ordinary (hard) systems work: grow the nucleus, then get the untrusted driver to dispatch one wave on real hardware.

## What "verified" will mean here

A green proof is never "the system is safe." It is "safe *modulo*": a small **trusted unsafe stub** (asm/MMIO/TLB/IOMMU-invalidate primitives, hand-audited, partly Kani-checked); a **trusted spec** (our models of x86-64 page tables and the AMD-Vi table format, checked by human reading of the Intel SDM / AMD IOMMU spec); a **pinned toolchain** (one Verus release, one rustc nightly, one Z3); and **hardware axioms** (the MMU and AMD-Vi interpret our tables per spec). The confinement claim is *storage + DMA* confinement — **not** timing side channels. Same shape of assurance as seL4; the tooling differs, the honesty does not.

## Milestones

| M | Capability | Machine-checked property | Enforced by |
|---|---|---|---|
| **M0** | Boots as KVM guest; untrusted `lite::` dispatches one gfx1201 wave | none (feasibility gate) | host IOMMU (plain VFIO) |
| **M1** | — | nucleus memory safety (V1) | host |
| **M2** | multiple address spaces | inter-AS isolation `reachable == capabilitied` (V2) + IPC no-amplification (V6) | host |
| **M3** | emulated vIOMMU | **first load-bearing** DMA-reach `dma_reach ⊆ authorized` (V3) + DTE-config (V4) | nucleus (emulated) |
| **M4** | bare-metal AMD-Vi | reclaim / stale-IOTLB safety (V5); V3/V4 on real silicon | nucleus (bare-metal) |
| **M5** | — | composed confinement assurance case | nucleus (composed) |

Under plain VFIO (M0–M2) the *host* programs the physical IOMMU. The nucleus's IOMMU proof is **not present at all** — there is no IOMMU code in this tree; the crate that was to hold it was an empty stub and is gone. It first matters at M3.

## Repo layout

```
rustproof/
├── crates/
│   ├── kernel/                                         # the generic nucleus (one impl, both arches)
│   ├── capabilities/ deleg/ regions/ runstate/ mm/     # TCB: host-tested, several exhaustive
│   ├── vspace/ vspace-riscv/  ipc/  sched/  loader/    # TCB: page tables, IPC, run queue, ELF
│   ├── arch-x86_64/  arch-riscv64/                     # trusted unsafe stubs (raw MMIO/MSR/asm)
│   ├── abi/  hal/                                      # shared syscall/IPC ABI; the Arch trait
│   ├── nucleus/  nucleus-riscv/                        # the bootable kernel images
│   ├── init/  riscv-init/                              # untrusted root tasks (the demo)
│   └── userland-rt/  driver-host/  driver-shim/        # EMPTY PLACEHOLDERS (see their doc comments)
├── tools/                                              # host-tests.sh, run-qemu*.sh (+ unbuilt scaffolds)
└── docs/                                               # design docs (below)
```

**Status, plainly.** Nothing here is verified by Verus; the TCB crates are covered by host unit
tests, several exhaustive over their whole state space, plus one scripted QEMU boot per arch.
Verus was evaluated and DECLINED on 2026-08-11 with a written reversal condition — see
docs/verification.md. This listing previously showed `nucleus-core/` and `iommu-amdvi/` as
"verified TCB (Verus)"; both were empty crates and were deleted on 2026-08-12. There is no
`vendor/`, no `libvirt/`, and no GPU driver in this tree yet — the milestone table above is the
plan, not a description of the repo.

The C++ `lite::` driver is **never linked into the nucleus** — it is a separate, untrusted process reached only over the capability/IPC ABI.

## Build & run

The fast inner loop **works today** — stable Rust, the built-in `x86_64-unknown-none`
target, and QEMU (cross-builds from any host; no GPU needed):

```bash
# build the nucleus image + boot it under QEMU + check the serial banner
tools/run-qemu.sh                          # => "rustproof: BOOT OK", exit 33 (PASS)

# or just build the image
cargo build -p nucleus --target x86_64-unknown-none --release
```

Requires a stable Rust toolchain with the `x86_64-unknown-none` target
(`rustup target add x86_64-unknown-none`) and `qemu-system-x86_64`.

Planned (not wired yet):

```bash
cargo xtask verify   # proof gate: pinned Verus + Z3 over the TCB crates (verification track)
cargo xtask run      # real loop on gpu-host: build → image → VFIO-bind (no-FLR) → boot guest
```

> `gpu-host` throughout these docs is a placeholder for your x86-64 GPU-passthrough machine; the GPU BDF (`0000:03:00.0`) and the libvirt domain/paths are examples — replace with your own.

## Docs

- [`docs/implementation-plan.md`](docs/implementation-plan.md) — master plan: milestones, work-breakdown, critical path, staffing gates, first two weeks.
- [`docs/repo-structure.md`](docs/repo-structure.md) — workspace, toolchain pinning, boot, hosting the untrusted driver.
- [`docs/milestone-M0.md`](docs/milestone-M0.md) — the near-term critical path (tasks T0.0–T0.11).
- [`docs/host-contract.md`](docs/host-contract.md) — nucleus ↔ untrusted-driver ABI (9 ops, capabilities, IPC, the crux argument).
- [`docs/nucleus-design.md`](docs/nucleus-design.md) — verified nucleus internals (caps, address spaces, IPC, scheduling, AMD-Vi manager, trusted stub).
- [`docs/verification.md`](docs/verification.md) — Verus invariant ladder V1–V7, honest proof TCB, toolchain pin, staffing gate.
- [`docs/dev-infra.md`](docs/dev-infra.md) — dev loop, CI, red-team DMA harness, gpu-host integration.

## License

Licensed under the **MIT License** — see [`LICENSE`](LICENSE).

*Unless you state otherwise, any contribution you submit is licensed under MIT.*
