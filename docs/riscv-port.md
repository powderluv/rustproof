# Rustproof — RISC-V (rv64) port plan

> Status: in progress (2026-07-23). RV-M0 (boot + portable core) is being brought up in
> parallel with the x86-64 work. This doc records the architecture-abstraction strategy,
> the RISC-V boot/trap/paging specifics, and the milestone ladder.

## 1. Why this is cheap: most of the kernel is already portable

Rustproof was built as a set of small crates against a shared `abi` contract, and the
majority of them contain **no architecture-specific code**. They compile for
`riscv64gc-unknown-none-elf` unchanged:

| Crate | Portable? | Notes |
|---|---|---|
| `abi` | ✅ portable | addresses, page constants, `MemoryRegion`, `FrameAllocator`, caps, IPC, syscall/host-contract types. `VirtAddr::table_index` returns 9-bit indices — true for x86-64 4-level **and** RISC-V Sv39/Sv48. |
| `mm` | ✅ portable | bitmap frame allocator; pure logic over `MemoryRegion`. |
| `capabilities` | ✅ portable | capability space + monotonic derivation; zero unsafe. |
| `ipc` | ✅ portable | synchronous-endpoint state machine. |
| `hostcontract` | ✅ portable | dispatch is pure over the `HostEnv` trait. |
| `loader` | ⚠️ near-portable | ELF64 loader; only the `e_machine` check is x86-specific (add `EM_RISCV = 243`). |
| `vspace` | ❌ x86-only | x86-64 4-level page tables + PTE format. |
| `sched` | ❌ arch-specific | run queue is portable; the `switch` asm + `Context` are per-arch. |
| `arch-x86_64` | ❌ x86-only | boot trampoline, IDT, GDT/TSS, `syscall`/`sysret`, port I/O serial. |
| `nucleus` (bin) | ❌ x86-only | the x86 kernel image. |

So the RISC-V port adds three things and reuses everything else:

- **`arch-riscv64`** — the RISC-V arch layer (analog of `arch-x86_64`): boot entry, UART
  console, trap handling, machine control, and the "exit" mechanism.
- **`vspace-riscv`** — RISC-V **Sv39** page tables (analog of `vspace`).
- **`nucleus-riscv`** (bin) — the RISC-V kernel image that wires `arch-riscv64` + the
  portable core into a bootable ELF.

## 2. Abstraction strategy

**Now (chosen): separate arch crates + a shared portable core + one nucleus bin per arch.**
Each arch crate exposes the same *shape* of surface (a `serial`/console, `interrupts`
init + trap dump, an `exit`, control-register access, and — later — context switch +
user-mode entry). The portable crates depend only on `abi`. The two `nucleus*` bins are
thin: they call their arch crate and then run the identical portable-core demo/logic.

**Later (planned): a small `Arch` trait (HAL).** Once both arches have the same set of
operations (console, traps, `map`/`activate` for paging, `context_switch`, `enter_user`),
factor them behind a `trait Arch` (or a set of focused traits) and collapse the two
`nucleus*` bins into one `nucleus` generic over `Arch`, selected by `#[cfg(target_arch)]`
at the leaves. This is deferred until the RISC-V surface is proven, so we don't design the
trait against one arch. See *Alternatives Considered*.

## 3. RISC-V boot & runtime specifics

- **Privilege / boot:** on `qemu-system-riscv64 -machine virt -bios default`, the
  **OpenSBI** firmware runs in M-mode, then loads the kernel ELF at **`0x8020_0000`** and
  jumps to `_start` in **S-mode** with `a0 = hartid`, `a1 = DTB pointer`. No mode-switch
  trampoline is needed (unlike x86's 32→64-bit long-mode dance) — we start in 64-bit
  S-mode. `_start` sets `sp`, zeroes `.bss`, and calls `kmain(hartid, dtb)`.
- **Console:** the NS16550A UART at MMIO `0x1000_0000` (poll LSR bit 5, write THR).
  (SBI console via `ecall` is the portable fallback.)
- **Traps:** `stvec` points at a trap vector; `scause` / `sepc` / `stval` describe the
  trap. RV-M0 treats every trap as fatal (dump + exit), mirroring the x86 exception dump.
- **Exit:** the SiFive test finisher at MMIO `0x10_0000` — write `0x5555` to shut QEMU
  down cleanly (success), `0x3333` for failure. (SBI `SYSTEM_RESET` is the alternative.)
- **Paging:** RV-M0 runs in **bare mode** (`satp = 0`, physical addressing). RV-M1 turns
  on **Sv39** (3-level, 4 KiB pages, 39-bit VA; PTE bits `V R W X U G A D` + `PPN`), via
  `vspace-riscv`; the kernel loads `satp = (8 << 60) | root_ppn`.
- **Syscalls / user mode (RV-M2):** U-mode `ecall` traps to S-mode (`scause = 8`); the
  kernel services it and returns with `sret` (set `sstatus.SPP = 0`, `sepc = return`).
  This replaces x86's `syscall`/`sysret`; the *contract* (`abi::sysno`, the host-contract
  ops) is identical, only the entry/exit asm differs.

## 4. Milestones

| Milestone | Goal | Reuses | New |
|---|---|---|---|
| **RV-M0** | Boot under QEMU virt (S-mode), UART console, trap dump, run the portable core (frame allocator + capabilities + IPC). | `abi`, `mm`, `capabilities`, `ipc` | `arch-riscv64`, `nucleus-riscv` |
| **RV-M1** | Sv39 paging: build an address space, map/translate, enable `satp`. | `abi` | `vspace-riscv` |
| **RV-M2** | User mode: `sret` to U-mode, `ecall` syscall entry, the capability-gated host contract (same `hostcontract::dispatch`). | `hostcontract`, `loader` (+`EM_RISCV`) | riscv user-entry + trap syscall path |
| **RV-M3** | Parity with the x86 M0 core + a RISC-V context switch; then factor the `Arch` HAL and unify the nucleus bins. | all | `sched` riscv `switch`; `Arch` trait |

## 5. Toolchain

Stable Rust `1.95.0` + `rustup target add riscv64gc-unknown-none-elf` (tier-2, precompiled
core/alloc — no build-std) + `qemu-system-riscv64`. Same "stable + built-in target" model
as the x86 exec build; the Verus proof track's pinned nightly is orthogonal and shared.
Build/boot: `tools/run-qemu-riscv.sh`.

## 6. Alternatives Considered

- **`#[cfg(target_arch)]` branches inside the existing `nucleus` and `arch` crates.**
  Rejected as the *starting* point: it would interleave RISC-V code into the actively-
  changing x86 files (merge friction during parallel bring-up) and mix two ISAs' `unsafe`
  in one module. Adopted *later* only at the thin leaves once a HAL exists.
- **A big `trait Arch` HAL designed up front.** Rejected for now: designing the trait
  against a single implemented arch tends to bake in x86 assumptions (segmented GDT/TSS,
  `syscall`/`sysret`) that don't fit RISC-V (`ecall`/`sret`, `satp`). We implement RISC-V
  first, *then* extract the trait from the two concrete arch layers (RV-M3).
- **A separate repository / fork for RISC-V.** Rejected: it would fork the portable core
  and lose the single-source reuse that makes this port cheap; the whole point is that
  `mm`/`capabilities`/`ipc`/`hostcontract` are one implementation across arches.

## 7. Honest scope

RV-M0 proves the portable core runs on a second ISA — real, but it is *bare-mode* with no
isolation yet (no paging, no user mode). The isolation guarantees (the whole reason
Rustproof exists) arrive with RV-M1/RV-M2, and formal verification remains gated on the
Verus toolchain exactly as on x86 (see [`ai-proof-writer.md`](ai-proof-writer.md)). RISC-V
is also a strong *verification* target long-term: it has a clean, formally-specified ISA
(Sail model), which is friendlier to a machine-checked hardware model than x86-64.
