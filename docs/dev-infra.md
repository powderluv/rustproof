# Rustproof — Dev loop, CI, red-team harness, gpu-host integration

> **Scope.** This is the engineering-infrastructure doc for Rustproof, the Rust+Verus isolation nucleus described in [`implementation-plan.md`](implementation-plan.md). It covers how a systems engineer actually *builds, boots, tests, and adversarially probes* the nucleus across the M0–M5 milestones. It does not re-litigate any of the decided facts in the decision doc (fresh ~6–8K-SLOC nucleus, `lite::` as an untrusted user process, Verus + Kani, the DMA-reach invariant over nucleus-owned AMD-Vi tables, the M0–M2-host-enforced / M3-emulated / M4-bare-metal staging). Where this doc makes a claim about isolation strength it uses the decision doc's V/A/U labels (V1–V7 verified, A1–A8 assumed, U1–U3 untrusted).
>
> **The load-bearing honesty rule for all of §1–§3:** under plain VFIO (M0–M2) the *host* Linux programs the physical AMD-Vi and pins **all** guest RAM to the device (axiom **A7**). Every "isolation" behaviour you observe on the fast loop and on plain-VFIO gpu-host is host-enforced, not nucleus-enforced. The nucleus IOMMU proof only becomes load-bearing at **M3** (emulated vIOMMU, axiom **A8**) and hardware-enforced at **M4** (bare-metal AMD-Vi). The red-team harness (§3) is therefore *meaningless before M3* and is explicitly gated to M3/M4.

---

## 0. Repo layout & pinning manifest

The nucleus lives in a new Cargo workspace, `rustproof/` (proposed `github.com/powderluv/rustproof`), separate from this meta-workspace. The gpu-host passthrough assets it reuses stay where they are today under ``.

```
rustproof/
├── rust-toolchain.toml           # pins STABLE 1.95.0 (Verus does NOT read it — see §2.1)
├── verus-toolchain.toml          # pins verus release + z3 (our file, read by tools/verus-run)
├── Cargo.toml                    # [workspace]
├── .cargo/config.toml            # build-std flags + QEMU test runner (§1.1)
├── targets/
│   └── x86_64-rustproof-none.json # custom bare-metal target (§1.1)
├── nucleus/                      # ★ the verified crate: AS/cap/IPC/sched (+ IOMMU mgr from M3)
│   ├── src/
│   │   ├── main.rs               # #![no_std] #![no_main] entry
│   │   ├── spec/                 # Verus spec fns + ghost state (the properties V1–V7)
│   │   ├── mem/                  # page-table + capability manager (V1/V2)
│   │   ├── ipc/  sched/
│   │   ├── iommu/                # AMD-Vi domain manager (V3/V4/V5) — added at M3
│   │   └── hal/                  # ★ the TRUSTED unsafe stub (§1.1, §2.4): asm ctx-switch,
│   │                             #   MMIO accessors, TLB/IOMMU invalidate — Verus-external, Kani-checked
│   └── tests/                    # QEMU integration tests (isa-debug-exit)
├── model/                        # host-runnable std crate: executable reference models of the
│                                 #   AMD-Vi page-table walker + reach() set — golden-vector tested (§2.3, §3)
├── host-contract/               # the amdgpu_lite ioctl → capability IPC spec (frozen at M0)
├── redteam/                      # the malicious-DMA harness + fault oracle (§3) — M3+ only
├── boot/                         # limine.cfg / bootloader glue, firmware-blob packer
└── tools/
    ├── repro-m0.sh               # one-command M0 (§5)
    ├── verus-run                 # pinned-verus wrapper
    └── qemu-inner.sh             # fast headless boot (§1.2)
```

**Everything that affects a proof or a boot is pinned and checked in.** The single source of truth for "what version of what":

| Thing | Pinned by | Why |
|---|---|---|
| rustc | `rust-toolchain.toml` (`channel = "1.95.0"`, STABLE) | Verus carries its own driver and ignores this pin; the two toolchains CANNOT share a build (E0514) — see §2.1 |
| Verus + bundled Z3 | `verus-toolchain.toml` → a vendored `verus-<ver>-<os>.zip` (contains `verus`, `z3`, `rust_verify`) | Verus has no LTS; proof stability is version-locked (decision doc R5) |
| Kani | `cargo kani --version` pinned in CI matrix | bounded model checker for the `hal/` stub |
| QEMU | recorded in `tools/qemu-inner.sh` + the nightly-HW runner image; `qemu-system-x86_64 --version` asserted in CI | vIOMMU behaviour (§4) is version-sensitive |
| libvirt domain XML | `rustproof-guest.xml` (checked in, secrets via env) | reproducible passthrough topology |
| firmware blobs (PSP SOS / SMU / MES / RLC) | SHA-256 manifest in `boot/firmware.sha256` | the untrusted driver loads these; hash-pin so a HW run is reproducible |

---

## 1. The local dev loop

Two loops, deliberately different in cost. Use the fast one for 99% of nucleus/proof work; touch gpu-host only when you need a real gfx1201 dispatch.

### 1.1 Building the nucleus image

Bare-metal `no_std` kernel, built with `-Zbuild-std` against a custom target. `targets/x86_64-rustproof-none.json`:

```json
{
  "llvm-target": "x86_64-unknown-none",
  "target-pointer-width": "64",
  "data-layout": "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-f80:128-n8:16:32:64-S128",
  "arch": "x86_64",
  "os": "none",
  "executables": true,
  "linker-flavor": "ld.lld",
  "linker": "rust-lld",
  "panic-strategy": "abort",
  "disable-redzone": true,
  "features": "-mmx,-sse,+soft-float"
}
```

`.cargo/config.toml`:

```toml
[build]
target = "targets/x86_64-rustproof-none.json"

[unstable]
build-std = ["core", "alloc", "compiler_builtins"]
build-std-features = ["compiler-builtins-mem"]

[target.'cfg(target_os = "none")']
runner = "tools/qemu-inner.sh"      # `cargo run` / `cargo test` boot under QEMU (§1.2)
```

Build:

```bash
cd rustproof
cargo build -p nucleus --release            # -> target/x86_64-rustproof-none/release/nucleus (ELF)
```

Package a bootable image. Use **Limine** (higher-half load, memory map, no BIOS-stub fiddling) via the `limine` crate for the protocol structs and the `limine` bootloader binary staged into `boot/`:

```bash
tools/mkimage.sh    # xorriso the nucleus ELF + boot/limine.cfg into target/rustproof.iso
```

`bootloader` (rust-osdev 0.11) is the lower-friction alternative for the very first M0 spike; switch to Limine before M1 because we need explicit control of the memory map for the later IOMMU/MMIO work. Either way the output is a single `rustproof.iso` (or `.img`) that boots identically under fast QEMU and under the gpu-host libvirt domain.

**Crates in the nucleus** (all `no_std`, all either verified or in the audited stub): `x86_64` (page tables, `Cr3`, port IO), `uart_16550` (serial), `linked_list_allocator` or `talc` (kernel heap for `alloc`), `spin` **only inside the audited stub** (the verified core uses the nucleus big-lock, not an unverified spinlock). Verus's `vstd`/`builtin`/`builtin_macros` are proof-only. No `bindgen`/no C++ in the nucleus — `lite::` is a *separate process* reached over IPC, never linked in.

**The TCB boundary is a directory: `nucleus/src/hal/`.** Every `unsafe` line that survives M1 lives there and is marked `#[verifier::external_body]` (Verus trusts its `ensures`, does not prove the body) and carries a `#[cfg_attr(kani, kani::proof)]` sibling harness. That is the entire "trusted stub" the decision doc's A4 refers to.

### 1.2 Fast inner loop — plain QEMU, no GPU

This is where memory-safety (M1) and isolation (M2) work happens. No GPU, no VFIO, no gpu-host — runs on any laptop/CI Linux. `tools/qemu-inner.sh`:

```bash
#!/usr/bin/env bash
# $1 = kernel/test ELF passed by cargo's runner
set -euo pipefail
IMG=$(tools/mkimage.sh "$1")
exec qemu-system-x86_64 \
  -machine q35,accel=kvm:tcg -cpu host -m 512M \
  -drive format=raw,file="$IMG",if=none,id=disk0 -device virtio-blk-pci,drive=disk0 \
  -serial stdio -display none -no-reboot \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04   # nucleus writes 0x10→exit(33)=pass, 0x11→exit(35)=fail
```

The nucleus exits QEMU by writing a success/failure code to the `isa-debug-exit` port; the cargo test runner maps `33` → test pass. This is the standard rust-osdev pattern and gives you `cargo test` for kernel integration tests (`nucleus/tests/*.rs`): each test binary boots, runs its assertions against real page tables / real capability ops, and exits with a code. Sub-second per test under KVM.

What runs here: everything CPU-side. AS map/unmap/grant/revoke, capability derivation/revocation, IPC round-trips, scheduler context switches, and — from M3 — the AMD-Vi domain manager driven against an **emulated** IOMMU (§4). What does *not* run here: any real gfx1201 dispatch. For that you need gpu-host.

### 1.3 Real loop — gpu-host libvirt/QEMU + VFIO passthrough

Only when you need `lite::` to dispatch a real wave on the physical gfx1201 (RX 9070 XT / AI PRO R9700, `1002:7551`). Reuse the in-repo fork [`libvirt/start-rustproof-vm.sh`](../libvirt/start-rustproof-vm.sh) of the internal, HW-proven `start-gpu-vm.sh` passthrough helper — verbatim.

The three non-obvious disciplines that helper encodes — **do not paper over them**:

1. **The no-FLR `reset_method` trick.** VFIO's default assign-time FLR wipes the gfx1201 VBIOS POST state (PSP SOS, SMU features, GC power) that `lite::` bring-up depends on. The script clears `/sys/bus/pci/devices/$GPU/reset_method` (writes empty string) *before* `virsh start`, so the last cold POST survives into the guest. The Rustproof guest is bound the same way; the nucleus does **not** re-POST the card.
2. **Cold-power-cycle-then-bind.** The correct flow is: BMC cold power-cycle the whole box → fresh VBIOS POST → run `start-gpu-vm.sh` (VFIO bind + reset_method clear) → guest sees a freshly-POSTed card. The nucleus boots into that pre-conditioned state.
3. **PSP wedges after ~1 bring-up.** Empirically the PSP tolerates roughly one bring-up per cold POST; a second bring-up in the same power cycle wedges. So the real loop is *not* re-runnable in place — every HW attempt is `power-cycle → boot → one dispatch attempt → power-cycle`. This is why the HW loop is slow and must be serialized (§2.5), and why the fast loop (§1.2) carries the iteration burden.

**BMC out-of-band control.** gpu-host's power is driven out-of-band; from the operator workstation the recipe is `ipmitool -I lanplus -H <bmc> -U <user> -P <pass-from-gitignored-env> chassis power cycle` (credentials live in a gitignored env file, never in a script or this doc — same discipline as `start-gpu-vm.sh`'s `VM_SSH_PASS`). Wrap it as `tools/gpuhost-powercycle.sh` and have it block until the box answers SSH again before the run proceeds.

**libvirt domain.** Check in `rustproof-guest.xml` — a q35 machine, OVMF firmware, the gfx1201 + its HDMI-audio function (`.1`) as `<hostdev>` VFIO passthrough, and a serial console wired to a host pty/log so the nucleus's boot log is captured. For M3 this XML gains a `<iommu>` device (§4); for M0–M2 it is plain passthrough.

The nucleus image from §1.1 is the *same artifact* booted here — the only delta versus the fast loop is that here a real gfx1201 is passed through and `lite::` (the untrusted user process) can actually reach it.

---

## 2. CI

### WHAT ACTUALLY RUNS (corrected 2026-08-13)

One workflow, `.github/workflows/ci.yml`, with **two** jobs. Everything else in this section is
design intent for a system that does not exist yet; read it as a plan, not a description.

| Job | Steps that actually run |
|---|---|
| `x86_64` | `cargo fmt --all --check`; `tools/host-tests.sh` (28 suites); `tools/run-qemu.sh`; `PROVOKE_FAULT=1 tools/run-qemu.sh` |
| `riscv64` | `tools/run-qemu-riscv.sh` |

There is **no proof job, no Kani job, no nightly hardware job**, and no `cargo xtask`. `tools/xtask`,
`tools/mkimage`, `tools/run-vm` and `tools/verify` all exist as crates that print
"not yet implemented (pre-M0 scaffold)" and exit — which is honest, and they are left alone.

`ci/build.yml` and `ci/verify.yml` were **deleted on 2026-08-13**. They were not in
`.github/workflows/`, so GitHub never ran either of them, yet they declared
`on: [push, pull_request]` and `verify.yml` described itself as a "separate and required" proof
gate. They invoked `cargo xtask verify` and `./toolchain/fetch-verus.sh` (which exits 1) over
`crates/nucleus-core` and `crates/iommu-amdvi` — two crates deleted the day before for claiming to
be a verified TCB over zero code. Prose that describes an unbuilt plan is fine when it says so; a
YAML file in a directory called `ci/` is read as configuration, and this one was three layers of
dead reference deep.

### The plan below is NOT implemented

Five jobs. Four run on ordinary GitHub-hosted (or self-hosted x86) Linux; one runs on the gpu-host self-hosted runner nightly. Fail-fast, no job silently degrades.

### 2.1 The rustc/Verus toolchain coupling (read this first)

**Corrected 2026-08-11 — this paragraph asserted a nightly, and that was measured false.** Verus is a rustc *driver*, but the pinned release (0.2026.08.09) is locked to **stable 1.97.1**, not a nightly, and it does not consult `rust-toolchain.toml` at all. The real constraint is sharper than the one described here: `rustc 1.95.0` **cannot link a Verus-built rlib** (`error[E0514]`), so the nucleus and the proof track cannot share a build at different versions — adopting `verus!{}` in any crate moves the WHOLE repo to Verus's rustc. That cost is why in-tree adoption was declined; see the decision block atop docs/verification.md. `tools/verus-run` and the vendored-unpack flow below remain unbuilt.

### 2.2 Proof job — `verus` (pinned)

```yaml
proof:
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - uses: Swatinem/rust-cache@v2        # cache target/ + the unpacked verus dir keyed on verus-toolchain.toml
    - run: tools/verus-run --crate-type=lib nucleus/src/main.rs --verify-module mem --verify-module ipc
    - run: tools/verus-run --crate-type=lib nucleus/src/main.rs --verify-module iommu   # from M3
```

- **What it proves, by milestone:** M1 → memory-safety/UB-freedom of the AS/cap/IPC/sched core (V1); M2 → the AS/cap non-interference theorem `reachable_frames(A) == capabilitied_frames(A)` (V2); M3 → the DMA-reach theorem `reach(GPU_domain) ⊆ authorized(GPU_domain)` + the DTE-config invariant (V3/V4); M4 → reclaim/stale-IOTLB safety (V5); M5 → no-authority-amplification-through-IPC (V6) and the optional submission-well-formedness stretch (V7).
- **Verus idioms this job is checking:** `spec fn`/`proof fn`, function `requires`/`ensures`, loop `invariant`/`decreases`, `assert(...) by { ... }`, `vstd` `Map`/`Set`/`Seq` for the page-table and reach-set specs, `tracked`/`ghost` permission state (`PointsTo`) so the verified code that touches raw page-table memory carries a proof it owns those frames, and `#[verifier::external_body]` on every `hal/` stub. The IOMMU reach proof is templated on Atmosphere's page-table *disjointness* proof (decision doc §4 / next-action #4) — extract that as the V2→V3 seed.
- **Determinism guard:** the job fails if `verus` reports any *unverified* function outside the declared `external_body` allowlist, and CI diffs the allowlist against `nucleus/src/hal/ALLOWLIST` so nobody silently grows the TCB. Run with a fixed Z3 seed and a per-function `rlimit`; a timeout is a *red* result, never a skip (SMT flakiness is a real risk — pin the seed, split hot lemmas, don't paper over with a longer wall clock).

### 2.3 Build job — the custom target + host models

```yaml
build:
  steps:
    - run: cargo fmt --check
    - run: cargo clippy -p nucleus --target targets/x86_64-rustproof-none.json -- -D warnings
    - run: cargo build --workspace --release          # nucleus on custom target; model/host-contract on host
    - run: cargo build -p nucleus --release && tools/mkimage.sh   # image actually assembles
```

`-Zbuild-std` means `build` also compiles `core`/`alloc` for the custom target, so this job catches target-JSON and `no_std` regressions the host crates would hide.

### 2.4 Host-side unit tests

Two flavours, both on GitHub-hosted Linux, no GPU:

- **`test-host` (`cargo test -p model -p host-contract`).** The `model/` crate is an *executable reference* of the AMD-Vi I/O page-table walker and the `reach()` set — plain `std` Rust. Golden-vector tests: build a DTE + page-table image, walk every IOVA, assert the yielded `(frame, perm)` set equals `authorized`. This is where you *develop and debug* the reach semantics fast before encoding them as Verus `spec fn`s, and it's the oracle the red-team harness (§3) checks the hardware against. It is a model, not a proof — but a wrong model here would make the Verus spec vacuously true, so it earns its own tests. `host-contract` tests exercise the ioctl→capability mapping (the frozen M0 spec surface) as pure data.
- **`test-kani` (`cargo kani`).** Bounded model checking of the `hal/` stub: for each `#[verifier::external_body]` function, a `#[kani::proof]` harness feeds `kani::any()` inputs under `kani::assume(precondition)` and asserts the `ensures` the rest of the proof trusts (e.g. the MMIO accessor writes exactly the requested width to exactly the requested offset; the IOMMU-invalidate stub's index arithmetic never wraps). Kani *finds bugs* in the stub; it does not prove absence (decision doc §4). A Kani counterexample is a CI failure.
- **`test-qemu` (`cargo test` via the §1.2 runner).** The kernel integration tests boot under headless QEMU (TCG on hosted CI, KVM on the self-hosted x86 runner) and exercise real map/unmap/grant/revoke and IPC against real page tables, exiting via `isa-debug-exit`. This is the dynamic counterpart to the static M2 proof — the proof says the invariant holds; this says the code that's *supposed* to establish it actually links and runs.

### 2.5 Nightly HW job — gpu-host

```yaml
nightly-hw:
  runs-on: [self-hosted, linux, x86_64, gpuhost, gpu-gfx1201]
  concurrency: { group: gpuhost-gpu, cancel-in-progress: false }   # GPU is a singleton; never two at once
  if: github.event_name == 'schedule'
  timeout-minutes: 60
  steps:
    - run: tools/gpuhost-powercycle.sh          # BMC cold cycle -> fresh VBIOS POST (PSP-wedge discipline)
    - run: start-gpu-vm.sh         # VFIO bind + reset_method clear + virsh start (no-FLR)
    - run: tools/deploy-nucleus.sh             # push target/rustproof.iso into the guest boot path
    - run: tools/run-m0-workload.sh            # in-guest: lite:: bring-up + multi_dispatch_test (§5)
    - run: tools/run-tri-os-smoke.sh           # regression workload (§5)
    - if: always()
      run: tools/collect-logs.sh               # serial console, dispatch log, IOMMU event log
```

Non-negotiables: **serialized** (`concurrency.group: gpuhost-gpu`, `cancel-in-progress: false`) because the physical GPU is a singleton and because the PSP wedges after ~1 bring-up per POST; **power-cycle before every run** for the same reason; **`if: always()` log collection** so a wedge produces a diagnosable artifact instead of a bare timeout. This job is a *canary*, not a gate — a red nightly-HW run files an issue and pages the operator; it does not block merges (a gpu-host wedge is an environment fault, not a code regression). The blocking gates are `proof`, `build`, `test-host`, `test-kani`, `test-qemu`.

---

## 3. The red-team DMA harness (M3/M4) — empirical corroboration, not proof

**Purpose.** The DMA-reach theorem (V3) is only as true as its hardware axioms A1 (AMD-Vi faithfully enforces the tables) and A2 (gfx1201 issues no DMA that bypasses AMD-Vi). Verus cannot discharge A1/A2 — no Rust tool can (decision doc §4 hard-limit #1, R2). The red-team harness is the *empirical* check on those axioms: a deliberately-hostile `lite::` build tries to DMA out of the granted domain, and we assert the (emulated at M3, real at M4) AMD-Vi faults **exactly where the proof's `authorized` set says it should.** A pass corroborates A1/A2; it never *proves* them. A surprise (a DMA that should fault but doesn't, or vice-versa) means either the model's `authorized` set is wrong (a proof bug we can fix) or the hardware has a bypass (axiom A2 is false — the ceiling the decision doc warns about). Both are exactly the failure modes we cannot prove away, so we test for them.

**Why this is M3+ only.** Under plain VFIO (M0–M2) the host pins *all* guest RAM to the device (A7): an "out-of-bounds" write that still lands inside guest RAM does **not** fault, because the nucleus's domain isn't load-bearing yet. Running the harness at M0–M2 would produce a green "no fault" that means nothing. The harness is `#[cfg(feature = "redteam")]` and its runner refuses to execute unless the guest booted with a vIOMMU present (M3) or on bare metal (M4).

**The malicious `lite::` build.** A build-time switch on the (still C++) driver — `-DLITE_REDTEAM_OOB_DMA=<mode>` (mirrored as a cargo feature `redteam` if/when the driver is Rust-ported). After a *normal* bring-up it deviates in exactly one way, one mode per test:

| mode | what the hostile driver does | proof-predicted result |
|---|---|---|
| `inbounds` | writes to `granted_base + 0x1000` (control: a legal grant) | **completes**; VRAM/GTT changes as expected |
| `past_grant` | programs GPUVM so the outbound IOVA = `granted_base + granted_size + 0x1000` (one page past the grant) | **AMD-Vi IO_PAGE_FAULT**, DeviceID = gfx1201 BDF, faulting addr = predicted IOVA; target memory unchanged |
| `foreign_frame` | targets a system IOVA the nucleus granted to a *different* domain | **fault** (cross-domain reach is outside `authorized`) |
| `post_revoke` | writes to a frame that was granted then `revoke`d before the dispatch | **fault** (V2/V3: revoke removes reachability) |
| `stale_iotlb` | (M4 only) writes in the window after `unmap` but before IOTLB flush | **fault** (V5 reclaim-safety: no stale-translation window) |

Because GPUVM only ever *produces IOVAs* (U3), the hostile driver can scribble GPUVM however it likes — the transaction's outbound IOVA is still walked by the nucleus-owned AMD-Vi tables. That is the whole point of the two-translation-layer design; the harness is what demonstrates it on real silicon.

**The fault oracle.** For each mode the harness computes the *predicted* verdict from `model/`'s `reach()`/`authorized()` (the same reference model the Verus spec mirrors, §2.4) and compares to the *observed* verdict:

- **Observed, emulated (M3):** QEMU raises an IOMMU event; capture it two ways — the nucleus's own IOMMU event-log ring (the code path M4 will use for real), and QEMU tracepoints (`-trace 'amdvi_*'` for emulated AMD-Vi, `-trace 'virtio_iommu_*'` for virtio-iommu) as an independent cross-check. Assert both agree with the prediction.
- **Observed, bare metal (M4):** the nucleus owns the physical AMD-Vi MMIO, including the Event Log base register and the IO_PAGE_FAULT event format. The harness reads the event log, extracts DeviceID + faulting address + access type, and asserts DeviceID == the gfx1201 BDF and address == the predicted IOVA. No emulator in the loop.

**Verdict matrix.** The harness runs all modes and emits a table `mode → {predicted, observed, agree?}`. Any `agree? = false` is a hard failure and — critically — is triaged as *either* a `model/`+spec bug *or* a suspected axiom violation, and the triage outcome is recorded in the residual-axiom register (decision doc M5 / §6). This is corroboration wired into the assurance case, not a green checkmark that stands alone.

---

## 4. The M3 emulated-vIOMMU setup — and its open question

M3 is the first milestone where the nucleus's IOMMU code is load-bearing, against an **emulated** IOMMU (axiom A8 — emulator fidelity now in the TCB). The guest nucleus programs IOMMU tables; QEMU shadows guest map/unmap into the host's real VFIO domain, so a guest-side "unmapped" IOVA actually faults on the physical device.

**The three candidate emulated IOMMUs, and the real tension:**

| QEMU device | VFIO passthrough (shadow to host) supported? | Table format the nucleus programs | Fit for our proof |
|---|---|---|---|
| `-device amd-iommu,intremap=on,device-iotlb=on` | **Historically weak/partial for *assigned* devices** — assigned-device DMA translation needs the AMD-Vi equivalent of intel-iommu's `caching-mode=on` (trap guest IOTLB invalidations, replay into VFIO MAP/UNMAP). **Must be confirmed on gpu-host's QEMU (§6).** | **Real AMD-Vi DTE + I/O page tables** — exactly the format V3/V4 verify and M4 will program on bare metal. | Ideal *if* it works with VFIO; otherwise unusable end-to-end. |
| `-device intel-iommu,caching-mode=on,intremap=on` | **Mature** — the first vIOMMU to support VFIO assigned-device shadowing. | VT-d format — **wrong ISA**; the nucleus would program tables we don't verify. | Rejected for the load-bearing path (defeats the point of an AMD-Vi proof). |
| `-device virtio-iommu-pci` | **Supported** with VFIO passthrough; guest MAP/UNMAP shadowed to host VFIO via the kernel. Portable, version-robust. | Paravirtual virtio-iommu descriptors — **not AMD-Vi tables**. | Exercises reach *semantics* end-to-end but not the AMD-Vi table-builder. |

**The honest resolution (a judgment call the team must confirm, decision doc R3).** Split the M3 claim into two provably-separable halves:

1. **AMD-Vi table-builder correctness (V3/V4, fully verified + golden-tested).** The nucleus builds DTE + I/O page tables in *real AMD-Vi format*; `model/`'s reference walker walks them and the Verus theorem proves `reach ⊆ authorized` over that format. This half needs **no** emulator — it's proven statically and golden-vector tested on the host (§2.4). It transfers *unchanged* to M4 bare metal.
2. **End-to-end fault plumbing (empirical, the §3 harness).** "Does an out-of-bounds DMA actually fault?" is tested against whichever emulated IOMMU gpu-host's QEMU *actually supports for VFIO* — **preferably `amd-iommu` if it works**, falling back to `virtio-iommu` if it doesn't. If we fall back, we accept that the emulated device's table *format* differs from AMD-Vi; the load-bearing proof (half 1) is still over AMD-Vi tables, and the emulator only corroborates the *reach semantics* (a weaker but honest M3 claim). The AMD-Vi-format end-to-end fault demonstration then waits for M4 real hardware.

This keeps V3/V4 meaningful even in the pessimistic case where amd-iommu+VFIO doesn't work on gpu-host, and it names the fallback explicitly instead of pretending M3 is a full hardware demonstration. **Open question owed a concrete answer before M3 starts (§6): can gpu-host's QEMU present an `amd-iommu` that translates for the passthrough gfx1201?** If yes, M3 is clean. If no, M3 runs half-1-verified + half-2-corroborated-via-virtio-iommu, and the first *AMD-Vi-format* end-to-end fault is an M4 deliverable.

**libvirt wiring for M3.** The `<hostdev>` gfx1201 passthrough stays; add the vIOMMU. For raw QEMU (easier to iterate than libvirt XML for this): `-machine q35,accel=kvm -device amd-iommu,intremap=on,device-iotlb=on -device vfio-pci,host=$GPU,iommu_platform=on ...`. For libvirt, an `<iommu model='...'/>` element plus `<driver iommu='on'/>` on the hostdev. Iterate on raw QEMU first; freeze the working invocation into `rustproof-guest-m3.xml`.

---

## 5. Reproducibility — one-command M0 + tri_os_smoke regression

**One-command M0.** `tools/repro-m0.sh` on the operator workstation (which has BMC + SSH reach to gpu-host) does the whole known-good baseline, so a fresh engineer can confirm the rig before touching nucleus code:

```bash
tools/repro-m0.sh
#  1. tools/gpuhost-powercycle.sh          # BMC cold cycle -> fresh VBIOS POST
#  2. ssh gpu-host start-gpu-vm.sh   # VFIO bind + no-FLR reset_method + virsh start
#  3. tools/deploy-nucleus.sh             # rustproof.iso -> guest boot; boot the nucleus
#  4. in-guest: lite:: bring-up (phase-9 PSP/SOS->GFX->MEC->MES->scheduler) then
#     multi_dispatch_test <N>             # the M0 workload (below)
#  5. assert: BOOTLOAD_COMPLETE -> completed wave -> "SURVIVED N dispatches; verify=PASS"
#  6. always: collect serial + dispatch logs to artifacts/
```

**The M0 workload is the existing multi-dispatch test**, reused unchanged: the internal `multi_dispatch_test.cpp` launches a trivial `inc` kernel N times through the ROCr/`lite::` MES path, synchronizing each iteration (like torch's `.item()`), copies back, and prints `SURVIVED N dispatches; verify=PASS`. It is the smallest thing that proves the untrusted-driver-over-nucleus architecture physically dispatches a real wave — exactly the M0 exit criterion. The driver bring-up retry logic (KIQ activation is flaky across cold boots) is already encoded in the internal `run-multi-dispatch-test.sh`; port its retry loop into `tools/run-m0-workload.sh` but drive the reset via the BMC power-cycle, respecting the one-bring-up-per-POST reality.

**Everything a repro depends on is pinned (§0):** rustc (stable 1.95.0), Verus+Z3 (unbuilt), QEMU version, the libvirt XML, and the firmware-blob SHA-256 manifest. A repro that can't match `boot/firmware.sha256` refuses to run rather than silently using a different PSP/SMU/MES blob.

**A portable smoke runner as the regression workload.** The internal `tri_os_smoke.py` is a portable PyTorch smoke runner used across the tri-OS effort; on the verified host it is the higher-level regression that a real ROCm workload still runs. `tools/run-tri-os-smoke.sh` invokes it in-guest against the SDK dist, e.g.:

```bash
SMOKE_FILE=$SDK/smoke-tests/pytorch_smoke_test.py \
SMOKE_SELECT='test_rocm_available or TestMatrixOperations' \
SMOKE_JUNIT=/shared/tri_os_smoke_junit.xml \
python3 tri_os_smoke.py
```

Parse its machine-readable `[tri_os_smoke] RESULT passed=… failed=… total=…` line into the CI summary. Two integration points:
- **M4 exit criterion** (decision doc): the smoke workload runs on the verified bare-metal host — so `run-tri-os-smoke.sh` is the M4 acceptance workload, not just a smoke test.
- **`SMOKE_ISOLATE=1` per-test-subprocess mode** matters here specifically: a GPU fault in one op becomes a clean FAIL for that op (and a fresh process resets clr's per-process error latch) instead of aborting the run. On a from-scratch nucleus where a single hostile/buggy dispatch could wedge the driver process, isolate mode is what keeps the regression run diagnosable — use it as the default on the HW job. Note the driver process restart still costs a bring-up; on gpu-host with the PSP-wedge reality, isolate mode across many tests may exhaust the one-bring-up-per-POST budget, so either keep the isolated slice small or power-cycle between chunks. Confirm the actual per-POST bring-up budget on gpu-host (§6) before choosing the slice size.

---

## 6. gpu-host facts to confirm *first* (before M3/M4 planning is real)

These are the decision doc's next-action #2 (R2/R3/R4) turned into commands you run on the box **today**. Until they're answered, M3/M4 scheduling is speculative.

**(a) ACS on the PCIe path (gates A2 — no unmediated P2P).**
```bash
lspci -nn | grep -i 1002:7551                     # find the gfx1201 BDF
lspci -vvv -s <BDF> | grep -iA2 'Access Control'  # ACSCtl on the upstream bridge(s)
ls -l /sys/kernel/iommu_groups/*/devices/ | grep <BDF>   # must be a SINGLETON group
grep -o 'pcie_acs_override[^ ]*' /proc/cmdline    # MUST be empty — an ACS override fakes isolation
```
Confirm the gfx1201 is alone in its IOMMU group *without* any `pcie_acs_override`. If ACS is faked, the P2P-containment axiom (A2) is empirically unfounded and the DMA-reach proof's boundary leaks — a go/no-go input for M4.

**(b) ReBAR / BAR-size posture (gates MAP_BAR sizing + lite:: allocator assumptions).**
```bash
lspci -vv -s <BDF> | grep -iA1 'Region 0'          # BAR0 size (VRAM aperture)
lspci -vv -s <BDF> | grep -iA3 'Resizable BAR'     # ReBAR capability + current size
```
Confirm whether x86 passthrough on gpu-host uses a full VRAM-size BAR (contrast a known constrained 256 MB / ReBAR-off BAR path). This sizes the `IOC_MAP_BAR` aperture in the frozen host contract and decides whether `lite::`'s VRAM bump-allocator assumptions hold unchanged.

**(c) Can QEMU present a nested/emulated AMD-Vi for the passthrough device? (gates the whole M3 shape — §4).**
```bash
qemu-system-x86_64 --version                                  # record it; vIOMMU behaviour is version-bound
qemu-system-x86_64 -device help 2>&1 | grep -iE 'iommu'       # amd-iommu / intel-iommu / virtio-iommu present?
qemu-system-x86_64 -device amd-iommu,help 2>&1                 # does it expose the props we need?
dmesg | grep -i 'AMD-Vi'                                       # host AMD-Vi actually initialised
grep -o 'amd_iommu=[^ ]*\|iommu=[^ ]*' /proc/cmdline          # host posture (expect amd_iommu=on iommu=pt)
ls /sys/class/iommu/                                          # host IOMMU present
```
The decisive test is empirical: boot a throwaway guest with `-device amd-iommu,intremap=on,device-iotlb=on` **and** the gfx1201 as `vfio-pci`, and check whether guest-side map/unmap actually translates for the assigned device (i.e. an unmapped guest IOVA faults on real DMA). If yes → M3 is clean AMD-Vi end-to-end. If no → M3 falls back to virtio-iommu for the fault-plumbing half while V3/V4 stay AMD-Vi-format-verified statically (§4), and the AMD-Vi-format end-to-end demonstration moves to M4.

**(d) Operational facts to confirm (gates the HW loop's cadence).**
- The BMC out-of-band power-cycle path from the operator workstation works and returns the box to a POSTed, SSH-reachable state (`tools/gpuhost-powercycle.sh`).
- The empirical **per-POST bring-up budget**: is it really ~1 before the PSP wedges, or can a warm re-bring-up sometimes succeed? This sets how many isolated `tri_os_smoke` tests fit per power cycle (§5) and how the nightly-HW job chunks its work.

Record all answers in `docs/gpuhost-facts.md` (a living file) and gate the M3/M4 milestone entries on them — a red answer to (a) or (c) changes the plan, not just the schedule.

---

## Appendix — job-to-milestone-to-property map

| Milestone | Blocking CI | HW (nightly) | Machine-checked property | Isolation enforced by |
|---|---|---|---|---|
| **M0** | build, fmt, clippy, test-qemu | repro-m0 + multi_dispatch_test | none (feasibility) | host VFIO (A7) |
| **M1** | + proof (V1), test-kani | — | nucleus core UB-free (V1) | host VFIO (A7) |
| **M2** | + proof (V2) | multi_dispatch still green | AS/cap non-interference (V2) | host VFIO (A7) |
| **M3** | + proof (V3/V4), test-host golden-vectors, **redteam** | redteam matrix vs emulated IOMMU | DMA-reach + DTE-config (V3/V4) | emulated vIOMMU (A8) |
| **M4** | + proof (V5) | redteam vs **real** AMD-Vi, tri_os_smoke | reclaim/stale-IOTLB safety (V5) | **bare-metal AMD-Vi (verified)** |
| **M5** | + proof (V6, opt V7) | full tri_os_smoke regression | no-authority-amplification (V6), submission well-formedness (V7) | composed assurance case |

*Internal assets referenced (the `lite::` driver's HW-validation harness — not in this repo): `start-gpu-vm.sh`, `multi_dispatch_test.cpp`, `run-multi-dispatch-test.sh`, `tri_os_smoke.py`. In-repo companions: [`implementation-plan.md`](implementation-plan.md), [`research-brief.md`](research-brief.md).*

