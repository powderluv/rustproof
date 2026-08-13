# `docs/repo-structure.md` — Rustproof: Repo Structure, Cargo Workspace, Toolchain & Boot

> Scope of this doc: the on-disk layout, the Cargo workspace, toolchain pinning for reproducible Verus proofs, the bare-metal build targets, the KVM/libvirt boot path on `gpu-host`, how the untrusted C++ `lite::` driver is vendored and hosted, and exactly what compiles at scaffold time vs. what is a stub. It does **not** re-litigate the decided architecture (new ~6–8K-SLOC Rust+Verus nucleus; `lite::` runs untrusted in userspace; Verus + Kani; DMA-reach proof over nucleus-owned AMD-Vi tables becomes load-bearing only at M3/M4).

---

## 0. Two-sentence orientation

The **trusted computing base (TCB)** is a small set of `no_std` library crates. As of 2026-08 it
is `kernel` (the generic nucleus), `capabilities`, `deleg`, `regions`, `runstate`, `vspace`(+riscv),
`mm`, `ipc` and `abi`, and it carries **no Verus proofs yet** — the crates named here are covered
by host unit tests, several exhaustive over their whole small state space (`tools/host-tests.sh`,
which refuses to run if a crate containing `#[test]` is missing from its list). `nucleus-core` and
`iommu-amdvi` were empty stubs that no build depended on, and were DELETED on 2026-08-12; the
remaining empty crates (`userland-rt`, `driver-shim`, `driver-host`) say so in their own doc
comments. This paragraph previously called the set
"verified" and said it "carries Verus proofs", neither of which was ever true; everything that touches raw hardware is quarantined into `arch-x86_64` (Kani-checked, Verus-external) and everything that is a full application — the GPU driver, `init` — runs as **untrusted userland ELFs** the nucleus loads. The physical `lite::` driver stack is vendored as C++ under `vendor/rocr-lite/`, compiled to a static archive, and linked into an untrusted `driver-host` process that reaches the nucleus only through the capability/IPC ABI in the shared `abi` crate.

---

## 1. Repository file tree

Root of the new repository (`github.com/<org>/rustproof`), Cargo virtual workspace:

```
rustproof/
├── Cargo.toml                     # [workspace] virtual manifest, no root package
├── Cargo.lock                     # committed — proof + build reproducibility
├── rust-toolchain.toml            # pins STABLE 1.95.0 (Verus needs no nightly and does
├── .cargo/
│   └── config.toml                # build-std, default target, `runner = tools/run-vm`
├── rustfmt.toml
├── verusfmt.toml                  # formatting for verus!{} blocks
├── deny.toml                      # cargo-deny: lock the dep graph of the TCB
├── xtask.toml                     # xtask config (image name, iso path, domain name)
│
├── targets/                       # custom bare-metal target specs (§3)
│   ├── x86_64-rustproof-kernel.json
│   └── x86_64-rustproof-user.json
│
├── toolchain/                     # pinned verifier toolchain (§2)
│   ├── verus.lock                 # UNFILLED placeholder (REPLACE_ME); Verus declined 2026-08-11
│   ├── fetch-verus.sh             # NOT IMPLEMENTED (exit 1)
│   └── README.md                  # how to reproduce the verifier bit-for-bit
│
├── crates/
│   │
│   │  # ---------- TCB (no_std, ZERO deps; host-tested, NOT verified) ----------
│   ├── kernel/                    # lib: the generic nucleus (this is the real one)
│   │   ├── Cargo.toml             #   deps: vstd, builtin, builtin_macros, abi, capabilities, vspace, ipc
│   │   ├── src/
│   │   │   ├── lib.rs             #   #![no_std]; verus!{} module tree
│   │   │   ├── state.rs           #   abstract kernel state (ghost model of all AS/objects)
│   │   │   ├── invariants.rs      #   spec fn: well-formedness, the isolation predicate
│   │   │   ├── syscall.rs         #   verified syscall dispatch (exec fn w/ requires/ensures)
│   │   │   ├── sched.rs           #   cooperative/round-robin scheduler over threads
│   │   │   └── objects.rs         #   kernel object table (untyped, endpoints, AS, frames)
│   │   └── proofs/
│   │       ├── isolation.rs       #   proof fn: inter-AS non-interference lemmas (M2)
│   │       └── memory_safety.rs   #   proof fn: no aliasing across owned frames (M1)
│   │
│   ├── capabilities/             # lib: capability system (CNode, rights, derivation tree)
│   │   ├── src/lib.rs             #   Cap, CapRights, CapType; verified derive/revoke/mint
│   │   └── (no proofs/ yet — authority monotonicity is asserted exhaustively over the
│   │       rights lattice in crates/abi's tests; there is no derivation tree to prove)
│   │
│   ├── vspace/                   # lib: x86_64 4-level page tables + address-space model
│   │   ├── src/lib.rs             #   PML4/PDPT/PD/PT types, map/unmap, verified
│   │   ├── src/model.rs           #   spec: walk(vaddr) -> Option<(paddr, perms)>
│   │   └── proofs/walk.rs         #   proof: HW page-walk refines the spec walk
│   │
│   ├── ipc/                      # lib: synchronous endpoints, message transfer
│   │   ├── src/lib.rs             #   Endpoint, send/recv/call state machine, verified
│   │   └── proofs/progress.rs     #   proof: no message forged, sender authority preserved
│   │
│   │                              # (iommu-amdvi/ DELETED 2026-08-12 — was an empty stub
│   │   ├── src/
│   │   │   ├── lib.rs             #   DTE, IO-PTE (AMD v1 4-level), IOVA->SPA walk
│   │   │   ├── model.rs           #   spec: reachable_spa(dev, iova) set  ── the CRUX
│   │   │   └── program.rs         #   #[verifier::external_body] register pokes -> hal
│   │   └── proofs/
│   │       └── dma_reach.rs       #   proof: reachable_spa(dev) ⊆ frames_owned_by(dev's AS)
│   │                              #          (load-bearing at M3/M4; stubbed w/ admits at M0)
│   │
│   │  # ---------- UNSAFE HARDWARE STUB (Kani, Verus-external) ----------
│   ├── arch-x86_64/              # lib: ALL raw hardware primitives live here
│   │   ├── src/
│   │   │   ├── lib.rs             #   #[verifier::external] whole crate
│   │   │   ├── mmio.rs            #   volatile read/write, MMIO windows
│   │   │   ├── msr.rs             #   rdmsr/wrmsr wrappers
│   │   │   ├── port.rs            #   in/out port I/O (serial, PCI cfg)
│   │   │   ├── cpu.rs             #   cr0/cr3/cr4, cpuid, hlt, control-flow asm
│   │   │   ├── gdt.rs, idt.rs     #   descriptor tables + trap entry trampolines (asm!)
│   │   │   └── boot.rs            #   Limine request structs / entry glue
│   │   └── kani/
│   │       └── mmio_bounds.rs     #   #[kani::proof] MMIO offset never leaves the window
│   │
│   │  # ---------- SHARED ABI (no_std, both sides of the trust boundary) ----------
│   ├── abi/                      # lib: syscall numbers, IPC message layout, cap indices
│   │   ├── src/lib.rs             #   #![no_std]; #[repr(C)] structs; used by kernel+user
│   │   ├── build.rs              #   cbindgen -> generates include/rustproof_abi.h
│   │   └── include/rustproof_abi.h  # generated C header consumed by the C++ driver
│   │
│   │  # ---------- KERNEL IMAGE (the bootable binary) ----------
│   ├── nucleus/                  # bin: links the verified libs into the guest kernel ELF
│   │   ├── Cargo.toml             #   [[bin]] name = "nucleus"; deps: nucleus-core, arch-x86_64, abi
│   │   ├── build.rs              #   emits linker script path
│   │   ├── linker-kernel.ld       #   higher-half load address, ELF sections
│   │   └── src/main.rs            #   #![no_std] #![no_main]; Limine entry -> init nucleus-core
│   │
│   │  # ---------- UNTRUSTED USERLAND ----------
│   ├── userland-rt/             # lib: EMPTY PLACEHOLDER (intended: userland runtime)
│   │   └── src/lib.rs            #   syscall stubs (from abi), talc allocator, panic=abort
│   │
│   ├── init/                    # bin: root task; loads driver-host, wires caps, runs M0
│   │   ├── src/main.rs           #   parse boot module list, spawn driver-host, grant caps
│   │   └── linker-user.ld
│   │
│   ├── driver-host/             # bin: UNTRUSTED process that hosts the C++ lite:: driver
│   │   ├── Cargo.toml
│   │   ├── build.rs             #   cc/cmake: compile vendor/rocr-lite -> static lib, link it
│   │   └── src/main.rs          #   receive GPU MMIO/DMA/IRQ caps, hand to C shim, run wave
│   │
│   └── driver-shim/            # lib (staticlib, C ABI): libc/POSIX subset for the driver
│       ├── src/lib.rs           #   mmap/munmap/open/ioctl/pthread/clock -> nucleus IPC
│       └── include/shim.h        #   C header the vendored driver #includes instead of <...>
│
├── vendor/
│   ├── rocr-lite/               # git submodule: subset of rocm-systems ROCr lite:: driver
│   │   ├── amd_lite_direct_queue.cpp   (+ the gfx1201 dispatch core)
│   │   └── CMakeLists.txt        #   builds a static libamd_lite.a against driver-shim
│   └── limine/                  # git submodule: pinned Limine bootloader binaries (§3)
│
├── tools/                       # host-side build/verify/run helpers (std, run on gpu-host)
│   ├── xtask/                   # bin: `cargo xtask build|image|run|verify` orchestrator
│   │   └── src/main.rs
│   ├── mkimage/                 # builds the Limine boot ISO (nucleus + init + driver-host)
│   ├── run-vm/                  # wraps libvirt/start-gpu-vm.sh to boot the ISO w/ passthrough
│   └── verify/                  # NOT IMPLEMENTED (prints 'verify: not yet implemented')
│
├── libvirt/
│   ├── rustproof-gpu.xml         # domain template: direct-boot ISO + gfx1201 <hostdev>
│   └── start-rustproof-vm.sh     # thin fork of start-gpu-vm.sh (VM=rustproof-gpu)
│
├── ci/
│   ├── verify.yml                # proof job: pinned toolchain, `cargo xtask verify`
│   └── build.yml                 # build job: image + boot-smoke in nested KVM
│
└── docs/
    ├── repo-structure.md         # (this document)
    ├── decision.md               # architecture decision record
    └── research-brief.md         # cited background
```

**Crate responsibility summary**

The **"Verified by"** column of this table used to read `Verus` for five crates. Nothing in this
tree is verified by Verus, which the paragraph at the top of this file already said — the prose was
corrected and the table under it was not, so the document contradicted itself in the place readers
scan. The column now names what actually checks each crate today. Verus was evaluated and declined
on 2026-08-11 with a written reversal condition; see docs/verification.md.

`nucleus-core` and `iommu-amdvi` are **gone**, not merely stale rows. Both were empty crates whose
doc comments called them "VERIFIED TCB" over zero functions and zero tests, and `nucleus-core`
claimed the role — kernel state machine, syscall dispatch, scheduler — that `kernel` actually
fills, so a reader looking for the dispatch could open it and find nothing. Their intent survives
in this file and in docs/verification.md, which is where intent belongs.

| Crate | Trust | Checked today by | Responsibility |
|---|---|---|---|
| `kernel` | TCB | 17 host properties + both QEMU boots | The generic nucleus: syscall dispatch, scheduler, grant tables, authority predicates |
| `capabilities` | TCB | 11 host tests, exhaustive over rights | Capability slots; insert / lookup / revoke |
| `deleg` | TCB | 18 host tests, exhaustive over ≤5-edge forests | Cross-space delegation ledger, transitive revocation |
| `runstate` | TCB | 17 host tests, exhaustive to `MAX_PROCS` slots | Park / deadlock / all-done decision |
| `regions` | TCB | 16 host tests, 10k configs | Shared-region lifetime as an ordered plan |
| `mm` | TCB | 18 host tests incl. arbitrary map shapes | Physical frame allocation; the DMA/general partition |
| `vspace`(+riscv) | TCB | 12 / 14 host tests | Page tables + address-space model |
| `ipc` | TCB | 14 host tests | Endpoints, synchronous transfer |
| `sched` | TCB | 10 host tests on any host, 13 on x86 (3 are `cfg(target_arch = "x86_64")`) | Round-robin run queue; per-arch context switch |
| `arch-x86_64` / `arch-riscv64` | TCB (unsafe) | QEMU boot only | All raw MMIO/MSR/port/asm; no host tests |
| `abi` | boundary | 5 host tests | Shared syscall/IPC layout and the rights lattice |
| `nucleus` / `nucleus-riscv` (bin) | TCB glue | QEMU boot | Link the nucleus into a bootable image |
| `init` / `riscv-init` (bin) | untrusted | QEMU boot (30 assertions/arch) | Root tasks: the demo that exercises the ABI |
| `userland-rt` | untrusted | — | **Empty placeholder.** Intended: Rust userland runtime |
| `driver-host` (bin) | **untrusted** | — | **Empty placeholder.** Intended: host the C++ `lite::` driver |
| `driver-shim` | untrusted | — | **Empty placeholder.** Intended: libc/POSIX subset → nucleus IPC |

---

## 2. Toolchain pinning (why it is non-negotiable)

Verus proofs are **not** reproducible across Z3 versions or Rust nightlies. The SMT solver's quantifier-instantiation and trigger selection change between Z3 releases; a proof that closes in 2 s under one Z3 can time out or report `rlimit exceeded` under another, with no source change. Likewise Verus is built against **one exact** Rust nightly (it ships its own `rust_verify` driver linked to a specific `rustc_private`). So we pin three things together and treat them as a single unit.

**`rust-toolchain.toml`** — copy the channel verbatim from the Verus release's own `rust-toolchain.toml`; do not pick a nightly yourself:

```toml
# rust-toolchain.toml
[toolchain]
# EXACT value comes from the pinned Verus release's rust-toolchain.toml.
# Example only — replace with your Verus release's nightly:
channel = "nightly-2025-03-01"
components = ["rust-src", "rustc-dev", "llvm-tools-preview"]
targets = ["x86_64-unknown-none"]
profile = "minimal"
```

- `rust-src` + `x86_64-unknown-none` target: needed for `-Z build-std` (no prebuilt `core`/`alloc` for our custom target).
- `rustc-dev` + `llvm-tools-preview`: Verus links against `rustc_private`.

**`toolchain/verus.lock`** — pin Verus and its bundled Z3 by exact identity:

```toml
# toolchain/verus.lock
verus_release = "release/0.2025.xx.xx"   # the tagged release you tested against
verus_git_sha = "<40-hex>"               # exact commit
z3_version    = "4.12.5"                 # the Z3 that ships in that release's zip
z3_sha256     = "<sha256 of the z3 binary>"
```

`toolchain/fetch-verus.sh` downloads that exact Verus release zip (which contains `verus`, `rust_verify`, `vstd`, and the matching `z3`), verifies the SHA-256, and installs it under `toolchain/verus/`. Verification **always** invokes that local `z3` (`--z3-path toolchain/verus/z3`) — never a system Z3.

**Verified crates depend on the Verus support crates**, pinned to the same release:

```toml
# crates/nucleus-core/Cargo.toml (excerpt)
[dependencies]
vstd           = { path = "../../toolchain/verus/source/vstd" }
builtin        = { path = "../../toolchain/verus/source/builtin" }
builtin_macros = { path = "../../toolchain/verus/source/builtin_macros" }
```

**Two distinct build modes, one toolchain:**
1. `cargo build` (produces the actual `nucleus`/`init`/`driver-host` binaries): plain rustc on the pinned nightly. The `verus!{ ... }` macro expands to ordinary Rust; `spec fn`/`proof fn` bodies are erased (never emitted as exec code), so proofs cost nothing at runtime.
2. `cargo xtask verify` (the CI gate): runs `toolchain/verus/verus` over each TCB crate with the pinned Z3, `--rlimit` set, and `--record` to dump the proof profile. A green build is **not** a green proof — CI requires both jobs.

`Cargo.lock`, `deny.toml`, `rust-toolchain.toml`, and `verus.lock` are all committed. `cargo-deny` locks the TCB dependency graph so no transitive dep silently enters the verified crates.

---

## 3. Build targets and boot

### 3.1 Custom bare-metal target (kernel)

`targets/x86_64-rustproof-kernel.json`:

```json
{
  "llvm-target": "x86_64-unknown-none",
  "data-layout": "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128",
  "arch": "x86_64",
  "target-endian": "little",
  "target-pointer-width": "64",
  "target-c-int-width": "32",
  "os": "none",
  "env": "",
  "vendor": "rustproof",
  "executables": true,
  "linker-flavor": "ld.lld",
  "linker": "rust-lld",
  "panic-strategy": "abort",
  "disable-redzone": true,
  "code-model": "kernel",
  "features": "-mmx,-sse,-sse2,+soft-float",
  "rustc-abi": "x86-softfloat"
}
```

Rationale for the non-obvious keys:
- `disable-redzone: true` — the System V red zone is unsafe once we take interrupts; trap entry would clobber it.
- `-sse,-sse2,+soft-float` (+ `"rustc-abi": "x86-softfloat"`, the key rustc now requires for softfloat targets) — the kernel never touches the SSE register file, so trap handlers don't have to save/restore it. Float in the kernel is a bug; soft-float makes accidental FP a link error.
- `code-model: "kernel"` — higher-half load; matches the Limine higher-half direct map (HHDM).
- `panic = abort` — no unwinding in `no_std`.

`targets/x86_64-rustproof-user.json` is the same minus the kernel-only knobs: hardware SSE **enabled** (the C++ driver and its math need it), `code-model: "small"`, PIE off (`init`/`driver-host` are loaded at fixed base by the loader). Two targets, because the driver must have real FP but the kernel must not.

`.cargo/config.toml`:

```toml
[build]
target = "targets/x86_64-rustproof-kernel.json"

[unstable]
build-std = ["core", "compiler_builtins", "alloc"]
build-std-features = ["compiler-builtins-mem"]

[target.'cfg(target_os = "none")']
runner = "cargo run -p run-vm --"
```

### 3.2 Bootloader choice — Limine (recommended), with a PVH `-kernel` fallback

| Option | What we'd own | Verdict |
|---|---|---|
| **Limine** (submodule, Boot Protocol) | A tiny set of `#[repr(C)]` request structs in `arch-x86_64/boot.rs`. Limine hands us: a normalized **memory map**, a **higher-half direct map (HHDM)** already set up, the RSDP, module list, optional framebuffer — all in long mode. | **Chosen.** Least trusted assembly we have to write and prove-adjacent; stable, documented handoff; first-class under QEMU/KVM. The nucleus is just an ELF Limine loads. |
| `bootloader` crate (rust-osdev) | Nothing, but it *wraps* our kernel in its build and pulls its own loader code into the boot TCB; its `BootInfo` ABI moves between releases. | Rejected — couples our build cadence and image format to an external crate, and puts more third-party code in the boot path. |
| multiboot2 / GRUB | A multiboot2 header + our own long-mode + paging trampoline. | Rejected as primary — GRUB is heavy and multiboot's memory info is clunkier than Limine's; but the *idea* (direct `-kernel` boot) is kept as the fallback below. |

**Why Limine over writing our own trampoline:** M1/M2 want the smallest possible amount of unverifiable early-boot assembly. Limine gets us into long mode with paging and a clean memory map before our first Rust instruction, so `arch-x86_64/boot.rs` is a handful of request structs rather than a mode-switch + paging bring-up we'd have to hand-audit.

**Boot path for the M0 gpu-host libvirt flow: PVH direct-boot.** Emit a **PVH ELF note** (or multiboot2 header) in `nucleus` so QEMU/libvirt can direct-boot it via `<kernel>` / `-kernel nucleus.elf` with **no ISO** — the simplest fit for the passthrough domain, and the M0 plan builds this early memory-map/paging glue as task T0.1. **Limine-on-ISO is retained as the standalone/dev alternative** (behind `--features limine-iso`) for booting outside libvirt; the two paths share everything above the handoff. *(Reconciled 2026-07-21: this section previously made Limine the default; PVH is the default for the gpu-host M0 flow, matching implementation-plan.md §5.)*

### 3.3 How it slots into the existing gpu-host libvirt/QEMU flow

The existing `start-gpu-vm.sh` already does the hard, gfx1201-specific part: it (1) binds the card (`1002:7551`, BDF `0000:03:00.0` + audio `.1`) to `vfio-pci`, (2) **clears `reset_method`** so the VBIOS POST state survives into the guest (the FLR would wipe PSP-SOS/SMU state the bring-up needs), then (3) `virsh start $VM` and waits for guest readiness.

We do **not** touch that logic. We add a Rustproof domain and a thin wrapper:

- `libvirt/rustproof-gpu.xml` — a new domain `rustproof-gpu` that is a clone of the working GPU-passthrough domain with two changes:
  - the OS section boots our image. Primary path: attach `rustproof-boot.iso` (Limine + `nucleus` + `init` + `driver-host` as boot modules) as a CD-ROM/disk and set boot order to it. Fallback path: `<os><kernel>/var/lib/rustproof/nucleus.elf</kernel><cmdline>…</cmdline></os>` (PVH/multiboot direct boot).
  - the **same** `<hostdev>` PCI passthrough block for the gfx1201 (and its audio function) — byte-identical to the Windows/Linux domains so the card is presented the same way.
- `libvirt/start-rustproof-vm.sh` — a fork of `start-gpu-vm.sh` that only overrides `VM=rustproof-gpu` and, since the guest has no SSH, replaces the "wait for SSH" loop with "tail the guest serial console for the `M0: WAVE OK` banner." Everything upstream (VFIO bind + `reset_method` clear + `virsh start`) is reused unchanged.
- `tools/run-vm` (the cargo `runner`) calls `mkimage` to (re)build `rustproof-boot.iso` from the freshly built ELFs, drops it where the domain expects it, then execs `start-rustproof-vm.sh`. So `cargo xtask run` on gpu-host is: build → image → VFIO-bind + boot the guest with the real GPU passed through.

The guest's serial port is wired to a host file/pty in the domain XML; that serial is the only channel the M0 smoke test needs (`nucleus` prints, `init` prints, `driver-host` prints `M0: WAVE OK`).

**M0–M2 isolation note (do not conflate):** under plain VFIO the *host* programs the physical IOMMU, so guest isolation at M0–M2 is host-enforced; our `iommu-amdvi` proof is present but **not yet load-bearing**. It first matters at **M3** (emulated vIOMMU inside the guest) and **M4** (bare-metal, nucleus programs real AMD-Vi). The repo is structured so that turning the crux proof load-bearing is a milestone flag flip in `iommu-amdvi`, not a restructure.

---

## 4. Vendoring and hosting the untrusted C++ `lite::` driver

**Today** the driver is C++ (the ROCr `lite::` direct-queue path, e.g. `amd_lite_direct_queue.cpp`, living in `rocm-systems`) plus **Python probe/harness scripts**. Decision:

- **In-guest runs C++ only.** The Python probes are a *host-side* test harness and stay on gpu-host; no Python interpreter runs inside the nucleus. The in-guest artifact is the compiled C++ dispatch core.
- **The driver is an untrusted userland ELF the nucleus loads.** `vendor/rocr-lite/` is a git submodule pinned to a subset of the ROCr lite path. Its `CMakeLists.txt` builds a **static** `libamd_lite.a`. `crates/driver-host/build.rs` drives that CMake build and links the archive into the `driver-host` binary (target `x86_64-rustproof-user.json`). `driver-host` runs in its own address space with **only** the capabilities `init` grants it: the GPU **BAR/doorbell MMIO window**, one **DMA-capable buffer region**, and an **IRQ endpoint**. It has no capability that reaches any other AS.
- **libc/POSIX shim (`driver-shim`).** The C++ driver expects a libc. We do **not** port a full libc. Instead `driver-shim` provides the *subset the driver actually calls*, each mapped to a nucleus IPC/capability op:

  | POSIX surface the driver uses | Shim implementation |
  |---|---|
  | `mmap`/`munmap` of MMIO & DMA | maps the granted MMIO/DMA **capability** into the process AS (no kernel `mmap` syscall in the Linux sense) |
  | `open`/`ioctl` on `/dev/kfd`, `/dev/dri/*` | replaced by capability-invoke IPC calls carrying the same command payloads (KFD/amdgpu-style ops become messages) |
  | `pthread_*`, TLS | `userland-rt` threads over nucleus scheduler primitives |
  | `clock_gettime`, `nanosleep` | nucleus time syscall |
  | `malloc`/`free` | `talc` heap over a granted anonymous region |
  | `write(2)` to stderr | serial-log IPC |

  The vendored C++ `#include`s resolve to `driver-shim/include/shim.h` (a tiny `<unistd.h>`/`<sys/mman.h>` replacement) plus `abi/include/rustproof_abi.h` (the cbindgen-generated capability/IPC ABI). Net effect: the C++ compiles unmodified against the shim; the `open`/`ioctl`/`mmap` calls it makes are re-pointed at nucleus IPC instead of a Linux kernel.

- **GPUVM stays untrusted.** The driver builds GPUVM page tables and emits **IOVAs only**. It cannot touch the AMD-Vi tables; those belong to the nucleus (`iommu-amdvi`). At M0–M2 the host IOMMU contains any bad IOVA; from M3 the nucleus-owned tables do, and the DMA-reach proof is what guarantees a driver-produced IOVA can only resolve to physical frames owned by the driver's own address space.

---

## 5. What actually `cargo`-builds at scaffold time vs. what is a stub

Goal at scaffold: `cargo xtask build` produces a bootable image, `cargo xtask run` boots it as the `rustproof-gpu` guest on gpu-host and prints `M0: WAVE OK` over serial, and `cargo xtask verify` runs Verus green over the (initially trivial) TCB proofs.

| Component | Scaffold state | Detail |
|---|---|---|
| Workspace / all `Cargo.toml` | **Builds** | virtual workspace, both custom targets, `build-std` wired |
| `abi` | **Builds** | real `#[repr(C)]` syscall/IPC types; cbindgen emits the C header |
| `arch-x86_64` | **Builds** | real MMIO/serial/GDT/IDT/Limine glue; `#[verifier::external]`; one **real** `#[kani::proof]` (MMIO offset stays in-window), rest of Kani harnesses stubbed |
| `nucleus` (bin) | **Builds + boots** | Limine handoff → serial init → GDT/IDT → paging from the Limine memory map → trivial capability table → one IPC round-trip → hand off to `init` |
| `nucleus-core` | **Builds; proofs partial** | real state/`invariants.rs` types; M1 memory-safety lemmas start as `#[verifier::external_body]`/admitted and get discharged during M1 |
| `capabilities`, `vspace`, `ipc` | **Builds; proofs partial** | real types + `spec fn` models compile and pass a *trivial* proof; the substantive lemmas (walk refinement, isolation) are admitted stubs until M1/M2 |
| `iommu-amdvi` | **Builds; proof STUBBED** | real DTE/IO-PTE types and the `reachable_spa` **spec**; `dma_reach.rs` proof is `admit()`-guarded and **not load-bearing** (host IOMMU covers M0–M2). Becomes real at M3/M4 |
| `userland-rt`, `init` | **Builds + runs** | `init` parses the boot module list, spawns `driver-host`, grants the three GPU caps |
| `driver-shim` | **Builds** | shim compiles; the handful of ops are real for the M0 path (map MMIO/DMA cap, IRQ endpoint, log) |
| `driver-host` + `vendor/rocr-lite` | **Builds; single-wave path only** | CMake builds `libamd_lite.a`; `driver-host` links it and runs exactly the "dispatch one wave" flow. The Python probes are **not** in-guest. Broader driver surface is gated behind post-M0 features |
| `vendor/limine` | **Present** | pinned submodule; consumed by `mkimage` |
| `tools/*` (`xtask`, `mkimage`, `run-vm`, `verify`) | **Builds + runs on host** | real ISO build + libvirt boot + Verus invocation |
| `libvirt/*` | **Present** | `rustproof-gpu.xml` + `start-rustproof-vm.sh` (fork of the working script) |

**One-line litmus for "scaffold done":** on gpu-host, `cargo xtask run` boots the nucleus as a KVM guest with the real gfx1201 passed through and the untrusted C++ `lite::` driver dispatches one wave (`M0: WAVE OK` on serial), **while** `cargo xtask verify` is green on the TCB crates whose only obligations so far are well-formedness — with `iommu-amdvi`'s DMA-reach proof present but explicitly admitted and non-load-bearing until M3.

---

### Notes / open items for the builder
- **Staffing gate:** M1 (first real Verus discharge) is blocked until a proof engineer is on; scaffold deliberately leaves the substantive lemmas admitted so the build/boot loop is usable before then.
- **Pin discipline:** never bump `rust-toolchain.toml`, `verus.lock`, or `Cargo.lock` independently — they move as one unit, re-run `cargo xtask verify` on any change, and expect proof churn when Z3 changes.
- **Reset-method trick is load-bearing for boot:** the `reset_method` clear in `start-gpu-vm.sh` is what keeps the gfx1201 POST state alive into the guest; `start-rustproof-vm.sh` must preserve it verbatim or the driver's bring-up assumptions break.

*(The passthrough script `libvirt/start-rustproof-vm.sh` is a fork of the internal, HW-proven `start-gpu-vm.sh`; see [`dev-infra.md`](dev-infra.md) §1.3.)*
