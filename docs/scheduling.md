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
  page tables map and only invoke authority its own `CapSpace` holds.

## Milestones

| Milestone | Goal | State |
|---|---|---|
| **x86-M1** ✅ | Cooperative multi-process: `YIELD` syscall, N processes each isolated (own AS + caps), round-robin, run to `EXIT`. Generic; x86 + RISC-V. | done |
| **x86-M2** | Preemptive: PIT/APIC timer interrupt drives the scheduler; a compute-bound process is time-sliced without cooperating. x86 only. | next |
| **x86-M3** | Inter-process IPC over the `ipc` endpoints across address spaces; a real `spawn`. | later |

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
