# Rustproof

A small, formally-verifiable **Rust + Verus isolation nucleus** that safely hosts an *untrusted* GPU compute driver. The existing C++ `lite::` gfx1201 (AMD RDNA4) driver runs as an ordinary user process and drives the GPU directly; the nucleus owns the address-space page tables and (later) the AMD-Vi IOMMU tables, and confines the driver so that — buggy or malicious — it cannot touch memory it was not granted. First target: a KVM guest on the x86_64 box `gpu-host`, dispatching one real gfx1201 wave through the untrusted driver.

> **Honest scope:** Rustproof aims to *verifiably isolate an untrusted GPU driver — it does **not** verify GPU computation.* Two guarantees, split permanently: **isolation / DMA-containment is in scope and verifiable; GPU compute correctness is permanently out of scope.** The nucleus never reasons about what a wave computes, only about what memory the device can reach.

**Status:** early M0. The nucleus **boots** — a minimal image comes up under QEMU via the PVH protocol, switches to 64-bit long mode, brings up COM1 serial, and exits cleanly (the first slice of task T0.1). Not yet: interrupt handling (IDT/exceptions), a frame allocator, address spaces, the GPU driver, or any verification. No in-house proof engineer, so the verification track (M1+) is unstaffed until that gate is restructured — see [`docs/ai-proof-writer.md`](docs/ai-proof-writer.md). The near-term work is ordinary (hard) systems work: grow the nucleus, then get the untrusted driver to dispatch one wave on real hardware.

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
├── libvirt/                                            # gpu-host domain + boot script (forks start-gpu-vm.sh)
├── tools/                                              # xtask, mkimage, run-vm, verify
└── docs/                                               # design docs (below)
```

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
