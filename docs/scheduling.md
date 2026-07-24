# Scheduling & multi-process

How Rustproof runs more than one isolated user process on one CPU, and how the
same machinery reaches preemption. Arch-generic (written against `hal::Arch`);
x86-64 lands first, RISC-V mirrors it, the timer is x86-only for now.

## The model: trap frames + a single kernel stack + one resume path

Every kernel entry from user mode — a `syscall`, later a timer interrupt —
saves the **full** user register state into a `hal::UserFrame` and runs the
scheduler on one kernel stack. There is exactly one way back to user mode:
`Arch::resume(token, &frame)`, which loads the target's page-table root
(`cr3` / `satp`) and returns to ring 3 / U-mode via `iretq` / `sret`, restoring
that frame. The kernel stack is re-entered fresh on each trap; it never parks a
half-finished handler, so a single stack suffices.

```
user (proc A) --syscall--> [stub saves TrapFrame on kstack] --> syscall_trap::<A>(frame)
   copy frame -> PROCS[cur].frame ;  service / YIELD / EXIT ;  pick next
   Arch::resume(PROCS[next].token, &PROCS[next].frame)   // load cr3, iretq -> user (proc B)
```

`YIELD` picks the next ready process (round-robin, reusing `sched::Scheduler`)
and resumes it; value-returning syscalls (`GET_INFO`, …) service under the
current process's page tables (still active — we have not switched yet), write
`rax`, and resume the *same* process; `EXIT` removes the process and resumes the
next, or — when the run queue drains — prints `rustproof: BOOT OK` and halts.

A process is `{ token, frame: UserFrame, caps: CapSpace, active }` in a fixed
`PROCS: [Process; N]` static (no heap). Each process has its **own** address
space (own page-table root, kernel shared in via `share_kernel`) and its **own**
capability space, so `map_bar`/`alloc_vram` are gated per process. First entry
and later resume are identical: a fresh process just starts with a `frame`
synthesized by `Arch::frame_init(entry, sp, arg0)` (arg0 = its id, delivered in
the first-arg register), so `resume` drops it into `_start`.

The `UserFrame` is an opaque fixed-size POD (`[u64; 40]`, room for both ISAs);
each arch casts it to its concrete `TrapFrame` and reads the syscall number/args
and writes the return value through `Arch::frame_{num,arg,set_ret}`. Keeping it
concrete lets the process table be a plain non-generic static.

## Why this shape

- **One resume path, preemption-ready.** A timer ISR saves the identical
  `TrapFrame` (the CPU pushes `rip/cs/rflags/rsp/ss`; the stub pushes the GPRs
  below, matching the layout the `syscall` stub synthesizes). Preemption is then
  "run `syscall_trap`'s scheduler tail from the timer instead of a syscall" — no
  new context-switch mechanism.
- **Per-process address + capability space** is the isolation boundary the whole
  project exists to (eventually) verify: a process can only touch what its own
  page tables map and only invoke authority its own `CapSpace` holds — including
  *who it can talk to*, since IPC endpoints are capabilities (see `ipc-caps`
  below) rather than integers any process could name.

Every authority-granting op now checks BOTH halves of the capability — the object
type and `rights ⊇ need`, as `host-contract.md` specifies: `SEND` needs `WRITE`
and `RECV` needs `READ` on an `Endpoint`; `MAP_BAR` needs `READ` on an `Mmio`
(mapping exposes the device's registers); `ALLOC_VRAM` and `SPAWN` need `WRITE`
on an `Untyped` (both carve memory out of it). `FREE_VRAM` needs no capability —
releasing your own frame grants no authority — and is ownership-checked instead.
So that the rights half is never vacuously true, `load_process` also mints two
deliberately under-powered caps (a `WRITE`-less `Untyped`, a `READ`-less `Mmio`)
that the demo uses to prove each refusal on real hardware.

The grant *policy* is per-role, not a uniform hand-out: `load_process` gives a
process exactly its role's capability table (`grants_for`), so a **producer**
holds `WRITE` on the shared endpoint and nothing else — it cannot receive on the
endpoint it sends to, map a device, allocate, or spawn — a **consumer** holds
only `READ` there, and a **worker** gets the device/memory authority but a
no-rights placeholder on the shared endpoint, so possession of a slot is not
authority. Cap ids stay aligned across roles because `insert` fills a fresh space
in order. The demo asserts each of those refusals on hardware.

Authority is never derived from a table index. Slots are recycled by `EXIT` /
`SPAWN`, so `load_process` takes the role as an explicit parameter: the boot
policy (`boot_role`) applies only to the initial, never-recycled ids, and a
`SPAWN`ed process is always a `Worker`, chosen at the spawn site. Deriving it
from the slot instead would let a worker spawn into the exited producer's slot
and receive `WRITE` on the shared endpoint — authority no worker holds, i.e. a
principal minting a stronger principal. Identity is likewise separate from the
slot (a monotonic counter), so a slot's current and former occupants can never be
conflated. The demo spawns late, into a deliberately recycled slot, and asserts
the child is still a worker.

Honest scope on what remains: the role table is a static boot policy, and
capabilities are only ever granted at load time — there is no delegation yet, so
a parent cannot hand (or attenuate) one of its own caps to a child at `SPAWN`.
`capabilities::derive` already provides the authority-monotonic primitive that
needs; wiring it through `SPAWN` is the next step.

Load-bearing detail: an x86 interrupt/exception gate clears `IF`/`TF` but **not**
`DF`, and `std` is unprivileged — so a ring-3 process can enter the kernel with
`DF=1`. The Rust handler's `rep movs`-lowered copies (e.g. the trap-frame save)
assume the SysV `DF=0` invariant, so every interrupt/exception stub `cld`s on
entry (the `syscall` path gets this from `FMASK` bit `0x400`). Omitting it lets
untrusted code corrupt kernel memory; the `init` demo deliberately runs its
compute loop with `DF=1` as a standing regression test.

## Milestones

| Milestone | Goal | State |
|---|---|---|
| **x86-M1** ✅ | Cooperative multi-process: `YIELD` syscall, N processes each isolated (own AS + caps), round-robin, run to `EXIT`. Generic; x86 + RISC-V. | done |
| **x86-M2** ✅ | Preemptive on x86: an 8259 PIC + 8254 PIT timer interrupt (IRQ0 → vector 0x20) drives the scheduler; a compute-bound process is time-sliced without cooperating. The timer ISR builds the identical `TrapFrame` and reuses `resume`. | done |
| **riscv-timer** ✅ | Preemptive on RISC-V too: the Sstc `stimecmp` supervisor timer (`scause` int code 5) routes to the same generic `preempt_trap`. Both arches now time-slice. | done |
| **x86-M3a** ✅ | Cross-address-space IPC: `SEND`/`RECV` synchronous 1-word rendezvous with process blocking (`ProcState` + run-queue add/remove), deadlock detection. Generic; x86 + RISC-V. | this change |
| **x86-M3b** ✅ | A real `SPAWN` syscall (Untyped-cap-gated): load the embedded image into a fresh process at runtime, with full frame reclamation on `EXIT` (a spawn/exit cycle leaks no address space). Generic; x86 + RISC-V. | done |
| **vram-quota** ✅ | Per-process VRAM quota + `FREE_VRAM`: `ALLOC_VRAM` refuses past the quota (`VRAM_QUOTA_FRAMES`); `FREE_VRAM(phys)` frees an owned frame (ownership-checked — a process can only free its own), returning quota; VRAM tracked separately from AS frames, both reclaimed on exit. Generic; x86 + RISC-V. | done |
| **ipc-caps** ✅ | IPC endpoints are capabilities, not raw integers: `SEND`/`RECV` take a `CapId`, require `CapType::Endpoint` with `WRITE`/`READ` respectively, and rendezvous on the cap's *object* — so two processes meet only when their caps name the same endpoint, and an unauthorized caller gets `NO_CAP` without blocking. Generic; x86 + RISC-V. | done |
| **role-caps** ✅ | Per-role grants: each process is loaded with only its role's capability table (producer = send-only, consumer = receive-only, worker = device/memory but no shared-endpoint authority), so the policy is least-authority rather than uniform. Generic; x86 + RISC-V. | done |

## Alternatives Considered

- **Per-process kernel stacks + a kernel↔kernel context switch (`sched::switch`)
  for `YIELD`.** Each process parks its syscall handler mid-stack and a
  callee-saved context switch resumes another's. Rejected as the foundation: it
  reuses the existing `switch` with less new code for the *cooperative* case, but
  it does **not** generalize to preemption (an interrupt cannot cooperatively
  unwind to a park point), so timer preemption would need the trap-frame model
  anyway. Building it once, here, avoids a throwaway.
- **A generic `enter_user` per syscall (the pre-M1 single-process path) plus an
  ad-hoc second process.** Rejected: no uniform save/restore, so no path to
  preemption and no clean place to store per-process state.
- **Keeping multi-process x86-only, leaving RISC-V on the single-process path.**
  Rejected: it would fork `kernel::run` back into per-arch tails right after the
  HAL unified them. RISC-V already assembles a full trap frame, so mirroring the
  `UserFrame` surface is cheap and keeps one generic scheduler.
- **Heap-allocated process table / green threads.** Rejected: no allocator in the
  nucleus by design; a fixed `[Process; N]` static is enough and keeps the state
  auditable.
